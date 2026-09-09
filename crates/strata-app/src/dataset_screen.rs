//! # Datasets screen (Dioxus 0.7): browse a staged dataset directory
//!
//! A "dataset" in strata is simply a directory of Parquet parts — flat
//! (`0000-sales.parquet`, …) or Hive-partitioned (`city=Moscow/…`), possibly
//! both nested. This screen opens such a directory and shows:
//!
//! * the list of part files (relative path + size, any depth);
//! * the combined preview across parts (columns of the first part);
//! * totals (files / bytes).
//!
//! All heavy lifting is in `strata_core` ([`list_parts`], [`preview_parts`]);
//! this component only renders results. It owns all of its state locally —
//! see the Dioxus state notes in `sources.rs`.

use dioxus::prelude::*;
use std::path::PathBuf;
use strata_core::{DatasetPart, Preview, list_parts, preview_parts};

/// How many rows the combined preview shows.
const PREVIEW_MAX_ROWS: usize = 100;

/// The whole Datasets screen.
#[component]
pub fn DatasetScreen() -> Element {
    let mut status = use_signal(String::new);
    let mut path_input = use_signal(String::new);
    let mut parts = use_signal(|| Option::<Vec<DatasetPart>>::None);
    let mut preview = use_signal(|| Option::<Preview>::None);

    // Open a dataset directory: list parts and build the combined preview.
    let mut open_dataset = move |dir: PathBuf| {
        match list_parts(&dir) {
            Ok(part_list) => {
                let total_bytes: u64 = part_list.iter().map(|p| p.size_bytes).sum();
                let part_count = part_list.len();
                *parts.write() = Some(part_list);
                *preview.write() = None; // cleared unless preview succeeds below
                match preview_parts(&dir, PREVIEW_MAX_ROWS) {
                    Ok(table) => {
                        *preview.write() = Some(table);
                        status.set(format!(
                            "dataset: {part_count} part file(s), {} total — combined preview below",
                            format_bytes(total_bytes)
                        ));
                    }
                    Err(err) => {
                        status.set(format!(
                            "dataset: {part_count} part(s), {} — preview failed: {err}",
                            format_bytes(total_bytes)
                        ));
                    }
                }
            }
            Err(err) => {
                *parts.write() = None;
                *preview.write() = None;
                status.set(format!("cannot open dataset {}: {err}", dir.display()));
            }
        }
    };

    rsx! {
        div { class: "screen",
            h1 { class: "screen-title", "Datasets" }
            p { class: "screen-sub",
                "Open a dataset directory (the output of folder staging). Shows "
                "its Parquet part files — flat or Hive-partitioned — and a "
                "combined preview."
            }

            div { class: "card",
                div { class: "card-body",
                    div { class: "toolbar",
                        input {
                            class: "path-input",
                            placeholder: "Path to a dataset directory (…/sales_dataset)…",
                            value: path_input,
                            oninput: move |evt: Event<FormData>| path_input.set(evt.value()),
                        }
                        button {
                            onclick: move |_| {
                                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                    path_input.set(dir.display().to_string());
                                    open_dataset(dir);
                                }
                            },
                            "Browse…"
                        }
                        button {
                            onclick: move |_| {
                                let text = path_input.read().trim().to_string();
                                if text.is_empty() {
                                    status.set(String::from("type or pick a dataset folder"));
                                } else {
                                    open_dataset(PathBuf::from(text));
                                }
                            },
                            "Open dataset"
                        }
                    }
                }
            }

            if let Some(parts) = parts.read().as_ref() {
                div { class: "card",
                    div { class: "card-head",
                        span { class: "card-title", "Parts" }
                        span { class: "card-badge", "{parts.len()} file(s)" }
                    }
                    div { class: "card-body",
                        div { class: "table-wrap",
                            table { class: "grid",
                                thead { tr { th { "Part (relative)" } th { "Size" } } }
                                tbody {
                                    for part in parts {
                                        tr {
                                            td { "{part.rel_path}" }
                                            td { "{format_bytes(part.size_bytes)}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if let Some(table) = preview.read().as_ref() {
                div { class: "card",
                    div { class: "card-head",
                        span { class: "card-title", "Combined preview" }
                        span { class: "card-badge", "{table.columns.len()} col(s) · {table.rows.len()} row(s)" }
                    }
                    div { class: "card-body",
                        crate::preview::PreviewTable { table: table.clone() }
                    }
                }
            }

            div { class: "status", "{status}" }
        }
    }
}

/// Human-readable byte size (`12.4 GB`, `880 KB`, …). Plain Rust, shared look
/// with the Sources screen.
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
