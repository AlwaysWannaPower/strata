//! # strata-app — the Strata desktop shell (Dioxus 0.7, WebView).
//!
//! This binary is deliberately thin: every data operation lives in
//! [`strata_core`] and is reached through its small public API
//! ([`strata_core::preview_source`], [`strata_core::source_to_parquet`]). The UI
//! only renders plain data structures and forwards user intent.
//!
//! ## M0.1 screen
//!
//! ```text
//!  [ path to CSV / TSV / Parquet …        ] [Browse…] [Open]
//!  …or open the bundled demo:  [Load sample]
//!  ┌───────────────────────────────────────────────────┐
//!  │ CSV · delimiter ';' · windows-1251   ← provenance │
//!  │ preview table: columns + first rows                │
//!  └───────────────────────────────────────────────────┘
//!  [Stage to Parquet]   status / import report
//! ```
//!
//! The toolbar is one coherent "add a source" widget: the path field is the
//! single place a source gets named — `Browse…` only fills it, `Open` imports
//! what it contains. No duplicate "open a file" buttons.
//!
//! UI copy is English on purpose: code, comments and user-facing strings stay
//! in one language (see `PLAN.md` §5); only the `docs/` guides are in Russian.

use dioxus::prelude::*;
use std::path::PathBuf;
use strata_core::{ImportReport, Preview, preview_source, source_to_parquet};

/// The number of rows the preview shows. It is a small constant on purpose:
/// the preview must never read more than a bounded prefix of a file.
const PREVIEW_MAX_ROWS: usize = 50;

/// Name of the bundled demo file, relative to `examples/sample_csv/` at the
/// repository root (see [`sample_csv_path`]).
const SAMPLE_FILE_NAME: &str = "sales_01.csv";

/// Application entry point.
///
/// `LaunchBuilder::desktop()` selects the desktop (WebView) platform; because
/// `strata-app` enables only the `desktop` feature of Dioxus, there is exactly
/// one renderer to choose. `launch` blocks the main thread and runs the event
/// loop until the window closes.
fn main() {
    dioxus::LaunchBuilder::desktop().launch(App);
}

/// Path of the bundled sample CSV.
///
/// `CARGO_MANIFEST_DIR` is the directory of *this* crate's manifest
/// (`crates/strata-app/`), so the sample lives two levels up in
/// `examples/sample_csv/`. The check with `try_exists` turns a path mistake
/// (repo moved, different layout) into a clear error instead of a panic.
fn sample_csv_path() -> Result<PathBuf, String> {
    let candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/sample_csv")
        .join(SAMPLE_FILE_NAME);
    match candidate.try_exists() {
        Ok(true) => Ok(candidate),
        Ok(false) => Err(format!("sample file not found at {}", candidate.display())),
        Err(err) => Err(format!("cannot inspect {}: {err}", candidate.display())),
    }
}

/// Native "open file" dialog offering every format the raw layer understands.
fn open_file_dialog() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Open a source file (CSV / TSV / Parquet)")
        .add_filter("Delimited text (CSV/TSV)", &["csv", "tsv", "txt"])
        .add_filter("Parquet", &["parquet"])
        .add_filter("All files", &["*"])
        .pick_file()
}

/// Native "save file" dialog, pre-filled with a sensible Parquet name.
fn save_parquet_dialog(default_name: &str) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Stage to a Parquet file")
        .set_file_name(default_name)
        .add_filter("Parquet files", &["parquet"])
        .save_file()
}

/// The root component. Owns all UI state as Dioxus signals.
///
/// State kept here (one place, visible at a glance):
/// * `status`      — last human-readable message or error;
/// * `path_input`  — text the user typed/picked into the source field;
/// * `preview`     — loaded table (columns + rows) or `None` before any load;
/// * `report`      — result of the last stage run or `None`;
/// * `source_path` — canonical path of the currently previewed file.
#[component]
fn App() -> Element {
    let mut status = use_signal(String::new);
    let mut path_input = use_signal(String::new);
    let mut preview = use_signal(|| Option::<Preview>::None);
    let mut report = use_signal(|| Option::<ImportReport>::None);
    let mut source_path = use_signal(|| Option::<PathBuf>::None);

    // Import a source into the preview. Shared by Browse→Open, the sample
    // button and the typed path — one code path means one behaviour.
    let mut import_from = move |path: PathBuf| {
        match preview_source(&path, PREVIEW_MAX_ROWS) {
            Ok(table) => {
                let shown = table.rows.len();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                // Provenance line: format, delimiter, encoding — the user can
                // verify the raw layer read the file the way they expect.
                let provenance = table.source.summary();
                let message = if shown == 0 {
                    format!("{name}: {provenance} — file has no data rows")
                } else {
                    format!("{name}: {provenance} — showing {shown} row(s)")
                };
                *preview.write() = Some(table);
                *source_path.write() = Some(path);
                *report.write() = None;
                status.set(message);
            }
            Err(err) => {
                status.set(format!("cannot import {}: {err}", path.display()));
            }
        }
    };

    // Stage the currently previewed source to Parquet (raw layer, no rules).
    let mut stage_to_parquet = move |parquet_path: PathBuf| {
        let Some(source) = source_path.read().clone() else {
            status.set(String::from("import a source first"));
            return;
        };
        match source_to_parquet(&source, &parquet_path) {
            Ok(report_data) => {
                status.set(format!(
                    "staged {} rows x {} columns → {} [{}]",
                    report_data.rows,
                    report_data.columns,
                    report_data.parquet_path,
                    report_data.source.summary(),
                ));
                *report.write() = Some(report_data);
            }
            Err(err) => status.set(format!("staging failed: {err}")),
        }
    };

    rsx! {
        document::Style { "{CSS}" }
        div { class: "app",
            header { class: "topbar",
                h1 { "Strata — Data Engineering Workbench" }
                p { class: "subtitle", "Raw layer (staging): File → Parquet · no business rules applied" }
            }

            // ---- Source widget: one place to name the input file -----------
            div { class: "toolbar sourcebar",
                input {
                    class: "path-input",
                    placeholder: "Path to CSV / TSV / Parquet file…",
                    value: path_input,
                    oninput: move |evt: Event<FormData>| path_input.set(evt.value()),
                }
                button {
                    onclick: move |_| {
                        if let Some(path) = open_file_dialog() {
                            path_input.set(path.display().to_string());
                        }
                    },
                    "Browse…"
                }
                button {
                    onclick: move |_| {
                        let text = path_input.read().trim().to_string();
                        if text.is_empty() {
                            status.set(String::from("type a path or use Browse…"));
                        } else {
                            import_from(PathBuf::from(text));
                        }
                    },
                    "Open"
                }
            }

            div { class: "toolbar hints",
                span { class: "hint", "…or open the bundled demo:" }
                button {
                    onclick: move |_| {
                        match sample_csv_path() {
                            Ok(path) => {
                                path_input.set(path.display().to_string());
                                import_from(path);
                            }
                            Err(err) => status.set(err),
                        }
                    },
                    "Load sample"
                }
            }

            if let Some(table) = preview.read().as_ref() {
                div { class: "panel",
                    h2 { class: "panel-title", "Preview — {table.source.summary()}" }
                    table { class: "grid",
                        thead {
                            tr {
                                for column in &table.columns {
                                    th { "{column.name}" }
                                }
                            }
                        }
                        tbody {
                            for row in &table.rows {
                                tr {
                                    for cell in row {
                                        td { "{cell}" }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "toolbar",
                    button {
                        onclick: move |_| {
                            // Default name: source stem + .parquet in the same folder.
                            let source = source_path.read().clone().unwrap_or_default();
                            let default_name = source
                                .file_stem()
                                .map(|s| format!("{}.parquet", s.to_string_lossy()))
                                .unwrap_or_else(|| "staged.parquet".to_string());
                            if let Some(path) = save_parquet_dialog(&default_name) {
                                stage_to_parquet(path);
                            }
                        },
                        "Stage to Parquet"
                    }
                }
            }

            if let Some(report) = report.read().as_ref() {
                div { class: "report",
                    strong { "Import report" }
                    p { "Rows: {report.rows} · Columns: {report.columns} · File: {report.parquet_path}" }
                }
            }

            div { class: "status", "{status}" }
        }
    }
}

/// A small stylesheet. Inline on purpose: M0 ships a single window with no
/// asset pipeline yet. Dark theme, monospace data grid.
const CSS: &str = r#"
    :root {
        --bg: #101418; --panel: #161c22; --line: #2a333d;
        --text: #d7dee6; --muted: #8b98a5; --accent: #4da3ff;
    }
    * { box-sizing: border-box; }
    body { margin: 0; background: var(--bg); color: var(--text);
           font-family: system-ui, sans-serif; }
    .app { display: flex; flex-direction: column; gap: 8px;
           padding: 14px; height: 100vh; }
    .topbar h1 { margin: 0; font-size: 18px; }
    .topbar .subtitle { margin: 2px 0 0; color: var(--muted); font-size: 12px; }
    .toolbar { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
    .sourcebar { border: 1px solid var(--line); border-radius: 8px;
                 padding: 8px; background: var(--panel); }
    .path-input { flex: 1; min-width: 260px; background: var(--bg);
                  border: 1px solid var(--line); border-radius: 6px;
                  color: var(--text); padding: 6px 8px; font-family: monospace; }
    .hints .hint { color: var(--muted); font-size: 12px; }
    button { background: var(--bg); border: 1px solid var(--line);
             border-radius: 6px; color: var(--text); padding: 6px 12px;
             cursor: pointer; }
    button:hover { border-color: var(--accent); }
    .panel { background: var(--panel); border: 1px solid var(--line);
             border-radius: 8px; padding: 10px; overflow: auto; flex: 1; }
    .panel-title { margin: 0 0 8px; font-size: 13px; color: var(--muted);
                font-weight: 600; font-family: monospace; }
    table.grid { border-collapse: collapse; font-family: monospace; font-size: 13px;
                 width: 100%; }
    table.grid th, table.grid td { border: 1px solid var(--line);
                 padding: 3px 8px; text-align: left; white-space: nowrap; }
    table.grid th { position: sticky; top: 0; background: var(--panel);
                 color: var(--muted); font-weight: 600; }
    table.grid tbody tr:nth-child(even) { background: #1a2129; }
    .report { background: var(--panel); border: 1px solid var(--line);
              border-radius: 8px; padding: 8px 12px; font-size: 13px; }
    .report p { margin: 2px 0 0; font-family: monospace; }
    .status { color: var(--muted); font-size: 13px; min-height: 1em;
              font-family: monospace; white-space: pre-wrap; }
"#;
