//! # Pipeline actions (M1c hub): engine operations shared by the UI
//!
//! The pipeline screen is a single hub; several components trigger the same
//! engine operations (scan mode B, confirm & bind, typed stage). Those live
//! here as plain functions over the shared signals, so event handlers stay
//! one-liners and the behaviour is testable without a window.
//!
//! Each function reports its outcome through the global `status` signal and
//! bumps the `refresh` tick when disk state changed (entity cards, the detail
//! panel and the workspace summary all re-read from disk on a tick change).

use std::path::PathBuf;

use dioxus::prelude::*;
use strata_core::{
    ColumnDef, FolderScan, ReaderOptions, SchemaFile, candidate_entity_name,
    list_entity_candidates, load_schema, save_schema, scan_folder, schema_from_folder,
    stage_folder_with_schema, upsert_binding,
};

use crate::workspace_state::{WsCtx, reload as reload_ws};

/// Create a workspace at `dir` and set it as current (name may be empty).
pub fn create_workspace_ui(
    dir: PathBuf,
    name: String,
    mut ws: Signal<Option<WsCtx>>,
    mut status: Signal<String>,
) {
    let final_name = if name.trim().is_empty() {
        dir.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("Workspace"))
    } else {
        name.trim().to_string()
    };
    match crate::workspace_state::create(dir.clone(), &final_name) {
        Ok(ctx) => {
            *ws.write() = Some(ctx);
            status.set(format!("workspace created at {}", dir.display()));
        }
        Err(err) => status.set(format!("create failed: {err}")),
    }
}

/// Open a workspace at `dir`; `Ok(None)`-style feedback is a status line.
pub fn open_workspace_ui(dir: PathBuf, mut ws: Signal<Option<WsCtx>>, mut status: Signal<String>) {
    match crate::workspace_state::open(dir.clone()) {
        Ok(Some(ctx)) => {
            *ws.write() = Some(ctx);
            status.set(format!("workspace loaded: {}", dir.display()));
        }
        Ok(None) => status.set(format!("{} has no workspace.toml", dir.display())),
        Err(err) => status.set(format!("open failed: {err}")),
    }
}

/// Close the current workspace (state only; files on disk stay untouched).
pub fn close_workspace_ui(mut ws: Signal<Option<WsCtx>>, mut status: Signal<String>) {
    *ws.write() = None;
    status.set(String::from("workspace closed"));
}

/// A mode-B scan candidate: one direct subfolder of a scanned root.
///
/// Only metadata is gathered at scan time (`files`, `size`) so scanning a big
/// root stays cheap; the full schema inference happens on "Inspect…".
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Absolute candidate folder path.
    pub folder: PathBuf,
    /// Suggested entity name (= folder name).
    pub entity: String,
    /// How many readable files the folder holds.
    pub files: usize,
    /// Total size of those files (bytes).
    pub size_bytes: u64,
}

/// One-line summary for a candidate row.
impl Candidate {
    pub fn summary(&self) -> String {
        format!(
            "{} file(s) · {}",
            self.files,
            crate::util::format_bytes(self.size_bytes)
        )
    }
}

/// Remember `root` as a scan root in the workspace config and persist it.
pub fn save_scan_root(mut ws: Signal<Option<WsCtx>>, mut status: Signal<String>, root: PathBuf) {
    let Some(mut current) = ws.read().clone() else {
        status.set(String::from("open a workspace first"));
        return;
    };
    let root_str = root.display().to_string();
    if !current.config.scan_roots.contains(&root_str) {
        current.config.scan_roots.push(root_str);
    }
    match strata_core::save_config(&current.dir, &current.config)
        .map_err(|e| e.to_string())
        .and_then(|_| reload_ws(&current))
    {
        Ok(next) => *ws.write() = Some(next),
        Err(err) => status.set(format!("cannot save workspace: {err}")),
    }
}

/// List mode-B candidates under `root`, skipping folders already bound.
///
/// Pure read: does not touch the workspace config.
pub fn scan_candidates(ws: &WsCtx, root: &std::path::Path) -> Result<Vec<Candidate>, String> {
    let bound: Vec<&str> = ws
        .config
        .bindings
        .iter()
        .map(|b| b.folder.as_str())
        .collect();
    let mut found = Vec::new();
    for folder in list_entity_candidates(root).map_err(|e| e.to_string())? {
        if bound.contains(&folder.display().to_string().as_str()) {
            continue;
        }
        // Metadata only (name/size/kind) — cheap even for big folders.
        let scan: Option<FolderScan> = scan_folder(&folder).ok();
        found.push(Candidate {
            entity: candidate_entity_name(&folder),
            files: scan.as_ref().map(|s| s.files.len()).unwrap_or(0),
            size_bytes: scan.map(|s| s.total_size_bytes).unwrap_or(0),
            folder,
        });
    }
    found.sort_by(|a, b| a.entity.cmp(&b.entity));
    Ok(found)
}

/// Confirm a candidate: infer the schema with `options`, save it into
/// `schemas/` and register the folder→entity binding in `workspace.toml`.
pub fn confirm_and_bind(
    mut ws: Signal<Option<WsCtx>>,
    mut status: Signal<String>,
    mut refresh: Signal<u64>,
    folder: PathBuf,
    options: ReaderOptions,
) {
    let Some(current) = ws.read().clone() else {
        status.set(String::from("open a workspace first"));
        return;
    };
    let entity = candidate_entity_name(&folder);
    let report = match schema_from_folder(&folder, options) {
        Ok(report) => report,
        Err(err) => {
            status.set(format!("inference failed: {err}"));
            return;
        }
    };
    let columns: Vec<ColumnDef> = report
        .columns
        .iter()
        .map(|c| ColumnDef {
            name: c.name.clone(),
            dtype: c.dtype.clone(),
        })
        .collect();
    let schema = SchemaFile::new(folder.clone(), options, columns);
    let saved_file = match save_schema(&current.dir, &schema) {
        Ok(file) => file,
        Err(err) => {
            status.set(format!("schema save failed: {err}"));
            return;
        }
    };

    let mut config = current.config.clone();
    if let Err(err) = upsert_binding(&current.dir, &mut config, entity.clone(), folder) {
        status.set(format!("binding failed: {err}"));
        return;
    }
    match reload_ws(&current) {
        Ok(next) => {
            *ws.write() = Some(next);
            *refresh.write() += 1;
            status.set(format!(
                "entity '{entity}' confirmed — schema saved as {saved_file}, \
                 {} column(s)",
                report.columns.len()
            ));
        }
        Err(err) => status.set(format!("reload failed: {err}")),
    }
}

/// Stage a bound entity folder under its confirmed schema into
/// `data/<entity>/`. Without a saved schema file we fall back to the raw
/// (untyped) folder staging and say so in the status line.
pub fn stage_entity(
    ws: Signal<Option<WsCtx>>,
    mut status: Signal<String>,
    mut refresh: Signal<u64>,
    entity: String,
) {
    let Some(current) = ws.read().clone() else {
        return;
    };
    let Some(binding) = current
        .config
        .bindings
        .iter()
        .find(|b| b.entity == entity)
        .cloned()
    else {
        status.set(format!("no binding for '{entity}'"));
        return;
    };
    let folder = PathBuf::from(&binding.folder);
    let dest = current.data_dir().join(&entity);

    let outcome = match current.entity_schema_file(&entity) {
        Some(schema_file) => {
            let schema = match load_schema(&current.dir, &schema_file) {
                Ok(schema) => schema,
                Err(err) => {
                    status.set(format!("cannot load {schema_file}: {err}"));
                    return;
                }
            };
            stage_folder_with_schema(&folder, &dest, &schema)
                .map(|report| (report, format!("schema-validated via {schema_file}")))
        }
        None => strata_core::folder_to_parquet(&folder, &dest)
            .map(|report| (report, String::from("raw stage (no schema file found)"))),
    };

    match outcome {
        Ok((report, mode)) => {
            let parts_count = strata_core::list_parts(&dest).map(|p| p.len()).unwrap_or(0);
            let skipped = report.skipped.len();
            *refresh.write() += 1;
            status.set(format!(
                "staged '{entity}': {} file(s) → {} row(s), {} skipped \
                 ({parts_count} part file(s), {mode})",
                report.staged.len(),
                report.total_rows,
                skipped
            ));
        }
        Err(err) => status.set(format!("stage '{entity}' failed: {err}")),
    }
}
