//! # Ingest benchmark (engine, release build)
//!
//! A tiny, honest benchmark for the raw layer: it generates a CSV with
//! `ROWS` data rows, then measures:
//!
//! 1. `preview_source` (must stay cheap — previews must NOT read whole files);
//! 2. `source_to_parquet` (full staging of the same file);
//! 3. peak heap via `mallinfo2`-style… (not portable — we measure wall time and
//!    rely on the bounded-preview design for memory).
//!
//! Run: `./scripts/bench.sh` (release profile with thin-LTO).
//!
//! These numbers are wall-clock on your machine; use them as *relative* signal
//! (e.g. after an optimization) rather than absolute truths.

use std::io::Write;
use std::time::Instant;

use strata_core::{preview_source, source_to_parquet};

/// Data rows to generate (200k ≈ tens of MB of CSV).
const ROWS: usize = 200_000;

fn main() {
    // --- 1. generate the sample file ---------------------------------------
    let dir = std::env::temp_dir().join("strata_bench");
    std::fs::create_dir_all(&dir).expect("create bench dir");
    let csv_path = dir.join("rows.csv");
    let parquet_path = dir.join("rows.parquet");

    let t0 = Instant::now();
    {
        let mut file = std::fs::File::create(&csv_path).expect("create csv");
        let header = b"id,date,amount,customer\n";
        file.write_all(header).expect("header");
        let mut line = Vec::with_capacity(64);
        for i in 0..ROWS {
            line.clear();
            let _ = write!(
                &mut line,
                "{i},2026-01-{:02},{}.{},\"Customer {i}\"\n",
                (i % 28) + 1,
                i % 1000,
                i % 100
            );
            file.write_all(&line).expect("row");
        }
    }
    println!(
        "generated {} rows in {:?} ({:?})",
        ROWS,
        t0.elapsed(),
        csv_path
    );

    // --- 2. preview --------------------------------------------------------
    let t1 = Instant::now();
    let preview = preview_source(&csv_path, 50).expect("preview");
    println!(
        "preview 50 rows: {:?}  ({} cols, {} rows shown)",
        t1.elapsed(),
        preview.columns.len(),
        preview.rows.len()
    );

    // --- 3. full staging -----------------------------------------------------
    let t2 = Instant::now();
    let report = source_to_parquet(&csv_path, &parquet_path).expect("stage");
    println!(
        "stage {} rows -> {:?}  ({}, {} MB file)",
        report.rows,
        t2.elapsed(),
        parquet_path.display(),
        std::fs::metadata(&parquet_path)
            .map(|m| m.len() / 1_000_000)
            .unwrap_or(0)
    );

    // --- 4. schema-validated folder staging (2 files) -----------------------
    // Reuse the same file twice under a folder to exercise folder_to_parquet.
    let folder = dir.join("folder");
    std::fs::create_dir_all(&folder).expect("mkdir");
    std::fs::copy(&csv_path, folder.join("a.csv")).expect("copy a");
    std::fs::copy(&csv_path, folder.join("b.csv")).expect("copy b");
    let out = dir.join("out");
    let t3 = Instant::now();
    let folder_report = strata_core::folder_to_parquet(&folder, &out).expect("folder stage");
    println!(
        "folder stage (2 files): {:?}  ({} rows total)",
        t3.elapsed(),
        folder_report.total_rows
    );

    // --- 5. cleanup ----------------------------------------------------------
    let _ = std::fs::remove_dir_all(&dir);
    println!("done (bench files removed)");
}
