//! # Folder sources: a directory = one raw dataset (M1a)
//!
//! The product idea (see `ТЗ.md` §2-§6 and `PLAN.md` M1) is that a **folder
//! with many files** becomes one logical `Source` and, after staging, one
//! **Parquet dataset** — a directory of `part-*.parquet` files, not a single
//! giant file.
//!
//! This module adds that folder story on top of the single-file staging of
//! [`crate::source_to_parquet`]:
//!
//! * [`scan_folder`] — list what a folder contains (names, sizes, rough kind)
//!   for the UI, *without* reading the data;
//! * [`folder_to_parquet`] — stage every file in a folder into its own
//!   `NNNN-<name>.parquet` part under a destination directory (each part is a
//!   faithful raw carry-over, see the staging philosophy in the crate docs);
//!   per-file failures are collected and reported, not fatal — like a real
//!   ETL run that keeps going when some file is broken;
//! * [`preview_parts`] — read back a dataset directory for a combined preview
//!   (merges only parts whose columns match the first part).
//!
//! Partitioning by business keys (`year=…/month=…`) and a shared
//! schema/validation step arrive in the next M1 slice; here every source file
//! simply becomes one part file.

use std::fs;
use std::path::Path;

use crate::{
    ColumnInfo, Preview, Result, SourceInfo, SourceKind, preview_source, source_to_parquet,
};

/// One file discovered inside a folder source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    /// File name (not the full path) — what the UI shows.
    pub name: String,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Rough kind label used for the listing: `CSV`, `TSV`, `Parquet`, …
    /// (Determined cheaply from extension + magic bytes, *before* real parsing;
    /// the authoritative per-file detection still happens at staging time.)
    pub kind: String,
}

/// Result of [`scan_folder`]: everything the UI needs to show a folder source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderScan {
    /// Files of the folder, sorted by name (deterministic order for the UI).
    pub files: Vec<FileMeta>,
    /// Sum of file sizes (bytes) — the "12.4 GB" card from `ТЗ.md` §5.
    pub total_size_bytes: u64,
}

/// One successfully staged file inside a dataset directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedFile {
    /// Source file name that produced this part.
    pub name: String,
    /// Absolute path of the written `NNNN-….parquet` part.
    pub part_path: String,
    /// Number of data rows staged from this file.
    pub rows: u64,
    /// Number of columns staged from this file.
    pub columns: usize,
}

/// Result of [`folder_to_parquet`] — the ETL-style run report.
///
/// Successful parts land in [`FolderReport::staged`]; files that could not be
/// read are listed in [`FolderReport::skipped`] with the error text so the
/// user can see *what* was skipped and *why* (this is the "problems" idea of
/// `ТЗ.md` §8 in its simplest raw form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderReport {
    /// Successfully staged parts.
    pub staged: Vec<StagedFile>,
    /// `(file name, error)` pairs for files that could not be staged.
    pub skipped: Vec<(String, String)>,
    /// Sum of rows across all staged parts.
    pub total_rows: u64,
    /// Destination directory holding the parts.
    pub dest_dir: String,
}

/// Inspect a folder (non-recursive, top level only in M1a) and describe its
/// files for the UI.
///
/// # Errors
/// Returns [`crate::StrataError::Io`] if the folder cannot be read at all.
pub fn scan_folder(dir: &Path) -> Result<FolderScan> {
    let mut files = Vec::new();
    let mut total_size_bytes = 0u64;

    let mut entries: Vec<_> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .collect();
    // Deterministic listing: sorting by name keeps the UI stable between runs
    // and gives the part numbering (`NNNN-…`) a predictable order.
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // hidden files (like .DS_Store) are not data
        }
        let path = entry.path();
        let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        total_size_bytes += size_bytes;
        files.push(FileMeta {
            name,
            size_bytes,
            kind: cheap_kind_label(&path),
        });
    }

    Ok(FolderScan {
        files,
        total_size_bytes,
    })
}

/// Cheap kind label without parsing: Parquet has a `PAR1` magic prefix, text
/// kinds fall back to the file extension.
fn cheap_kind_label(path: &Path) -> String {
    if let Ok(mut file) = fs::File::open(path) {
        use std::io::Read;
        let mut magic = [0u8; 4];
        if file.read_exact(&mut magic).is_ok() && &magic == b"PAR1" {
            return "Parquet".to_string();
        }
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "csv" => "CSV".to_string(),
        "tsv" => "TSV".to_string(),
        "txt" => "Text".to_string(),
        "parquet" | "pq" => "Parquet".to_string(),
        "xlsx" | "xls" => "Excel".to_string(),
        _ => "Other".to_string(),
    }
}

/// Stage every file of `src_dir` into a dataset directory `dest_dir`.
///
/// Each source file becomes one `NNNN-<stem>.parquet` part. Files that fail to
/// stage are skipped (with the error kept in the report) so that one broken
/// file does not stop the whole folder — a deliberate ETL-like behaviour.
///
/// # Errors
/// Returns an error only if the destination directory cannot be created.
/// Per-file problems are reported through [`FolderReport::skipped`].
pub fn folder_to_parquet(src_dir: &Path, dest_dir: &Path) -> Result<FolderReport> {
    fs::create_dir_all(dest_dir)?;

    let scan = scan_folder(src_dir)?;
    let mut staged = Vec::new();
    let mut skipped = Vec::new();
    let mut total_rows = 0u64;

    for (index, meta) in scan.files.iter().enumerate() {
        // Only files we can make sense of are staged. Anything of an unknown
        // kind (random binaries, system junk) is *reported*, not guessed at —
        // guessing would violate the "faithful carry-over" staging promise.
        if meta.kind == "Other" {
            skipped.push((meta.name.clone(), String::from("unsupported file kind")));
            continue;
        }

        let source_path = src_dir.join(&meta.name);
        let stem = source_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("file{index}"));
        // Numbered prefix keeps parts unique even when two source files share
        // the same stem ("sales.csv" next to "sales.tsv").
        let part_path = dest_dir.join(format!("{index:04}-{stem}.parquet"));

        match source_to_parquet(&source_path, &part_path) {
            Ok(report) => {
                total_rows += report.rows;
                staged.push(StagedFile {
                    name: meta.name.clone(),
                    part_path: part_path.display().to_string(),
                    rows: report.rows,
                    columns: report.columns,
                });
            }
            Err(err) => skipped.push((meta.name.clone(), err.to_string())),
        }
    }

    Ok(FolderReport {
        staged,
        skipped,
        total_rows,
        dest_dir: dest_dir.display().to_string(),
    })
}

/// Stage every file of `src_dir` into a **partitioned** dataset under
/// `dest_root`, grouping rows by the distinct string values of
/// `partition_column` (Hive-style `column=value/` directories).
///
/// Same ETL semantics as [`folder_to_parquet`]: good files → parts, unknown
/// kinds / failing files → [`FolderReport::skipped`]. Each source file may
/// produce several partition directories.
pub fn folder_to_parquet_partitioned(
    src_dir: &Path,
    dest_root: &Path,
    partition_column: &str,
) -> Result<FolderReport> {
    fs::create_dir_all(dest_root)?;
    let scan = scan_folder(src_dir)?;

    let mut staged = Vec::new();
    let mut skipped = Vec::new();
    let mut total_rows = 0u64;

    for meta in scan.files.iter() {
        if meta.kind == "Other" {
            skipped.push((meta.name.clone(), String::from("unsupported file kind")));
            continue;
        }
        let source_path = src_dir.join(&meta.name);
        match crate::source_to_parquet_partitioned(
            &source_path,
            dest_root,
            partition_column,
            crate::ReaderOptions::default(),
        ) {
            Ok(report) => {
                total_rows += report.rows;
                staged.push(StagedFile {
                    name: meta.name.clone(),
                    part_path: report.parquet_path.clone(),
                    rows: report.rows,
                    columns: report.columns,
                });
            }
            Err(err) => skipped.push((meta.name.clone(), err.to_string())),
        }
    }

    Ok(FolderReport {
        staged,
        skipped,
        total_rows,
        dest_dir: dest_root.display().to_string(),
    })
}

/// One Parquet part file inside a dataset directory (any nesting level).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasetPart {
    /// Path relative to the dataset root, e.g. `city=Moscow/part-0000-x.parquet`.
    pub rel_path: String,
    /// File size in bytes.
    pub size_bytes: u64,
}

/// List every Parquet part under `dataset_dir`, recursively (partitioned
/// datasets nest parts inside `column=value/` folders).
///
/// Sorted by relative path for a stable listing. Unknown folders/files other
/// than `*.parquet` are ignored.
pub fn list_parts(dataset_dir: &Path) -> Result<Vec<DatasetPart>> {
    let mut found = Vec::new();
    collect_parts(dataset_dir, dataset_dir, &mut found)?;
    found.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(found)
}

fn collect_parts(root: &Path, dir: &Path, found: &mut Vec<DatasetPart>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_parts(root, &path, found)?;
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("parquet"))
        {
            let rel_path = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            found.push(DatasetPart {
                rel_path,
                size_bytes,
            });
        }
    }
    Ok(())
}

/// Combined preview of a dataset directory (all `*.parquet` parts, any
/// nesting depth — plain part files and partitioned layouts).
///
/// Parts are previewed in sorted order. A part whose columns differ from the
/// first part is skipped (a schema mismatch is a *validation* concern — M2+ —
/// not something this raw preview should silently guess around).
///
/// # Errors
/// Returns [`crate::StrataError::Io`] if the directory cannot be read.
pub fn preview_parts(dataset_dir: &Path, max_rows: usize) -> Result<Preview> {
    let parts = list_parts(dataset_dir)?;

    let mut columns: Option<Vec<ColumnInfo>> = None;
    let mut rows = Vec::new();

    for part in parts {
        if rows.len() >= max_rows {
            break;
        }
        let part_path = dataset_dir.join(&part.rel_path);
        let Ok(preview) = preview_source(&part_path, max_rows - rows.len()) else {
            continue; // unreadable part: skip silently here; staging would report it
        };
        match &columns {
            None => columns = Some(preview.columns),
            // Only merge when the part matches the shape of the first part.
            Some(expected) if *expected != preview.columns => continue,
            Some(_) => {}
        }
        rows.extend(preview.rows);
    }

    Ok(Preview {
        columns: columns.unwrap_or_default(),
        rows,
        // Binary provenance; the UI adds the part count from the listing.
        source: SourceInfo {
            kind: SourceKind::Parquet,
            encoding: String::from("— (binary)"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Unique temp directories so parallel tests never clash.
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "strata_folder_{}_{}_{}",
            std::process::id(),
            n,
            tag
        ));
        let _ = fs::remove_dir_all(&dir); // clean any stale leftovers
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    const SAMPLE_CSV: &str = "id,date,amount,customer\n\
1,2026-01-02,120.50,Acme Corp\n\
2,2026-01-02,75.00,Globex\n\
3,2026-01-03,240.00,Initech\n";

    #[test]
    fn scan_folder_lists_files_with_kind_and_size() {
        let dir = temp_dir("scan");
        let mut a = fs::File::create(dir.join("sales_a.csv")).unwrap();
        a.write_all(SAMPLE_CSV.as_bytes()).unwrap();
        let mut b = fs::File::create(dir.join("sales_b.tsv")).unwrap();
        b.write_all(b"id\tname\n1\tAlice\n").unwrap();
        fs::write(dir.join("notes.txt"), b"not data at all").unwrap();

        let scan = scan_folder(&dir).expect("scan succeeds");
        assert_eq!(scan.files.len(), 3);
        // Sorted by name: "notes.txt" < "sales_a.csv" < "sales_b.tsv".
        assert_eq!(scan.files[0].kind, "Text");
        assert_eq!(scan.files[1].kind, "CSV");
        assert_eq!(scan.files[2].kind, "TSV");
        assert!(scan.total_size_bytes > 0);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn folder_staging_keeps_good_files_and_reports_bad_ones() {
        let src = temp_dir("stage_src");
        let mut a = fs::File::create(src.join("sales_a.csv")).unwrap();
        a.write_all(SAMPLE_CSV.as_bytes()).unwrap();
        let mut b = fs::File::create(src.join("sales_b.csv")).unwrap();
        b.write_all(
            "id,date,amount,customer\n4,2026-02-01,10.00,Wayne Ent\n\
             5,2026-02-02,20.00,Wayne Ent\n6,2026-02-03,30.00,Wayne Ent\n"
                .as_bytes(),
        )
        .unwrap();
        // A file that cannot be parsed as data: must be skipped, not fatal.
        fs::write(src.join("broken.dat"), b"\x00\x01\x02not a table").unwrap();

        let dest = temp_dir("stage_dst");
        let report = folder_to_parquet(&src, &dest).expect("staging succeeds");

        assert_eq!(report.staged.len(), 2, "two good files staged");
        assert_eq!(report.skipped.len(), 1, "broken file reported as skipped");
        assert_eq!(report.total_rows, 6, "3 + 3 rows across both parts");
        assert_eq!(report.skipped[0].0, "broken.dat");

        // Every staged file produced a real .parquet part on disk.
        let parts: Vec<String> = fs::read_dir(&dest)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".parquet"))
            .collect();
        assert_eq!(parts.len(), 2);

        // And the combined preview returns all rows with the same columns.
        let preview = preview_parts(&dest, 100).expect("preview parts");
        assert_eq!(preview.columns.len(), 4);
        assert_eq!(preview.rows.len(), 6);

        let _ = fs::remove_dir_all(src);
        let _ = fs::remove_dir_all(dest);
    }

    // ------------------------------------------------------------------
    // M1b (step 2): partitioned dataset writes + recursive dataset listing
    // ------------------------------------------------------------------

    #[test]
    fn partitioned_staging_writes_hive_folders_per_value() {
        let csv = temp_dir("part_src").join("sales.csv");
        fs::create_dir_all(csv.parent().unwrap()).unwrap();
        fs::write(&csv, "id,city\n1,Moscow\n2,Kazan\n3,Moscow\n4,Kazan\n").unwrap();
        let dest = temp_dir("part_dst");

        let report = crate::source_to_parquet_partitioned(
            &csv,
            &dest,
            "city",
            crate::ReaderOptions::default(),
        )
        .expect("partitioned staging succeeds");

        assert_eq!(report.rows, 4);
        assert_eq!(report.columns, 2);
        assert_eq!(report.partitions, 2, "Moscow + Kazan");

        // Hive-style folders exist and each holds a part file.
        assert!(dest.join("city=Moscow").is_dir());
        assert!(dest.join("city=Kazan").is_dir());
        let parts = list_parts(&dest).expect("list parts");
        assert_eq!(parts.len(), 2);

        // Recursive combined preview sees every row again.
        let preview = preview_parts(&dest, 100).expect("preview partitioned");
        assert_eq!(preview.columns.len(), 2);
        assert_eq!(preview.rows.len(), 4);

        let _ = fs::remove_dir_all(csv.parent().unwrap());
        let _ = fs::remove_dir_all(dest);
    }

    #[test]
    fn partitioned_folder_staging_reports_rows_and_skips() {
        let src = temp_dir("pfsrc");
        fs::write(src.join("a.csv"), "id,city\n1,Moscow\n2,Kazan\n").unwrap();
        fs::write(src.join("b.csv"), "id,city\n3,Moscow\n").unwrap();
        fs::write(src.join("junk.dat"), b"\x00\x01").unwrap();
        let dest = temp_dir("pfdst");

        let report = folder_to_parquet_partitioned(&src, &dest, "city").expect("folder stage");
        assert_eq!(report.staged.len(), 2);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, "junk.dat");
        assert_eq!(report.total_rows, 3);

        // Both cities became partition folders, parts are found recursively.
        assert!(dest.join("city=Moscow").is_dir());
        assert!(dest.join("city=Kazan").is_dir());
        assert_eq!(list_parts(&dest).expect("list").len(), 3); // 2 Moscow parts + 1 Kazan part

        let _ = fs::remove_dir_all(src);
        let _ = fs::remove_dir_all(dest);
    }

    #[test]
    fn partitioning_requires_a_string_column() {
        let csv = temp_dir("badpart").join("nums.csv");
        fs::create_dir_all(csv.parent().unwrap()).unwrap();
        fs::write(&csv, "id,amount\n1,12.5\n2,7.25\n").unwrap();
        let dest = temp_dir("badpart_dst");

        let err = crate::source_to_parquet_partitioned(
            &csv,
            &dest,
            "amount", // Float64 — not partitionable in the raw layer
            crate::ReaderOptions::default(),
        )
        .expect_err("numeric partition column must fail");

        let crate::StrataError::PartitionColumn(name) = err else {
            panic!("expected PartitionColumn error, got {err:?}");
        };
        assert_eq!(name, "amount");

        let _ = fs::remove_dir_all(csv.parent().unwrap());
        let _ = fs::remove_dir_all(dest);
    }
}
