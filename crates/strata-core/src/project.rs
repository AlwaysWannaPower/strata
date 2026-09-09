//! # Project persistence: `project.toml` + saved schemas (M1b, step 3)
//!
//! Layout on disk follows `ТЗ.md` §10 (the minimal version we need now):
//!
//! ```text
//! <project dir>/
//! ├── project.toml        # metadata: name, version, created
//! ├── schemas/            # one TOML file per saved schema (a source + its
//! │                       #   columns + reader options), so a confirmed
//! │                       #   schema survives a restart
//! └── logs/               # reserved for run history (later milestone)
//! ```
//!
//! The schema file stores exactly what the user *confirmed* in the Schemas
//! screen: the column list (name + inferred/chosen type), the reader options
//! (encoding / delimiter / header flag) and the source path. It is plain
//! data — no Polars or Dioxus types leak into the file.
//!
//! Reader options are stored as **canonical tokens** (e.g. `"cp1251"`,
//! `"semicolon"`) rather than raw chars/enums: tokens are stable for humans
//! editing TOML by hand and easy to migrate. [`encoding_from_token`] /
//! [`encoding_token`] and [`delimiter_from_token`] / [`delimiter_token`]
//! convert both ways.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::{EncodingChoice, ReaderOptions, StrataError};

/// Current schema-file format version (bump when fields change meaning).
const SCHEMA_FORMAT: u32 = 1;

/// Project metadata stored in `project.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMeta {
    /// Human-readable project name.
    pub name: String,
    /// Project file format version.
    pub version: u32,
    /// Creation time, UTC ISO-8601 (informational only).
    pub created_utc: String,
}

/// One confirmed column inside a saved schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDef {
    /// Column name (header or Polars auto-name).
    pub name: String,
    /// Confirmed Polars type label, e.g. `"i64"`.
    pub dtype: String,
}

/// A saved schema: a source file + the confirmed columns + reader options.
///
/// `encoding` / `delimiter` hold canonical tokens (`"auto"` when unset).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaFile {
    /// Schema file format version.
    pub format: u32,
    /// Absolute path of the source file this schema describes.
    pub source: String,
    /// `false` for header-less files.
    pub has_header: bool,
    /// Encoding token, e.g. `"auto"`, `"utf8"`, `"cp1251"`, `"cp1252"`, `"utf16le"`, `"utf16be"`.
    pub encoding: String,
    /// Delimiter token, e.g. `"auto"`, `"comma"`, `"semicolon"`, `"tab"`, `"pipe"`.
    pub delimiter: String,
    /// Confirmed columns in order.
    pub columns: Vec<ColumnDef>,
    /// When this schema was saved (UTC ISO-8601, informational).
    pub saved_utc: String,
}

impl SchemaFile {
    /// Build a schema file from current [`ReaderOptions`] + columns.
    pub fn new(source: PathBuf, options: ReaderOptions, columns: Vec<ColumnDef>) -> Self {
        SchemaFile {
            format: SCHEMA_FORMAT,
            source: source.display().to_string(),
            has_header: options.has_header,
            encoding: encoding_token(options.encoding).to_string(),
            delimiter: delimiter_token(options.delimiter).to_string(),
            columns,
            saved_utc: now_utc(),
        }
    }
}

// ---------------------------------------------------------------------------
// Project lifecycle
// ---------------------------------------------------------------------------

/// Create a new project directory: writes `project.toml` and creates the
/// `schemas/` and `logs/` folders. Fails if a project already exists there.
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

/// Open an existing project: returns its metadata, or `None` when the folder
/// has no `project.toml` (it is simply not a project).
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
// Saved schemas
// ---------------------------------------------------------------------------

/// Persist a schema as `schemas/<safe-name>.toml` inside the project.
///
/// `<safe-name>` is derived from the source file name (header-less sources
/// fall back to a counter), sanitized for file-system safety. Saving twice
/// overwrites — the latest confirmation wins.
pub fn save_schema(project_dir: &Path, schema: &SchemaFile) -> crate::Result<String> {
    let file_name = schema_file_name(schema);
    let path = project_dir.join("schemas").join(&file_name);
    let text = toml::to_string(schema)
        .map_err(|e| StrataError::ProjectFile(format!("serialize schema: {e}")))?;
    std::fs::write(&path, text)?;
    Ok(file_name)
}

/// Load one saved schema by its file name (e.g. from [`schema_names`]).
pub fn load_schema(project_dir: &Path, file_name: &str) -> crate::Result<SchemaFile> {
    let path = project_dir.join("schemas").join(file_name);
    let text = std::fs::read_to_string(&path)?;
    toml::from_str(&text).map_err(|e| StrataError::ProjectFile(format!("parse {file_name}: {e}")))
}

/// List saved schema file names (`.toml` in `schemas/`), sorted.
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

/// Where the schema file for `schema` should be stored (`schemas/<name>.toml`).
fn schema_file_name(schema: &SchemaFile) -> String {
    let stem = Path::new(&schema.source)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(sanitize_file_part)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "schema".to_string());
    format!("{stem}.toml")
}

/// Replace characters that are invalid/awkward in file names.
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
// Token <-> value conversions (stable for hand-edited TOML)
// ---------------------------------------------------------------------------

/// Canonical token for an encoding option (`"auto"` when unset).
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

/// Parse an encoding token back into an option (`"auto"`/unknown → `None`).
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

/// Canonical token for a delimiter option (`"auto"` when unset).
pub fn delimiter_token(delimiter: Option<char>) -> &'static str {
    match delimiter {
        None => "auto",
        Some(',') => "comma",
        Some(';') => "semicolon",
        Some('\t') => "tab",
        Some('|') => "pipe",
        Some(_) => "auto", // unsupported char → auto (cannot round-trip)
    }
}

/// Parse a delimiter token back into an option.
pub fn delimiter_from_token(token: &str) -> Option<char> {
    match token {
        "comma" => Some(','),
        "semicolon" => Some(';'),
        "tab" => Some('\t'),
        "pipe" => Some('|'),
        _ => None,
    }
}

/// Current UTC time, ISO-8601-ish (`YYYY-MM-DD HH:MM:SS UTC`) — informational.
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

        // Re-creating over an existing project must fail loudly.
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

        // Token helpers round-trip with the engine types.
        assert_eq!(
            encoding_from_token(&loaded.encoding),
            Some(EncodingChoice::Windows1251)
        );
        assert_eq!(delimiter_from_token(&loaded.delimiter), Some(';'));

        let _ = std::fs::remove_dir_all(dir);
    }
}
