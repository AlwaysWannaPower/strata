//! # Персистентность проекта: `project.toml` + сохранённые схемы (M1b, шаг 3)
//!
//! Раскладка на диске следует `ТЗ.md` §10 (минимальная версия, нужная сейчас):
//!
//! ```text
//! <project dir>/
//! ├── project.toml        # метаданные: имя, версия, время создания
//! ├── schemas/            # один TOML-файл на сохранённую схему (источник + его
//! │                       #   колонки + опции читалки), чтобы подтверждённая
//! │                       #   схема переживала перезапуск
//! └── logs/               # зарезервировано под историю прогонов (позже)
//! ```
//!
//! Файл схемы хранит ровно то, что пользователь *подтвердил* на экране Schemas:
//! список колонок (имя + выведенный/выбранный тип), опции читалки
//! (кодировка / разделитель / флаг заголовка) и путь источника. Это простые
//! данные — никакие типы Polars или Dioxus в файл не протекают.
//!
//! Опции читалки хранятся как **канонические токены** (например `"cp1251"`,
//! `"semicolon"`), а не как сырые символы/enum: токены стабильны для человека,
//! правящего TOML руками, и легко мигрируют. [`encoding_from_token`] /
//! [`encoding_token`] и [`delimiter_from_token`] / [`delimiter_token`]
//! конвертируют в обе стороны.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::{EncodingChoice, ReaderOptions, StrataError};

/// Текущая версия формата файла схемы (поднимайте, когда меняется смысл полей).
const SCHEMA_FORMAT: u32 = 1;

/// Метаданные проекта, хранимые в `project.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMeta {
    /// Человекочитаемое имя проекта.
    pub name: String,
    /// Версия формата файла проекта.
    pub version: u32,
    /// Время создания, UTC ISO-8601 (только для справки).
    pub created_utc: String,
}

/// Одна подтверждённая колонка внутри сохранённой схемы.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDef {
    /// Имя колонки (заголовок или авто-имя Polars).
    pub name: String,
    /// Подтверждённая метка типа Polars, например `"i64"`.
    pub dtype: String,
}

/// Сохранённая схема: исходный файл + подтверждённые колонки + опции читалки.
///
/// `encoding` / `delimiter` хранят канонические токены (`"auto"`, когда не заданы).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemaFile {
    /// Версия формата файла схемы.
    pub format: u32,
    /// Абсолютный путь исходного файла, который описывает эта схема.
    pub source: String,
    /// `false` для файлов без заголовка.
    pub has_header: bool,
    /// Токен кодировки, например `"auto"`, `"utf8"`, `"cp1251"`, `"cp1252"`, `"utf16le"`, `"utf16be"`.
    pub encoding: String,
    /// Токен разделителя, например `"auto"`, `"comma"`, `"semicolon"`, `"tab"`, `"pipe"`.
    pub delimiter: String,
    /// Подтверждённые колонки по порядку.
    pub columns: Vec<ColumnDef>,
    /// Правила качества (бизнес-проверки) этой сущности. Поле добавлено позже,
    /// поэтому старые файлы схем читаются с пустым списком правил.
    #[serde(default)]
    pub quality: Vec<crate::quality::ColumnRule>,
    /// Когда эта схема была сохранена (UTC ISO-8601, для справки).
    pub saved_utc: String,
}

impl SchemaFile {
    /// Собирает файл схемы из текущих [`ReaderOptions`] + колонок.
    pub fn new(source: PathBuf, options: ReaderOptions, columns: Vec<ColumnDef>) -> Self {
        SchemaFile {
            format: SCHEMA_FORMAT,
            source: source.display().to_string(),
            has_header: options.has_header,
            encoding: encoding_token(options.encoding).to_string(),
            delimiter: delimiter_token(options.delimiter).to_string(),
            columns,
            quality: Vec::new(),
            saved_utc: now_utc(),
        }
    }
}

// ---------------------------------------------------------------------------
// Жизненный цикл проекта
// ---------------------------------------------------------------------------

/// Создаёт директорию нового проекта: пишет `project.toml` и создаёт папки
/// `schemas/` и `logs/`. Падает, если проект там уже есть.
pub fn create_project(dir: &Path, name: &str) -> crate::Result<ProjectMeta> {
    let project_file = dir.join("project.toml");
    if project_file.exists() {
        return Err(StrataError::ProjectExists(dir.to_path_buf()));
    }
    let meta = ProjectMeta {
        name: name.trim().to_string(),
        version: SCHEMA_FORMAT,
        created_utc: now_utc(),
    };
    std::fs::create_dir_all(dir.join("schemas"))?;
    std::fs::create_dir_all(dir.join("logs"))?;
    let text = toml::to_string(&meta)
        .map_err(|e| StrataError::ProjectFile(format!("serialize project.toml: {e}")))?;
    std::fs::write(&project_file, text)?;
    Ok(meta)
}

/// Открывает существующий проект: возвращает его метаданные или `None`, когда в
/// папке нет `project.toml` (значит, это просто не проект).
pub fn open_project(dir: &Path) -> crate::Result<Option<ProjectMeta>> {
    let project_file = dir.join("project.toml");
    if !project_file.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&project_file)?;
    let meta: ProjectMeta = toml::from_str(&text)
        .map_err(|e| StrataError::ProjectFile(format!("parse project.toml: {e}")))?;
    Ok(Some(meta))
}

// ---------------------------------------------------------------------------
// Сохранённые схемы
// ---------------------------------------------------------------------------

/// Сохраняет схему как `schemas/<safe-name>.toml` внутри проекта.
///
/// `<safe-name>` выводится из имени исходного файла (для источников без
/// заголовка используется счётчик) и санитизируется для безопасности файловой
/// системы. Повторное сохранение перезаписывает — побеждает последнее
/// подтверждение.
pub fn save_schema(project_dir: &Path, schema: &SchemaFile) -> crate::Result<String> {
    let file_name = schema_file_name(schema);
    let path = project_dir.join("schemas").join(&file_name);
    let text = toml::to_string(schema)
        .map_err(|e| StrataError::ProjectFile(format!("serialize schema: {e}")))?;
    std::fs::write(&path, text)?;
    Ok(file_name)
}

/// Загружает одну сохранённую схему по имени файла (например, из [`schema_names`]).
pub fn load_schema(project_dir: &Path, file_name: &str) -> crate::Result<SchemaFile> {
    let path = project_dir.join("schemas").join(file_name);
    let text = std::fs::read_to_string(&path)?;
    toml::from_str(&text).map_err(|e| StrataError::ProjectFile(format!("parse {file_name}: {e}")))
}

/// Перечисляет имена файлов сохранённых схем (`.toml` в `schemas/`), отсортированные.
pub fn schema_names(project_dir: &Path) -> crate::Result<Vec<String>> {
    let dir = project_dir.join("schemas");
    let mut names: Vec<String> = std::fs::read_dir(&dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".toml"))
        .collect();
    names.sort();
    Ok(names)
}

/// Где должен лежать файл схемы для `schema` (`schemas/<name>.toml`).
fn schema_file_name(schema: &SchemaFile) -> String {
    let stem = Path::new(&schema.source)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(sanitize_file_part)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "schema".to_string());
    format!("{stem}.toml")
}

/// Заменяет символы, невалидные/неудобные в именах файлов.
fn sanitize_file_part(value: &str) -> String {
    let out: String = value
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            other => other,
        })
        .collect();
    if out.is_empty() { "_".to_string() } else { out }
}

// ---------------------------------------------------------------------------
// Преобразования токен <-> значение (стабильны для TOML, правленного руками)
// ---------------------------------------------------------------------------

/// Канонический токен для опции кодировки (`"auto"`, когда не задана).
pub fn encoding_token(choice: Option<EncodingChoice>) -> &'static str {
    match choice {
        None => "auto",
        Some(EncodingChoice::Utf8) => "utf8",
        Some(EncodingChoice::Windows1251) => "cp1251",
        Some(EncodingChoice::Windows1252) => "cp1252",
        Some(EncodingChoice::Utf16Le) => "utf16le",
        Some(EncodingChoice::Utf16Be) => "utf16be",
    }
}

/// Разбирает токен кодировки обратно в опцию (`"auto"`/неизвестное → `None`).
pub fn encoding_from_token(token: &str) -> Option<EncodingChoice> {
    match token {
        "utf8" => Some(EncodingChoice::Utf8),
        "cp1251" => Some(EncodingChoice::Windows1251),
        "cp1252" => Some(EncodingChoice::Windows1252),
        "utf16le" => Some(EncodingChoice::Utf16Le),
        "utf16be" => Some(EncodingChoice::Utf16Be),
        _ => None,
    }
}

/// Канонический токен для опции разделителя (`"auto"`, когда не задана).
pub fn delimiter_token(delimiter: Option<char>) -> &'static str {
    match delimiter {
        None => "auto",
        Some(',') => "comma",
        Some(';') => "semicolon",
        Some('\t') => "tab",
        Some('|') => "pipe",
        Some(_) => "auto", // неподдерживаемый символ → auto (round-trip невозможен)
    }
}

/// Разбирает токен разделителя обратно в опцию.
pub fn delimiter_from_token(token: &str) -> Option<char> {
    match token {
        "comma" => Some(','),
        "semicolon" => Some(';'),
        "tab" => Some('\t'),
        "pipe" => Some('|'),
        _ => None,
    }
}

/// Текущее время UTC, в духе ISO-8601 (`YYYY-MM-DD HH:MM:SS UTC`) — для справки.
fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs} UTC")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("strata_proj_{}_{}_{}", std::process::id(), n, tag));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn create_and_open_project_roundtrip() {
        let dir = temp_dir("roundtrip");
        let meta = create_project(&dir, "Продажи 2026").expect("create");
        assert_eq!(meta.name, "Продажи 2026");
        assert_eq!(meta.version, 1);
        assert!(dir.join("project.toml").exists());
        assert!(dir.join("schemas").is_dir());

        let opened = open_project(&dir).expect("open").expect("exists");
        assert_eq!(opened.name, meta.name);

        // Повторное создание поверх существующего проекта должно громко падать.
        let err = create_project(&dir, "again").expect_err("duplicate create fails");
        assert!(matches!(err, StrataError::ProjectExists(_)));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn open_non_project_returns_none() {
        let dir = temp_dir("none");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(open_project(&dir).expect("read").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn schema_save_list_load_roundtrip() {
        let dir = temp_dir("schema");
        create_project(&dir, "p").unwrap();

        let options = ReaderOptions {
            encoding: Some(EncodingChoice::Windows1251),
            delimiter: Some(';'),
            has_header: true,
        };
        let columns = vec![
            ColumnDef {
                name: "дата".into(),
                dtype: "str".into(),
            },
            ColumnDef {
                name: "сумма".into(),
                dtype: "f64".into(),
            },
        ];
        let schema = SchemaFile::new(PathBuf::from("/data/отчёт_2026.csv"), options, columns);

        let file_name = save_schema(&dir, &schema).expect("save");
        assert_eq!(file_name, "отчёт_2026.toml");

        assert_eq!(schema_names(&dir).expect("names"), vec!["отчёт_2026.toml"]);
        let loaded = load_schema(&dir, &file_name).expect("load");
        assert_eq!(loaded.source, schema.source);
        assert_eq!(loaded.has_header, true);
        assert_eq!(loaded.encoding, "cp1251");
        assert_eq!(loaded.delimiter, "semicolon");
        assert_eq!(loaded.columns, schema.columns);

        // Хелперы токенов проходят round-trip с типами движка.
        assert_eq!(
            encoding_from_token(&loaded.encoding),
            Some(EncodingChoice::Windows1251)
        );
        assert_eq!(delimiter_from_token(&loaded.delimiter), Some(';'));

        let _ = std::fs::remove_dir_all(dir);
    }
}
