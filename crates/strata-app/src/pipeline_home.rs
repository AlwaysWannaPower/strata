//! # Pipeline home (Dioxus 0.7, M1c): workspace + entity pipeline cards
//!
//! The "one pipeline" idea from `docs/design-workspace-pipeline.md`: instead
//! of disconnected tabs, this screen is the home. It owns the **workspace**
//! (create/open), its **bindings** (confirmed folder→entity, rule "one folder
//! = one schema") and the **mode-B scan roots** (a root whose direct
//! subfolders are candidate entities).
//!
//! Schema flow (user decision): the engine only ever *proposes* schemas —
//! nothing is written to `schemas/` until the user clicks **Inspect & confirm**.
//!
//! ## Dioxus lesson encoded here
//!
//! Event-handler closures need `'static` data: they may fire long after the
//! render finished. So anything a handler touches is **cloned into owned
//! values before `rsx!`** (snapshots of the state), and loop handlers capture
//! owned clones, never references into signal guards. Reading a `Signal` with
//! `.read()` only inside `rsx!` and borrowing it across a `move` closure is
//! the exact borrow error this file avoids.

use dioxus::prelude::*;
use std::path::PathBuf;
use strata_core::{
    ColumnDef, ReaderOptions, SchemaFile, WorkspaceConfig, candidate_entity_name, create_workspace,
    data_dir, folder_to_parquet, list_entity_candidates, open_workspace, preview_parts,
    save_config, save_schema, schema_from_folder, upsert_binding,
};

/// Max rows for combined dataset previews.
const PREVIEW_MAX_ROWS: usize = 50;

/// The workspace state this screen owns: its directory + current config.
#[derive(Clone, PartialEq)]
struct Ws {
    dir: PathBuf,
    config: WorkspaceConfig,
}

/// What a scan candidate looks like after inspection (in-memory only).
#[derive(Clone, PartialEq)]
struct Candidate {
    /// Absolute candidate folder path.
    folder: PathBuf,
    /// Suggested entity name (= folder name).
    entity: String,
    /// How many files were readable/inspected.
    files: usize,
    /// Number of columns in the proposed schema.
    columns: usize,
    /// First few conflict summaries (column → expected), for display.
    conflicts: Vec<String>,
}

impl Candidate {
    /// One-line summary for the candidate row.
    fn summary(&self) -> String {
        let mut text = format!("{} file(s), {} column(s)", self.files, self.columns);
        if !self.conflicts.is_empty() {
            text.push_str(&format!(" — conflicts: {}", self.conflicts.join("; ")));
        }
        text
    }
}

#[component]
pub fn PipelineHome() -> Element {
    let ws = use_signal(|| Option::<Ws>::None);
    let mut status = use_signal(String::new);
    let mut name_input = use_signal(String::new);
    let mut root_input = use_signal(String::new);
    let candidates = use_signal(|| Option::<Vec<Candidate>>::None);

    // --- owned snapshots for rendering (see module docs) --------------------
    // Everything handlers need is cloned up front; `rsx!` only reads these
    // owned values, so no signal guard is borrowed into a `move` closure.
    let ws_snapshot = ws.read().clone();
    let ws_name = ws_snapshot
        .as_ref()
        .map(|w| w.config.name.clone())
        .unwrap_or_default();
    let ws_dir = ws_snapshot
        .as_ref()
        .map(|w| w.dir.display().to_string())
        .unwrap_or_default();
    let bindings_owned: Vec<(String, String)> = ws_snapshot
        .as_ref()
        .map(|w| {
            w.config
                .bindings
                .iter()
                .map(|b| (b.entity.clone(), b.folder.clone()))
                .collect()
        })
        .unwrap_or_default();
    let candidates_owned = candidates.read().clone().unwrap_or_default();
    let has_workspace = ws_snapshot.is_some();

    rsx! {
        div { class: "screen",
            h1 { class: "screen-title", "Pipeline" }
            p { class: "screen-sub",
                "One workspace = one pipeline. Bind a folder to an entity "
                "(one folder = one schema); the engine proposes schemas, you confirm."
            }

            // ---- workspace control -------------------------------------
            div { class: "card",
                div { class: "card-body",
                    div { class: "toolbar",
                        input {
                            class: "path-input",
                            placeholder: "Workspace name (optional; folder name if empty)…",
                            value: name_input,
                            oninput: move |evt: Event<FormData>| name_input.set(evt.value()),
                        }
                        button {
                            onclick: move |_| {
                                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                    let name = name_input.read().trim().to_string();
                                    create_workspace_ui(dir, name, ws, status);
                                }
                            },
                            "Create workspace…"
                        }
                        button {
                            onclick: move |_| {
                                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                    open_workspace_ui(dir, ws, status);
                                }
                            },
                            "Open workspace…"
                        }
                    }
                }
            }

            // ---- workspace body: scan root (mode B) + bindings ----------
            if has_workspace {
                div { class: "card",
                    div { class: "card-head",
                        span { class: "card-title", "Workspace: {ws_name}" }
                        span { class: "card-badge", "{ws_dir}" }
                    }
                    div { class: "card-body",
                        div { class: "toolbar",
                            input {
                                class: "path-input",
                                placeholder: "Root folder whose direct subfolders are entities…",
                                value: root_input,
                                oninput: move |evt: Event<FormData>| root_input.set(evt.value()),
                            }
                            button {
                                onclick: move |_| {
                                    let text = root_input.read().trim().to_string();
                                    if text.is_empty() {
                                        status.set(String::from("type or pick a root folder"));
                                    } else {
                                        scan_root_ui(PathBuf::from(text), ws, candidates, status);
                                    }
                                },
                                "Scan root (mode B)"
                            }
                            button {
                                onclick: move |_| {
                                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                        root_input.set(dir.display().to_string());
                                        scan_root_ui(dir, ws, candidates, status);
                                    }
                                },
                                "Browse root…"
                            }
                        }

                        // Confirmed bindings (mode A and confirmed B).
                        if !bindings_owned.is_empty() {
                            h3 { class: "schema-h", "Bound entities" }
                            for (entity, folder) in bindings_owned {
                                div { class: "entity-row",
                                    div { class: "entity-main",
                                        strong { "{entity}" }
                                        span { class: "hint", "  {folder}" }
                                    }
                                    button {
                                        onclick: move |_| {
                                            stage_entity_ui(entity.clone(), PathBuf::from(folder.clone()), ws, status);
                                        },
                                        "Stage ▶"
                                    }
                                }
                            }
                        }

                        // Scan candidates awaiting confirmation.
                        if !candidates_owned.is_empty() {
                            h3 { class: "schema-h", "Candidates (proposals — nothing saved yet)" }
                            for candidate in candidates_owned {
                                div { class: "entity-row",
                                    div { class: "entity-main",
                                        strong { "{candidate.entity}" }
                                        span { class: "hint", "  {candidate.summary()}" }
                                    }
                                    button {
                                        onclick: move |_| {
                                            confirm_candidate_ui(candidate.clone(), ws, status);
                                        },
                                        "Inspect & confirm"
                                    }
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

// ---------------------------------------------------------------------------
// Behaviour as module functions (not closures): an event handler must be
// `'static`, and several handlers may share one behaviour — so the behaviour
// is a plain function taking the (Copy) signals it needs. Handlers stay tiny.
// ---------------------------------------------------------------------------

fn create_workspace_ui(
    dir: PathBuf,
    name: String,
    mut ws: Signal<Option<Ws>>,
    mut status: Signal<String>,
) {
    let final_name = if name.trim().is_empty() {
        "Workspace"
    } else {
        name.trim()
    };
    match create_workspace(&dir, final_name) {
        Ok(config) => {
            *ws.write() = Some(Ws {
                dir: dir.clone(),
                config,
            });
            status.set(format!("workspace created at {}", dir.display()));
        }
        Err(err) => status.set(format!("create failed: {err}")),
    }
}

fn open_workspace_ui(dir: PathBuf, mut ws: Signal<Option<Ws>>, mut status: Signal<String>) {
    match open_workspace(&dir) {
        Ok(Some(config)) => {
            *ws.write() = Some(Ws {
                dir: dir.clone(),
                config,
            });
            status.set(format!("workspace loaded: {}", dir.display()));
        }
        Ok(None) => status.set(format!("{} has no workspace.toml", dir.display())),
        Err(err) => status.set(format!("open failed: {err}")),
    }
}

fn scan_root_ui(
    root: PathBuf,
    mut ws: Signal<Option<Ws>>,
    mut candidates: Signal<Option<Vec<Candidate>>>,
    mut status: Signal<String>,
) {
    let Some(mut current) = ws.read().clone() else {
        status.set(String::from("create/open a workspace first"));
        return;
    };
    let root_str = root.display().to_string();
    if !current.config.scan_roots.contains(&root_str) {
        current.config.scan_roots.push(root_str);
    }
    if let Err(err) = save_config(&current.dir, &current.config) {
        status.set(format!("cannot save workspace: {err}"));
        return;
    }
    *ws.write() = Some(current);

    let mut found = Vec::new();
    match list_entity_candidates(&root) {
        Ok(folders) => {
            for folder in &folders {
                match schema_from_folder(folder, ReaderOptions::default()) {
                    Ok(report) => found.push(Candidate {
                        folder: folder.clone(),
                        entity: candidate_entity_name(folder),
                        files: report.files_inspected,
                        columns: report.columns.len(),
                        conflicts: report
                            .conflicts
                            .iter()
                            .take(3)
                            .map(|c| format!("{}: expected {}", c.column, c.expected))
                            .collect(),
                    }),
                    Err(err) => status.set(format!("cannot inspect {}: {err}", folder.display())),
                }
            }
        }
        Err(err) => status.set(format!("cannot read root: {err}")),
    }
    let count = found.len();
    *candidates.write() = Some(found);
    status.set(format!("scan root: {count} candidate entit(ies)"));
}

fn confirm_candidate_ui(
    candidate: Candidate,
    mut ws: Signal<Option<Ws>>,
    mut status: Signal<String>,
) {
    let Some(mut current) = ws.read().clone() else {
        status.set(String::from("create/open a workspace first"));
        return;
    };
    let report = match schema_from_folder(&candidate.folder, ReaderOptions::default()) {
        Ok(report) => report,
        Err(err) => {
            status.set(format!("inference failed: {err}"));
            return;
        }
    };
    let columns: Vec<ColumnDef> = report
        .columns
        .into_iter()
        .map(|c| ColumnDef {
            name: c.name,
            dtype: c.dtype,
        })
        .collect();
    let schema = SchemaFile::new(candidate.folder.clone(), ReaderOptions::default(), columns);
    match save_schema(&current.dir, &schema) {
        Ok(file) => {
            if let Err(err) = upsert_binding(
                &current.dir,
                &mut current.config,
                candidate.entity.clone(),
                candidate.folder,
            ) {
                status.set(format!("binding failed: {err}"));
                return;
            }
            *ws.write() = Some(current);
            status.set(format!(
                "entity '{}' confirmed — schema saved as {file}",
                candidate.entity
            ));
        }
        Err(err) => status.set(format!("schema save failed: {err}")),
    }
}

fn stage_entity_ui(
    entity: String,
    folder: PathBuf,
    ws: Signal<Option<Ws>>,
    mut status: Signal<String>,
) {
    let Some(current) = ws.read().clone() else {
        return;
    };
    let dest = data_dir(&current.dir, &current.config).join(&entity);
    match folder_to_parquet(&folder, &dest) {
        Ok(report) => {
            let preview_ok = preview_parts(&dest, PREVIEW_MAX_ROWS)
                .map(|p| p.rows.len())
                .unwrap_or(0);
            status.set(format!(
                "staged {entity}: {} file(s) → {} rows, {} skipped (preview {} row(s))",
                report.staged.len(),
                report.total_rows,
                report.skipped.len(),
                preview_ok
            ));
        }
        Err(err) => status.set(format!("stage {entity} failed: {err}")),
    }
}
