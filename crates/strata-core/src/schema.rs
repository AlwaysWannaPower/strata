//! # Схема: модель колонок, выведенная из реальных файлов (M1b, шаг «Schemas»)
//!
//! Продуктовое обещание (см. `ТЗ.md` §4 и пользовательскую историю в `PLAN.md`)
//! в том, что пользователь либо пишет схему руками, **либо** программа
//! предлагает её, читая табличные файлы. Этот модуль реализует вторую половину:
//!
//! * [`schema_from_file`] — выводит список колонок + типы Polars по одному файлу;
//! * [`schema_from_folder`] — выводит по папке и сообщает о **конфликтах**
//!   (одна и та же колонка типизирована по-разному в разных файлах — `ТЗ.md` §7)
//!   и о **пропущенных** колонках по каждому файлу.
//!
//! Типы — это строковые метки, которые выдаёт Polars (например `Int64`, `String`);
//! сама схема намеренно сделана удобными простыми данными для UI, наружу не
//! протекает ни один тип Polars. Запись схемы в Parquet (приведение к
//! подтверждённым типам) происходит на шаге «confirm & stage», который строится
//! поверх этого модуля.

use std::collections::HashMap;
use std::path::Path;

use polars::prelude::{DataType, LazyFrame, ParquetWriter};

use crate::folder::{FileMeta, FolderReport, scan_folder};
use crate::{
    ColumnDef, ReaderOptions, SchemaFile, delimiter_from_token, encoding_from_token,
    preview_source_with,
};

/// Сколько строк каждого файла читаем для инференса типов. Типы выводятся по
/// заголовку + выборке значений; чтение большего объёма редко меняет ответ и
/// только замедляет инференс.
const SCHEMA_SAMPLE_ROWS: usize = 200;

/// Сколько файлов папки просматривается (защита от огромных папок).
const SCHEMA_MAX_FILES: usize = 50;

/// Одна колонка предложенной схемы: имя + выведенная метка типа Polars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaColumn {
    /// Имя колонки (заголовок) или авто-имя Polars для файлов без заголовка.
    pub name: String,
    /// Тип данных Polars в виде строки, например `"Int64"`.
    pub dtype: String,
}

/// Схема, предложенная для одного файла.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaProposal {
    /// Колонки в порядке файла.
    pub columns: Vec<SchemaColumn>,
}

/// Конфликт схемы: колонка `column` ожидается как `expected`, но перечисленные
/// файлы типизировали её иначе (`found` = файл → фактический тип).
///
/// Повторяет диалог `⚠ Schema conflict` из `ТЗ.md` §7: UI показывает ожидаемый
/// тип, найденный тип и затронутые файлы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaConflict {
    /// Колонка с несогласованными типами.
    pub column: String,
    /// Тип, с которым согласны большинство файлов («ожидаемый»).
    pub expected: String,
    /// Пары `(имя файла, фактический dtype)` для каждого файла, который расходится.
    pub found: Vec<(String, String)>,
}

/// Результат инференса схемы по папке: предложение + проблемы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSchema {
    /// Предложенные колонки (порядок первого появления среди просмотренных файлов).
    pub columns: Vec<SchemaColumn>,
    /// Колонки с конфликтами типов между файлами.
    pub conflicts: Vec<SchemaConflict>,
    /// Колонки, отсутствующие в некоторых файлах: `(колонка, файлы без неё)`.
    pub missing: Vec<(String, Vec<String>)>,
    /// Файлы, которые не удалось прочитать/вывести (с текстом ошибки).
    pub failed: Vec<(String, String)>,
    /// Сколько файлов успешно просмотрено.
    pub files_inspected: usize,
}

/// Выводит предложение схемы по одному файлу, предпросматривая его голову.
///
/// # Errors
/// Любая ошибка чтения/декодирования/разбора самого файла.
pub fn schema_from_file(path: &Path, options: ReaderOptions) -> crate::Result<SchemaProposal> {
    let preview = preview_source_with(path, SCHEMA_SAMPLE_ROWS, options)?;
    Ok(SchemaProposal {
        columns: preview
            .columns
            .into_iter()
            .map(|c| SchemaColumn {
                name: c.name,
                dtype: c.dtype,
            })
            .collect(),
    })
}

/// Выводит схему по папке, максимум по [`SCHEMA_MAX_FILES`] файлам.
///
/// Никогда не падает из-за одного битого файла: нечитаемые файлы попадают в
/// [`FolderSchema::failed`]. Правила слияния см. в документации модуля.
pub fn schema_from_folder(dir: &Path, options: ReaderOptions) -> crate::Result<FolderSchema> {
    let scan = scan_folder(dir)?;
    let candidates: Vec<&FileMeta> = scan
        .files
        .iter()
        .filter(|meta| !matches!(meta.kind.as_str(), "Other"))
        .take(SCHEMA_MAX_FILES)
        .collect();

    // Упорядоченное объединение имён колонок (в порядке первого появления),
    // наблюдаемый dtype по каждому файлу и колонке, и то, какие колонки реально
    // есть в каждом файле.
    let mut column_order: Vec<String> = Vec::new();
    let mut observed: HashMap<String, Vec<(String, String)>> = HashMap::new(); // колонка -> (файл, dtype)
    let mut file_columns: Vec<(String, Vec<String>)> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();

    for meta in &candidates {
        let path = dir.join(&meta.name);
        match preview_source_with(&path, SCHEMA_SAMPLE_ROWS, options) {
            Ok(preview) => {
                let mut names = Vec::with_capacity(preview.columns.len());
                for column in preview.columns {
                    if !column_order.contains(&column.name) {
                        column_order.push(column.name.clone());
                    }
                    observed
                        .entry(column.name.clone())
                        .or_default()
                        .push((meta.name.clone(), column.dtype));
                    names.push(column.name);
                }
                file_columns.push((meta.name.clone(), names));
            }
            Err(err) => failed.push((meta.name.clone(), err.to_string())),
        }
    }

    let files_ok = file_columns.len();
    // Ожидаемый тип колонки = самый частый наблюдаемый тип; ничьи разрешаются
    // по порядку файлов (побеждает тип самого раннего файла), чтобы предложение
    // было детерминированным между прогонами.
    let mut columns = Vec::with_capacity(column_order.len());
    let mut conflicts = Vec::new();
    for name in &column_order {
        let per_file = observed.get(name).expect("column was observed");
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for (_, dtype) in per_file {
            *counts.entry(dtype.as_str()).or_default() += 1;
        }
        let max = counts.values().copied().max().unwrap_or(0);
        let expected = per_file
            .iter()
            .find(|(_, dtype)| counts.get(dtype.as_str()) == Some(&max))
            .map(|(_, dtype)| dtype.clone())
            .unwrap_or_default();
        columns.push(SchemaColumn {
            name: name.clone(),
            dtype: expected.clone(),
        });

        // Файлы, тип которых отличается от ожидаемого.
        let offenders: Vec<(String, String)> = per_file
            .iter()
            .filter(|(_, dtype)| *dtype != expected)
            .map(|(file, dtype)| (file.clone(), dtype.clone()))
            .collect();
        if !offenders.is_empty() {
            conflicts.push(SchemaConflict {
                column: name.clone(),
                expected,
                found: offenders,
            });
        }
    }

    // Колонки, отсутствующие в некоторых просмотренных файлах.
    let mut missing: Vec<(String, Vec<String>)> = Vec::new();
    for name in &column_order {
        let without: Vec<String> = file_columns
            .iter()
            .filter(|(_, names)| !names.contains(name))
            .map(|(file, _)| file.clone())
            .collect();
        if !without.is_empty() {
            missing.push((name.clone(), without));
        }
    }

    Ok(FolderSchema {
        columns,
        conflicts,
        missing,
        failed,
        files_inspected: files_ok,
    })
}

/// Стейджит каждый файл папки **по подтверждённой схеме**.
///
/// Это исполняемый контракт «одна папка = одна схема»: каждый файл читается с
/// опциями читалки из схемы (кодировка/разделитель/заголовок), а его выведенные
/// колонки/типы сравниваются с подтверждённой [`SchemaFile`]. Файл, который не
/// подходит, **не** стейджится вслепую — он уходит в [`FolderReport::skipped`] с
/// конкретной причиной (здесь и живёт правило «либо файлы соответствуют схеме,
/// либо вы получаете ошибку» — `ТЗ.md` §7 в его виде для сырого слоя).
/// Подходящие файлы становятся частями `NNNN-<stem>.parquet` в `dest_dir`.
///
/// # Errors
/// Фатален только сбой записи/скана в директории назначения; проблемы по
/// отдельным файлам сообщаются, но никогда не выбрасываются.
pub fn stage_folder_with_schema(
    src_dir: &Path,
    dest_dir: &Path,
    schema: &SchemaFile,
) -> crate::Result<FolderReport> {
    stage_folder_with_schema_progress(src_dir, dest_dir, schema, |_, _| {})
}

/// То же, что [`stage_folder_with_schema`], но сообщает прогресс после каждого файла.
///
/// `on_progress(done_files, total_files)` вызывается один раз на обработанный
/// файл — включая пропущенные, потому что «done» значит «мы прошли этот файл», а
/// именно это и должен показывать прогресс-бар. Веб-сервис использует это, чтобы
/// стримить фрагмент прогресса, пока работа идёт в фоновом потоке.
pub fn stage_folder_with_schema_progress<F>(
    src_dir: &Path,
    dest_dir: &Path,
    schema: &SchemaFile,
    mut on_progress: F,
) -> crate::Result<FolderReport>
where
    F: FnMut(usize, usize),
{
    use std::fs;
    fs::create_dir_all(dest_dir)?;

    let options = ReaderOptions {
        encoding: encoding_from_token(&schema.encoding),
        delimiter: delimiter_from_token(&schema.delimiter),
        has_header: schema.has_header,
    };

    let scan = scan_folder(src_dir)?;
    let mut staged = Vec::new();
    let mut skipped = Vec::new();
    let mut total_rows = 0u64;

    let total_files = scan.files.len();
    for (index, meta) in scan.files.iter().enumerate() {
        if meta.kind == "Other" {
            skipped.push((meta.name.clone(), String::from("unsupported file kind")));
            on_progress(index + 1, total_files);
            continue;
        }
        let source_path = src_dir.join(&meta.name);

        // 1. Проверяем файл на соответствие подтверждённой схеме до записи.
        let proposal = match schema_from_file(&source_path, options) {
            Ok(proposal) => proposal,
            Err(err) => {
                skipped.push((meta.name.clone(), format!("cannot read: {err}")));
                on_progress(index + 1, total_files);
                continue;
            }
        };
        if let Some(reason) = schema_mismatch(&schema.columns, &proposal.columns) {
            skipped.push((meta.name.clone(), format!("schema mismatch: {reason}")));
            continue;
        }

        // 2. Подходит: полное чтение с опциями схемы + запись части.
        let stem = source_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("file{index}"));
        let part_path = dest_dir.join(format!("{index:04}-{stem}.parquet"));
        match source_to_parquet_typed(&source_path, &part_path, options, &schema.columns) {
            Ok(report) => {
                // Проверка типов по всему файлу: инференс по *всему* файлу может
                // отличаться от выборки (поздняя плохая строка превращает колонку
                // в String). Читаем реальную схему части (1 строки достаточно —
                // схема лежит в заголовке Parquet) и сравниваем.
                match verify_part_types(&part_path, &schema.columns) {
                    Ok(None) => {
                        total_rows += report.rows;
                        staged.push(crate::folder::StagedFile {
                            name: meta.name.clone(),
                            part_path: part_path.display().to_string(),
                            rows: report.rows,
                            columns: report.columns,
                        });
                    }
                    Ok(Some(reason)) => {
                        let _ = std::fs::remove_file(&part_path);
                        skipped
                            .push((meta.name.clone(), format!("full-file type check: {reason}")));
                    }
                    Err(err) => {
                        let _ = std::fs::remove_file(&part_path);
                        skipped.push((
                            meta.name.clone(),
                            format!("type verification failed: {err}"),
                        ));
                    }
                }
            }
            Err(err) => skipped.push((meta.name.clone(), format!("stage failed: {err}"))),
        }
        on_progress(index + 1, total_files);
    }

    Ok(FolderReport {
        staged,
        skipped,
        total_rows,
        dest_dir: dest_dir.display().to_string(),
    })
}

/// Читает реальные типы колонок записанной части Parquet и сравнивает их с
/// подтверждённой схемой. Возвращает причину несоответствия или `None`, когда
/// файл сходится со схемой.
fn verify_part_types(part: &Path, expected: &[ColumnDef]) -> crate::Result<Option<String>> {
    let frame = LazyFrame::scan_parquet(crate::to_plref_path(part)?, Default::default())?
        .limit(1)
        .collect()?;
    let actual: Vec<(String, String)> = frame
        .columns()
        .iter()
        .map(|c| (c.name().to_string(), c.dtype().to_string()))
        .collect();

    if expected.len() != actual.len() {
        return Ok(Some(format!(
            "{} column(s) expected, parquet has {}",
            expected.len(),
            actual.len()
        )));
    }
    for (expected, (actual_name, actual_dtype)) in expected.iter().zip(actual) {
        if expected.name != actual_name {
            return Ok(Some(format!(
                "expected column '{}', parquet has '{}'",
                expected.name, actual_name
            )));
        }
        if expected.dtype != actual_dtype {
            return Ok(Some(format!(
                "'{}': expected type {}, parquet has {}",
                expected.name, expected.dtype, actual_dtype
            )));
        }
    }
    Ok(None)
}

/// Переводит метку типа схемы (как она хранится в `*.schema.toml`) в dtype Polars.
///
/// Набор намеренно маленький: это типы, которые сырой слой умеет порождать и
/// между которыми умеет приводить. Неизвестная метка — ошибка, а не догадка.
fn dtype_from_label(label: &str) -> Option<DataType> {
    Some(match label {
        "i8" => DataType::Int8,
        "i16" => DataType::Int16,
        "i32" => DataType::Int32,
        "i64" => DataType::Int64,
        "u8" => DataType::UInt8,
        "u16" => DataType::UInt16,
        "u32" => DataType::UInt32,
        "u64" => DataType::UInt64,
        "f32" => DataType::Float32,
        "f64" => DataType::Float64,
        "bool" => DataType::Boolean,
        "str" => DataType::String,
        _ => return None,
    })
}

/// *Совместимы* ли две метки типов по правилу «одна папка = одна схема»?
///
/// Совместимость шире равенства, но только там, где расширение типа
/// безболезненно и очевидно — сырой слой никогда не угадывает:
///
/// * одинаковые типы совместимы;
/// * любая целочисленная ширина может расшириться до более широкой (`i32` → `i64`);
/// * любое целое может расшириться до float (`i64` → `f64`) — классический
///   случай колонки, которая в одном файле выглядит целой, а в другом дробной;
/// * `f32` может расшириться до `f64`.
///
/// Всё остальное (строка ↔ число, дата ↔ строка, …) — это *конвертация*, а не
/// расширение, и принадлежит слою валидации/ODS, поэтому остаётся
/// несоответствием.
fn types_compatible(expected: &str, actual: &str) -> bool {
    if expected == actual {
        return true;
    }
    let expected_rank = numeric_rank(expected);
    let actual_rank = numeric_rank(actual);
    match (expected_rank, actual_rank) {
        // int -> int (расширение), int -> float, float -> float (f32 -> f64)
        (Some(want), Some(have)) => want >= have,
        _ => false,
    }
}

/// Числовой порядок для проверок расширения: целые 1..4, float 5..6.
/// `None` для нечисловых меток.
fn numeric_rank(label: &str) -> Option<u8> {
    Some(match label {
        "i8" | "u8" => 1,
        "i16" | "u16" => 2,
        "i32" | "u32" => 3,
        "i64" | "u64" => 4,
        "f32" => 5,
        "f64" => 6,
        _ => return None,
    })
}

/// Стейджит файл в часть Parquet **с приведением к подтверждённым типам схемы**.
///
/// Читает весь файл с опциями читалки из схемы, приводит любую колонку, чей тип
/// лишь *совместим* (расширение, см. [`types_compatible`]), к объявленному типу,
/// затем пишет часть. Несовместимые типы сюда никогда не доходят — их отсекают
/// раньше с понятной причиной.
pub fn source_to_parquet_typed(
    path: &Path,
    part_path: &Path,
    options: ReaderOptions,
    columns: &[ColumnDef],
) -> crate::Result<crate::ImportReport> {
    let (mut frame, source) = crate::open_any(path, options, None)?;

    for column in columns {
        let name = column.name.as_str();
        let expected = dtype_from_label(&column.dtype).ok_or_else(|| {
            crate::StrataError::SchemaType(format!("{} (column '{}')", column.dtype, column.name))
        })?;
        let actual_column = frame.column(name)?;
        if actual_column.dtype() != &expected {
            let casted = actual_column.cast(&expected)?;
            frame.with_column(casted)?;
        }
    }

    let rows = frame.height();
    let column_count = frame.width();

    let mut file = std::fs::File::create(part_path)?;
    let writer = ParquetWriter::new(&mut file);
    writer.finish(&mut frame.into())?;

    Ok(crate::ImportReport {
        rows: rows as u64,
        columns: column_count,
        source_files: 1,
        parquet_path: part_path.display().to_string(),
        source,
        partitions: 1,
    })
}

/// Сравнивает колонки подтверждённой схемы с тем, что реально отдаёт файл.
///
/// Возвращает человеческую причину первого несоответствия (порядок, имя или
/// тип) или `None`, когда файл подходит.
fn schema_mismatch(expected: &[ColumnDef], actual: &[SchemaColumn]) -> Option<String> {
    if expected.len() != actual.len() {
        return Some(format!(
            "{} column(s) expected, {} found",
            expected.len(),
            actual.len()
        ));
    }
    for (expected, actual) in expected.iter().zip(actual) {
        if expected.name != actual.name {
            return Some(format!(
                "expected column '{}', file has '{}'",
                expected.name, actual.name
            ));
        }
        if !types_compatible(&expected.dtype, &actual.dtype) {
            return Some(format!(
                "'{}': expected type {}, found {}",
                expected.name, expected.dtype, actual.dtype
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "strata_schema_{}_{}_{}",
            std::process::id(),
            n,
            tag
        ))
    }

    fn write_text(path: &std::path::Path, text: &str) {
        let mut f = std::fs::File::create(path).expect("create temp");
        f.write_all(text.as_bytes()).expect("write temp");
    }

    #[test]
    fn schema_from_file_uses_headers_and_types() {
        let path = temp_path("one.csv");
        write_text(
            &path,
            "id,date,amount,customer\n1,2026-01-02,120.50,Acme Corp\n2,2026-01-03,75.00,Globex\n",
        );

        let proposal = schema_from_file(&path, ReaderOptions::default()).expect("infer");
        let names: Vec<&str> = proposal.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "date", "amount", "customer"]);
        assert_eq!(proposal.columns[0].dtype, "i64");
        assert_eq!(proposal.columns[2].dtype, "f64");
        assert_eq!(proposal.columns[3].dtype, "str");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn headerless_file_treated_as_data_when_option_says_so() {
        let path = temp_path("noheader.csv");
        write_text(&path, "1,2026-01-02\n2,2026-01-03\n");

        // С опциями по умолчанию первая строка была бы съедена как заголовок.
        let with_header = schema_from_file(&path, ReaderOptions::default()).expect("infer");
        assert_eq!(with_header.columns.len(), 2); // id + date

        let options = ReaderOptions {
            has_header: false,
            ..ReaderOptions::default()
        };
        let proposal = schema_from_file(&path, options).expect("infer no-header");
        assert_eq!(proposal.columns.len(), 2);
        // Теперь значения — это данные, а имена — авто-имена Polars ("column_…").
        assert!(proposal.columns[0].name.starts_with("column_"));
        assert_eq!(proposal.columns[0].dtype, "i64");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn folder_schema_reports_type_conflicts_between_files() {
        let dir = temp_path("dir");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // a.csv: amount — число. b.csv: та же колонка — текст.
        write_text(&dir.join("a.csv"), "id,amount\n1,12.5\n2,7.25\n");
        write_text(&dir.join("b.csv"), "id,amount\n3,n/a\n4,unknown\n");

        let report = schema_from_folder(&dir, ReaderOptions::default()).expect("folder infer");
        assert_eq!(report.files_inspected, 2);
        assert!(report.failed.is_empty());

        // Оба файла прочитаны; `amount` должна быть помечена как конфликтующая.
        let amount_conflict = report
            .conflicts
            .iter()
            .find(|c| c.column == "amount")
            .expect("amount conflict present");
        assert_eq!(amount_conflict.expected, "f64"); // ничья → побеждает первый файл
        assert_eq!(amount_conflict.found.len(), 1);
        assert_eq!(amount_conflict.found[0].0, "b.csv");
        assert_eq!(amount_conflict.found[0].1, "str");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn folder_schema_notes_missing_columns_and_broken_files() {
        let dir = temp_path("dir2");
        std::fs::create_dir_all(&dir).expect("mkdir");
        write_text(&dir.join("full.csv"), "id,name,extra\n1,A,9\n");
        write_text(&dir.join("short.csv"), "id,name\n2,B\n");
        // Файл неизвестного вида не пробуем (как и в folder staging): он просто
        // не является частью картины схемы.
        fs_write(&dir.join("broken.dat"), b"\x00\x01\x02 not text at all");

        let report = schema_from_folder(&dir, ReaderOptions::default()).expect("folder infer");
        assert_eq!(report.files_inspected, 2);
        assert!(report.failed.is_empty());

        // `extra` есть только в full.csv.
        let extra_missing = report
            .missing
            .iter()
            .find(|(column, _)| column == "extra")
            .expect("extra is reported missing");
        assert_eq!(extra_missing.1, vec![String::from("short.csv")]);

        let _ = std::fs::remove_dir_all(dir);
    }

    fn fs_write(path: &std::path::Path, bytes: &[u8]) {
        use std::fs::write;
        write(path, bytes).expect("write temp bytes");
    }

    fn sample_schema() -> SchemaFile {
        use crate::ColumnDef;
        SchemaFile {
            format: 1,
            source: String::new(),
            has_header: true,
            encoding: "auto".to_string(),
            delimiter: "auto".to_string(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    dtype: "i64".into(),
                },
                ColumnDef {
                    name: "amount".into(),
                    dtype: "f64".into(),
                },
                ColumnDef {
                    name: "name".into(),
                    dtype: "str".into(),
                },
            ],
            saved_utc: "t".into(),
        }
    }

    fn conform_csv(text: &str) -> String {
        // id,amount,name: amount числовой, name строковый.
        format!("id,amount,name\n{text}")
    }

    #[test]
    fn schema_validated_staging_stages_conforming_files() {
        let dir = temp_path("sv_ok");
        std::fs::create_dir_all(&dir).unwrap();
        write_text(
            &dir.join("a.csv"),
            &conform_csv("1,12.5,Alpha\n2,7.0,Beta\n"),
        );
        write_text(&dir.join("b.csv"), &conform_csv("3,9.25,Gamma\n"));
        let dest = temp_path("sv_ok_dst");

        let report = stage_folder_with_schema(&dir, &dest, &sample_schema()).expect("stage");
        assert_eq!(report.staged.len(), 2);
        assert!(report.skipped.is_empty(), "no skips: {:?}", report.skipped);
        assert_eq!(report.total_rows, 3);

        let preview = crate::preview_parts(&dest, 100).expect("preview");
        assert_eq!(preview.rows.len(), 3);

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dest);
    }

    #[test]
    fn schema_validated_staging_reports_nonconforming_file() {
        let dir = temp_path("sv_bad");
        std::fs::create_dir_all(&dir).unwrap();
        write_text(&dir.join("good.csv"), &conform_csv("1,12.5,Alpha\n"));
        // Здесь amount — текст, что нарушает подтверждённую схему f64.
        write_text(&dir.join("bad.csv"), "id,amount,name\n2,n/a,Beta\n");
        let dest = temp_path("sv_bad_dst");

        let report = stage_folder_with_schema(&dir, &dest, &sample_schema()).expect("stage");
        assert_eq!(report.staged.len(), 1, "good file staged");
        assert_eq!(report.skipped.len(), 1, "bad file rejected");
        assert_eq!(report.skipped[0].0, "bad.csv");
        assert!(
            report.skipped[0].1.contains("expected type f64"),
            "reason names the expected type: {}",
            report.skipped[0].1
        );

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dest);
    }

    #[test]
    fn schema_reader_options_are_applied_during_validated_stage() {
        // Файл windows-1251 с разделителем ';', привязанный к схеме, которая это
        // и утверждает: staging должен использовать эти опции и чисто прочитать
        // русский текст.
        let dir = temp_path("sv_opts");
        std::fs::create_dir_all(&dir).unwrap();
        let text = "дата;сумма\n2026-01-05;12.50\n2026-01-06;7.25\n";
        let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode(text);
        fs_write(&dir.join("rus.csv"), &bytes);
        let dest = temp_path("sv_opts_dst");

        let mut schema = sample_schema();
        schema.columns = vec![
            crate::ColumnDef {
                name: "дата".into(),
                dtype: "str".into(),
            },
            crate::ColumnDef {
                name: "сумма".into(),
                dtype: "f64".into(),
            },
        ];
        schema.encoding = "cp1251".to_string();
        schema.delimiter = "semicolon".to_string();

        let report = stage_folder_with_schema(&dir, &dest, &schema).expect("stage");
        assert_eq!(report.staged.len(), 1);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);

        let preview = crate::preview_parts(&dest, 100).expect("preview");
        assert_eq!(preview.rows.len(), 2);
        assert_eq!(preview.rows[0][0], "2026-01-05");
        assert_eq!(preview.columns[0].name, "дата");

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dest);
    }

    #[test]
    fn staging_reports_per_file_progress() {
        let dir = temp_path("progress");
        std::fs::create_dir_all(&dir).unwrap();
        write_text(&dir.join("a.csv"), &conform_csv("1,12.5,Alpha\n"));
        write_text(&dir.join("b.csv"), &conform_csv("2,7.5,Beta\n"));
        let dest = temp_path("progress_dst");

        let mut seen: Vec<(usize, usize)> = Vec::new();
        let report =
            stage_folder_with_schema_progress(&dir, &dest, &sample_schema(), |done, total| {
                seen.push((done, total));
            })
            .expect("stage");
        assert_eq!(report.staged.len(), 2);
        // Один колбэк на файл, монотонно растёт, знаменатель стабилен.
        assert_eq!(seen, vec![(1, 2), (2, 2)]);

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dest);
    }

    #[test]
    fn numeric_widening_is_accepted_and_cast_to_the_schema_type() {
        // В одном файле суммы дробные (f64), в другом целые (i64).
        // Подтверждённая схема говорит f64: i64 — это *расширение*, поэтому оба
        // файла должны застейджиться, а записанная часть обязана быть f64.
        let dir = temp_path("widening");
        std::fs::create_dir_all(&dir).unwrap();
        write_text(&dir.join("a.csv"), &conform_csv("1,12.5,Alpha\n"));
        write_text(&dir.join("b.csv"), &conform_csv("2,7,Gamma\n")); // amount = i64
        let dest = temp_path("widening_dst");

        let report = stage_folder_with_schema(&dir, &dest, &sample_schema()).expect("stage");
        assert_eq!(
            report.staged.len(),
            2,
            "widening must not reject: {:?}",
            report.skipped
        );
        assert!(report.skipped.is_empty());
        assert_eq!(report.total_rows, 2);

        // Проверяем *записанные* типы: в каждой части amount обязан быть f64.
        let parts = crate::list_parts(&dest).expect("parts");
        assert_eq!(parts.len(), 2);
        for part in &parts {
            let preview =
                preview_source_with(&dest.join(&part.rel_path), 5, ReaderOptions::default())
                    .expect("preview part");
            let amount = preview
                .columns
                .iter()
                .find(|c| c.name == "amount")
                .expect("amount column");
            assert_eq!(
                amount.dtype, "f64",
                "part {} kept {amount:?}",
                part.rel_path
            );
        }

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dest);
    }

    #[test]
    fn incompatible_types_are_still_rejected() {
        // str против f64 — это конвертация, а не расширение: остаётся ошибкой.
        assert!(!types_compatible("f64", "str"));
        assert!(!types_compatible("str", "i64"));
        assert!(types_compatible("f64", "i64"));
        assert!(types_compatible("i64", "i32"));
        assert!(types_compatible("f64", "f32"));
        assert!(types_compatible("str", "str"));
    }

    #[test]
    fn validated_stage_rejects_late_row_type_drift() {
        // Первые 100 строк (выборка Polars для инференса) числовые, поэтому
        // проверка схемы по выборке проходит — но строка ~102 содержит "n/a",
        // что превращает всю колонку в String при полном чтении. Проверка типов
        // по всему файлу должна отклонить файл, а не записать часть с неверным
        // типом.
        let dir = temp_path("sv_drift");
        std::fs::create_dir_all(&dir).unwrap();
        let mut content = String::from("id,amount,name\n");
        for i in 1..=101 {
            content.push_str(&format!("{i},12.5,Alpha\n"));
        }
        content.push_str("102,n/a,Beta\n");
        write_text(&dir.join("drift.csv"), &content);
        let dest = temp_path("sv_drift_dst");

        let report = stage_folder_with_schema(&dir, &dest, &sample_schema()).expect("stage");
        assert_eq!(
            report.staged.len(),
            0,
            "nothing may be staged with a wrong type"
        );
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, "drift.csv");
        // Причина отклонения — либо наша явная проверка типов по всему файлу,
        // либо собственная строгая ошибка разбора Polars на плохом значении;
        // и то и другое значит, что файл не был застейджен с неверным типом.
        let reason_ok = report.skipped[0].1.contains("full-file type check")
            || report.skipped[0].1.contains("n/a");
        assert!(
            reason_ok,
            "reason explains rejection: {}",
            report.skipped[0].1
        );
        // И никакой оставшейся части на диске.
        assert_eq!(crate::list_parts(&dest).expect("parts").len(), 0);

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dest);
    }
}
