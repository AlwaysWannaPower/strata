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

use crate::folder::{FileMeta, scan_folder};
use crate::{ReaderOptions, preview_source_with};

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
}
