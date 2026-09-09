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
    FileMeta, FolderReport, FolderScan, StagedFile, folder_to_parquet, preview_parts, scan_folder,
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
}

impl SourceKind {
    /// Short human label used in UI summaries, e.g. `CSV` / `Parquet`.
    pub fn label(&self) -> &'static str {
        match self {
            SourceKind::DelimitedText { delimiter: ',' } => "CSV",
            SourceKind::DelimitedText { delimiter: '\t' } => "TSV",
            SourceKind::DelimitedText { .. } => "Text",
            SourceKind::Parquet => "Parquet",
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
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Read at most `max_rows` rows of any supported source file ([`SourceKind`]).
///
/// # Errors
/// I/O errors, decode errors (see [`StrataError::Encoding`]) and Polars parse
/// errors are all reported through [`StrataError`].
pub fn preview_source(path: &Path, max_rows: usize) -> Result<Preview> {
    let head = read_head(path, HEAD_BYTES)?;

    if looks_like_parquet(path, &head) {
        let frame = scan_parquet_head(path, Some(max_rows))?;
        let source = SourceInfo {
            kind: SourceKind::Parquet,
            encoding: String::from("— (binary)"),
        };
        return Ok(preview_from_frame(&frame, source));
    }

    // Text file: detect encoding, then delimiter from the decoded first line.
    let charset = detect_charset(&head);
    let delimiter = detect_delimiter(first_line(&decode_sample(&head, charset)?));

    let frame = read_text_frame(path, charset, delimiter, Some(max_rows))?;
    let source = SourceInfo {
        kind: SourceKind::DelimitedText { delimiter },
        encoding: charset.label().to_string(),
    };
    Ok(preview_from_frame(&frame, source))
}

/// Stage any supported source file into a single Parquet file (raw layer).
///
/// "Stage" = faithful carry-over: decode/type correctly, but change no values.
/// See the module docs for the staging philosophy.
///
/// # Errors
/// Same error surface as [`preview_source`].
pub fn source_to_parquet(path: &Path, parquet_path: &Path) -> Result<ImportReport> {
    let head = read_head(path, HEAD_BYTES)?;

    let (frame, source) = if looks_like_parquet(path, &head) {
        // Parquet → Parquet: normalization only (single row-group file).
        let frame = scan_parquet_head(path, None)?;
        let source = SourceInfo {
            kind: SourceKind::Parquet,
            encoding: String::from("— (binary)"),
        };
        (frame, source)
    } else {
        let charset = detect_charset(&head);
        let delimiter = detect_delimiter(first_line(&decode_sample(&head, charset)?));
        let frame = read_text_frame(path, charset, delimiter, None)?;
        let source = SourceInfo {
            kind: SourceKind::DelimitedText { delimiter },
            encoding: charset.label().to_string(),
        };
        (frame, source)
    };

    let rows = frame.height();
    let columns = frame.width();

    let mut file = std::fs::File::create(parquet_path)?;
    let writer = ParquetWriter::new(&mut file);
    writer.finish(&mut frame.into())?;

    Ok(ImportReport {
        rows: rows as u64,
        columns,
        source_files: 1,
        parquet_path: parquet_path.display().to_string(),
        source,
    })
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

/// A Parquet file is identified by its magic bytes (`PAR1` at offset 0),
/// with the extension as a fallback hint for truncated reads.
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

/// Decode *sampling* bytes (may be truncated mid-character): lossy is fine,
/// we only use the result to find the first line and the delimiter.
fn decode_sample(bytes: &[u8], charset: Charset) -> Result<String> {
    match charset {
        Charset::Utf8 => Ok(String::from_utf8_lossy(bytes).into_owned()),
        Charset::Utf8Bom => Ok(String::from_utf8_lossy(&bytes[3.min(bytes.len())..]).into_owned()),
        Charset::Utf16Le => Ok(encoding_rs::UTF_16LE
            .decode(&bytes[2.min(bytes.len())..])
            .0
            .into_owned()),
        Charset::Utf16Be => Ok(encoding_rs::UTF_16BE
            .decode(&bytes[2.min(bytes.len())..])
            .0
            .into_owned()),
        Charset::Windows1251 => Ok(WINDOWS_1251.decode(bytes).0.into_owned()),
        Charset::Windows1252 => Ok(encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned()),
    }
}

/// Decode a *whole* file strictly. For declared-UTF-8 content a stray invalid
/// byte becomes a hard error (better than silently inserting U+FFFD into
/// staging data).
fn decode_bytes(bytes: &[u8], charset: Charset) -> Result<String> {
    match charset {
        Charset::Utf8 => String::from_utf8(bytes.to_vec())
            .map_err(|e| StrataError::Encoding(format!("file is not valid UTF-8: {e}"))),
        Charset::Utf8Bom => {
            let body = &bytes[3.min(bytes.len())..];
            String::from_utf8(body.to_vec())
                .map_err(|e| StrataError::Encoding(format!("file is not valid UTF-8: {e}")))
        }
        Charset::Utf16Le => Ok(encoding_rs::UTF_16LE
            .decode(&bytes[2.min(bytes.len())..])
            .0
            .into_owned()),
        Charset::Utf16Be => Ok(encoding_rs::UTF_16BE
            .decode(&bytes[2.min(bytes.len())..])
            .0
            .into_owned()),
        Charset::Windows1251 => Ok(WINDOWS_1251.decode(bytes).0.into_owned()),
        Charset::Windows1252 => Ok(encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned()),
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
/// * pure UTF-8 → lazy stream straight from the file (scalable);
/// * anything else → decode the whole file into UTF-8 first, then parse from
///   memory (whole-file read; chunked streaming for huge non-UTF-8 files is a
///   later milestone — see `PLAN.md` M6).
///
/// `max_rows: None` reads everything (staging); `Some(n)` reads a preview.
fn read_text_frame(
    path: &Path,
    charset: Charset,
    delimiter: char,
    max_rows: Option<usize>,
) -> Result<DataFrame> {
    if charset == Charset::Utf8 {
        read_text_lazy(path, delimiter, max_rows)
    } else {
        let bytes = std::fs::read(path)?;
        let text = decode_bytes(&bytes, charset)?;
        read_text_from_buffer(text, delimiter, max_rows)
    }
}

/// Stream a pure-UTF-8 delimited file through the lazy engine.
///
/// `max_rows: None` means "read everything" (used by staging); `Some(n)`
/// limits the parse (used by previews) so we never read a huge file fully
/// just to show 50 rows.
fn read_text_lazy(path: &Path, delimiter: char, max_rows: Option<usize>) -> Result<DataFrame> {
    let lazy = LazyCsvReader::new(to_plref_path(path)?)
        .with_has_header(true)
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
    max_rows: Option<usize>,
) -> Result<DataFrame> {
    let options = CsvReadOptions::default()
        .with_has_header(true)
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
}
