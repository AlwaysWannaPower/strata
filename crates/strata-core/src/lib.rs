//! # strata-core — движок данных воркбенча Strata.
//!
//! Крейт владеет всей логикой данных, которая работает с Polars / Arrow / Parquet.
//! У него сознательно **нет UI-зависимостей**: десктопное приложение (а позже и
//! CLI) сидит поверх этой библиотеки и обменивается с ней только простыми
//! owned-структурами ([`Preview`], [`ImportReport`]).
//!
//! ## Staging philosophy (important, see `PLAN.md`)
//!
//! Модуль реализует **сырой слой (raw/staging)** продукта: `File → Parquet`.
//! «Сырой» **не** значит «байты как есть»: это значит, что мы переносим данные
//! *достоверно* — правильная кодировка (без моджибейка), правильный разделитель,
//! заголовки и типы колонок из инференса типов Polars. Проблемы бизнес-уровня мы
//! здесь **не** чиним (отрицательные суммы, битые email, дубликаты…). Это дело
//! слоя валидации/нормализации, который превращает staging-датасет в ODS
//! (см. `ТЗ.md`: File → Schema → Validate → Normalize → Parquet → ODS). Если
//! моджибейк попал в staging, ни одно правило позже его не починит — поэтому
//! поддержка кодировок живёт здесь, в сыром слое, а не в ODS.
//!
//! ## Formats & encodings supported (M0.1)
//!
//! * Текст с разделителями — CSV/TSV/`;`/`|` (разделитель определяется
//!   автоматически) и
//! * Apache Parquet (нативные колонки, декодирование текста не нужно).
//!
//! Кодировки текстовых файлов: UTF-8 (с BOM и без), UTF-16 LE/BE (BOM),
//! windows-1251, windows-1252. Определение автоматическое: BOM выигрывает,
//! затем строгая проверка UTF-8, затем небольшая кириллическая эвристика между
//! windows-1251/1252. Ручное переопределение («читать как …») запланировано на
//! шаге схемы в M1.

use encoding_rs::WINDOWS_1251;
use polars::prelude::*;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Источники-папки: директория становится одним датасетом из Parquet-частей
/// ([`folder::scan_folder`], [`folder::folder_to_parquet`], [`folder::preview_parts`]).
pub mod folder;
pub use folder::{
    DatasetPart, FileMeta, FolderReport, FolderScan, StagedFile, folder_to_parquet,
    folder_to_parquet_partitioned, list_parts, preview_parts, scan_folder,
};

/// Инференс типов схемы по файлам/папкам ([`schema::schema_from_file`],
/// [`schema::schema_from_folder`]) — милестон «Schemas» из M1b.
pub mod schema;
pub use schema::{
    FolderSchema, SchemaColumn, SchemaConflict, SchemaProposal, schema_from_file,
    schema_from_folder, source_to_parquet_typed, stage_folder_with_schema,
    stage_folder_with_schema_progress,
};

/// Персистентность проекта: `project.toml`, сохранённые схемы, хелперы токенов.
pub mod project;
pub use project::{
    ColumnDef, ProjectMeta, SchemaFile, create_project, delimiter_from_token, delimiter_token,
    encoding_from_token, encoding_token, load_schema, open_project, save_schema, schema_names,
};

/// **Прикладной API**: стабильный фасад, который фронтенды (сегодня — axum web)
/// должны вызывать вместо внутренностей движка.
pub mod api;

/// Чтение Excel (XLSX/XLS) через calamine + CSV-пайплайн движка.
pub mod excel;
pub use excel::read_excel_frame;

/// Модель воркспейса (M1c): конфиг + привязки + scan roots + кандидаты в сущности.
pub mod workspace;
pub use workspace::{
    Binding, WorkspaceConfig, candidate_entity_name, create_workspace, data_dir,
    list_entity_candidates, open_workspace, save_config, schemas_dir, upsert_binding,
};

// ---------------------------------------------------------------------------
// Публичные доменные типы
// ---------------------------------------------------------------------------

/// Тип ошибки на весь крейт.
///
/// Оборачивает три источника ошибок, с которыми может столкнуться движок:
/// файловую систему ОС, сам Polars и декодирование текста (кодировки).
#[derive(Debug, Error)]
pub enum StrataError {
    /// Polars нужен UTF-8-путь; путь, не являющийся валидным UTF-8, использовать нельзя.
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),

    /// Ошибка операционной системы (файл не найден, нет прав доступа, ...).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Ошибка, поднятая движком Polars.
    #[error("data engine error: {0}")]
    Engine(#[from] PolarsError),

    /// Байты файла не удалось декодировать определённой кодировкой.
    #[error("cannot decode file: {0}")]
    Encoding(String),

    /// Колонка партиционирования должна содержать строковые значения (M1b, партиционированная запись).
    #[error("partition column must be a string column: {0}")]
    PartitionColumn(String),

    /// В директории проекта уже лежит `project.toml`.
    #[error("project already exists: {0}")]
    ProjectExists(PathBuf),

    /// TOML проекта/схемы не удалось сериализовать или разобрать.
    #[error("project file error: {0}")]
    ProjectFile(String),

    /// Схема требует тип колонки, который движок не умеет строить.
    #[error("unsupported schema type: {0}")]
    SchemaType(String),
}

/// Удобный алиас, используемый каждой публичной функцией крейта.
pub type Result<T> = std::result::Result<T, StrataError>;

/// С каким видом исходного файла мы имеем дело.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// Текстовый файл с разделителями (CSV, TSV, `;`-разделитель, ...) и
    /// определённым разделителем. Хранится как `char` для удобного показа; Polars хочет `u8`.
    DelimitedText { delimiter: char },
    /// Файл Apache Parquet (колоночный, бинарный).
    Parquet,
    /// Книга Excel (читается первый лист).
    Excel,
}

impl SourceKind {
    /// Короткая человеческая метка для сводок в UI, например `CSV` / `Parquet`.
    pub fn label(&self) -> &'static str {
        match self {
            SourceKind::DelimitedText { delimiter: ',' } => "CSV",
            SourceKind::DelimitedText { delimiter: '\t' } => "TSV",
            SourceKind::DelimitedText { .. } => "Text",
            SourceKind::Parquet => "Parquet",
            SourceKind::Excel => "Excel",
        }
    }
}

/// Факты о происхождении загруженного файла: формат + кодировка.
///
/// Показываются пользователю, чтобы он мог *проверить*, что сырой слой не
/// прочитал его файл молча и неправильно (в этом весь смысл философии staging выше).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    /// Определённый вид файла.
    pub kind: SourceKind,
    /// Определённая кодировка текста, например `"UTF-8"`, `"windows-1251"`, `"UTF-16 LE"`.
    /// Файлы Parquet бинарные: кодировка — `"—"`.
    pub encoding: String,
}

impl Default for SourceInfo {
    fn default() -> Self {
        SourceInfo {
            kind: SourceKind::DelimitedText { delimiter: ',' },
            encoding: String::from("UTF-8"),
        }
    }
}

impl SourceInfo {
    /// Сводка в одну строку вида `CSV · delimiter ';' · windows-1251`.
    pub fn summary(&self) -> String {
        match &self.kind {
            SourceKind::DelimitedText { delimiter } => {
                format!(
                    "{} · delimiter '{}' · {}",
                    self.kind.label(),
                    delimiter,
                    self.encoding
                )
            }
            SourceKind::Parquet => format!("{} · {}", self.kind.label(), self.encoding),
            SourceKind::Excel => format!("{} · {}", self.kind.label(), self.encoding),
        }
    }
}

/// Одна колонка предпросматриваемой таблицы: её имя и выведенный движком Polars
/// тип данных в виде строки (например `"Int64"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    /// Заголовок колонки, как он есть в файле.
    pub name: String,
    /// Строковое представление [`DataType`] Polars, выведенного для этой колонки.
    pub dtype: String,
}

/// Предпросмотр «головы» файла-источника плюс происхождение ([`Preview::source`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// Колонки в порядке файла, с выведенными dtype.
    pub columns: Vec<ColumnInfo>,
    /// До `max_rows` строк в порядке файла; каждая ячейка — строка.
    pub rows: Vec<Vec<String>>,
    /// Чем оказался файл (формат, кодировка).
    pub source: SourceInfo,
}

/// Итог завершённого прогона staging File → Parquet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// Всего записано строк данных (заголовок не считается).
    pub rows: u64,
    /// Сколько колонок записано.
    pub columns: usize,
    /// Сколько исходных файлов прогнано (в M0 всегда 1).
    pub source_files: usize,
    /// Абсолютный путь записанного Parquet-файла, как он показывается пользователю.
    pub parquet_path: String,
    /// Факты о происхождении источника, который был застейджен.
    pub source: SourceInfo,
    /// Сколько Parquet-частей/партиций записано (1 = обычный одиночный файл).
    pub partitions: usize,
}

/// Кодировка текста, которую пользователь может задать вместо автоопределения.
///
/// Автоопределение (BOM → строгий UTF-8 → кириллическая эвристика) верно в
/// большинстве случаев, но не во всех — например, файл windows-1252, байты
/// которого выглядят как windows-1251. Этот enum позволяет пользователю (а
/// позже — сохранённой схеме проекта) сказать «нет, читай это как …». См.
/// [`ReaderOptions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodingChoice {
    /// Обычный UTF-8 (UTF-8 BOM, если он есть, всё равно срезается).
    Utf8,
    /// windows-1251 (кириллица).
    Windows1251,
    /// windows-1252 (западноевропейская / надмножество Latin-1).
    Windows1252,
    /// UTF-16 little-endian (младший байт первым).
    Utf16Le,
    /// UTF-16 big-endian (старший байт первым).
    Utf16Be,
}

impl EncodingChoice {
    /// Человекочитаемое имя, переиспользуется в строке происхождения.
    pub fn label(self) -> &'static str {
        match self {
            EncodingChoice::Utf8 => "UTF-8",
            EncodingChoice::Windows1251 => "windows-1251",
            EncodingChoice::Windows1252 => "windows-1252",
            EncodingChoice::Utf16Le => "UTF-16 LE",
            EncodingChoice::Utf16Be => "UTF-16 BE",
        }
    }
}

/// Переопределения при чтении *текстового* источника. По умолчанию сохраняется
/// авто-поведение сырого слоя из M0.1 (`None` = автоопределение).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderOptions {
    /// Принудительная кодировка текста или `None` для автоопределения.
    pub encoding: Option<EncodingChoice>,
    /// Принудительный разделитель полей или `None` для автоопределения.
    pub delimiter: Option<char>,
    /// Является ли первая строка заголовком с именами колонок. По умолчанию
    /// `true`; снимите галочку для файлов без заголовка (тогда Polars сам
    /// называет колонки `column_0, column_1, …` и считает первую строку данными).
    pub has_header: bool,
}

impl Default for ReaderOptions {
    fn default() -> Self {
        ReaderOptions {
            encoding: None,
            delimiter: None,
            has_header: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Публичный API
// ---------------------------------------------------------------------------

/// Читает не более `max_rows` строк любого поддерживаемого файла ([`SourceKind`]).
///
/// Эквивалент [`preview_source_with`] с [`ReaderOptions`] по умолчанию (авто).
///
/// # Errors
/// Ошибки ввода-вывода, ошибки декодирования (см. [`StrataError::Encoding`]) и
/// ошибки разбора Polars — всё сообщается через [`StrataError`].
pub fn preview_source(path: &Path, max_rows: usize) -> Result<Preview> {
    preview_source_with(path, max_rows, ReaderOptions::default())
}

/// Как [`preview_source`], но учитывает ручные переопределения [`ReaderOptions`]
/// (кодировка / разделитель) для текстовых файлов.
pub fn preview_source_with(
    path: &Path,
    max_rows: usize,
    options: ReaderOptions,
) -> Result<Preview> {
    let (frame, source) = open_any(path, options, Some(max_rows))?;
    Ok(preview_from_frame(&frame, source))
}

/// Стейджит любой поддерживаемый файл в один Parquet-файл (сырой слой).
///
/// Эквивалент [`source_to_parquet_with`] с [`ReaderOptions`] по умолчанию
/// (авто). «Stage» = достоверный перенос: декодировать/типизировать правильно,
/// но не менять значения (см. документацию модуля).
///
/// # Errors
/// Тот же набор ошибок, что и у [`preview_source`].
pub fn source_to_parquet(path: &Path, parquet_path: &Path) -> Result<ImportReport> {
    source_to_parquet_with(path, parquet_path, ReaderOptions::default())
}

/// Как [`source_to_parquet`], но учитывает ручные переопределения
/// [`ReaderOptions`] для текстовых файлов.
pub fn source_to_parquet_with(
    path: &Path,
    parquet_path: &Path,
    options: ReaderOptions,
) -> Result<ImportReport> {
    let (mut frame, source) = open_any(path, options, None)?;
    let rows = frame.height();
    let columns = frame.width();

    let mut file = std::fs::File::create(parquet_path)?;
    let writer = ParquetWriter::new(&mut file);
    writer.finish(&mut frame)?;

    Ok(ImportReport {
        rows: rows as u64,
        columns,
        source_files: 1,
        parquet_path: parquet_path.display().to_string(),
        source,
        partitions: 1,
    })
}

/// Стейджит один файл в *партиционированный* каталог датасета.
///
/// Строки группируются по уникальным значениям `partition_column` (строковая
/// колонка, например `date` = `2026-01-05` или `city` = `Moscow`), и каждая
/// группа пишется в `<dest_root>/<column>=<value>/part-….parquet` — раскладка в
/// стиле Hive, которую нижестоящие инструменты (Polars, DuckDB, …) читают нативно.
///
/// Колонка должна быть `String`-колонкой Polars; всё остальное громко падает с
/// [`StrataError::PartitionColumn`] (партиционированию нужны чистые, известные
/// значения — это забота валидатора/ODS, а не догадка сырого этапа).
pub fn source_to_parquet_partitioned(
    path: &Path,
    dest_root: &Path,
    partition_column: &str,
    options: ReaderOptions,
) -> Result<ImportReport> {
    let (frame, source) = open_any(path, options, None)?;
    let column_series = frame.column(partition_column)?;
    if !matches!(column_series.dtype(), DataType::String) {
        return Err(StrataError::PartitionColumn(partition_column.to_string()));
    }

    // Уникальные значения в порядке первого появления (их мало; ключи партиций низкокардинальны).
    let mut values: Vec<String> = Vec::new();
    for index in 0..column_series.len() {
        if let Ok(value) = column_series.get(index) {
            let text = match &value {
                AnyValue::String(text) => text.to_string(),
                AnyValue::StringOwned(text) => text.to_string(),
                _ => continue,
            };
            if !values.contains(&text) {
                values.push(text);
            }
        }
    }

    std::fs::create_dir_all(dest_root)?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "source".to_string());

    // Запоминаем ширину до цикла: `DataFrame::lazy()` потребляет фрейм,
    // поэтому каждая итерация работает с дешёвой копией.
    let total_columns = frame.width();
    let mut rows_total = 0u64;
    for (index, value) in values.iter().enumerate() {
        // Именование Hive: `<column>=<value>`; санитизируем, чтобы разделитель
        // пути или странный символ не вырвались из директории партиции.
        let dir_name = format!(
            "{}={}",
            sanitize_partition_key(partition_column),
            sanitize_partition_key(value)
        );
        let part_dir = dest_root.join(&dir_name);
        std::fs::create_dir_all(&part_dir)?;

        let group = frame
            .clone()
            .lazy()
            .filter(col(partition_column).eq(lit(value.as_str())))
            .collect()?;
        let mut group = group;
        rows_total += group.height() as u64;

        let part_path = part_dir.join(format!("part-{index:04}-{stem}.parquet"));
        let mut file = std::fs::File::create(&part_path)?;
        let writer = ParquetWriter::new(&mut file);
        writer.finish(&mut group)?;
    }

    Ok(ImportReport {
        rows: rows_total,
        columns: total_columns,
        source_files: 1,
        parquet_path: dest_root.display().to_string(),
        source,
        partitions: values.len(),
    })
}

/// Заменяет символы, небезопасные в именах каталогов/файлов, на `_`.
/// Так Hive-папка партиции (`city=New York` → `city=New_York`) остаётся
/// валидной в любой ОС.
fn sanitize_partition_key(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            other => other,
        })
        .collect();
    if sanitized.is_empty() {
        "_".to_string()
    } else {
        sanitized
    }
}

/// Открывает любой поддерживаемый файл в материализованный фрейм плюс происхождение.
///
/// Центральная точка решения, общая для предпросмотра и staging, чтобы они
/// всегда совпадали:
/// 1. Parquet → нативный скан (без декодирования текста);
/// 2. текст → определяем (кодировку, разделитель) из [`ReaderOptions`] или
///    автоопределением, затем читаем (лениво для чистого авто-UTF-8, иначе
///    декодируем и разбираем).
fn open_any(
    path: &Path,
    options: ReaderOptions,
    max_rows: Option<usize>,
) -> Result<(DataFrame, SourceInfo)> {
    let head = read_head(path, HEAD_BYTES)?;

    if looks_like_parquet(path, &head) {
        let frame = scan_parquet_head(path, max_rows)?;
        let source = SourceInfo {
            kind: SourceKind::Parquet,
            encoding: String::from("— (binary)"),
        };
        return Ok((frame, source));
    }

    if looks_like_excel(path) {
        let frame = read_excel_frame(path, options.has_header, max_rows)?;
        let source = SourceInfo {
            kind: SourceKind::Excel,
            encoding: String::from("— (workbook)"),
        };
        return Ok((frame, source));
    }

    let (charset, delimiter) = resolve_text_parameters(&head, options)?;
    // Ленивый потоковый разбор безопасен, только когда кодировка *не* задана
    // принудительно: явный выбор надо валидировать строго (декодировать весь
    // файл), чтобы неверное переопределение падало громко, а не давало молча моджибейк.
    let stream_if_pure_utf8 = options.encoding.is_none();
    let frame = read_text_frame(
        path,
        charset,
        delimiter,
        options.has_header,
        max_rows,
        stream_if_pure_utf8,
    )?;
    let source = SourceInfo {
        kind: SourceKind::DelimitedText { delimiter },
        encoding: charset.label().to_string(),
    };
    Ok((frame, source))
}

/// Определяет (кодировку, разделитель) для текстового файла: переопределения
/// `ReaderOptions` выигрывают, иначе работает автоопределение (BOM → строгий
/// UTF-8 → кириллическая эвристика).
fn resolve_text_parameters(head: &[u8], options: ReaderOptions) -> Result<(Charset, char)> {
    let charset = match options.encoding {
        Some(choice) => {
            let charset = choice.into_charset();
            // Даже принудительное чтение UTF-8 должно срезать UTF-8 BOM,
            // иначе BOM попадёт в имя первой колонки.
            if charset == Charset::Utf8 && head.starts_with(&[0xEF, 0xBB, 0xBF]) {
                Charset::Utf8Bom
            } else {
                charset
            }
        }
        None => detect_charset(head),
    };

    let delimiter = match options.delimiter {
        Some(delimiter) => delimiter,
        None => detect_delimiter(first_line(&decode_sample(head, charset)?)),
    };

    Ok((charset, delimiter))
}

// ---------------------------------------------------------------------------
// CSV-удобства для обратной совместимости (используются тестами и старыми вызовами)
// ---------------------------------------------------------------------------

/// Удобство только для CSV: [`preview_source`] с эталонными ожиданиями M0.
pub fn preview_csv(path: &Path, max_rows: usize) -> Result<Preview> {
    preview_source(path, max_rows)
}

/// Удобство только для CSV: [`source_to_parquet`].
pub fn csv_to_parquet(csv_path: &Path, parquet_path: &Path) -> Result<ImportReport> {
    source_to_parquet(csv_path, parquet_path)
}

// ---------------------------------------------------------------------------
// Внутреннее: разведка файла
// ---------------------------------------------------------------------------

/// Сколько ведущих байт читаем, чтобы определить формат/кодировку/разделитель.
const HEAD_BYTES: usize = 32 * 1024;

/// Кандидаты в разделители, в порядке предпочтения при равенстве счётчиков.
const DELIMITER_CANDIDATES: [char; 4] = [',', ';', '\t', '|'];

/// Читает до `limit` байт с начала `path`.
fn read_head(path: &Path, limit: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; limit];
    let mut total = 0;
    while total < limit {
        let read = file.read(&mut buf[total..])?;
        if read == 0 {
            break;
        }
        total += read;
    }
    buf.truncate(total);
    Ok(buf)
}

/// Сколько ведущих байт читает *предпросмотр*, когда файл нужно декодировать.
/// Чистые UTF-8-предпросмотры идут ленивым потоком и не буферизуют файл;
/// не-UTF-8 файлы сначала декодируются в UTF-8, поэтому мы ограничиваем эту
/// работу парой строк вместо декодирования многогигабайтного файла ради 50 строк.
const PREVIEW_DECODE_BUDGET: usize = 4 * 1024 * 1024;

/// Обрезает префикс байт по последней целой записи (переводу строки), чтобы
/// усечённое декодирование не разбирало рваную половину строки. Если переводов
/// строки нет вовсе (например, файл в одну строку), берётся весь префикс.
fn clip_to_record_boundary(bytes: &[u8]) -> &[u8] {
    if bytes.is_empty() || bytes[bytes.len() - 1] == b'\n' {
        return bytes;
    }
    match bytes.iter().rposition(|&b| b == b'\n') {
        Some(index) => &bytes[..=index],
        None => bytes,
    }
}

/// Parquet-файл опознаётся по magic-байтам (`PAR1` по смещению 0),
/// расширение — запасная подсказка для усечённых чтений.
/// Книга Excel определяется по расширению (XLSX/XLS).
fn looks_like_excel(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "xlsx" | "xls"))
}

fn looks_like_parquet(path: &Path, head: &[u8]) -> bool {
    if head.starts_with(b"PAR1") {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("parquet"))
}

/// Кодировки, которые умеет читать сырой слой. Определяются по префиксу байт.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Charset {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Windows1251,
    Windows1252,
}

impl Charset {
    /// Человекочитаемое имя для показа в UI.
    fn label(self) -> &'static str {
        match self {
            Charset::Utf8 => "UTF-8",
            Charset::Utf8Bom => "UTF-8 (BOM)",
            Charset::Utf16Le => "UTF-16 LE",
            Charset::Utf16Be => "UTF-16 BE",
            Charset::Windows1251 => "windows-1251",
            Charset::Windows1252 => "windows-1252",
        }
    }
}

impl EncodingChoice {
    /// Переводит публичную, выбираемую пользователем кодировку во внутренний набор декодеров.
    fn into_charset(self) -> Charset {
        match self {
            EncodingChoice::Utf8 => Charset::Utf8,
            EncodingChoice::Windows1251 => Charset::Windows1251,
            EncodingChoice::Windows1252 => Charset::Windows1252,
            EncodingChoice::Utf16Le => Charset::Utf16Le,
            EncodingChoice::Utf16Be => Charset::Utf16Be,
        }
    }
}

/// Определяет кодировку текстового файла по его ведущим байтам.
///
/// Приоритет: BOM (авторитетный) → строгий UTF-8 → небольшая эвристика между
/// windows-1251 и windows-1252 (обе отображают любой байт, так что ни одна
/// никогда не «ошибается»; берём ту, что даёт больше кириллического текста).
fn detect_charset(prefix: &[u8]) -> Charset {
    if prefix.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Charset::Utf8Bom;
    }
    if prefix.starts_with(&[0xFF, 0xFE]) {
        return Charset::Utf16Le;
    }
    if prefix.starts_with(&[0xFE, 0xFF]) {
        return Charset::Utf16Be;
    }
    if std::str::from_utf8(prefix).is_ok() {
        return Charset::Utf8;
    }

    let (as1251, _, _) = WINDOWS_1251.decode(prefix);
    let (as1252, _, _) = encoding_rs::WINDOWS_1252.decode(prefix);
    let cyrillic_1251 = count_cyrillic(&as1251);
    let cyrillic_1252 = count_cyrillic(&as1252);
    if cyrillic_1251 >= cyrillic_1252 {
        Charset::Windows1251
    } else {
        Charset::Windows1252
    }
}

fn count_cyrillic(text: &str) -> usize {
    text.chars()
        .filter(|c| matches!(c, '\u{0400}'..='\u{04FF}'))
        .count()
}

/// Байтовое смещение BOM для `charset`, если он есть. Явное чтение UTF-16
/// валидно *с BOM и без*, поэтому срезаем только то, что реально там есть.
fn bom_len(charset: Charset, bytes: &[u8]) -> usize {
    match charset {
        Charset::Utf8Bom if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) => 3,
        Charset::Utf16Le if bytes.starts_with(&[0xFF, 0xFE]) => 2,
        Charset::Utf16Be if bytes.starts_with(&[0xFE, 0xFF]) => 2,
        _ => 0,
    }
}

/// Декодирует *пробные* байты (могут быть обрезаны посреди символа): потери
/// допустимы, результат нужен только чтобы найти первую строку и разделитель.
fn decode_sample(bytes: &[u8], charset: Charset) -> Result<String> {
    let body = &bytes[bom_len(charset, bytes)..];
    match charset {
        Charset::Utf8 | Charset::Utf8Bom => Ok(String::from_utf8_lossy(body).into_owned()),
        Charset::Utf16Le => Ok(encoding_rs::UTF_16LE.decode(body).0.into_owned()),
        Charset::Utf16Be => Ok(encoding_rs::UTF_16BE.decode(body).0.into_owned()),
        Charset::Windows1251 => Ok(WINDOWS_1251.decode(body).0.into_owned()),
        Charset::Windows1252 => Ok(encoding_rs::WINDOWS_1252.decode(body).0.into_owned()),
    }
}

/// Строго декодирует *весь* файл. Для контента, объявленного UTF-8, случайный
/// невалидный байт становится жёсткой ошибкой (лучше, чем молча вставить
/// U+FFFD в staging-данные).
fn decode_bytes(bytes: &[u8], charset: Charset) -> Result<String> {
    let body = &bytes[bom_len(charset, bytes)..];
    match charset {
        Charset::Utf8 | Charset::Utf8Bom => String::from_utf8(body.to_vec())
            .map_err(|e| StrataError::Encoding(format!("file is not valid UTF-8: {e}"))),
        Charset::Utf16Le => Ok(encoding_rs::UTF_16LE.decode(body).0.into_owned()),
        Charset::Utf16Be => Ok(encoding_rs::UTF_16BE.decode(body).0.into_owned()),
        Charset::Windows1251 => Ok(WINDOWS_1251.decode(body).0.into_owned()),
        Charset::Windows1252 => Ok(encoding_rs::WINDOWS_1252.decode(body).0.into_owned()),
    }
}

/// Первая физическая строка декодированного образца (пустая строка, если её ещё нет).
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

/// Определяет разделитель полей по строке заголовка, игнорируя области в кавычках.
///
/// Возвращает кандидата с наибольшим числом вхождений, а по умолчанию `,` —
/// если не совпало ничего (в файле с одной колонкой разделителя просто нет).
fn detect_delimiter(line: &str) -> char {
    let mut counts = [0usize; DELIMITER_CANDIDATES.len()];
    let mut in_quotes = false;
    let mut prev = '\0';

    for ch in line.chars() {
        if ch == '"' && prev != '\\' {
            in_quotes = !in_quotes;
        }
        if !in_quotes {
            if let Some(idx) = DELIMITER_CANDIDATES.iter().position(|&d| d == ch) {
                counts[idx] += 1;
            }
        }
        prev = ch;
    }

    let (best_idx, _) = counts
        .iter()
        .enumerate()
        .max_by_key(|(_, count)| *count)
        .unwrap_or((0, &0));
    if counts[best_idx] == 0 {
        DELIMITER_CANDIDATES[0]
    } else {
        DELIMITER_CANDIDATES[best_idx]
    }
}

// ---------------------------------------------------------------------------
// Внутреннее: чтение в DataFrame
// ---------------------------------------------------------------------------

/// Читает текстовый файл с разделителями во фрейм, выбирая правильный путь:
/// * чистый автоопределённый UTF-8 → ленивый поток прямо из файла (масштабируемо);
/// * всё остальное (включая любую *принудительную* кодировку) → сначала
///   декодируем весь файл в UTF-8, затем разбираем из памяти. Принудительные
///   кодировки валидируются строго намеренно: неверное переопределение должно
///   падать громко, а не давать моджибейк.
///
/// `max_rows: None` читает всё (staging); `Some(n)` читает предпросмотр.
fn read_text_frame(
    path: &Path,
    charset: Charset,
    delimiter: char,
    has_header: bool,
    max_rows: Option<usize>,
    stream_if_pure_utf8: bool,
) -> Result<DataFrame> {
    if charset == Charset::Utf8 && stream_if_pure_utf8 {
        read_text_lazy(path, delimiter, has_header, max_rows)
    } else if let Some(rows) = max_rows {
        // Предпросмотр не-UTF-8 файла: декодируем только ограниченный префикс
        // (обрезанный по целой записи), а не весь файл — та самая экономия
        // памяти и CPU, что делает дешёвым взгляд на экспорт cp1251 в 2 ГБ.
        let bytes = read_head(path, PREVIEW_DECODE_BUDGET)?;
        let bytes = clip_to_record_boundary(&bytes).to_vec();
        let text = decode_bytes(&bytes, charset)?;
        read_text_from_buffer(text, delimiter, has_header, Some(rows))
    } else {
        // Полное staging-чтение: весь файл, строго декодированный.
        let bytes = std::fs::read(path)?;
        let text = decode_bytes(&bytes, charset)?;
        read_text_from_buffer(text, delimiter, has_header, None)
    }
}

/// Проводит чисто UTF-8 файл с разделителями через ленивый движок.
///
/// `max_rows: None` означает «читать всё» (использует staging); `Some(n)`
/// ограничивает разбор (используют предпросмотры), чтобы мы никогда не читали
/// огромный файл целиком ради показа 50 строк.
fn read_text_lazy(
    path: &Path,
    delimiter: char,
    has_header: bool,
    max_rows: Option<usize>,
) -> Result<DataFrame> {
    let lazy = LazyCsvReader::new(to_plref_path(path)?)
        .with_has_header(has_header)
        .with_n_rows(max_rows)
        .map_parse_options(|options| options.with_separator(delimiter as u8))
        .finish()?;
    Ok(lazy.collect()?)
}

/// Разбирает уже декодированный UTF-8-текст из буфера в памяти (жадно).
///
/// Используется, когда файл нужно было перекодировать; Polars умеет разбирать
/// только UTF-8, поэтому мы отдаём ему `Cursor` над декодированными байтами.
/// В M0.1 здесь читаются файлы целиком; потоковая обработка кусками для очень
/// больших не-UTF-8 файлов — отдельный милестон.
fn read_text_from_buffer(
    text: String,
    delimiter: char,
    has_header: bool,
    max_rows: Option<usize>,
) -> Result<DataFrame> {
    let options = CsvReadOptions::default()
        .with_has_header(has_header)
        .with_n_rows(max_rows)
        .with_parse_options(CsvParseOptions::default().with_separator(delimiter as u8));
    let reader = options.into_reader_with_file_handle(Cursor::new(text.into_bytes()));
    Ok(reader.finish()?)
}

/// Сканирует голову Parquet-файла (лениво; `limit` позволяет движку прочитать
/// только то, что нужно для запрошенного числа строк).
fn scan_parquet_head(path: &Path, max_rows: Option<usize>) -> Result<DataFrame> {
    let lazy = LazyFrame::scan_parquet(to_plref_path(path)?, Default::default())?;
    let limited = match max_rows {
        Some(n) => lazy.limit(n as IdxSize),
        None => lazy,
    };
    Ok(limited.collect()?)
}

// ---------------------------------------------------------------------------
// Внутреннее: формирование вывода
// ---------------------------------------------------------------------------

/// Превращает материализованный фрейм в [`Preview`] (строки для UI).
fn preview_from_frame(frame: &DataFrame, source: SourceInfo) -> Preview {
    let columns = frame
        .columns()
        .iter()
        .map(|column| ColumnInfo {
            name: column.name().to_string(),
            dtype: column.dtype().to_string(),
        })
        .collect();

    let mut rows = Vec::with_capacity(frame.height());
    for row_index in 0..frame.height() {
        let mut row = Vec::with_capacity(frame.width());
        for column in frame.columns() {
            match column.get(row_index) {
                Ok(value) => row.push(cell_to_string(&value)),
                // Неудачное чтение ячейки не должно случаться на корректном
                // фрейме; мы отдаём пустую ячейку, а не роняем UI паникой.
                Err(_) => row.push(String::new()),
            }
        }
        rows.push(row);
    }

    Preview {
        columns,
        rows,
        source,
    }
}

/// Отрисовывает одну ячейку (`AnyValue`) как обычный текст, который ждёт пользователь.
///
/// Большинство значений нормально форматируется через `Display`, но `String`-
/// ячейки Polars печатает *в кавычках* (`"Globex"`), потому что его `Display`
/// повторяет отладочную конвенцию. Таблица предпросмотра должна показывать
/// `Globex`, поэтому два строковых варианта обрабатываются здесь отдельно.
fn cell_to_string(value: &AnyValue<'_>) -> String {
    match value {
        AnyValue::String(text) => (*text).to_string(),
        AnyValue::StringOwned(text) => text.to_string(),
        other => format!("{other}"),
    }
}

/// Превращает `std::path::Path` в `PlRefPath`, который Polars 0.55 использует для сканов.
///
/// Polars представляет пути к файлам как UTF-8-строки (они же служат облачными
/// локациями вроде `s3://...`), поэтому не-UTF-8 локальный путь — доменная
/// ошибка, а не то, что можно молча исковеркать.
fn to_plref_path(path: &Path) -> Result<PlRefPath> {
    let as_str = path
        .to_str()
        .ok_or_else(|| StrataError::NonUtf8Path(path.to_path_buf()))?;
    Ok(PlRefPath::from(as_str))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Уникальные временные пути: тесты идут параллельно, имена не должны совпадать.
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn unique_temp(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("strata_m01_{}_{}_{}", std::process::id(), n, name))
    }

    fn write_temp_bytes(bytes: &[u8], name: &str) -> PathBuf {
        let path = unique_temp(name);
        let mut file = std::fs::File::create(&path).expect("create temp file");
        file.write_all(bytes).expect("write temp file");
        path
    }

    fn write_temp_text(text: &str, name: &str) -> PathBuf {
        write_temp_bytes(text.as_bytes(), name)
    }

    const SAMPLE_CSV: &str = "id,date,amount,customer\n\
1,2026-01-02,120.50,Acme Corp\n\
2,2026-01-02,75.00,Globex\n\
3,2026-01-03,240.00,Initech\n";

    // ------------------------------------------------------------------
    // Стражи оптимизаций (M-perf)
    // ------------------------------------------------------------------

    #[test]
    fn clip_to_record_boundary_never_leaves_a_ragged_tail() {
        // Заканчивается переводом строки: без изменений.
        assert_eq!(clip_to_record_boundary(b"a,b\n1,2\n"), b"a,b\n1,2\n");
        // Обрыв посреди записи: неполный хвост отбрасывается.
        assert_eq!(clip_to_record_boundary(b"a,b\n1,2\n3,"), b"a,b\n1,2\n");
        // Переводов строки нет вовсе (одна строка): сохраняется целиком.
        assert_eq!(clip_to_record_boundary(b"only,one,line"), b"only,one,line");
    }

    // ------------------------------------------------------------------
    // Основы M0
    // ------------------------------------------------------------------

    #[test]
    fn preview_limits_rows_and_keeps_headers() {
        let csv = write_temp_text(SAMPLE_CSV, "basic.csv");
        let preview = preview_csv(&csv, 2).expect("preview should succeed");

        let names: Vec<&str> = preview.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "date", "amount", "customer"]);
        assert_eq!(preview.rows.len(), 2);
        assert_eq!(preview.rows[0][0], "1");
        assert_eq!(preview.rows[1][3], "Globex");
        for row in &preview.rows {
            assert_eq!(row.len(), preview.columns.len());
        }
        // Происхождение: это был обычный UTF-8 CSV с запятой-разделителем.
        assert_eq!(
            preview.source,
            SourceInfo {
                kind: SourceKind::DelimitedText { delimiter: ',' },
                encoding: String::from("UTF-8"),
            }
        );

        let _ = std::fs::remove_file(csv);
    }

    #[test]
    fn csv_roundtrip_to_parquet_preserves_rows_and_columns() {
        let csv = write_temp_text(SAMPLE_CSV, "roundtrip.csv");
        let parquet = unique_temp("roundtrip.parquet");

        let report = csv_to_parquet(&csv, &parquet).expect("conversion should succeed");
        assert_eq!(report.rows, 3);
        assert_eq!(report.columns, 4);
        assert_eq!(report.source_files, 1);
        assert!(parquet.exists());

        let back = LazyFrame::scan_parquet(
            to_plref_path(&parquet).expect("temp path is utf-8"),
            Default::default(),
        )
        .expect("scan written parquet")
        .collect()
        .expect("collect written parquet");

        assert_eq!(back.height(), 3);
        assert_eq!(back.width(), 4);
        let names: Vec<String> = back
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect();
        assert_eq!(names, vec!["id", "date", "amount", "customer"]);

        let _ = std::fs::remove_file(csv);
        let _ = std::fs::remove_file(parquet);
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let missing = unique_temp("missing.csv");
        let err = preview_csv(&missing, 10).expect_err("missing file must fail");
        assert!(matches!(err, StrataError::Io(_) | StrataError::Engine(_)));
    }

    // ------------------------------------------------------------------
    // M0.1: кодировки и разделители (тесты «достоверности сырого слоя»)
    // ------------------------------------------------------------------

    #[test]
    fn cp1251_semicolon_file_is_decoded_without_mojibake() {
        // Классический русский экспорт из Excel: windows-1251 + разделитель ';'.
        let text = "дата;сумма;клиент\n2026-01-05;12.50;ООО Ромашка\n2026-01-06;7.25;ИП Иванов\n";
        let (bytes, _, _) = WINDOWS_1251.encode(text);
        let path = write_temp_bytes(&bytes, "cp1251_semicolon.csv");

        let preview = preview_source(&path, 50).expect("cp1251 preview should succeed");

        // Без моджибейка: заголовки декодировались в настоящие русские слова.
        let names: Vec<&str> = preview.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["дата", "сумма", "клиент"]);
        assert_eq!(preview.rows[0][2], "ООО Ромашка");

        // Происхождение отражает то, что мы определили.
        assert_eq!(
            preview.source.kind,
            SourceKind::DelimitedText { delimiter: ';' }
        );
        assert_eq!(preview.source.encoding, "windows-1251");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn tsv_delimiter_is_detected() {
        let path = write_temp_text("id\tname\n1\tAlice\n2\tBob\n", "tab.tsv");
        let preview = preview_source(&path, 50).expect("tsv preview should succeed");

        assert_eq!(
            preview.source.kind,
            SourceKind::DelimitedText { delimiter: '\t' }
        );
        assert_eq!(preview.source.kind.label(), "TSV");
        let names: Vec<&str> = preview.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "name"]);
        assert_eq!(preview.rows[1][1], "Bob");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn utf16le_with_bom_is_decoded() {
        // UTF-16LE в encoding_rs только декодирует (его `encode` возвращает
        // UTF-8, как сказано в документации), поэтому LE-байты для теста
        // собираем вручную.
        let mut bytes = vec![0xFF, 0xFE]; // BOM для UTF-16 LE
        for unit in "id,name\n1,Ann\n2,Zoe\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let path = write_temp_bytes(&bytes, "utf16.csv");

        let preview = preview_source(&path, 50).expect("utf16 preview should succeed");
        assert_eq!(preview.source.encoding, "UTF-16 LE");
        let names: Vec<&str> = preview.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "name"]);
        assert_eq!(preview.rows[0][1], "Ann");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parquet_source_can_be_previewed() {
        // Сначала стейджим CSV, затем смотрим предпросмотр полученного Parquet-файла.
        let csv = write_temp_text(SAMPLE_CSV, "forparquet.csv");
        let parquet = unique_temp("forparquet.parquet");
        source_to_parquet(&csv, &parquet).expect("staging should succeed");

        let preview = preview_source(&parquet, 50).expect("parquet preview should succeed");
        assert_eq!(preview.source.kind, SourceKind::Parquet);
        let names: Vec<&str> = preview.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "date", "amount", "customer"]);
        assert_eq!(preview.rows.len(), 3, "all staged rows come back");

        let _ = std::fs::remove_file(csv);
        let _ = std::fs::remove_file(parquet);
    }

    #[test]
    fn cp1251_roundtrip_keeps_cyrillic_through_parquet() {
        // Обещание staging: никакого моджибейка даже после сырого слоя.
        let text = "дата;название\n2026-01-05;Зима\n2026-01-06;Весна\n";
        let (bytes, _, _) = WINDOWS_1251.encode(text);
        let path = write_temp_bytes(&bytes, "cp1251_roundtrip.csv");
        let parquet = unique_temp("cp1251_roundtrip.parquet");

        let report = source_to_parquet(&path, &parquet).expect("staging should succeed");
        assert_eq!(report.rows, 2);
        assert_eq!(report.source.encoding, "windows-1251");

        let preview = preview_source(&parquet, 10).expect("parquet preview");
        assert_eq!(preview.rows[0][1], "Зима");
        assert_eq!(preview.rows[1][1], "Весна");

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(parquet);
    }

    // ------------------------------------------------------------------
    // M1b: ручные переопределения ReaderOptions (когда автоопределение ошибается)
    // ------------------------------------------------------------------

    #[test]
    fn encoding_override_fixes_windows1252_misdetection() {
        // "café" в windows-1252 — это байты ...E9. Автоопределение предпочитает
        // здесь windows-1251 (декодирование 0xE9 как 1251 даёт кириллическую
        // 'й', которая выигрывает кириллическую эвристику). Принудительный 1252
        // обязан прочитать 'é'.
        let text = "café;prix\n1;2\n3;4\n";
        let (bytes, _, _) = encoding_rs::WINDOWS_1252.encode(text);
        let path = write_temp_bytes(&bytes, "cp1252.csv");

        let options = ReaderOptions {
            encoding: Some(EncodingChoice::Windows1252),
            delimiter: None,
            has_header: true,
        };
        let forced = preview_source_with(&path, 10, options).expect("forced 1252 preview");
        assert_eq!(forced.columns[0].name, "café");
        assert_eq!(forced.source.encoding, "windows-1252");

        // Staging уважает то же переопределение.
        let parquet = unique_temp("cp1252.parquet");
        let report = source_to_parquet_with(&path, &parquet, options).expect("forced 1252 stage");
        assert_eq!(report.source.encoding, "windows-1252");
        assert_eq!(report.columns, 2);

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(parquet);
    }

    #[test]
    fn delimiter_override_changes_parsing() {
        // Автоопределение находит '|'; принудительная ',' не должна ничего
        // разделить и даёт одну широкую колонку — доказательство, что
        // переопределение правда применено.
        let path = write_temp_text("a|b|c\n1|2|3\n", "pipes.txt");
        let auto = preview_source(&path, 5).expect("auto preview");
        assert_eq!(auto.columns.len(), 3);

        let options = ReaderOptions {
            encoding: None,
            delimiter: Some(','),
            has_header: true,
        };
        let forced = preview_source_with(&path, 5, options).expect("forced preview");
        assert_eq!(forced.columns.len(), 1);
        assert_eq!(forced.columns[0].name, "a|b|c");
        assert_eq!(
            forced.source.kind,
            SourceKind::DelimitedText { delimiter: ',' }
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn forced_utf8_on_cp1251_file_fails_loudly() {
        // Неверное явное переопределение должно дать ошибку, а не молча испортить данные.
        let text = "дата;сумма\n2026-01-05;1.5\n";
        let (bytes, _, _) = WINDOWS_1251.encode(text);
        let path = write_temp_bytes(&bytes, "cp1251_forbidden_utf8.csv");

        let options = ReaderOptions {
            encoding: Some(EncodingChoice::Utf8),
            delimiter: None,
            has_header: true,
        };
        let err = preview_source_with(&path, 5, options).expect_err("utf8 override must fail");
        assert!(matches!(err, StrataError::Encoding(_)));

        let _ = std::fs::remove_file(path);
    }
}
