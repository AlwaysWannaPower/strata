//! # Schemas screen (Dioxus 0.7): infer the schema from a file or a folder
//!
//! The product story from `PLAN.md` M1b / `ТЗ.md` §4: "the user either writes
//! a schema by hand **or** the program proposes one by reading the tabular
//! files". This screen implements the *proposal* half:
//!
//! * point at a **file** → see its columns and inferred Polars types;
//! * point at a **folder** → see the shared column list plus **conflicts**
//!   (same column, different types in different files — `ТЗ.md` §7) and
//!   columns missing from some files.
//!
//! Reader options (encoding / delimiter / "first row is header") apply here
//! exactly like in the Sources screen — schema inference and preview must
//! read the file the same way, or the two screens would disagree.
//!
//! Hand-written schemas and "confirm & stage with these types" build on this
//! module in the next step.

use dioxus::prelude::*;
use std::path::PathBuf;
use strata_core::{FolderSchema, SchemaProposal, schema_from_file, schema_from_folder};

use crate::sources::reader_options_from;

/// The whole Schemas screen: one source row + results.
#[component]
pub fn SchemaScreen() -> Element {
    // --- State (owned here; see the Dioxus notes in sources.rs) -----------
    let mut status = use_signal(String::new);
    let mut path_input = use_signal(String::new);
    let mut encoding_choice = use_signal(|| String::from("auto"));
    let mut delimiter_choice = use_signal(|| String::from("auto"));
    let mut header_choice = use_signal(|| true);
    let mut file_schema = use_signal(|| Option::<SchemaProposal>::None);
    let mut folder_schema = use_signal(|| Option::<FolderSchema>::None);

    // --- Behaviour ---------------------------------------------------------
    // Infer from whatever the path points at: a directory → folder-wide
    // schema + conflicts; a file → single-file proposal.
    let mut infer = move || {
        let text = path_input.read().trim().to_string();
        if text.is_empty() {
            status.set(String::from("type or pick a path first"));
            return;
        }
        let path = PathBuf::from(text);
        let options = reader_options_from(
            &encoding_choice.read(),
            &delimiter_choice.read(),
            *header_choice.read(),
        );

        let is_dir = std::fs::metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if is_dir {
            match schema_from_folder(&path, options) {
                Ok(report) => {
                    let conflict_note = if report.conflicts.is_empty() {
                        "no conflicts".to_string()
                    } else {
                        format!("{} conflict(s)", report.conflicts.len())
                    };
                    *file_schema.write() = None;
                    *folder_schema.write() = Some(report);
                    status.set(format!(
                        "folder: {} file(s) inspected → {} column(s), {conflict_note}",
                        folder_schema
                            .read()
                            .as_ref()
                            .map(|r| r.files_inspected)
                            .unwrap_or(0),
                        folder_schema
                            .read()
                            .as_ref()
                            .map(|r| r.columns.len())
                            .unwrap_or(0),
                    ));
                }
                Err(err) => status.set(format!("folder schema failed: {err}")),
            }
        } else {
            match schema_from_file(&path, options) {
                Ok(proposal) => {
                    *folder_schema.write() = None;
                    *file_schema.write() = Some(proposal);
                    status.set(format!(
                        "file: {} column(s) inferred",
                        file_schema
                            .read()
                            .as_ref()
                            .map(|p| p.columns.len())
                            .unwrap_or(0)
                    ));
                }
                Err(err) => status.set(format!("schema failed: {err}")),
            }
        }
    };

    rsx! {
        div { class: "screen",
            h1 { class: "screen-title", "Schemas" }
            p { class: "screen-sub",
                "Point at a file or a folder — the engine proposes columns and types. "
                "Conflicts (same column, different types) and missing columns are listed. "
                "Reader options apply here exactly as in Sources."
            }

            div { class: "card",
                div { class: "card-head",
                    span { class: "card-title", "Infer schema" }
                }
                div { class: "card-body",
                    div { class: "toolbar",
                        input {
                            class: "path-input",
                            placeholder: "Path to a CSV/TSV/Parquet file or a folder…",
                            value: path_input,
                            oninput: move |evt: Event<FormData>| path_input.set(evt.value()),
                        }
                        button {
                            onclick: move |_| {
                                if let Some(path) = pick_any() {
                                    path_input.set(path.display().to_string());
                                }
                            },
                            "Browse…"
                        }
                        button { onclick: move |_| infer(), "Infer schema" }
                    }

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
                    }
                }
            }

            // Single-file proposal: a plain column table.
            if let Some(proposal) = file_schema.read().as_ref() {
                div { class: "card",
                    div { class: "card-head",
                        span { class: "card-title", "Proposed schema (file)" }
                    }
                    div { class: "card-body",
                        SchemaColumnsTable { columns: proposal.columns.clone() }
                    }
                }
            }

            // Folder result: columns + conflicts + missing + failed files.
            if let Some(report) = folder_schema.read().as_ref() {
                div { class: "card",
                    div { class: "card-head",
                        span { class: "card-title", "Proposed schema (folder)" }
                        span { class: "card-badge", "{report.files_inspected} file(s)" }
                    }
                    div { class: "card-body",
                        SchemaColumnsTable { columns: report.columns.clone() }

                        if !report.conflicts.is_empty() {
                            h3 { class: "schema-h", "⚠ Schema conflicts" }
                            for conflict in &report.conflicts {
                                div { class: "conflict",
                                    span { class: "conflict-line",
                                        "{conflict.column}: expected {conflict.expected} but found"
                                    }
                                    ul {
                                        for (file, dtype) in &conflict.found {
                                            li { "{file} → {dtype}" }
                                        }
                                    }
                                }
                            }
                        }

                        if !report.missing.is_empty() {
                            h3 { class: "schema-h", "Missing columns" }
                            ul {
                                for (column, files) in &report.missing {
                                    li { "{column} missing in: {files:?}" }
                                }
                            }
                        }

                        if !report.failed.is_empty() {
                            h3 { class: "schema-h", "Unreadable files" }
                            ul {
                                for (file, err) in &report.failed {
                                    li { "{file}: {err}" }
                                }
                            }
                        }
                    }
                }
            }

            div { class: "status", "{status}" }
        }
    }
}

/// Props of [`SchemaColumnsTable`]: a plain list of name+type pairs.
#[derive(Props, Clone, PartialEq)]
struct SchemaColumnsTableProps {
    columns: Vec<strata_core::SchemaColumn>,
}

/// Render columns as a small table (name | type). A pure presentational
/// component — no state, shows whatever it is given.
#[component]
fn SchemaColumnsTable(props: SchemaColumnsTableProps) -> Element {
    rsx! {
        div { class: "table-wrap",
            table { class: "grid",
                thead { tr { th { "Column" } th { "Type" } } }
                tbody {
                    for column in &props.columns {
                        tr {
                            td { "{column.name}" }
                            td { class: "mono", "{column.dtype}" }
                        }
                    }
                }
            }
        }
    }
}

/// Native dialog for a file or a folder.
fn pick_any() -> Option<PathBuf> {
    // A single dialog can't pick both; we let the user browse for a file and
    // fall back to folder selection if they cancel — simplest robust choice
    // for now (folder picker returns an existing directory).
    rfd::FileDialog::new()
        .set_title("Pick a file (or cancel to pick a folder)")
        .add_filter("Data files", &["csv", "tsv", "txt", "parquet"])
        .add_filter("All files", &["*"])
        .pick_file()
        .or_else(|| {
            rfd::FileDialog::new()
                .set_title("Pick a folder")
                .pick_folder()
        })
}
