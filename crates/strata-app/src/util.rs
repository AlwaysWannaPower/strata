//! # Shared UI helpers (no Dioxus state, plain Rust)
//!
//! Small utilities used by more than one screen/component:
//!
//! * [`reader_options_from`] — map the three UI selectors (encoding /
//!   delimiter / "first row is header") to the engine's [`ReaderOptions`];
//! * [`format_bytes`] — human-readable byte sizes (`12.4 GB`, `880 KB`);
//! * [`pick_folder`] — one native folder dialog with a stable title.

use std::path::PathBuf;

use strata_core::{EncodingChoice, ReaderOptions};

/// Build the engine [`ReaderOptions`] from the UI selectors ("auto" = `None`).
///
/// A tiny plain function (no Dioxus involved) so the mapping between UI values
/// and engine options lives in exactly one place and is easy to unit-test.
pub(crate) fn reader_options_from(
    encoding: &str,
    delimiter: &str,
    has_header: bool,
) -> ReaderOptions {
    let encoding = match encoding {
        "utf8" => Some(EncodingChoice::Utf8),
        "cp1251" => Some(EncodingChoice::Windows1251),
        "cp1252" => Some(EncodingChoice::Windows1252),
        "utf16le" => Some(EncodingChoice::Utf16Le),
        "utf16be" => Some(EncodingChoice::Utf16Be),
        _ => None, // "auto"
    };
    let delimiter = match delimiter {
        "," => Some(','),
        ";" => Some(';'),
        "tab" => Some('\t'),
        "|" => Some('|'),
        _ => None, // "auto"
    };
    ReaderOptions {
        encoding,
        delimiter,
        has_header,
    }
}

/// Human-readable byte size: `12.4 GB`, `880 KB`, …
pub(crate) fn format_bytes(bytes: u64) -> String {
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

/// Native folder picker (used for workspace, roots and dataset destinations).
pub(crate) fn pick_folder(title: &str) -> Option<PathBuf> {
    rfd::FileDialog::new().set_title(title).pick_folder()
}
