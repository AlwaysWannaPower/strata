//! # Sources screen (Dioxus 0.7): single file + folder (dataset) cards
//!
//! This screen is the M1a UI: two "cards" that answer two questions:
//!
//! 1. **Single file card** — preview one CSV/TSV/Parquet and stage it;
//! 2. **Folder card** — scan a folder of files (raw dataset source), show what
//!    it contains, then stage the whole folder into a dataset directory of
//!    `part-*.parquet` files and preview the combined result.
//!
//! ## Dioxus patterns demonstrated here (read the inline comments)
//!
//! * components as annotated functions returning `Element`;
//! * state = `Signal<T>` created with `use_signal(|| initial)` — the argument
//!   is a **closure**, not a value (Dioxus 0.7 API);
//! * reading state: `signal.read()`; writing: `*signal.write() = value` or
//!   `signal.set(value)`; both trigger a re-render of this component;
//! * event handlers (`onclick`, `oninput`) are `move` closures capturing the
//!   (Copy) signals they need;
//! * `rsx!` markup: elements `div { … }`, `{expr}` dynamic values and text,
//!   `for … in …` loops, `if let Some(x) = signal.read().as_ref()` branches;
//! * native dialogs via the `rfd` crate: they return `Option<PathBuf>`.
//!
//! ## State architecture (one card = one owner)
//!
//! Each card owns *its own* signals. That is the "state lives where it is
//! used" pattern: the single-file card and the folder card do not share state
//! today, so lifting their signals up to the screen would only add plumbing.
//! When screens must share data (e.g. a global "Logs" history in M4), we will
//! lift the shared signal(s) to the `App` component and pass them down.

use dioxus::prelude::*;
use std::path::PathBuf;
use strata_core::{
    EncodingChoice, FolderReport, FolderScan, ImportReport, Preview, ReaderOptions,
    folder_to_parquet, preview_parts, preview_source_with, scan_folder, source_to_parquet_with,
};

use crate::preview::{PreviewCard, PreviewTable};

/// Build the engine [`ReaderOptions`] from the two selectors ("auto" = None).
///
/// A tiny plain function (no Dioxus involved) so the mapping between UI values
/// and engine options lives in exactly one place and is easy to unit-test.
pub(crate) fn reader_options_from(
    encoding: &str,
    delimiter: &str,
    has_header: bool,
) -> ReaderOptions {
    let encoding = match encoding {
        "utf8" => Some(EncodingChoice::Utf8),
        "cp1251" => Some(EncodingChoice::Windows1251),
        "cp1252" => Some(EncodingChoice::Windows1252),
        "utf16le" => Some(EncodingChoice::Utf16Le),
        "utf16be" => Some(EncodingChoice::Utf16Be),
        _ => None, // "auto"
    };
    let delimiter = match delimiter {
        "," => Some(','),
        ";" => Some(';'),
        "tab" => Some('\t'),
        "|" => Some('|'),
        _ => None, // "auto"
    };
    ReaderOptions {
        encoding,
        delimiter,
        has_header,
    }
}

/// How many rows each preview shows. Kept deliberately small so a preview
/// never reads a huge file fully — the raw staging reads everything, the
/// *preview* must not.
const PREVIEW_MAX_ROWS: usize = 50;

/// The whole Sources screen: two cards stacked vertically.
#[component]
pub fn SourcesScreen() -> Element {
    rsx! {
        div { class: "screen",
            h1 { class: "screen-title", "Sources" }
            p { class: "screen-sub",
                "Raw layer: carry files over faithfully (encoding, delimiter, "
                "types) — no business rules here. Business rules live in the "
                "ODS step (see PLAN.md)."
            }
            // Each card is its own component with its own state; that is why
            // we can simply place them next to each other.
            SingleFileCard {}
            FolderCard {}
        }
    }
}

/// ---------------------------------------------------------------------------
/// Card 1: single file
/// ---------------------------------------------------------------------------

/// Pick-and-preview one file, then stage it to a single Parquet file.
#[component]
fn SingleFileCard() -> Element {
    // --- State: every value this card shows lives in a Signal -------------
    // `use_signal` takes a *closure* returning the initial value (0.7 API).
    // `Option<T>` expresses "nothing yet" — see `if let Some(...)` below.
    let mut status = use_signal(String::new);
    let mut path_input = use_signal(String::new);
    let mut preview = use_signal(|| Option::<Preview>::None);
    let mut report = use_signal(|| Option::<ImportReport>::None);
    let mut source_path = use_signal(|| Option::<PathBuf>::None);
    // Manual reader overrides. "auto" (default) = engine auto-detection.
    let mut encoding_choice = use_signal(|| String::from("auto"));
    let mut delimiter_choice = use_signal(|| String::from("auto"));
    // Whether the first row of a text file is a header (default yes).
    let mut header_choice = use_signal(|| true);

    // --- Behaviour: import a path into the preview ------------------------
    // Shared by Browse→Open and the demo button. Defined as a closure inside
    // the component so it can capture the signals; `move` because the closure
    // outlives this scope (it is stored in the event handlers).
    let mut import_from = move |path: PathBuf| {
        // Apply the currently selected manual options (read at click time, so
        // choosing a new encoding and pressing Open uses the new value).
        let options = reader_options_from(
            &encoding_choice.read(),
            &delimiter_choice.read(),
            *header_choice.read(),
        );
        let manual = options.encoding.is_some() || options.delimiter.is_some();
        match preview_source_with(&path, PREVIEW_MAX_ROWS, options) {
            Ok(table) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                *preview.write() = Some(table.clone());
                *source_path.write() = Some(path);
                *report.write() = None;
                let note = if manual {
                    " (manual options applied)"
                } else {
                    ""
                };
                status.set(format!(
                    "{name}: {}{} — showing {} row(s)",
                    table.source.summary(),
                    note,
                    table.rows.len()
                ));
            }
            Err(err) => status.set(format!("cannot import {}: {err}", path.display())),
        }
    };

    // Stage the currently previewed source to Parquet (raw layer, no rules).
    let mut stage_to = move |parquet_path: PathBuf| {
        let Some(source) = source_path.read().clone() else {
            status.set(String::from("import a source first"));
            return;
        };
        let options = reader_options_from(
            &encoding_choice.read(),
            &delimiter_choice.read(),
            *header_choice.read(),
        );
        match source_to_parquet_with(&source, &parquet_path, options) {
            Ok(report_data) => {
                status.set(format!(
                    "staged {} rows x {} columns → {} [{}]",
                    report_data.rows,
                    report_data.columns,
                    report_data.parquet_path,
                    report_data.source.summary()
                ));
                *report.write() = Some(report_data);
            }
            Err(err) => status.set(format!("staging failed: {err}")),
        }
    };

    rsx! {
        div { class: "card",
            div { class: "card-head",
                span { class: "card-title", "Single file" }
            }
            div { class: "card-body",
                // The source widget row: one text field is the single place a
                // file gets named; Browse only fills it, Open imports it.
                div { class: "toolbar",
                    input {
                        class: "path-input",
                        placeholder: "Path to CSV / TSV / Parquet file…",
                        value: path_input,
                        oninput: move |evt: Event<FormData>| path_input.set(evt.value()),
                    }
                    button {
                        onclick: move |_| {
                            if let Some(path) = pick_data_file() {
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
                // Advanced (optional): manual reader overrides for when
                // auto-detection guesses wrong. Each <select> writes its value
                // into a Signal; import_from/stage_to read them at click time.
                div { class: "toolbar opts",
                    span { class: "hint", "Reader options:" }
                    label { class: "opt",
                        "Encoding "
                        select {
                            onchange: move |evt: Event<FormData>| encoding_choice.set(evt.value()),
                            option { value: "auto", "Auto" }
                            option { value: "utf8", "UTF-8" }
                            option { value: "cp1251", "windows-1251" }
                            option { value: "cp1252", "windows-1252" }
                            option { value: "utf16le", "UTF-16 LE" }
                            option { value: "utf16be", "UTF-16 BE" }
                        }
                    }
                    label { class: "opt",
                        "Delimiter "
                        select {
                            onchange: move |evt: Event<FormData>| delimiter_choice.set(evt.value()),
                            option { value: "auto", "Auto" }
                            option { value: ",", "comma (,)" }
                            option { value: ";", "semicolon (;)" }
                            option { value: "tab", "tab" }
                            option { value: "|", "pipe (|)" }
                        }
                    }
                    label { class: "opt checkbox",
                        input {
                            r#type: "checkbox",
                            checked: *header_choice.read(),
                            onchange: move |evt: Event<FormData>| {
                                header_choice.set(evt.checked());
                            },
                        }
                        " first row is header"
                    }
                    span { class: "hint", "Re-run Open to apply." }
                }
                // Demo convenience (dev/test): load the bundled sample.
                div { class: "toolbar hints",
                    span { class: "hint", "…or the bundled demo:" }
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

                // Conditional rendering: only when a preview exists.
                // `signal.read().as_ref()` borrows the Option; the rsx branch
                // pattern-binds the inner Preview so the whole card is re-rendered
                // whenever the signal changes.
                if let Some(table) = preview.read().as_ref() {
                    PreviewCard {
                        title: table.source.summary(),
                        table: table.clone(),
                    }
                }

                // "Stage to Parquet": appears only when there is something to stage.
                if preview.read().is_some() {
                    div { class: "toolbar",
                        button {
                            onclick: move |_| {
                                let source = source_path.read().clone().unwrap_or_default();
                                let default_name = source
                                    .file_stem()
                                    .map(|s| format!("{}.parquet", s.to_string_lossy()))
                                    .unwrap_or_else(|| "staged.parquet".to_string());
                                if let Some(path) = save_parquet_file(&default_name) {
                                    stage_to(path);
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
}

/// ---------------------------------------------------------------------------
/// Card 2: folder (dataset) source
/// ---------------------------------------------------------------------------

/// Scan a folder of raw files, show its contents, stage it to a dataset
/// directory of Parquet parts and preview the combined result.
#[component]
fn FolderCard() -> Element {
    let mut status = use_signal(String::new);
    let mut path_input = use_signal(String::new);
    let mut scan = use_signal(|| Option::<FolderScan>::None);
    let mut report = use_signal(|| Option::<FolderReport>::None);
    let mut dataset_preview = use_signal(|| Option::<Preview>::None);

    // Scan the folder and remember the result for the UI.
    let mut scan_folder_at = move |dir: PathBuf| match scan_folder(&dir) {
        Ok(folder_scan) => {
            *report.write() = None;
            *dataset_preview.write() = None;
            status.set(format!(
                "folder: {} file(s), {} bytes total",
                folder_scan.files.len(),
                format_bytes(folder_scan.total_size_bytes)
            ));
            *scan.write() = Some(folder_scan);
        }
        Err(err) => status.set(format!("cannot read folder {}: {err}", dir.display())),
    };

    // Stage every file of the scanned folder into a dataset directory of parts.
    let mut stage_folder_at = move |dest_dir: PathBuf| {
        if scan.read().is_none() {
            status.set(String::from("scan a folder first"));
            return;
        }
        let src_dir = PathBuf::from(path_input.read().trim());
        match folder_to_parquet(&src_dir, &dest_dir) {
            Ok(folder_report) => {
                let summary = format!(
                    "staged {} file(s) → {} rows, {} skipped",
                    folder_report.staged.len(),
                    folder_report.total_rows,
                    folder_report.skipped.len()
                );
                // Preview the combined dataset we just produced.
                match preview_parts(&dest_dir, PREVIEW_MAX_ROWS) {
                    Ok(table) => {
                        *dataset_preview.write() = Some(table);
                        status.set(format!(
                            "{summary}. Parts: {} skipped file(s) — see report.",
                            folder_report.skipped.len()
                        ));
                    }
                    Err(err) => status.set(format!("{summary}. Preview failed: {err}")),
                }
                *report.write() = Some(folder_report);
            }
            Err(err) => status.set(format!("staging failed: {err}")),
        }
    };

    rsx! {
        div { class: "card",
            div { class: "card-head",
                span { class: "card-title", "Folder (dataset)" }
                span { class: "card-badge", "M1a" }
            }
            div { class: "card-body",
                div { class: "toolbar",
                    input {
                        class: "path-input",
                        placeholder: "Path to a folder of CSV/TSV/Parquet files…",
                        value: path_input,
                        oninput: move |evt: Event<FormData>| path_input.set(evt.value()),
                    }
                    button {
                        onclick: move |_| {
                            if let Some(dir) = pick_folder() {
                                path_input.set(dir.display().to_string());
                                scan_folder_at(dir);
                            }
                        },
                        "Browse folder…"
                    }
                    button {
                        onclick: move |_| {
                            let text = path_input.read().trim().to_string();
                            if text.is_empty() {
                                status.set(String::from("type a folder path first"));
                            } else {
                                scan_folder_at(PathBuf::from(text));
                            }
                        },
                        "Scan"
                    }
                }

                // File listing (from the scan; no data read yet).
                if let Some(folder_scan) = scan.read().as_ref() {
                    div { class: "table-wrap",
                        table { class: "grid files",
                            thead { tr { th { "File" } th { "Kind" } th { "Size" } } }
                            tbody {
                                for file in &folder_scan.files {
                                    tr {
                                        td { "{file.name}" }
                                        td { "{file.kind}" }
                                        td { "{format_bytes(file.size_bytes)}" }
                                    }
                                }
                            }
                        }
                    }
                }

                // Stage to a dataset directory of parts (raw layer).
                if scan.read().is_some() {
                    div { class: "toolbar",
                        button {
                            onclick: move |_| {
                                if let Some(dir) = pick_folder() {
                                    stage_folder_at(dir);
                                }
                            },
                            "Stage folder → Parquet parts…"
                        }
                    }
                }

                if let Some(report) = report.read().as_ref() {
                    div { class: "report",
                        strong { "Folder report" }
                        p { "Staged: {report.staged.len()} · Rows total: {report.total_rows} · Skipped: {report.skipped.len()} · Dataset: {report.dest_dir}" }
                        // Broken files are *kept visible*: the raw layer must
                        // never silently drop data (staging philosophy).
                        if !report.skipped.is_empty() {
                            ul {
                                for (name, err) in &report.skipped {
                                    li { "{name}: {err}" }
                                }
                            }
                        }
                    }
                }

                if let Some(table) = dataset_preview.read().as_ref() {
                    PreviewTable { table: table.clone() }
                }

                div { class: "status", "{status}" }
            }
        }
    }
}

/// ---------------------------------------------------------------------------
/// Small helpers (no Dioxus here — plain Rust functions)
/// ---------------------------------------------------------------------------

/// Human-readable byte size: `12.4 GB`, `880 KB`, …
fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Native open-file dialog with every format the raw layer understands.
fn pick_data_file() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Open a source file (CSV / TSV / Parquet)")
        .add_filter("Delimited text (CSV/TSV)", &["csv", "tsv", "txt"])
        .add_filter("Parquet", &["parquet"])
        .add_filter("All files", &["*"])
        .pick_file()
}

/// Native save-file dialog for a single staged Parquet file.
fn save_parquet_file(default_name: &str) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Stage to a Parquet file")
        .set_file_name(default_name)
        .add_filter("Parquet files", &["parquet"])
        .save_file()
}

/// Native folder picker (used as dataset destination).
fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Choose a folder")
        .pick_folder()
}

/// Path of the bundled demo CSV (`examples/sample_csv/sales_01.csv`).
///
/// `CARGO_MANIFEST_DIR` is the compile-time directory of the *current* crate
/// (`crates/strata-app/`), so the sample is two levels up.
fn sample_csv_path() -> Result<PathBuf, String> {
    let candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/sample_csv")
        .join("sales_01.csv");
    match candidate.try_exists() {
        Ok(true) => Ok(candidate),
        Ok(false) => Err(format!("sample file not found at {}", candidate.display())),
        Err(err) => Err(format!("cannot inspect {}: {err}", candidate.display())),
    }
}
