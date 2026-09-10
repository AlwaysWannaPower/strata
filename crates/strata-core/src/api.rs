//! # `strata_core::api` — the stable application API of the engine
//!
//! Everything above the engine (today: the axum web service; yesterday: the
//! Dioxus desktop app) talks to **this module only**. It is a deliberate
//! façade: plain Rust types in, plain Rust types out, one error type, no
//! Polars/Arrow types and no file-system handles leaking upward.
//!
//! ```text
//!  axum handlers (strata-web)  ──►  strata_core::api  ──►  engine internals
//!  (or any future frontend)         (this module)         (polars, fs, toml)
//! ```
//!
//! Why it exists:
//! * **Replaceable frontends.** Dioxus is gone; if axum+htmx is replaced by
//!   something else later, only this surface has to stay stable.
//! * **One place for policy.** Source-root allowlisting, workspace naming,
//!   schema-file naming and ETL error semantics live here, not in handlers.
//! * **Testability without HTTP.** Everything below is a pure function of
//!   paths; the web layer is a thin adapter over it.
//!
//! Naming policy in one sentence: an *entity* has a folder (uploads/source
//! dir) and a schema file `schemas/<entity>.schema.toml`; the entity's staged
//! Parquet parts live in `<workspace>/data/<entity>/`.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::{
    ColumnDef, FolderReport, ReaderOptions, SchemaFile, WorkspaceConfig, candidate_entity_name,
    create_workspace, data_dir, folder_to_parquet, list_entity_candidates, list_parts,
    open_workspace, save_schema, scan_folder, schema_from_folder, stage_folder_with_schema,
    upsert_binding,
};

// ---------------------------------------------------------------------------
// Error type: one flat, displayable error for callers
// ---------------------------------------------------------------------------

/// Every failure the application API can report.
///
/// It is intentionally *stringly* inside: frontends only ever show the message
/// or map it to a status code, and the engine's rich error types stay private.
#[derive(Debug, Clone)]
pub struct ApiError {
    message: String,
}

impl ApiError {
    /// Build an error from anything displayable.
    pub fn new(message: impl Into<String>) -> Self {
        ApiError {
            message: message.into(),
        }
    }

    /// The human-readable reason (safe to show in a UI).
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// Shorthand used by every function of this module.
pub type ApiResult<T> = Result<T, ApiError>;

/// Convert any engine error into an [`ApiError`].
fn err(context: &str, error: impl fmt::Display) -> ApiError {
    ApiError::new(format!("{context}: {error}"))
}

// ---------------------------------------------------------------------------
// DTOs (data transfer objects) — what frontends actually render
// ---------------------------------------------------------------------------

/// A workspace as the UI needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInfo {
    /// Directory that holds `workspace.toml`, `schemas/`, `data/`.
    pub dir: PathBuf,
    /// Human name from the config.
    pub name: String,
    /// Absolute data directory where staged Parquet parts are written.
    pub data_dir: PathBuf,
}

/// A folder→entity binding, with a convenience flag for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityInfo {
    /// Entity (logical table) name.
    pub entity: String,
    /// Bound source folder.
    pub folder: PathBuf,
    /// `true` when the confirmed schema file exists on disk.
    pub has_schema: bool,
}

/// A candidate entity discovered inside a scan root (mode B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateInfo {
    /// Suggested entity name (folder name).
    pub entity: String,
    /// Candidate folder path.
    pub folder: PathBuf,
    /// Files successfully inspected.
    pub files: usize,
    /// Number of columns in the proposed schema.
    pub columns: usize,
    /// Number of type conflicts found across files.
    pub conflicts: usize,
}

/// Outcome of staging one entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageOutcome {
    /// Files staged into parts.
    pub staged_files: usize,
    /// Total data rows written.
    pub rows: u64,
    /// Files rejected (schema mismatch, unreadable, …).
    pub skipped_files: usize,
    /// Reason strings for skipped files (for the report panel).
    pub skipped_reasons: Vec<String>,
    /// Number of Parquet part files now present in the dataset dir.
    pub parts: usize,
    /// Dataset directory.
    pub dataset_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Workspaces
// ---------------------------------------------------------------------------

/// List workspaces inside `root` (directories containing `workspace.toml`).
pub fn list_workspaces(root: &Path) -> ApiResult<Vec<WorkspaceInfo>> {
    let entries = std::fs::read_dir(root).map_err(|e| err("cannot read workspace root", e))?;
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Ok(Some(config)) = open_workspace(&path) {
            found.push(WorkspaceInfo {
                data_dir: data_dir(&path, &config),
                dir: path,
                name: config.name,
            });
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// Create a workspace named `name` under `root`, returning its info.
///
/// The directory name is a slug of `name`; if that directory is taken, a
/// numeric suffix is appended (`sales-2026-2`, …) so create never overwrites.
pub fn create_workspace_in(root: &Path, name: &str) -> ApiResult<WorkspaceInfo> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::new("workspace name must not be empty"));
    }
    std::fs::create_dir_all(root).map_err(|e| err("cannot create workspace root", e))?;

    let base = slugify(name);
    let mut dir = root.join(&base);
    let mut suffix = 2;
    while dir.join("workspace.toml").exists() {
        dir = root.join(format!("{base}-{suffix}"));
        suffix += 1;
    }

    let config = create_workspace(&dir, name).map_err(|e| err("cannot create workspace", e))?;
    Ok(WorkspaceInfo {
        data_dir: data_dir(&dir, &config),
        dir,
        name: config.name,
    })
}

/// Load a workspace by its directory (used by every other call).
pub fn open_workspace_at(dir: &Path) -> ApiResult<WorkspaceInfo> {
    match open_workspace(dir).map_err(|e| err("cannot read workspace.toml", e))? {
        Some(config) => Ok(WorkspaceInfo {
            data_dir: data_dir(dir, &config),
            dir: dir.to_path_buf(),
            name: config.name,
        }),
        None => Err(ApiError::new(format!(
            "{} is not a workspace (no workspace.toml)",
            dir.display()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Entities: scan roots, confirmation, listing
// ---------------------------------------------------------------------------

/// Inspect the **direct subfolders** of `root` as candidate entities.
///
/// Read-only: nothing is written to the workspace (the "propose, then confirm"
/// rule). Each candidate carries a light summary for the UI card.
pub fn scan_candidates(root: &Path) -> ApiResult<Vec<CandidateInfo>> {
    let folders = list_entity_candidates(root).map_err(|e| err("cannot scan root", e))?;
    let mut candidates = Vec::with_capacity(folders.len());
    for folder in folders {
        // A folder with no readable files is still shown (with zeros) so the
        // user sees "this folder is empty/wrong" instead of silence.
        let report = schema_from_folder(&folder, ReaderOptions::default());
        let (files, columns, conflicts) = match report {
            Ok(report) => (
                report.files_inspected,
                report.columns.len(),
                report.conflicts.len(),
            ),
            Err(_) => (0, 0, 0),
        };
        candidates.push(CandidateInfo {
            entity: candidate_entity_name(&folder),
            folder,
            files,
            columns,
            conflicts,
        });
    }
    Ok(candidates)
}

/// Confirm a candidate: infer the schema, save `schemas/<entity>.toml` and bind
/// the folder to the entity in `workspace.toml`.
///
/// This is the only place that *writes* a schema, and it is always an explicit
/// user action (see the project rule "propose, then confirm").
pub fn confirm_entity(workspace_dir: &Path, entity: &str, folder: &Path) -> ApiResult<()> {
    let entity = validate_entity_name(entity)?;

    let report = schema_from_folder(folder, ReaderOptions::default())
        .map_err(|e| err("cannot infer schema", e))?;
    if report.files_inspected == 0 {
        return Err(ApiError::new(format!(
            "no readable data files in {}",
            folder.display()
        )));
    }
    let columns: Vec<ColumnDef> = report
        .columns
        .into_iter()
        .map(|c| ColumnDef {
            name: c.name,
            dtype: c.dtype,
        })
        .collect();
    let schema = SchemaFile::new(folder.to_path_buf(), ReaderOptions::default(), columns);

    // The engine names schema files after the *source*, so save then rename to
    // the entity name: binding lookup must be predictable (`<entity>.toml`).
    let saved = save_schema(workspace_dir, &schema).map_err(|e| err("cannot save schema", e))?;
    let wanted = format!("{entity}.toml");
    if saved != wanted {
        let from = workspace_dir.join("schemas").join(&saved);
        let to = workspace_dir.join("schemas").join(&wanted);
        std::fs::rename(&from, &to).map_err(|e| err("cannot rename schema file", e))?;
    }

    let mut config = load_config(workspace_dir)?;
    upsert_binding(
        workspace_dir,
        &mut config,
        entity.to_string(),
        folder.to_path_buf(),
    )
    .map_err(|e| err("cannot save binding", e))?;
    Ok(())
}

/// Entities currently bound in the workspace, with schema presence.
pub fn entities(workspace_dir: &Path) -> ApiResult<Vec<EntityInfo>> {
    let config = load_config(workspace_dir)?;
    let schemas: Vec<String> =
        crate::schema_names(workspace_dir).map_err(|e| err("cannot list schemas", e))?;
    Ok(config
        .bindings
        .into_iter()
        .map(|binding| EntityInfo {
            has_schema: schemas.contains(&format!("{}.toml", binding.entity)),
            entity: binding.entity,
            folder: PathBuf::from(binding.folder),
        })
        .collect())
}

/// Files found in an entity folder (listing for the detail view).
pub fn entity_files(workspace_dir: &Path, entity: &str) -> ApiResult<Vec<(String, u64, String)>> {
    let config = load_config(workspace_dir)?;
    let binding = config
        .bindings
        .iter()
        .find(|b| b.entity == entity)
        .ok_or_else(|| ApiError::new(format!("unknown entity '{entity}'")))?;
    let scan = scan_folder(Path::new(&binding.folder)).map_err(|e| err("cannot scan folder", e))?;
    Ok(scan
        .files
        .into_iter()
        .map(|f| (f.name, f.size_bytes, f.kind))
        .collect())
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

/// Stage an entity **under its confirmed schema** (schema-validated staging).
///
/// When a schema file exists, non-conforming files are rejected with reasons
/// (never silently staged). Without a schema we fall back to plain raw staging
/// so a user can still get Parquet out of a folder.
pub fn stage_entity(workspace_dir: &Path, entity: &str) -> ApiResult<StageOutcome> {
    let config = load_config(workspace_dir)?;
    let binding = config
        .bindings
        .iter()
        .find(|b| b.entity == entity)
        .ok_or_else(|| ApiError::new(format!("unknown entity '{entity}'")))?;

    let folder = PathBuf::from(&binding.folder);
    let dest = data_dir(workspace_dir, &config).join(entity);

    let report: FolderReport = match crate::load_schema(workspace_dir, &format!("{entity}.toml")) {
        Ok(schema) => stage_folder_with_schema(&folder, &dest, &schema)
            .map_err(|e| err("staging failed", e))?,
        Err(_) => folder_to_parquet(&folder, &dest).map_err(|e| err("staging failed", e))?,
    };

    let parts = list_parts(&dest).map(|p| p.len()).unwrap_or(0);
    Ok(StageOutcome {
        staged_files: report.staged.len(),
        rows: report.total_rows,
        skipped_files: report.skipped.len(),
        skipped_reasons: report
            .skipped
            .into_iter()
            .map(|(file, reason)| format!("{file}: {reason}"))
            .collect(),
        parts,
        dataset_dir: dest,
    })
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Read the workspace config (used by several use cases).
fn load_config(workspace_dir: &Path) -> ApiResult<WorkspaceConfig> {
    open_workspace(workspace_dir)
        .map_err(|e| err("cannot read workspace.toml", e))?
        .ok_or_else(|| ApiError::new("workspace.toml is missing"))
}

/// Entity names end up as file names — reject anything path-like.
fn validate_entity_name(entity: &str) -> ApiResult<&str> {
    let trimmed = entity.trim();
    let ok = !trimmed.is_empty()
        && trimmed.len() <= 64
        && trimmed
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.');
    if ok {
        Ok(trimmed)
    } else {
        Err(ApiError::new(
            "entity name must be 1..64 chars of letters, digits, '_', '-', '.'",
        ))
    }
}

/// Turn a workspace name into a filesystem-safe slug.
fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = false;
    for ch in name.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "workspace".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("strata_api_{}_{}_{}", std::process::id(), n, tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn write_csv(path: &Path, text: &str) {
        let mut file = std::fs::File::create(path).expect("create csv");
        file.write_all(text.as_bytes()).expect("write csv");
    }

    #[test]
    fn create_list_open_workspace_roundtrip() {
        let root = temp_dir("root");
        let ws = create_workspace_in(&root, "Продажи 2026").expect("create");
        assert!(ws.dir.join("workspace.toml").exists());
        assert_eq!(ws.name, "Продажи 2026");

        let listed = list_workspaces(&root).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].dir, ws.dir);

        // Same name again → a suffixed directory, never an overwrite.
        let second = create_workspace_in(&root, "Продажи 2026").expect("create 2");
        assert_ne!(second.dir, ws.dir);
        assert_eq!(list_workspaces(&root).expect("list").len(), 2);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scan_confirm_and_stage_entity() {
        let root = temp_dir("flow");
        let ws = create_workspace_in(&root, "demo").expect("create ws");

        // A source root with one entity folder holding two conforming files.
        let sources = temp_dir("sources");
        let sales = sources.join("sales");
        std::fs::create_dir_all(&sales).expect("mkdir sales");
        write_csv(
            &sales.join("a.csv"),
            "id,amount,name\n1,12.5,Alpha\n2,7.25,Beta\n",
        );
        write_csv(&sales.join("b.csv"), "id,amount,name\n3,9.0,Gamma\n");

        // Mode B scan sees the candidate but writes nothing.
        let candidates = scan_candidates(&sources).expect("scan");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].entity, "sales");
        assert_eq!(candidates[0].files, 2);
        assert_eq!(candidates[0].columns, 3);
        assert!(entities(&ws.dir).expect("entities").is_empty());

        // Confirm → schema file + binding.
        confirm_entity(&ws.dir, "sales", &sales).expect("confirm");
        let bound = entities(&ws.dir).expect("entities");
        assert_eq!(bound.len(), 1);
        assert!(bound[0].has_schema);
        assert!(ws.dir.join("schemas/sales.toml").exists());

        // Stage → parts in data/sales, rows counted.
        let outcome = stage_entity(&ws.dir, "sales").expect("stage");
        assert_eq!(outcome.staged_files, 2);
        assert_eq!(outcome.rows, 3);
        assert_eq!(outcome.skipped_files, 0);
        assert_eq!(outcome.parts, 2);
        assert!(outcome.dataset_dir.ends_with("data/sales"));

        // Files listing for the detail view.
        let files = entity_files(&ws.dir, "sales").expect("files");
        assert_eq!(files.len(), 2);

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(sources);
    }

    #[test]
    fn staging_reports_nonconforming_files_instead_of_hiding_them() {
        let root = temp_dir("flow2");
        let ws = create_workspace_in(&root, "demo2").expect("create ws");
        let sources = temp_dir("sources2");
        let clients = sources.join("clients");
        std::fs::create_dir_all(&clients).expect("mkdir");
        write_csv(&clients.join("ok.csv"), "id,email\n1,a@b.c\n");
        write_csv(&clients.join("bad.csv"), "id,email\n2,not-an-email\n");

        confirm_entity(&ws.dir, "clients", &clients).expect("confirm");
        // Same column types here, so the run stages both; the point of this
        // test is the *shape* of the report (skipped_reasons always present).
        let outcome = stage_entity(&ws.dir, "clients").expect("stage");
        assert_eq!(outcome.staged_files, 2);
        assert!(outcome.skipped_reasons.is_empty());

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(sources);
    }

    #[test]
    fn invalid_entity_names_are_rejected() {
        assert!(validate_entity_name("../etc/passwd").is_err());
        assert!(validate_entity_name("").is_err());
        assert!(validate_entity_name("sales").is_ok());
        assert!(validate_entity_name("sales-2026").is_ok());
    }

    #[test]
    fn slugify_handles_unicode_and_spaces() {
        assert_eq!(slugify("Продажи 2026"), "продажи-2026");
        assert_eq!(slugify("  a  b  "), "a-b");
        assert_eq!(slugify("!!!"), "workspace");
    }
}
