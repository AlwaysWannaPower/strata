//! # Entity detail (M1c): Sources / Schema / Stage for one bound entity
//!
//! The pipeline hub shows every bound entity as a card with stage chips; a
//! card's **Open details** mounts this panel for that entity. The three tabs
//! are the old "aspects" (Sources screen, Schema screen, Datasets screen)
//! scoped to one entity and reading everything from disk:
//!
//! * **Sources** — the bound folder's files (metadata only);
//! * **Schema** — the saved `schemas/<entity>.toml` (columns + reader options);
//! * **Stage** — the dataset in `data/<entity>/`: part files, combined preview
//!   and the (re)stage action.
//!
//! ## Freshness
//!
//! The panel never caches disk state: it re-reads whenever the workspace
//! signal or the global `refresh` tick changes, so staging from anywhere in
//! the UI is reflected here automatically. Mounting a fresh instance per
//! entity (`key` on the component) resets the tab selection.

use dioxus::prelude::*;
use std::path::PathBuf;
use strata_core::{
    DatasetPart, FolderScan, Preview, SchemaFile, list_parts, load_schema, preview_parts,
    scan_folder,
};

use crate::actions;
use crate::preview::PreviewTable;
use crate::util::format_bytes;
use crate::workspace_state::WsCtx;

/// How many rows the combined dataset preview shows (bounded on purpose).
const PREVIEW_MAX_ROWS: usize = 60;

/// The three tabs of an entity detail panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailTab {
    Sources,
    Schema,
    Stage,
}

/// Disk facts about one entity, loaded by an effect.
#[derive(Clone)]
struct DetailData {
    /// Absolute bound source folder.
    folder: PathBuf,
    /// Files of the source folder (`None` = folder unreadable).
    files: Option<FolderScan>,
    /// Saved schema, if the schema file exists and parses.
    schema: Option<SchemaFile>,
    /// Parquet parts already staged into `data/<entity>/`.
    parts: Vec<DatasetPart>,
}

/// Props of [`EntityDetail`]: shared signals + which entity to show.
#[derive(Props, Clone, PartialEq)]
pub struct EntityDetailProps {
    /// The open workspace (owned by `App`).
    pub ws: Signal<Option<WsCtx>>,
    /// Global status line (bottom bar) — for action feedback.
    pub status: Signal<String>,
    /// Bumped after any disk/config change so this panel re-reads.
    pub refresh: Signal<u64>,
    /// The hub-owned selection signal. Reading it inside the panel is what
    /// makes the panel follow a selection change (no `key` remount needed).
    pub selected: Signal<Option<String>>,
}

/// The whole entity detail panel (see module docs).
#[component]
pub fn EntityDetail(props: EntityDetailProps) -> Element {
    let ws = props.ws;
    let status = props.status;
    let refresh = props.refresh;
    let selected = props.selected;

    let mut tab = use_signal(|| DetailTab::Sources);
    let mut data = use_signal(|| Option::<DetailData>::None);
    let preview = use_signal(|| Option::<Preview>::None);

    // Load disk facts whenever the workspace, the refresh tick or the
    // *selected entity* changes. The current entity is re-read inside the
    // effect, so switching selection updates this panel without a remount.
    use_effect(move || {
        let _tick = *refresh.read();
        let Some(entity) = selected.read().clone() else {
            data.set(None);
            return;
        };
        let Some(current) = ws.read().clone() else {
            data.set(None);
            return;
        };
        let Some(binding) = current
            .config
            .bindings
            .iter()
            .find(|b| b.entity == entity)
            .cloned()
        else {
            data.set(None);
            return;
        };
        let folder = PathBuf::from(&binding.folder);
        let files = scan_folder(&folder).ok();
        let schema = current
            .entity_schema_file(&entity)
            .and_then(|file| load_schema(&current.dir, &file).ok());
        let dataset_dir = current.data_dir().join(&entity);
        let parts = list_parts(&dataset_dir).unwrap_or_default();
        let has_parts = !parts.is_empty();
        data.set(Some(DetailData {
            folder,
            files,
            schema,
            parts,
        }));
        // When the user is looking at the dataset and a stage finished, keep
        // the preview fresh too.
        if *tab.read() == DetailTab::Stage && has_parts {
            load_preview_into(dataset_dir, preview);
        }
    });

    let Some(entity) = selected.read().clone() else {
        return rsx! { p { class: "empty-note", "Nothing selected." } };
    };

    let Some(current_data) = data.read().clone() else {
        let name = entity.clone();
        return rsx! { p { class: "empty-note", "Nothing to show for '{name}'." } };
    };

    let parts_owned = current_data.parts.clone();
    let folder_display = current_data.folder.display().to_string();

    // Handlers need one owned clone of the entity name each (handlers are
    // 'static; a String cannot be shared between several move closures).
    let stage_name = entity.clone();
    let tabs: [(DetailTab, &'static str, String); 3] = [
        (DetailTab::Sources, "Sources", entity.clone()),
        (DetailTab::Schema, "Schema", entity.clone()),
        (DetailTab::Stage, "Stage / Dataset", entity.clone()),
    ];
    let refresh_name = entity.clone();

    rsx! {
        div { class: "card detail-card",
            div { class: "card-head",
                span { class: "card-title", "Entity: {entity}" }
                span { class: "card-badge", "{parts_owned.len()} part(s)" }
                button {
                    class: "mini-btn",
                    onclick: move |_| {
                        actions::stage_entity(ws, status, refresh, stage_name.clone());
                    },
                    "Stage ▶"
                }
            }

            // ---- mini tab bar: the old screens, scoped to this entity ------
            div { class: "detail-tabs",
                for (value, label, tab_entity) in tabs {
                    button {
                        class: if *tab.read() == value {
                            "tab-btn active"
                        } else {
                            "tab-btn"
                        },
                        onclick: move |_| {
                            tab.set(value);
                            // Entering the Stage tab reloads the preview, in
                            // case a stage finished while we were elsewhere.
                            if value == DetailTab::Stage
                                && let Some(current) = ws.read().clone()
                            {
                                load_preview_into(
                                    current.data_dir().join(&tab_entity),
                                    preview,
                                );
                            }
                        },
                        "{label}"
                    }
                }
            }

            div { class: "card-body detail-body",
                match *tab.read() {
                    DetailTab::Sources => rsx! {
                        div { class: "toolbar",
                            span { class: "hint mono", "{folder_display}" }
                        }
                        match current_data.files.as_ref() {
                            Some(scan) => rsx! {
                                div { class: "table-wrap",
                                    table { class: "grid files",
                                        thead { tr { th { "File" } th { "Kind" } th { "Size" } } }
                                        tbody {
                                            for file in &scan.files {
                                                tr {
                                                    td { "{file.name}" }
                                                    td { "{file.kind}" }
                                                    td { "{format_bytes(file.size_bytes)}" }
                                                }
                                            }
                                        }
                                    }
                                }
                                p { class: "hint",
                                    "{scan.files.len()} file(s), {format_bytes(scan.total_size_bytes)} total"
                                }
                            },
                            None => rsx! { p { class: "empty-note",
                                "Source folder is unreadable — check the binding path."
                            } },
                        }
                    },
                    DetailTab::Schema => rsx! {
                        match current_data.schema.as_ref() {
                            Some(schema) => rsx! {
                                div { class: "toolbar opts",
                                    span { class: "hint", "Reader options:" }
                                    span { class: "opt",
                                        "encoding "
                                        strong { class: "mono", "{schema.encoding}" }
                                    }
                                    span { class: "opt",
                                        "delimiter "
                                        strong { class: "mono", "{schema.delimiter}" }
                                    }
                                    span { class: "opt",
                                        "header "
                                        strong { class: "mono",
                                            if schema.has_header { "yes" } else { "no" }
                                        }
                                    }
                                    span { class: "hint", "saved {schema.saved_utc}" }
                                }
                                crate::preview::ColumnsTable {
                                    rows: schema
                                        .columns
                                        .iter()
                                        .map(|c| (c.name.clone(), c.dtype.clone()))
                                        .collect(),
                                }
                            },
                            None => rsx! { p { class: "empty-note",
                                "No schema file for '{entity}' in schemas/. Confirm a \
                                 candidate from the pipeline to create one."
                            } },
                        }
                    },
                    DetailTab::Stage => rsx! {
                        if parts_owned.is_empty() {
                            p { class: "empty-note",
                                "Nothing staged yet. Press “Stage ▶” to run the folder \
                                 through its confirmed schema into data/{entity}/."
                            }
                        } else {
                            div { class: "table-wrap",
                                table { class: "grid files",
                                    thead { tr { th { "Part (relative)" } th { "Size" } } }
                                    tbody {
                                        for part in &parts_owned {
                                            tr {
                                                td { "{part.rel_path}" }
                                                td { "{format_bytes(part.size_bytes)}" }
                                            }
                                        }
                                    }
                                }
                            }
                            div { class: "toolbar",
                                button {
                                    onclick: move |_| {
                                        if let Some(current) = ws.read().clone() {
                                            load_preview_into(
                                                current.data_dir().join(&refresh_name),
                                                preview,
                                            );
                                        }
                                    },
                                    "Refresh preview"
                                }
                            }
                            match preview.read().as_ref() {
                                Some(table) => rsx! {
                                    PreviewTable { table: table.clone() }
                                },
                                None => rsx! { p { class: "empty-note",
                                    "No preview loaded (click “Refresh preview”)."
                                } },
                            }
                        }
                    },
                }
            }
        }
    }
}

/// Load a combined dataset preview into a signal (blocking, bounded rows).
fn load_preview_into(dataset_dir: PathBuf, mut preview: Signal<Option<Preview>>) {
    match preview_parts(&dataset_dir, PREVIEW_MAX_ROWS) {
        Ok(table) => preview.set(Some(table)),
        Err(_) => preview.set(None),
    }
}
