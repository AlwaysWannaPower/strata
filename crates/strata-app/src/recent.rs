//! # Recent workspaces (M1c): remember the last opened workspace folders
//!
//! "Я создал workspace, переключился на другую вкладку, вернулся — всё
//! пропало." Part of the fix is architectural (workspace state now lives in
//! `App`, not in a screen), the other part is this file: the *last used
//! workspace* is persisted so the app restores it on the next launch.
//!
//! The store is a tiny plain-text file — one folder path per line, newest
//! first, de-duplicated. It lives in the platform config directory:
//!
//! * Linux:   `$XDG_CONFIG_HOME/strata/recent.txt` (or `~/.config/strata/…`);
//! * macOS:   `~/Library/Application Support/strata/recent.txt`;
//! * Windows: `%APPDATA%\strata\recent.txt`.
//!
//! No extra crates: the config path is derived from environment variables,
//! which is all a desktop app on three platforms needs here.
//!
//! The small read/write helpers take the store *file path* as a parameter so
//! unit tests can point them at a sandbox folder (no global env mutation).

use std::path::{Path, PathBuf};

/// How many recent workspaces we keep.
const MAX_RECENT: usize = 6;

/// Path of the recent-workspaces file (create parent dirs on demand).
fn recent_file() -> PathBuf {
    let dir = config_dir().join("strata");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("recent.txt")
}

/// The platform config directory (see module docs).
pub(crate) fn config_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg);
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata);
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join("Library/Application Support");
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".config");
        }
    }
    // Last resort: current directory (tests, exotic platforms).
    PathBuf::from(".")
}

/// Read the list of recent workspace folders, newest first.
///
/// Entries that no longer exist on disk are skipped (and dropped on the next
/// [`remember`] write).
pub fn list() -> Vec<PathBuf> {
    read_store(&recent_file())
}

/// Push `dir` to the front of the recent list (de-duplicated, capped).
pub fn remember(dir: &Path) {
    push_to_store(&recent_file(), dir);
}

/// Internal list reader, parameterised by the store file (testable).
pub(crate) fn read_store(file: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    text.lines()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect()
}

/// Internal list writer, parameterised by the store file (testable).
pub(crate) fn push_to_store(file: &Path, dir: &Path) {
    // Existing entries minus `dir`, with `dir` re-added at the front.
    let mut entries: Vec<PathBuf> = read_store(file);
    entries.retain(|p| p != dir);
    entries.insert(0, dir.to_path_buf());
    entries.truncate(MAX_RECENT);
    let text = entries
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(file, text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A sandbox store file plus two existing folders for testing.
    fn sandbox() -> (PathBuf, PathBuf, PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("strata_rec_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        (root.join("store.txt"), root.join("a"), root.join("b"))
    }

    #[test]
    fn push_dedups_and_moves_to_front() {
        let (file, a, b) = sandbox();
        push_to_store(&file, &a);
        push_to_store(&file, &b);
        push_to_store(&file, &a); // must move back to front, not duplicate

        assert_eq!(read_store(&file), vec![a.clone(), b.clone()]);

        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn list_skips_missing_folders() {
        let root = std::env::temp_dir().join(format!(
            "strata_rec_gone_{}",
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("store.txt");
        let dead = root.join("gone"); // never created
        let alive = root.join("alive");
        std::fs::create_dir_all(&alive).unwrap();
        push_to_store(&file, &dead);
        push_to_store(&file, &alive);

        assert_eq!(read_store(&file), vec![alive.clone()]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
