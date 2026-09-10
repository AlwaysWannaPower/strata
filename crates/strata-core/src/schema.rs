//! # Schema: column model inferred from real files (M1b, step "Schemas")
//!
//! The product promise (see `ТЗ.md` §4 and the user story in `PLAN.md`) is
//! that the user either writes a schema by hand **or** the program proposes
//! one by reading the tabular files. This module implements the second half:
//!
//! * [`schema_from_file`] — infer the column list + Polars types from one file;
//! * [`schema_from_folder`] — infer across a folder and report **conflicts**
//!   (the same column typed differently by different files — `ТЗ.md` §7) and
//!   **missing** columns per file.
//!
//! Types are the string labels Polars produces (e.g. `Int64`, `String`); the
//! schema itself is deliberately UI-friendly plain data, no Polars types leak
//! out. Writing the schema into Parquet (casting to the confirmed types)
//! happens at the "confirm & stage" step that builds on this module.

use std::collections::HashMap;
use std::path::Path;

use polars::prelude::{DataType, LazyFrame, ParquetWriter};

use crate::folder::{FileMeta, FolderReport, scan_folder};
use crate::{
    ColumnDef, ReaderOptions, SchemaFile, delimiter_from_token, encoding_from_token,
    preview_source_with,
};

/// How many rows of each file we read to infer types. Types are inferred from
/// the header + a sample of values; reading more than this rarely changes the
/// answer and only slows inference down.
const SCHEMA_SAMPLE_ROWS: usize = 200;

/// How many files of a folder are inspected (defence against huge folders).
const SCHEMA_MAX_FILES: usize = 50;

/// One column of a schema proposal: name + inferred Polars type label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaColumn {
    /// Column name (header) or Polars auto-name for header-less files.
    pub name: String,
    /// Polars data type rendered as a string, e.g. `"Int64"`.
    pub dtype: String,
}

/// Schema proposed for a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaProposal {
    /// Columns in file order.
    pub columns: Vec<SchemaColumn>,
}

/// A schema conflict: column `column` is expected to be `expected`, but the
/// listed files typed it differently (`found` = file → actual type).
///
/// Mirrors the `⚠ Schema conflict` dialog of `ТЗ.md` §7: the UI shows the
/// expected type, the found type and the affected files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaConflict {
    /// The column with inconsistent types.
    pub column: String,
    /// The type most files agree on (the "expected" one).
    pub expected: String,
    /// `(file name, actual dtype)` for every file that disagrees.
    pub found: Vec<(String, String)>,
}

/// Folder-wide schema inference result: proposal + problems.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSchema {
    /// Proposed columns (first-seen order across the inspected files).
    pub columns: Vec<SchemaColumn>,
    /// Columns with type conflicts across files.
    pub conflicts: Vec<SchemaConflict>,
    /// Columns missing in some files: `(column, files without it)`.
    pub missing: Vec<(String, Vec<String>)>,
    /// Files that could not be read/inferred (with the error text).
    pub failed: Vec<(String, String)>,
    /// How many files were successfully inspected.
    pub files_inspected: usize,
}

/// Infer a schema proposal from a single file by previewing its head.
///
/// # Errors
/// Any read/decode/parse error of the file itself.
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

/// Infer a folder-wide schema across up to [`SCHEMA_MAX_FILES`] files.
///
/// Never fails because one file is broken: unreadable files land in
/// [`FolderSchema::failed`]. See module docs for the merge rules.
pub fn schema_from_folder(dir: &Path, options: ReaderOptions) -> crate::Result<FolderSchema> {
    let scan = scan_folder(dir)?;
    let candidates: Vec<&FileMeta> = scan
        .files
        .iter()
        .filter(|meta| !matches!(meta.kind.as_str(), "Other"))
        .take(SCHEMA_MAX_FILES)
        .collect();

    // Ordered union of column names (first-seen order), the observed dtype per
    // file per column, and which files each file actually has.
    let mut column_order: Vec<String> = Vec::new();
    let mut observed: HashMap<String, Vec<(String, String)>> = HashMap::new(); // col -> (file, dtype)
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
    // Expected type per column = the most common observed type; ties are
    // broken by file order (the earliest file's type wins), so the proposal
    // is deterministic across runs.
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

        // Files whose type differs from the expected one.
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

    // Columns missing in some inspected files.
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

/// Stage every file of a folder **under a confirmed schema**.
///
/// This is the "one folder = one schema" contract made executable: each file
/// is read with the schema's reader options (encoding/delimiter/header) and
/// its inferred columns/types are compared with the confirmed
/// [`SchemaFile`]. A file that does not conform is **not** staged blindly —
/// it goes to [`FolderReport::skipped`] with a concrete reason (this is where
/// "either the files match the schema or you get an error" lives, `ТЗ.md` §7
/// in its raw-layer form). Conforming files become `NNNN-<stem>.parquet`
/// parts in `dest_dir`.
///
/// # Errors
/// Only a destination write/scan failure is fatal; per-file problems are
/// reported, never thrown.
pub fn stage_folder_with_schema(
    src_dir: &Path,
    dest_dir: &Path,
    schema: &SchemaFile,
) -> crate::Result<FolderReport> {
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

    for (index, meta) in scan.files.iter().enumerate() {
        if meta.kind == "Other" {
            skipped.push((meta.name.clone(), String::from("unsupported file kind")));
            continue;
        }
        let source_path = src_dir.join(&meta.name);

        // 1. Verify the file against the confirmed schema before writing.
        let proposal = match schema_from_file(&source_path, options) {
            Ok(proposal) => proposal,
            Err(err) => {
                skipped.push((meta.name.clone(), format!("cannot read: {err}")));
                continue;
            }
        };
        if let Some(reason) = schema_mismatch(&schema.columns, &proposal.columns) {
            skipped.push((meta.name.clone(), format!("schema mismatch: {reason}")));
            continue;
        }

        // 2. Conforming: full read with the schema's options + write the part.
        let stem = source_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("file{index}"));
        let part_path = dest_dir.join(format!("{index:04}-{stem}.parquet"));
        match source_to_parquet_typed(&source_path, &part_path, options, &schema.columns) {
            Ok(report) => {
                // Full-file type check: inference over the *whole* file can
                // differ from the sample (a late bad row turns a column into
                // String). Read the part's real schema (1 row is enough — the
                // schema lives in the Parquet header) and compare.
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
    }

    Ok(FolderReport {
        staged,
        skipped,
        total_rows,
        dest_dir: dest_dir.display().to_string(),
    })
}

/// Read the real column types of a written Parquet part and compare them with
/// the confirmed schema. Returns a mismatch reason or `None` when conforming.
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

/// Map a schema type label (as stored in `*.schema.toml`) to a Polars dtype.
///
/// The set is intentionally small: these are the types the raw layer can
/// produce and cast between. An unknown label is an error, not a guess.
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

/// Are two type labels *compatible* for the "one folder = one schema" rule?
///
/// Compatibility is wider than equality, but only where widening is lossless
/// and obvious — the raw layer never guesses:
///
/// * identical types are compatible;
/// * any integer width may widen to a wider integer (`i32` → `i64`);
/// * any integer may widen to a float (`i64` → `f64`) — the classic case of a
///   column that looks integral in one file and fractional in another;
/// * `f32` may widen to `f64`.
///
/// Everything else (string ↔ number, date ↔ string, …) is a *conversion*, not a
/// widening, and belongs to the validation/ODS layer — so it stays a mismatch.
fn types_compatible(expected: &str, actual: &str) -> bool {
    if expected == actual {
        return true;
    }
    let expected_rank = numeric_rank(expected);
    let actual_rank = numeric_rank(actual);
    match (expected_rank, actual_rank) {
        // int -> int (widen), int -> float, float -> float (f32 -> f64)
        (Some(want), Some(have)) => want >= have,
        _ => false,
    }
}

/// Numeric ordering used for widening checks: ints 1..4, floats 5..6.
/// `None` for non-numeric labels.
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

/// Stage a file into a Parquet part **cast to the confirmed schema types**.
///
/// Reads the whole file with the schema's reader options, casts any column
/// whose type is merely *compatible* (widening, see [`types_compatible`]) to
/// the declared type, then writes the part. Incompatible types never reach
/// this function — they are rejected earlier with a readable reason.
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

/// Compare the confirmed schema columns with what a file actually exposes.
///
/// Returns a human reason for the first mismatch (order, name or type), or
/// `None` when the file conforms.
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

        // With default options the first row would be eaten as a header.
        let with_header = schema_from_file(&path, ReaderOptions::default()).expect("infer");
        assert_eq!(with_header.columns.len(), 2); // id + date

        let options = ReaderOptions {
            has_header: false,
            ..ReaderOptions::default()
        };
        let proposal = schema_from_file(&path, options).expect("infer no-header");
        assert_eq!(proposal.columns.len(), 2);
        // Values are now data, and names are Polars auto-names ("column_…").
        assert!(proposal.columns[0].name.starts_with("column_"));
        assert_eq!(proposal.columns[0].dtype, "i64");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn folder_schema_reports_type_conflicts_between_files() {
        let dir = temp_path("dir");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // a.csv: amount is a number. b.csv: same column is text.
        write_text(&dir.join("a.csv"), "id,amount\n1,12.5\n2,7.25\n");
        write_text(&dir.join("b.csv"), "id,amount\n3,n/a\n4,unknown\n");

        let report = schema_from_folder(&dir, ReaderOptions::default()).expect("folder infer");
        assert_eq!(report.files_inspected, 2);
        assert!(report.failed.is_empty());

        // Both files were read; `amount` must be flagged as conflicting.
        let amount_conflict = report
            .conflicts
            .iter()
            .find(|c| c.column == "amount")
            .expect("amount conflict present");
        assert_eq!(amount_conflict.expected, "f64"); // tie → first file wins
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
        // An unknown-kind file is not attempted (like folder staging): it is
        // simply not part of the schema picture.
        fs_write(&dir.join("broken.dat"), b"\x00\x01\x02 not text at all");

        let report = schema_from_folder(&dir, ReaderOptions::default()).expect("folder infer");
        assert_eq!(report.files_inspected, 2);
        assert!(report.failed.is_empty());

        // `extra` exists only in full.csv.
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
        // id,amount,name with numeric amount and a string name.
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
        // amount is text here — violates the confirmed f64 schema.
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
        // A windows-1251, semicolon-delimited file bound to a schema that says
        // so: staging must use those options and read Russian cleanly.
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
    fn numeric_widening_is_accepted_and_cast_to_the_schema_type() {
        // One file has fractional amounts (f64), the other integral (i64).
        // The confirmed schema says f64: i64 is a *widening*, so BOTH files
        // must stage, and the written part must really be f64.
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

        // Verify the *written* types: every part must have f64 amount.
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
        // str vs f64 is a conversion, not a widening: stays an error.
        assert!(!types_compatible("f64", "str"));
        assert!(!types_compatible("str", "i64"));
        assert!(types_compatible("f64", "i64"));
        assert!(types_compatible("i64", "i32"));
        assert!(types_compatible("f64", "f32"));
        assert!(types_compatible("str", "str"));
    }

    #[test]
    fn validated_stage_rejects_late_row_type_drift() {
        // The first 100 rows (Polars' sample for inference) are numeric, so the
        // sample-based schema check passes — but row ~102 contains "n/a", which
        // turns the whole column into String on a full read. The full-file type
        // check must reject the file instead of staging a wrong-typed part.
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
        // The rejection reason is either our explicit full-file type check or
        // Polars' own strict parse error for the bad value — both mean the file
        // was never staged with a wrong type.
        let reason_ok = report.skipped[0].1.contains("full-file type check")
            || report.skipped[0].1.contains("n/a");
        assert!(
            reason_ok,
            "reason explains rejection: {}",
            report.skipped[0].1
        );
        // No leftover part on disk either.
        assert_eq!(crate::list_parts(&dest).expect("parts").len(), 0);

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dest);
    }
}
