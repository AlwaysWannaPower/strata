//! # strata-core — data engine of the Strata workbench.
//!
//! This crate owns every piece of data logic that touches Polars / Arrow / Parquet.
//! It deliberately has **no UI dependencies**: the desktop application (and, later,
//! a CLI) sits on top of this library and only exchanges plain owned data
//! structures ([`Preview`], [`ImportReport`]) with it.
//!
//! ## Staging philosophy (important, see `PLAN.md`)
//!
//! This module implements the **raw/staging layer** of the product:
//! `File → Parquet`. "Raw" does **not** mean "bytes as they are": it means we
//! carry the data over *faithfully* — the right encoding (no mojibake), the
//! right delimiter, headers, and per-column types from Polars inference.
//! We do **not** fix business-level problems here (negative amounts, broken
//! emails, duplicates…). Those belong to the validation/normalization layer
//! that turns a staging dataset into an ODS (see `ТЗ.md`: File → Schema →
//! Validate → Normalize → Parquet → ODS). If mojibake gets into staging, no
//! later rule can repair it — hence encoding support lives here, in the raw
//! layer, not in ODS.
//!
//! ## Formats & encodings supported (M0.1)
//!
//! * Delimited text — CSV/TSV/`;`-separated/`|`-separated (auto-detected
//!   delimiter) and
//! * Apache Parquet (native columns, no text decoding needed).
//!
//! Encodings for text files: UTF-8 (with or without BOM), UTF-16 LE/BE (BOM),
//! windows-1251, windows-1252. Detection is automatic: BOM wins, then a strict
//! UTF-8 check, then a small Cyrillic heuristic between windows-1251/1252.
//! A human override (explicit "treat as …") is planned for the schema step in M1.

use encoding_rs::WINDOWS_1251;
use polars::prelude::*;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Folder sources: a directory becomes one dataset directory of Parquet parts
/// ([`folder::scan_folder`], [`folder::folder_to_parquet`], [`folder::preview_parts`]).
pub mod folder;
pub use folder::{
    DatasetPart, FileMeta, FolderReport, FolderScan, StagedFile, folder_to_parquet,
    folder_to_parquet_partitioned, list_parts, preview_parts, scan_folder,
};

/// Schema inference from files/folders ([`schema::schema_from_file`],
/// [`schema::schema_from_folder`]) — the "Schemas" milestone of M1b.
pub mod schema;
pub use schema::{
    FolderSchema, SchemaColumn, SchemaConflict, SchemaProposal, schema_from_file,
    schema_from_folder, stage_folder_with_schema,
};

/// Project persistence: `project.toml`, saved schemas, token helpers.
pub mod project;
pub use project::{
    ColumnDef, ProjectMeta, SchemaFile, create_project, delimiter_from_token, delimiter_token,
    encoding_from_token, encoding_token, load_schema, open_project, save_schema, schema_names,
};

/// Excel (XLSX/XLS) reading via calamine + the engine's CSV pipeline.
pub mod excel;
pub use excel::read_excel_frame;

/// Workspace model (M1c): config + bindings + scan roots + entity candidates.
pub mod workspace;
pub use workspace::{
    Binding, WorkspaceConfig, candidate_entity_name, create_workspace, data_dir,
    list_entity_candidates, open_workspace, save_config, schemas_dir, upsert_binding,
};

// ---------------------------------------------------------------------------
// Public domain types
// ---------------------------------------------------------------------------

/// Crate-wide error type.
///
/// Wraps the three error sources the engine can hit: the OS file system,
/// Polars itself, and text decoding (encodings).
#[derive(Debug, Error)]
pub enum StrataError {
    /// Polars needs a UTF-8 path; a path that is not valid UTF-8 cannot be used.
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),

    /// An operating-system error (file not found, permission denied, ...).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// An error raised by the Polars engine.
    #[error("data engine error: {0}")]
    Engine(#[from] PolarsError),

    /// The file bytes could not be decoded with the detected charset.
    #[error("cannot decode file: {0}")]
    Encoding(String),

    /// A partitioning column must hold string values (M1b, partitioned writes).
    #[error("partition column must be a string column: {0}")]
    PartitionColumn(String),

    /// A project directory already contains `project.toml`.
    #[error("project already exists: {0}")]
    ProjectExists(PathBuf),

    /// Project/schema TOML could not be serialized or parsed.
    #[error("project file error: {0}")]
    ProjectFile(String),
}

/// Convenience alias used by every public function of this crate.
pub type Result<T> = std::result::Result<T, StrataError>;

/// What kind of source file we are dealing with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// A delimited text file (CSV, TSV, `;`-separated, ...) with the detected
    /// delimiter. Carried as `char` for easy display; Polars wants `u8`.
    DelimitedText { delimiter: char },
    /// An Apache Parquet file (columnar, binary).
    Parquet,
    /// An Excel workbook (first worksheet is read).
    Excel,
}

impl SourceKind {
    /// Short human label used in UI summaries, e.g. `CSV` / `Parquet`.
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

/// Provenance facts about a loaded source file: format + encoding.
///
/// Shown to the user so they can *verify* the raw layer did not silently
/// misread their file (this is the whole point of the staging philosophy above).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    /// Detected file kind.
    pub kind: SourceKind,
    /// Detected text encoding, e.g. `"UTF-8"`, `"windows-1251"`, `"UTF-16 LE"`.
    /// Parquet files are binary: encoding is `"—"`.
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
    /// One-line summary like `CSV · delimiter ';' · windows-1251`.
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

/// One column of a previewed table: its name and its inferred Polars data type
/// rendered as a string (e.g. `"Int64"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    /// Column header as it appears in the file.
    pub name: String,
    /// String rendering of the Polars [`DataType`] inferred for this column.
    pub dtype: String,
}

/// A "head" preview of a source file plus provenance ([`Preview::source`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// Columns in file order, with inferred dtypes.
    pub columns: Vec<ColumnInfo>,
    /// Up to `max_rows` rows, in file order; each cell is a string.
    pub rows: Vec<Vec<String>>,
    /// What the file turned out to be (format, encoding).
    pub source: SourceInfo,
}

/// Summary of a finished File → Parquet staging run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// Total number of data rows written (header excluded).
    pub rows: u64,
    /// Number of columns written.
    pub columns: usize,
    /// Number of source files ingested (always 1 in M0).
    pub source_files: usize,
    /// Absolute path of the written Parquet file, as displayed to the user.
    pub parquet_path: String,
    /// Provenance facts of the source that was staged.
    pub source: SourceInfo,
    /// How many Parquet parts/partitions were written (1 = plain single file).
    pub partitions: usize,
}

/// A text encoding the user can force instead of auto-detection.
///
/// Auto-detection (BOM → strict UTF-8 → Cyrillic heuristic) is right in most
/// cases, but not all — e.g. a windows-1252 file whose bytes happen to look
/// like windows-1251. This enum lets the user (or, later, a saved project
/// schema) say "no, read it as …". See [`ReaderOptions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodingChoice {
    /// Standard UTF-8 (a UTF-8 BOM, if present, is still stripped).
    Utf8,
    /// windows-1251 (Cyrillic).
    Windows1251,
    /// windows-1252 (Western European / Latin-1 superset).
    Windows1252,
    /// UTF-16 little-endian.
    Utf16Le,
    /// UTF-16 big-endian.
    Utf16Be,
}

impl EncodingChoice {
    /// Human-readable name, reused for the provenance line.
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

/// Overrides applied when reading a *text* source. Defaults keep the auto
/// behaviour the raw layer had since M0.1 (`None` = auto-detect).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderOptions {
    /// Forced text encoding, or `None` for auto-detection.
    pub encoding: Option<EncodingChoice>,
    /// Forced field delimiter, or `None` for auto-detection.
    pub delimiter: Option<char>,
    /// Whether the first row is a header with column names. Default `true`;
    /// untick for header-less files (Polars then auto-names columns
    /// `column_0, column_1, …` and treats the first row as data).
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
// Public API
// ---------------------------------------------------------------------------

/// Read at most `max_rows` rows of any supported source file ([`SourceKind`]).
///
/// Equivalent to [`preview_source_with`] with default (auto) [`ReaderOptions`].
///
/// # Errors
/// I/O errors, decode errors (see [`StrataError::Encoding`]) and Polars parse
/// errors are all reported through [`StrataError`].
pub fn preview_source(path: &Path, max_rows: usize) -> Result<Preview> {
    preview_source_with(path, max_rows, ReaderOptions::default())
}

/// Like [`preview_source`], but honouring manual [`ReaderOptions`] overrides
/// (encoding / delimiter) for text files.
pub fn preview_source_with(
    path: &Path,
    max_rows: usize,
    options: ReaderOptions,
) -> Result<Preview> {
    let (frame, source) = open_any(path, options, Some(max_rows))?;
    Ok(preview_from_frame(&frame, source))
}

/// Stage any supported source file into a single Parquet file (raw layer).
///
/// Equivalent to [`source_to_parquet_with`] with default (auto)
/// [`ReaderOptions`]. "Stage" = faithful carry-over: decode/type correctly,
/// but change no values (see module docs).
///
/// # Errors
/// Same error surface as [`preview_source`].
pub fn source_to_parquet(path: &Path, parquet_path: &Path) -> Result<ImportReport> {
    source_to_parquet_with(path, parquet_path, ReaderOptions::default())
}

/// Like [`source_to_parquet`], but honouring manual [`ReaderOptions`]
/// overrides for text files.
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

/// Stage one file into a *partitioned* dataset directory.
///
/// Rows are grouped by the distinct values of `partition_column` (a string
/// column, e.g. `date` = `2026-01-05` or `city` = `Moscow`) and each group is
/// written under `<dest_root>/<column>=<value>/part-….parquet` — a
/// Hive-style layout that downstream tools (Polars, DuckDB, …) read natively.
///
/// The column must be a Polars `String` column; anything else fails loudly
/// with [`StrataError::PartitionColumn`] (partitioning needs clean, known
/// values — a validator/ODS concern, not a raw-stage guess).
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

    // Distinct values in first-seen order (small; partition keys are low-cardinality).
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

    // Capture width before the loop: `DataFrame::lazy()` consumes the frame,
    // so each iteration works on a cheap clone.
    let total_columns = frame.width();
    let mut rows_total = 0u64;
    for (index, value) in values.iter().enumerate() {
        // Hive naming: `<column>=<value>`; sanitize so no path separator/weird
        // char can escape the partition directory.
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

/// Replace characters that are unsafe in directory/file names with `_`.
/// This keeps a Hive partition folder (`city=New York` → `city=New_York`)
/// valid on every OS.
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

/// Open any supported file into a materialized frame plus provenance.
///
/// Central decision point shared by preview and staging so both always agree:
/// 1. Parquet → native scan (no text decoding);
/// 2. text → resolve (charset, delimiter) from [`ReaderOptions`] or auto
///    detection, then read (lazy for pure auto-UTF-8, decode-then-parse
///    otherwise).
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
    // Lazy streaming is only safe when we did *not* force an encoding: an
    // explicit choice must be validated strictly (decode the whole file), so
    // a wrong override fails loudly instead of silently producing mojibake.
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

/// Decide (charset, delimiter) for a text file: `ReaderOptions` overrides win,
/// otherwise auto-detection (BOM → strict UTF-8 → Cyrillic heuristic) runs.
fn resolve_text_parameters(head: &[u8], options: ReaderOptions) -> Result<(Charset, char)> {
    let charset = match options.encoding {
        Some(choice) => {
            let charset = choice.into_charset();
            // Even a forced UTF-8 read must strip a UTF-8 BOM, otherwise the
            // BOM becomes part of the first header name.
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
// Backwards-compatible CSV conveniences (used by tests and older callers)
// ---------------------------------------------------------------------------

/// CSV-only convenience: [`preview_source`] with the sample expectations of M0.
pub fn preview_csv(path: &Path, max_rows: usize) -> Result<Preview> {
    preview_source(path, max_rows)
}

/// CSV-only convenience: [`source_to_parquet`].
pub fn csv_to_parquet(csv_path: &Path, parquet_path: &Path) -> Result<ImportReport> {
    source_to_parquet(csv_path, parquet_path)
}

// ---------------------------------------------------------------------------
// Internals: file probing
// ---------------------------------------------------------------------------

/// How many leading bytes we read to sniff the format/encoding/delimiter.
const HEAD_BYTES: usize = 32 * 1024;

/// Candidate delimiters, in the order we prefer when counts tie.
const DELIMITER_CANDIDATES: [char; 4] = [',', ';', '\t', '|'];

/// Read up to `limit` bytes from the start of `path`.
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

/// How many leading bytes a *preview* reads when the file needs decoding.
/// Pure UTF-8 previews stream lazily and never buffer the file; non-UTF-8
/// files must be decoded to UTF-8 first, so we cap that work to a few rows'
/// worth instead of decoding a multi-GB file to show 50 rows.
const PREVIEW_DECODE_BUDGET: usize = 4 * 1024 * 1024;

/// Clip a byte prefix to the last complete record (newline), so a truncated
/// decode never parses a ragged half-line. Falls back to the whole prefix when
/// there is no newline at all (e.g. a single-line file).
fn clip_to_record_boundary(bytes: &[u8]) -> &[u8] {
    if bytes.is_empty() || bytes[bytes.len() - 1] == b'\n' {
        return bytes;
    }
    match bytes.iter().rposition(|&b| b == b'\n') {
        Some(index) => &bytes[..=index],
        None => bytes,
    }
}

/// A Parquet file is identified by its magic bytes (`PAR1` at offset 0),
/// with the extension as a fallback hint for truncated reads.
/// An Excel workbook is detected by extension (XLSX/XLS).
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

/// Encodings the raw layer can read. Auto-detected from a byte prefix.
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
    /// Human-readable name shown in the UI.
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
    /// Map a public, user-selectable encoding onto the internal decoder set.
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

/// Detect the charset of a text file from its leading bytes.
///
/// Priority: byte-order marks (authoritative) → strict UTF-8 → a small
/// heuristic between windows-1251 and windows-1252 (both map every byte, so
/// neither ever "errors"; we pick the one that yields more Cyrillic text).
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

/// Byte-offset of the BOM for `charset`, when present. Explicit UTF-16 reads
/// are valid *with or without* a BOM, so we only strip what is actually there.
fn bom_len(charset: Charset, bytes: &[u8]) -> usize {
    match charset {
        Charset::Utf8Bom if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) => 3,
        Charset::Utf16Le if bytes.starts_with(&[0xFF, 0xFE]) => 2,
        Charset::Utf16Be if bytes.starts_with(&[0xFE, 0xFF]) => 2,
        _ => 0,
    }
}

/// Decode *sampling* bytes (may be truncated mid-character): lossy is fine,
/// we only use the result to find the first line and the delimiter.
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

/// Decode a *whole* file strictly. For declared-UTF-8 content a stray invalid
/// byte becomes a hard error (better than silently inserting U+FFFD into
/// staging data).
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

/// First physical line of the decoded sample (empty string if none yet).
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

/// Detect the field delimiter from the header line, ignoring quoted regions.
///
/// Returns the candidate with the highest occurrence count, `,` as a default
/// when nothing matches (a single-column file has no delimiter at all).
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
// Internals: reading into DataFrames
// ---------------------------------------------------------------------------

/// Read a delimited text file into a frame, choosing the right path:
/// * pure auto-detected UTF-8 → lazy stream straight from the file (scalable);
/// * anything else (incl. every *forced* encoding) → decode the whole file
///   into UTF-8 first, then parse from memory. Forced encodings are validated
///   strictly on purpose: a wrong override must fail loudly, not mojibake.
///
/// `max_rows: None` reads everything (staging); `Some(n)` reads a preview.
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
        // Preview of a non-UTF-8 file: decode only a bounded prefix (clipped
        // to a whole record) instead of the entire file — the memory/CPU win
        // that keeps a 2 GB cp1251 export cheap to peek at.
        let bytes = read_head(path, PREVIEW_DECODE_BUDGET)?;
        let bytes = clip_to_record_boundary(&bytes).to_vec();
        let text = decode_bytes(&bytes, charset)?;
        read_text_from_buffer(text, delimiter, has_header, Some(rows))
    } else {
        // Full staging read: whole file, decoded strictly.
        let bytes = std::fs::read(path)?;
        let text = decode_bytes(&bytes, charset)?;
        read_text_from_buffer(text, delimiter, has_header, None)
    }
}

/// Stream a pure-UTF-8 delimited file through the lazy engine.
///
/// `max_rows: None` means "read everything" (used by staging); `Some(n)`
/// limits the parse (used by previews) so we never read a huge file fully
/// just to show 50 rows.
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

/// Parse already-decoded UTF-8 text from an in-memory buffer (eager).
///
/// Used when the file needed transcoding; Polars can only parse UTF-8, so we
/// hand it a `Cursor` over the decoded bytes. M0.1 reads whole files here;
/// chunked streaming for very large non-UTF-8 files is a later milestone.
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

/// Scan the head of a Parquet file (lazy; `limit` lets the engine read only
/// what it needs for the requested number of rows).
fn scan_parquet_head(path: &Path, max_rows: Option<usize>) -> Result<DataFrame> {
    let lazy = LazyFrame::scan_parquet(to_plref_path(path)?, Default::default())?;
    let limited = match max_rows {
        Some(n) => lazy.limit(n as IdxSize),
        None => lazy,
    };
    Ok(limited.collect()?)
}

// ---------------------------------------------------------------------------
// Internals: output shaping
// ---------------------------------------------------------------------------

/// Turn a materialized frame into a [`Preview`] (strings for the UI).
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
                // A failed cell read should never happen on a well-formed frame;
                // we degrade to an empty cell instead of panicking the UI.
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

/// Render one cell (`AnyValue`) as the plain text a user expects to see.
///
/// Most values format fine via `Display`, but Polars renders `String` cells
/// *with surrounding quotes* (`"Globex"`) because its `Display` mirrors the
/// debugging convention. A preview table should show `Globex`, so the two
/// string variants are special-cased here.
fn cell_to_string(value: &AnyValue<'_>) -> String {
    match value {
        AnyValue::String(text) => (*text).to_string(),
        AnyValue::StringOwned(text) => text.to_string(),
        other => format!("{other}"),
    }
}

/// Convert a `std::path::Path` into the `PlRefPath` Polars 0.55 uses for scans.
///
/// Polars represents file paths as UTF-8 strings (they also serve cloud
/// locations such as `s3://...`), so a non-UTF-8 local path is a domain error
/// rather than something we can silently mangle.
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

    /// Unique temp paths: tests run in parallel, so names must never clash.
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
    // Optimization guards (M-perf)
    // ------------------------------------------------------------------

    #[test]
    fn clip_to_record_boundary_never_leaves_a_ragged_tail() {
        // Ends with a newline: unchanged.
        assert_eq!(clip_to_record_boundary(b"a,b\n1,2\n"), b"a,b\n1,2\n");
        // Ends mid-record: the partial tail is dropped.
        assert_eq!(clip_to_record_boundary(b"a,b\n1,2\n3,"), b"a,b\n1,2\n");
        // No newline at all (single line): kept whole.
        assert_eq!(clip_to_record_boundary(b"only,one,line"), b"only,one,line");
    }

    // ------------------------------------------------------------------
    // M0 basics
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
        // Provenance: it was plain UTF-8 CSV with a comma delimiter.
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
    // M0.1: encodings and delimiters (the "raw layer fidelity" tests)
    // ------------------------------------------------------------------

    #[test]
    fn cp1251_semicolon_file_is_decoded_without_mojibake() {
        // A classic Russian Excel export: windows-1251 + ';' delimiter.
        let text = "дата;сумма;клиент\n2026-01-05;12.50;ООО Ромашка\n2026-01-06;7.25;ИП Иванов\n";
        let (bytes, _, _) = WINDOWS_1251.encode(text);
        let path = write_temp_bytes(&bytes, "cp1251_semicolon.csv");

        let preview = preview_source(&path, 50).expect("cp1251 preview should succeed");

        // No mojibake: headers decoded to real Russian words.
        let names: Vec<&str> = preview.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["дата", "сумма", "клиент"]);
        assert_eq!(preview.rows[0][2], "ООО Ромашка");

        // Provenance reflects what we detected.
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
        // encoding_rs's UTF-16LE is decode-only (its `encode` returns UTF-8,
        // per its docs), so we build LE bytes by hand for this test.
        let mut bytes = vec![0xFF, 0xFE]; // UTF-16 LE BOM
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
        // Stage a CSV first, then preview the produced Parquet file.
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
        // The staging promise: no mojibake even after the raw layer.
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
    // M1b: manual ReaderOptions overrides (when auto-detection is wrong)
    // ------------------------------------------------------------------

    #[test]
    fn encoding_override_fixes_windows1252_misdetection() {
        // "café" in windows-1252 is bytes ...E9. Auto-detection prefers
        // windows-1251 here (decoding 0xE9 as 1251 yields Cyrillic 'й',
        // which wins the Cyrillic heuristic). Forcing 1252 must read 'é'.
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

        // Staging honours the same override.
        let parquet = unique_temp("cp1252.parquet");
        let report = source_to_parquet_with(&path, &parquet, options).expect("forced 1252 stage");
        assert_eq!(report.source.encoding, "windows-1252");
        assert_eq!(report.columns, 2);

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(parquet);
    }

    #[test]
    fn delimiter_override_changes_parsing() {
        // Auto-detection finds '|'; forcing ',' must split nothing and yield
        // one wide column — proving the override is really applied.
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
        // A wrong explicit override must error, not silently corrupt.
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
