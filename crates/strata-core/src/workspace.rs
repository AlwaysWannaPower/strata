//! # Workspace model (M1c): IDE-like settings folder + `data/` outputs
//!
//! Layout (see `docs/design-workspace-pipeline.md`):
//!
//! ```text
//! <workspace>/
//! ├── workspace.toml     # name, data_dir, bindings (folder→entity), scan roots
//! ├── schemas/           # *.schema.toml — confirmed schemas (project.rs helpers)
//! ├── data/<entity>/     # staged Parquet parts go here (default data_dir = "data")
//! ├── plugins/           # reserved
//! └── logs/              # reserved
//! ```
//!
//! Core rule: **one folder = one schema**. Two binding modes:
//!
//! * explicit [`Binding`] — user said "folder X is entity `sales`";
//! * scan roots — user gave a root; its **direct subfolders** are candidate
//!   entities (mode B). Roots are remembered here; candidates are discovered
//!   at scan time and become explicit bindings only after the user confirms.
//!
//! Schemas themselves are stored/loaded by the existing `project.rs` helpers
//! (the schema file format is shared); this module owns the *workspace
//! configuration* only.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::StrataError;

/// Configuration file format version.
const CONFIG_FORMAT: u32 = 1;

/// One confirmed mapping: a folder whose files belong to one entity/schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// Logical table name (also the schema file name and the `data/<entity>/` dir).
    pub entity: String,
    /// Absolute folder path holding the entity's files.
    pub folder: String,
    /// When the binding was added (UTC, informational).
    pub added_utc: String,
}

/// Everything the workspace knows about its sources, stored in
/// `workspace.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// File format version.
    pub format: u32,
    /// Human name of the workspace.
    pub name: String,
    /// Directory (relative to the workspace) where Parquet outputs live.
    pub data_dir: String,
    /// Confirmed folder→entity mappings (mode A and confirmed mode B).
    #[serde(default)]
    pub bindings: Vec<Binding>,
    /// Roots scanned for candidate entities (mode B). Candidates become
    /// bindings only after user confirmation.
    #[serde(default)]
    pub scan_roots: Vec<String>,
}

/// Absolute path where this workspace stores its Parquet datasets.
pub fn data_dir(workspace_dir: &Path, config: &WorkspaceConfig) -> PathBuf {
    let path = PathBuf::from(&config.data_dir);
    if path.is_absolute() {
        path
    } else {
        workspace_dir.join(path)
    }
}

/// Absolute path of the workspace's `schemas/` folder.
pub fn schemas_dir(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("schemas")
}

/// Create a new workspace directory (fails if `workspace.toml` already exists).
pub fn create_workspace(dir: &Path, name: &str) -> crate::Result<WorkspaceConfig> {
    if dir.join("workspace.toml").exists() {
        return Err(StrataError::ProjectExists(dir.to_path_buf()));
    }
    for sub in ["schemas", "data", "plugins", "logs"] {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    let config = WorkspaceConfig {
        format: CONFIG_FORMAT,
        name: name.trim().to_string(),
        data_dir: String::from("data"),
        bindings: Vec::new(),
        scan_roots: Vec::new(),
    };
    save_config(dir, &config)?;
    Ok(config)
}

/// Open a workspace: returns its config, or `None` if the folder is not one.
pub fn open_workspace(dir: &Path) -> crate::Result<Option<WorkspaceConfig>> {
    let file = dir.join("workspace.toml");
    if !file.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&file)?;
    let config: WorkspaceConfig = toml::from_str(&text)
        .map_err(|e| StrataError::ProjectFile(format!("parse workspace.toml: {e}")))?;
    Ok(Some(config))
}

/// Persist the config back to `workspace.toml` (whole-file rewrite).
pub fn save_config(dir: &Path, config: &WorkspaceConfig) -> crate::Result<()> {
    let text = toml::to_string(config)
        .map_err(|e| StrataError::ProjectFile(format!("serialize workspace.toml: {e}")))?;
    std::fs::write(dir.join("workspace.toml"), text)?;
    Ok(())
}

/// Direct subfolders of `root` (mode B candidates); files/symlinks ignored.
/// Sub-subfolders are deliberately not returned: one level only — the design
/// rule is "no nesting under an entity folder".
pub fn list_entity_candidates(root: &Path) -> crate::Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// Human name for the candidate folder (its directory name).
pub fn candidate_entity_name(folder: &Path) -> String {
    folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "entity".to_string())
}

/// Add (or replace) a confirmed folder→entity binding and persist the config.
pub fn upsert_binding(
    dir: &Path,
    config: &mut WorkspaceConfig,
    entity: String,
    folder: PathBuf,
) -> crate::Result<()> {
    // Replace any previous binding for the same entity (latest wins).
    config.bindings.retain(|b| b.entity != entity);
    config.bindings.push(Binding {
        entity,
        folder: folder.display().to_string(),
        added_utc: String::from("now"),
    });
    save_config(dir, config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("strata_ws_{}_{}_{}", std::process::id(), n, tag));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn workspace_create_open_and_binding_roundtrip() {
        let dir = temp_dir("ws");
        let config = create_workspace(&dir, "Продажи 2026").expect("create");
        assert_eq!(config.name, "Продажи 2026");
        assert!(dir.join("data").is_dir());
        assert!(dir.join("schemas").is_dir());

        let mut config = open_workspace(&dir).expect("open").expect("exists");
        assert_eq!(config.name, "Продажи 2026");
        assert_eq!(data_dir(&dir, &config), dir.join("data"));

        upsert_binding(
            &dir,
            &mut config,
            "sales".into(),
            PathBuf::from("/data/sales"),
        )
        .unwrap();
        let reopened = open_workspace(&dir).expect("open").expect("exists");
        assert_eq!(reopened.bindings.len(), 1);
        assert_eq!(reopened.bindings[0].entity, "sales");

        let err = create_workspace(&dir, "again").expect_err("duplicate fails");
        assert!(matches!(err, StrataError::ProjectExists(_)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn entity_candidates_are_direct_subfolders_only() {
        let root = temp_dir("root");
        std::fs::create_dir_all(root.join("sales/inner")).unwrap(); // deeper dir must NOT be a candidate
        std::fs::create_dir_all(root.join("clients")).unwrap();
        std::fs::write(root.join("notes.txt"), "x").unwrap();

        let candidates = list_entity_candidates(&root).expect("list");
        let names: Vec<String> = candidates
            .iter()
            .map(|p| candidate_entity_name(p))
            .collect();
        // Direct subfolders only: sales, clients — NOT sales/inner, NOT the file.
        assert_eq!(names, vec!["clients", "sales"]);
        let _ = std::fs::remove_dir_all(root);
    }
}
