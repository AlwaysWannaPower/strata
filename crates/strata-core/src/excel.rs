//! # Excel (XLSX/XLS) support — engine (M2)
//!
//! Polars' Rust bindings do **not** read Excel natively, so this module uses
//! `calamine` (a pure-Rust workbook reader) and then feeds the rows through
//! the same UTF-8 CSV pipeline the rest of the engine uses — which gives us
//! Polars type inference, the header option and strict decode for free.
//!
//! The conversion is: workbook → (first) sheet → in-memory CSV text →
//! [`crate::read_text_from_buffer`]. Cost is one sheet-sized buffer per read;
//! acceptable for staging-layer sizes, and the same "preview reads a bounded
//! prefix" rule applies via `max_rows` (calamine stops iterating rows early,
//! so we never load a whole giant sheet just to show 50 rows).
//!
//! ## Fidelity note (honest)
//! Data *formatting* (colors, number formats, fonts) is intentionally lost —
//! we carry values, not presentation. Numbers and strings keep their types;
//! dates arrive as Excel serial numbers/strings depending on the file.

use calamine::{Data, Reader, Xlsx};
use polars::prelude::DataFrame;
use std::path::Path;

use crate::read_text_from_buffer;

/// Read the first worksheet of an Excel workbook into a Polars `DataFrame`.
///
/// * `has_header` — first row is column names (same option as for CSV);
/// * `max_rows: None` reads the whole sheet (staging), `Some(n)` stops early
///   (preview — cheap even on huge sheets).
///
/// Excel has no single "delimiter", so reader delimiters/encodings do not
/// apply; values are already decoded Unicode by `calamine`.
pub fn read_excel_frame(
    path: &Path,
    has_header: bool,
    max_rows: Option<usize>,
) -> crate::Result<DataFrame> {
    let mut workbook: Xlsx<_> = calamine::open_workbook(path).map_err(|err| {
        crate::StrataError::Encoding(format!("cannot open workbook {}: {err}", path.display()))
    })?;

    // First sheet by default (sheet selection is a later milestone).
    let sheet_name = workbook
        .sheet_names()
        .into_iter()
        .next()
        .ok_or_else(|| crate::StrataError::Encoding("workbook has no sheets".into()))?;
    let range = workbook
        .worksheet_range(&sheet_name)
        .map_err(|err| crate::StrataError::Encoding(format!("read sheet '{sheet_name}': {err}")))?;

    // Rows as cells → write them as a tiny in-memory CSV that our text reader
    // can parse with Polars inference. Quotes/commas/newlines are escaped so
    // cell text never corrupts the CSV shape.
    let mut csv = String::with_capacity(64 * 1024);
    let mut emitted_rows = 0usize;
    for (row_index, row) in range.rows().enumerate() {
        if let Some(limit) = max_rows {
            // Keep the header row, then stop once we emitted enough data rows.
            let data_emitted = if has_header {
                emitted_rows.saturating_sub(1)
            } else {
                emitted_rows
            };
            if row_index > 0 && data_emitted >= limit {
                break;
            }
        }
        emit_csv_row(&mut csv, row)?;
        emitted_rows += 1;
    }
    // A header-only file still needs a trailing newline for the CSV parser.
    if !csv.ends_with('\n') {
        csv.push('\n');
    }

    // Reuse the engine's eager CSV parse (comma-delimited, UTF-8 by construction).
    read_text_from_buffer(csv, ',', has_header, max_rows)
}

/// Append one sheet row to the in-memory CSV with proper quoting.
fn emit_csv_row(out: &mut String, row: &[Data]) -> crate::Result<()> {
    for (index, cell) in row.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let text = cell_text(cell);
        if text.contains([',', '"', '\n', '\r']) {
            out.push('"');
            for ch in text.chars() {
                if ch == '"' {
                    out.push('"'); // escape embedded quotes (CSV convention)
                }
                out.push(ch);
            }
            out.push('"');
        } else {
            out.push_str(&text);
        }
    }
    out.push('\n');
    Ok(())
}

/// Render one Excel cell to text for CSV round-tripping.
fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Int(value) => value.to_string(),
        Data::Float(value) => format_float(*value),
        Data::String(value) => value.clone(),
        Data::Bool(value) => value.to_string(),
        Data::DateTime(value) => format!("{value}"), // Excel serial date
        Data::DateTimeIso(value) => value.clone(),
        Data::DurationIso(value) => value.clone(),
        Data::Error(error) => format!("ERROR:{error}"),
        Data::Empty => String::new(),
    }
}

/// Keep floats readable (`12.5`, not `12.5000000000001`) — a display-level
/// concern only; Polars will still type the column as f64.
fn format_float(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        let text = format!("{value}");
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview_source_with;
    use rust_xlsxwriter::Workbook;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_xlsx(tag: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "strata_xlsx_{}_{}_{}.xlsx",
            std::process::id(),
            n,
            tag
        ))
    }

    fn write_sample(path: &std::path::Path, rows: &[(&str, f64)]) {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_worksheet();
        sheet.write_string(0, 0, "date").unwrap();
        sheet.write_string(0, 1, "amount").unwrap();
        for (i, &(date, amount)) in rows.iter().enumerate() {
            let row = (i + 1) as u32;
            sheet.write_string(row, 0, date).unwrap();
            sheet.write_number(row, 1, amount).unwrap();
        }
        workbook.save(path).unwrap();
    }

    #[test]
    fn excel_file_is_read_into_typed_columns() {
        let path = temp_xlsx("basic");
        write_sample(&path, &[("2026-01-05", 12.5), ("2026-01-06", 7.25)]);

        let options = crate::ReaderOptions {
            has_header: true,
            ..crate::ReaderOptions::default()
        };
        let preview = preview_source_with(&path, 50, options).expect("preview xlsx");
        assert_eq!(preview.source.kind, crate::SourceKind::Excel);
        assert_eq!(preview.columns.len(), 2);
        assert_eq!(preview.columns[0].name, "date");
        assert_eq!(preview.columns[0].dtype, "str");
        assert_eq!(preview.columns[1].dtype, "f64");
        assert_eq!(preview.rows.len(), 2);
        assert_eq!(preview.rows[0][0], "2026-01-05");
        assert_eq!(preview.rows[0][1], "12.5");

        // Staging an Excel file works through the generic pipeline too.
        let parquet = path.with_extension("parquet");
        let report = crate::source_to_parquet(&path, &parquet).expect("stage xlsx");
        assert_eq!(report.rows, 2);
        assert_eq!(report.columns, 2);
        assert_eq!(report.source.kind, crate::SourceKind::Excel);

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(parquet);
    }
}
