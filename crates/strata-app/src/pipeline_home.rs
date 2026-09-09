//! # Pipeline home (Dioxus 0.7, M1c): the one hub for everything
//!
//! The whole app is this screen (plus the shell around it): a single pipeline
//! per workspace, where every "aspect" of the product lives **in the context
//! of an entity** instead of disconnected tabs:
//!
//! ```text
//! [Sources] → [Schema] → [Validate (M3)] → [Stage]
//!     │           │            │               │
//!  bound folder  schemas/     future M3    data/<entity>/ parquet
//! ```
//!
//! * bound entities → one **card** each with its stage chips; *Open details*
//!   mounts [`EntityDetail`] (Sources files / saved schema / dataset+preview);
//! * mode B (scan a root, its subfolders are candidate entities) stays here;
//!   candidates are proposals only — nothing is written until **Inspect &
//!   confirm**, which shows the inferred columns/conflicts and the reader
//!   options (encoding / delimiter / header).
//!
//! ## Why the workspace does not disappear anymore
//!
//! `ws` (directory + config) is owned by `App` — the shell — not by this
//! screen, so it survives navigation (and restarts, via `recent.rs`).
//! Everything else here is derived from disk again on every action and after
//! every global `refresh` tick, so the UI can never drift from what is saved.
//!
//! ## Dioxus notes encoded here
//!
//! * Statements (`let`) are not allowed between markup nodes inside `rsx!` —
//!   per-item data is prepared before the markup, and repeatable rows are
//!   their own components ([`EntityCard`], [`CandidateRow`]).
//! * Event-handler closures need `'static` data: they capture **owned clones**
//!   of per-item values and Copy signals, never borrows into signal guards.

use dioxus::prelude::*;
use std::path::{Path, PathBuf};

use strata_core::list_parts;

use crate::actions::{self, Candidate};
use crate::entity_detail::EntityDetail;
use crate::util::{pick_folder, reader_options_from};
use crate::workspace_state::WsCtx;

/// Disk facts about one bound entity, recomputed after each change.
#[derive(Debug, Clone, PartialEq)]
struct EntitySummary {
    entity: String,
    folder: String,
    /// Saved schema file name, if present in `schemas/`.
    schema_file: Option<String>,
    /// How many Parquet parts are already staged into `data/<entity>/`.
    parts_count: usize,
    /// Total bytes of those parts.
    parts_bytes: u64,
}

/// Props of [`PipelineHome`]: the shared signals owned by `App`.
#[derive(Props, Clone, PartialEq)]
pub struct PipelineHomeProps {
    /// The open workspace (never dies with a screen).
    pub ws: Signal<Option<WsCtx>>,
    /// Global status line (bottom bar).
    pub status: Signal<String>,
    /// Bumped after any disk/config change; screens re-read on change.
    pub refresh: Signal<u64>,
}

/// The whole pipeline hub (see module docs).
#[component]
pub fn PipelineHome(props: PipelineHomeProps) -> Element {
    let ws = props.ws;
    let mut status = props.status;
    let refresh = props.refresh;

    // --- hub-local state --------------------------------------------------
    let mut summaries = use_signal(Vec::<EntitySummary>::new);
    let mut candidates = use_signal(|| Option::<Vec<Candidate>>::None);
    let mut selected = use_signal(|| Option::<String>::None);
    let mut inspecting = use_signal(|| Option::<PathBuf>::None);
    let mut root_input = use_signal(String::new);
    let mut name_input = use_signal(String::new);
    // Typed-path fallback: creating/opening must not depend on the native file
    // dialog (it can crash on systems with a broken GTK/glibc setup).
    let mut folder_input = use_signal(String::new);

    // --- owned snapshots for rendering (see module docs) -------------------
    let ws_snapshot = ws.read().clone();
    let has_ws = ws_snapshot.is_some();
    let ws_name = ws_snapshot
        .as_ref()
        .map(|w| w.config.name.clone())
        .unwrap_or_default();
    let ws_dir = ws_snapshot
        .as_ref()
        .map(|w| w.dir.display().to_string())
        .unwrap_or_default();
    let summaries_owned = summaries.read().clone();
    let candidates_owned = candidates.read().clone().unwrap_or_default();
    let selected_owned = selected.read().clone();
    let inspected_path = inspecting.read().clone();
    // Counters for the workspace summary bar (computed once per render).
    let entity_count = summaries_owned.len();
    let schema_count = summaries_owned
        .iter()
        .filter(|s| s.schema_file.is_some())
        .count();
    let staged_parts: usize = summaries_owned.iter().map(|s| s.parts_count).sum();
    // Recent workspaces are only interesting while no workspace is open.
    let recents = if has_ws {
        Vec::new()
    } else {
        crate::recent::list()
    };
    // Precomputed inspector arguments (the rsx branch must stay statement-free).
    let inspector_arg: Option<(PathBuf, String)> = inspected_path
        .as_ref()
        .map(|path| (path.clone(), strata_core::candidate_entity_name(path)));

    // --- effects -----------------------------------------------------------
    // Recomputed entity summaries. Runs on mount, whenever the workspace
    // changes and after every `refresh` tick (e.g. an entity was confirmed or
    // staged elsewhere in the UI). All reads are from disk — the UI is a
    // projection of what is saved.
    use_effect(move || {
        let _tick = *refresh.read();
        let Some(current) = ws.read().clone() else {
            summaries.set(Vec::new());
            candidates.set(None);
            selected.set(None);
            inspecting.set(None);
            return;
        };
        let base = current.data_dir();
        let mut list = Vec::with_capacity(current.config.bindings.len());
        for binding in &current.config.bindings {
            let parts = list_parts(&base.join(&binding.entity)).unwrap_or_default();
            let parts_bytes: u64 = parts.iter().map(|p| p.size_bytes).sum();
            list.push(EntitySummary {
                entity: binding.entity.clone(),
                folder: binding.folder.clone(),
                schema_file: current.entity_schema_file(&binding.entity),
                parts_count: parts.len(),
                parts_bytes,
            });
        }
        list.sort_by(|a, b| a.entity.cmp(&b.entity));
        summaries.set(list);
    });

    rsx! {
        div { class: "screen pipeline",
            h1 { class: "screen-title", "Pipeline" }
            p { class: "screen-sub",
                "One workspace = one pipeline. Every entity is a card: bind a \
                 folder to it, confirm its schema, stage it into data/. All \
                 aspects live here — in the context of the entity."
            }

            if has_ws {
                // ---- workspace summary bar --------------------------------
                div { class: "card",
                    div { class: "card-head",
                        span { class: "card-title", "Workspace: {ws_name}" }
                        span { class: "card-badge", "{ws_dir}" }
                        button {
                            class: "mini-btn",
                            onclick: move |_| actions::close_workspace_ui(ws, status),
                            "Close"
                        }
                    }
                    div { class: "card-body",
                        div { class: "toolbar opts ws-stats",
                            span { class: "opt",
                                "entities "
                                strong { class: "mono", "{entity_count}" }
                            }
                            span { class: "opt",
                                "schemas "
                                strong { class: "mono", "{schema_count}" }
                            }
                            span { class: "opt",
                                "staged parts "
                                strong { class: "mono", "{staged_parts}" }
                            }
                        }
                    }
                }

                // ---- entity cards ------------------------------------------
                if summaries_owned.is_empty() {
                    div { class: "card",
                        div { class: "card-body",
                            p { class: "empty-note",
                                "No bound entities yet. Scan a root folder below — its \
                                 direct subfolders become candidate entities (mode B)."
                            }
                        }
                    }
                } else {
                    div { class: "section-head",
                        h3 { class: "schema-h", "Bound entities" }
                    }
                    div { class: "entity-grid",
                        for summary in &summaries_owned {
                            EntityCard {
                                key: "{summary.entity}",
                                ws: ws,
                                status: status,
                                refresh: refresh,
                                selected: selected,
                                summary: summary.clone(),
                            }
                        }
                    }
                }

                // ---- selected entity detail --------------------------------
                if selected_owned.is_some() {
                    div { class: "toolbar detail-bar",
                        span { class: "hint", "Entity details — click a card to close." }
                        button {
                            class: "mini-btn",
                            onclick: move |_| selected.set(None),
                            "Close details"
                        }
                    }
                    EntityDetail {
                        ws: ws,
                        status: status,
                        refresh: refresh,
                        selected: selected,
                    }
                }

                // ---- add an entity: mode B scan root -----------------------
                div { class: "card",
                    div { class: "card-head",
                        span { class: "card-title", "Add entity — scan a root (mode B)" }
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
                                        scan_root_ui(PathBuf::from(text), ws, status, candidates);
                                    }
                                },
                                "Scan root"
                            }
                            button {
                                onclick: move |_| {
                                    if let Some(dir) = pick_folder("Pick a scan root folder") {
                                        root_input.set(dir.display().to_string());
                                        scan_root_ui(dir, ws, status, candidates);
                                    }
                                },
                                "Browse root…"
                            }
                        }

                        if !candidates_owned.is_empty() {
                            h3 { class: "schema-h", "Candidates (proposals — nothing saved yet)" }
                            for candidate in &candidates_owned {
                                CandidateRow {
                                    key: "{candidate.entity}",
                                    inspecting: inspecting,
                                    candidate: candidate.clone(),
                                }
                            }
                        }

                        if let Some((folder, entity)) = &inspector_arg {
                            CandidateInspector {
                                ws: ws,
                                status: status,
                                refresh: refresh,
                                folder: folder.clone(),
                                entity: entity.clone(),
                                candidates: candidates,
                                inspecting: inspecting,
                            }
                        }
                    }
                }
            } else {
                // ---- welcome: no workspace open -----------------------------
                div { class: "card",
                    div { class: "card-body welcome",
                        h3 { "Open or create a workspace to start the pipeline" }
                        p { class: "hint",
                            "A workspace is a folder with workspace.toml, schemas/ and \
                             data/. The last workspace you used is remembered and \
                             restored automatically on the next launch."
                        }
                        div { class: "toolbar",
                            input {
                                class: "path-input",
                                placeholder: "New workspace name (optional)…",
                                value: name_input,
                                oninput: move |evt: Event<FormData>| name_input.set(evt.value()),
                            }
                            button {
                                onclick: move |_| {
                                    if let Some(dir) = pick_folder("Create a workspace here") {
                                        let name = name_input.read().trim().to_string();
                                        actions::create_workspace_ui(dir, name, ws, status);
                                    }
                                },
                                "Create…"
                            }
                            button {
                                onclick: move |_| {
                                    if let Some(dir) = pick_folder("Open a workspace") {
                                        actions::open_workspace_ui(dir, ws, status);
                                    }
                                },
                                "Open…"
                            }
                        }

                        // Typed-path fallback: no native dialog involved. If
                        // the dialog buttons above crash on your system, use
                        // these: type the folder path and press the action.
                        div { class: "toolbar",
                            input {
                                class: "path-input",
                                placeholder: "Folder path (type it here if the dialog fails)…",
                                value: folder_input,
                                oninput: move |evt: Event<FormData>| folder_input.set(evt.value()),
                            }
                            button {
                                onclick: move |_| {
                                    let text = folder_input.read().trim().to_string();
                                    if text.is_empty() {
                                        status.set(String::from(
                                            "type a folder path or use the dialog buttons",
                                        ));
                                    } else {
                                        let name = name_input.read().trim().to_string();
                                        actions::create_workspace_ui(
                                            PathBuf::from(text),
                                            name,
                                            ws,
                                            status,
                                        );
                                    }
                                },
                                "Create at path"
                            }
                            button {
                                onclick: move |_| {
                                    let text = folder_input.read().trim().to_string();
                                    if text.is_empty() {
                                        status.set(String::from(
                                            "type a folder path or use the dialog buttons",
                                        ));
                                    } else {
                                        actions::open_workspace_ui(PathBuf::from(text), ws, status);
                                    }
                                },
                                "Open at path"
                            }
                        }

                        if !recents.is_empty() {
                            h3 { class: "schema-h", "Recent workspaces" }
                            for path in recents {
                                RecentWorkspaceRow {
                                    key: "{path:?}",
                                    path: path,
                                    ws: ws,
                                    status: status,
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Workspace actions (module-level, shared by several buttons)
// ---------------------------------------------------------------------------

/// Scan `root` (mode B): remember it in the workspace config, then list its
/// direct subfolders as candidate entities (metadata only).
fn scan_root_ui(
    root: PathBuf,
    ws: Signal<Option<WsCtx>>,
    mut status: Signal<String>,
    mut candidates: Signal<Option<Vec<Candidate>>>,
) {
    if ws.read().is_none() {
        status.set(String::from("open a workspace first"));
        return;
    }
    actions::save_scan_root(ws, status, root.clone());
    let Some(current) = ws.read().clone() else {
        return;
    };
    match actions::scan_candidates(&current, &root) {
        Ok(found) => {
            let count = found.len();
            *candidates.write() = if found.is_empty() { None } else { Some(found) };
            status.set(format!(
                "scan: {count} candidate entit(ies) under {}",
                root.display()
            ));
        }
        Err(err) => status.set(format!("cannot read root: {err}")),
    }
}

/// Drop `folder` from the candidate list (confirmed → now bound; cancelled →
/// reappears on the next re-scan).
fn remove_candidate(mut candidates: Signal<Option<Vec<Candidate>>>, folder: &Path) {
    let mut list = candidates.write();
    if let Some(items) = list.as_mut() {
        items.retain(|c| c.folder != folder);
        if items.is_empty() {
            *list = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Recent workspace row (welcome screen)
// ---------------------------------------------------------------------------

/// Props of [`RecentWorkspaceRow`].
#[derive(Props, Clone, PartialEq)]
struct RecentWorkspaceRowProps {
    path: PathBuf,
    ws: Signal<Option<WsCtx>>,
    status: Signal<String>,
}

/// One row in the "Recent workspaces" list: path + an Open button.
#[component]
fn RecentWorkspaceRow(props: RecentWorkspaceRowProps) -> Element {
    let path = props.path.clone();
    let ws = props.ws;
    let status = props.status;
    let path_text = path.display().to_string();
    rsx! {
        div { class: "entity-row",
            div { class: "entity-main",
                span { class: "hint mono", "{path_text}" }
            }
            button {
                onclick: move |_| actions::open_workspace_ui(path.clone(), ws, status),
                "Open"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entity card: name + folder + the four stage chips + actions
// ---------------------------------------------------------------------------

/// Props of [`EntityCard`].
#[derive(Props, Clone, PartialEq)]
struct EntityCardProps {
    /// Shared signals (passed through to actions).
    ws: Signal<Option<WsCtx>>,
    status: Signal<String>,
    refresh: Signal<u64>,
    /// Hub-owned selection signal (which entity detail is open).
    selected: Signal<Option<String>>,
    /// The entity facts this card renders.
    summary: EntitySummary,
}

/// One bound entity as a card with per-stage status. Clicking the card opens
/// or closes its details panel (selection lives in the shared signal).
#[component]
fn EntityCard(props: EntityCardProps) -> Element {
    let ws = props.ws;
    let status = props.status;
    let refresh = props.refresh;
    let selected = props.selected;
    let summary = props.summary.clone();

    let entity = summary.entity.clone();
    let folder = summary.folder.clone();
    let schema_ok = summary.schema_file.is_some();
    let staged = summary.parts_count > 0;
    let is_open = selected.read().as_deref() == Some(entity.as_str());

    // One owned clone per handler (handlers are 'static).
    let toggle_entity = entity.clone();
    let stage_entity_name = entity.clone();
    let stage_label = if staged { "Re-stage ▶" } else { "Stage ▶" };
    // Stage chips as (state, label, note) — owned Strings because conditional
    // expressions cannot feed &'static str props through rsx.
    let chips: [(String, String, String); 4] = [
        (
            String::from("ok"),
            String::from("Sources"),
            String::from("bound"),
        ),
        (
            if schema_ok {
                String::from("ok")
            } else {
                String::from("todo")
            },
            String::from("Schema"),
            if schema_ok {
                String::from("confirmed")
            } else {
                String::from("missing")
            },
        ),
        (
            String::from("off"),
            String::from("Validate"),
            String::from("M3"),
        ),
        (
            if staged {
                String::from("ok")
            } else {
                String::from("todo")
            },
            String::from("Stage"),
            if staged {
                format!("{} part(s)", summary.parts_count)
            } else {
                String::from("not run")
            },
        ),
    ];

    rsx! {
        div {
            class: if is_open { "entity-card selected" } else { "entity-card" },
            onclick: move |_| {
                let mut sel = selected;
                if sel.read().as_deref() == Some(toggle_entity.as_str()) {
                    sel.set(None);
                } else {
                    sel.set(Some(toggle_entity.clone()));
                }
            },
            div { class: "entity-main",
                strong { class: "entity-name", "{entity}" }
                span { class: "hint entity-folder", "{folder}" }
            }

            div { class: "stage-chips",
                for (state, label, note) in chips {
                    Chip { state: state, label: label, note: note }
                }
            }

            div { class: "toolbar entity-actions",
                button {
                    onclick: move |evt| {
                        // A click on "Stage" must not toggle the card.
                        evt.stop_propagation();
                        actions::stage_entity(ws, status, refresh, stage_entity_name.clone());
                    },
                    "{stage_label}"
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stage chip (inside an entity card)
// ---------------------------------------------------------------------------

/// Props of [`Chip`]: a stage marker with a state colour.
#[derive(Props, Clone, PartialEq)]
struct ChipProps {
    /// `ok` (green), `todo` (dim), `off` (grey "not yet").
    state: String,
    /// Stage name shown in the chip.
    label: String,
    /// Short status text.
    note: String,
}

/// A single stage chip inside an entity card.
#[component]
fn Chip(props: ChipProps) -> Element {
    rsx! {
        div { class: format!("chip chip-{}", props.state),
            span { class: "chip-label", "{props.label}" }
            span { class: "chip-note", "{props.note}" }
        }
    }
}

// ---------------------------------------------------------------------------
// Candidate row: one mode-B proposal + an Inspect toggle
// ---------------------------------------------------------------------------

/// Props of [`CandidateRow`].
#[derive(Props, Clone, PartialEq)]
struct CandidateRowProps {
    /// Hub-owned "which candidate is being inspected" signal.
    inspecting: Signal<Option<PathBuf>>,
    /// The candidate to show.
    candidate: Candidate,
}

/// One mode-B candidate row: suggested entity name, file summary and an
/// Inspect toggle (opens the inspector panel below the list).
#[component]
fn CandidateRow(props: CandidateRowProps) -> Element {
    let inspecting = props.inspecting;
    let candidate = props.candidate.clone();

    let entity = candidate.entity.clone();
    let folder = candidate.folder.clone();
    let summary = candidate.summary();
    let is_open = inspecting.read().as_deref() == Some(folder.as_path());
    let label = if is_open {
        "Inspect… (open)"
    } else {
        "Inspect…"
    };

    rsx! {
        div { class: "entity-row",
            div { class: "entity-main",
                strong { "{entity}" }
                span { class: "hint", "  {summary}" }
            }
            button {
                onclick: move |_| {
                    let mut ins = inspecting;
                    if ins.read().as_deref() == Some(folder.as_path()) {
                        ins.set(None);
                    } else {
                        ins.set(Some(folder.clone()));
                    }
                },
                "{label}"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Candidate inspector: show the proposal, choose reader options, confirm
// ---------------------------------------------------------------------------

/// Props of [`CandidateInspector`].
#[derive(Props, Clone, PartialEq)]
struct CandidateInspectorProps {
    ws: Signal<Option<WsCtx>>,
    status: Signal<String>,
    refresh: Signal<u64>,
    /// Candidate folder being inspected.
    folder: PathBuf,
    /// Suggested entity name.
    entity: String,
    /// Hub's candidate list — the confirmed/cancelled candidate is removed
    /// from it here (confirmed = bound now, cancelled = reappears on re-scan).
    candidates: Signal<Option<Vec<Candidate>>>,
    /// Hub's "which candidate is inspected" signal — cleared on close.
    inspecting: Signal<Option<PathBuf>>,
}

/// Inspect one candidate: infer the schema (columns/conflicts), let the user
/// adjust the reader options, then confirm (save schema + bind) or cancel.
#[component]
fn CandidateInspector(props: CandidateInspectorProps) -> Element {
    let ws = props.ws;
    let mut status = props.status;
    let refresh = props.refresh;
    let folder = props.folder.clone();
    let entity = props.entity.clone();
    let candidates = props.candidates;
    let mut inspecting = props.inspecting;

    // Reader options (apply on "Inspect" and on confirm).
    let mut encoding_choice = use_signal(|| String::from("auto"));
    let mut delimiter_choice = use_signal(|| String::from("auto"));
    let mut header_choice = use_signal(|| true);
    // The last inspection result (columns + conflicts) for this folder.
    let mut report = use_signal(|| Option::<strata_core::FolderSchema>::None);
    // Drop a stale inspection report when the user switches to a different
    // candidate (this component is reused across candidates).
    let mut last_folder = use_signal(|| folder.clone());
    if *last_folder.read() != folder {
        *last_folder.write() = folder.clone();
        report.set(None);
    }

    // Handlers get their own owned clone of folder/entity ('static data).
    let inspect_folder = folder.clone();
    let inspect_entity = entity.clone();
    let inspect = move |_| {
        let options = reader_options_from(
            &encoding_choice.read(),
            &delimiter_choice.read(),
            *header_choice.read(),
        );
        match strata_core::schema_from_folder(&inspect_folder, options) {
            Ok(result) => {
                let note = if result.conflicts.is_empty() {
                    "no conflicts".to_string()
                } else {
                    format!("{} conflict(s)", result.conflicts.len())
                };
                *report.write() = Some(result.clone());
                status.set(format!(
                    "inspected {inspect_entity}: {} file(s), {} column(s), {note}",
                    result.files_inspected,
                    result.columns.len()
                ));
            }
            Err(err) => status.set(format!("inspection failed: {err}")),
        }
    };

    // Confirm: persist the schema and bind the folder (reader options as
    // shown), then drop this candidate and close the inspector.
    let confirm_folder = folder.clone();
    let confirm = move |_| {
        let options = reader_options_from(
            &encoding_choice.read(),
            &delimiter_choice.read(),
            *header_choice.read(),
        );
        actions::confirm_and_bind(ws, status, refresh, confirm_folder.clone(), options);
        remove_candidate(candidates, &confirm_folder);
        inspecting.set(None);
    };

    // Cancel: close without saving anything.
    let cancel_folder = folder.clone();
    let cancel = move |_| {
        remove_candidate(candidates, &cancel_folder);
        inspecting.set(None);
    };

    let folder_display = folder.display().to_string();
    let report_owned = report.read().clone();
    let hint_enc = String::from("Auto");
    let hint_delim = String::from("Auto");

    rsx! {
        div { class: "card inspector",
            div { class: "card-head",
                span { class: "card-title", "Inspect: {entity}" }
                span { class: "card-badge", "{folder_display}" }
            }
            div { class: "card-body",
                div { class: "toolbar opts",
                    span { class: "hint", "Reader options:" }
                    label { class: "opt",
                        "Encoding "
                        select {
                            value: encoding_choice,
                            onchange: move |evt: Event<FormData>| encoding_choice.set(evt.value()),
                            option { value: "auto", "{hint_enc}" }
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
                            value: delimiter_choice,
                            onchange: move |evt: Event<FormData>| delimiter_choice.set(evt.value()),
                            option { value: "auto", "{hint_delim}" }
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
                            onchange: move |evt: Event<FormData>| header_choice.set(evt.checked()),
                        }
                        " first row is header"
                    }
                }

                div { class: "toolbar",
                    button { onclick: inspect, "Inspect" }
                    button { onclick: confirm, "Confirm schema & bind" }
                    button { onclick: cancel, "Cancel" }
                }

                match report_owned.as_ref() {
                    None => rsx! { p { class: "empty-note",
                        "Nothing inspected yet — press “Inspect” to infer the \
                         schema (columns, types, conflicts) with these options."
                    } },
                    Some(result) => rsx! {
                        crate::preview::ColumnsTable {
                            rows: result
                                .columns
                                .iter()
                                .map(|c| (c.name.clone(), c.dtype.clone()))
                                .collect(),
                        }

                        if !result.conflicts.is_empty() {
                            h3 { class: "schema-h", "⚠ Schema conflicts" }
                            for conflict in &result.conflicts {
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

                        if !result.missing.is_empty() {
                            h3 { class: "schema-h", "Missing columns" }
                            ul {
                                for (column, files) in &result.missing {
                                    li { "{column} missing in: {files:?}" }
                                }
                            }
                        }

                        if !result.failed.is_empty() {
                            h3 { class: "schema-h", "Unreadable files" }
                            ul {
                                for (file, err) in &result.failed {
                                    li { "{file}: {err}" }
                                }
                            }
                        }
                    },
                }
            }
        }
    }
}
