//! # Workspace state (M1c): the app-wide handle to the open workspace
//!
//! A "workspace" (see `strata_core::workspace`) is a folder holding
//! `workspace.toml` + `schemas/` + `data/`. This module wraps it for the UI as
//! [`WsCtx`] — directory plus parsed config — together with the operations the
//! shell needs:
//!
//! * [`create`] / [`open`] — user actions (both remember the folder);
//! * [`reload`] — re-read `workspace.toml` from disk after an engine action
//!   that persisted config (bind/scan-root), keeping disk as source of truth;
//! * [`entity_schema_file`] — find the saved schema file of an entity.
//!
//! ## Why this lives in `App`, not in a screen
//!
//! Screens unmount when you navigate away; a `use_signal` created inside a
//! screen dies with it. The workspace must survive tab switches (and restarts
//! via `recent.rs`), so `App` owns a `Signal<Option<WsCtx>>` and hands it down.

use std::path::{Path, PathBuf};

use strata_core::{
    WorkspaceConfig, create_workspace, data_dir as data_dir_of, open_workspace, schema_names,
};

use crate::recent;

/// The open workspace: its directory + its parsed configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct WsCtx {
    /// Absolute workspace directory (holds `workspace.toml`).
    pub dir: PathBuf,
    /// Parsed configuration from `workspace.toml`.
    pub config: WorkspaceConfig,
}

impl WsCtx {
    /// Absolute path of the Parquet output folder (`data/` of the workspace).
    pub fn data_dir(&self) -> PathBuf {
        data_dir_of(&self.dir, &self.config)
    }

    /// Find the saved schema file for `entity` (`schemas/<entity>.toml`),
    /// comparing by file stem because the storage layer sanitizes file names.
    pub fn entity_schema_file(&self, entity: &str) -> Option<String> {
        let names = schema_names(&self.dir).ok()?;
        names.into_iter().find(|name| {
            Path::new(name)
                .file_stem()
                .is_some_and(|stem| stem == entity)
        })
    }
}

/// Create a new workspace at `dir` (name may be empty → folder-derived).
pub fn create(dir: PathBuf, name: &str) -> Result<WsCtx, String> {
    create_internal(dir, name, None)
}

/// Open an existing workspace. `Ok(None)` = the folder is not a workspace.
pub fn open(dir: PathBuf) -> Result<Option<WsCtx>, String> {
    open_internal(dir, None)
}

/// Re-read the config of `current` from disk (after a bind/scan-root write).
pub fn reload(current: &WsCtx) -> Result<WsCtx, String> {
    match open_workspace(&current.dir).map_err(|e| e.to_string())? {
        Some(config) => Ok(WsCtx {
            dir: current.dir.clone(),
            config,
        }),
        None => Err(format!(
            "{} no longer contains workspace.toml",
            current.dir.display()
        )),
    }
}

/// Restore the most recent workspace, if any (used at startup).
pub fn restore_last() -> Option<WsCtx> {
    recent::list()
        .into_iter()
        .find_map(|dir| open(dir).ok().flatten())
}

/// Internal [`create`] with an optional custom recent-store file (tests use a
/// sandbox file so they never touch the real user config).
fn create_internal(dir: PathBuf, name: &str, store: Option<&Path>) -> Result<WsCtx, String> {
    let final_name = if name.trim().is_empty() {
        "Workspace"
    } else {
        name.trim()
    };
    let config = create_workspace(&dir, final_name).map_err(|e| e.to_string())?;
    remember_into(store, &dir);
    Ok(WsCtx { dir, config })
}

/// Internal [`open`] with an optional custom recent-store file.
fn open_internal(dir: PathBuf, store: Option<&Path>) -> Result<Option<WsCtx>, String> {
    match open_workspace(&dir).map_err(|e| e.to_string())? {
        Some(config) => {
            remember_into(store, &dir);
            Ok(Some(WsCtx { dir, config }))
        }
        None => Ok(None),
    }
}

/// Remember into the given store file, or the real user store when `None`.
fn remember_into(store: Option<&Path>, dir: &Path) {
    match store {
        Some(file) => recent::push_to_store(file, dir),
        None => recent::remember(dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(tag: &str) -> (PathBuf, PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("strata_wsc_{}_{}_{}", std::process::id(), n, tag));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        (root.join("store.txt"), root.join("ws"))
    }

    #[test]
    fn create_open_reload_roundtrip() {
        let (store, dir) = temp_dir("ws");
        std::fs::create_dir_all(&dir).unwrap();
        let created = create_internal(dir.clone(), "Демо", Some(&store)).expect("create");
        assert_eq!(created.config.name, "Демо");
        assert!(dir.join("data").is_dir());
        assert!(dir.join("schemas").is_dir());

        let opened = open_internal(dir.clone(), Some(&store))
            .expect("open")
            .expect("some");
        assert_eq!(opened.config.name, "Демо");
        assert_eq!(opened.data_dir(), dir.join("data"));

        // reload() re-reads from disk and keeps the same directory.
        let reloaded = reload(&opened).expect("reload");
        assert_eq!(reloaded.dir, dir);
        assert_eq!(reloaded.config.bindings.len(), 0);

        // Opening a plain folder yields Ok(None).
        let (store2, plain) = temp_dir("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(
            open_internal(plain.clone(), Some(&store2))
                .expect("read")
                .is_none()
        );

        // The workspace was remembered (sandbox store, not the real one).
        assert_eq!(recent::read_store(&store), vec![dir.clone()]);

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        let _ = std::fs::remove_dir_all(plain.parent().unwrap());
    }

    #[test]
    fn create_fails_when_workspace_exists() {
        let (store, dir) = temp_dir("dup");
        std::fs::create_dir_all(&dir).unwrap();
        create_internal(dir.clone(), "one", Some(&store)).expect("first create ok");
        let err = create_internal(dir.clone(), "two", Some(&store)).expect_err("second fails");
        assert!(!err.is_empty());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
