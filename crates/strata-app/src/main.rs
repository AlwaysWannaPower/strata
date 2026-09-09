//! # strata-app — the Strata desktop shell (Dioxus 0.7, WebView)
//!
//! This binary is deliberately thin: every data operation lives in
//! [`strata_core`]. The UI only renders plain data structures and forwards
//! user intent.
//!
//! ## Layout of this crate (read top-down)
//!
//! ```text
//! src/
//! ├── main.rs      — entry point, App frame, navigation rail, CSS,
//! │                  placeholder screens (Schema/Datasets/Quality/Logs)
//! ├── sources.rs   — the "Sources" screen: single-file card + folder card
//! └── preview.rs   — reusable presentational components (PreviewCard/Table)
//! ```
//!
//! ## How a Dioxus desktop app is structured (0.7)
//!
//! 1. `main()` builds the platform ("desktop" = system WebView) and launches
//!    the root component [`App`]. `launch` blocks the main thread and runs the
//!    event loop until the window closes.
//! 2. [`App`] is the **root component**. A component is a plain function
//!    annotated `#[component]` that returns an `Element` (a virtual node).
//!    Dioxus re-runs the function when its reactive state changes and diffs
//!    the output — declarative UI, no manual DOM updates.
//! 3. State lives in **signals** (`use_signal`). Reading: `sig.read()`;
//!    writing: `sig.set(v)` / `*sig.write() = v`; each write schedules a
//!    re-render of the components that read the signal.
//! 4. Screens are components too. [`App`] owns one small piece of state — the
//!    *active screen* — and renders exactly one screen at a time via a match.
//!    This is "state as low as possible": only truly global state lives here.
//!
//! ## UI copy
//!
//! English on purpose: code, comments and user-facing strings stay in one
//! language (see `PLAN.md` §5). Beginner explanations in Russian live in
//! `docs/guide-1-dioxus.md`.

#[allow(dead_code)]
mod api;
mod dataset_screen;
mod pipeline_home;
pub(crate) mod preview;
mod project_screen;
mod schema_screen;
pub(crate) mod sources;

use dataset_screen::DatasetScreen;
use dioxus::prelude::*;
use pipeline_home::PipelineHome;
use project_screen::ProjectScreen;
use schema_screen::SchemaScreen;
use sources::SourcesScreen;
use std::path::PathBuf;

/// The five top-level screens. Navigation is a plain Rust enum: the compiler
/// guarantees every screen is handled in the `match` below (no stringly-typed
/// routing for the desktop shell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Pipeline,
    Sources,
    Schema,
    Datasets,
    Quality,
    Logs,
}

impl Screen {
    /// All screens in navigation order (used to render the rail).
    const ALL: [Screen; 6] = [
        Screen::Pipeline,
        Screen::Sources,
        Screen::Schema,
        Screen::Datasets,
        Screen::Quality,
        Screen::Logs,
    ];

    /// Small glyph shown next to the label in the rail.
    fn icon(self) -> &'static str {
        match self {
            Screen::Pipeline => "🛠",
            Screen::Sources => "🗂",
            Screen::Schema => "🧬",
            Screen::Datasets => "📦",
            Screen::Quality => "🛡",
            Screen::Logs => "🗂",
        }
    }

    /// Rail label.
    fn label(self) -> &'static str {
        match self {
            Screen::Pipeline => "Pipeline",
            Screen::Sources => "Sources",
            Screen::Schema => "Schemas",
            Screen::Datasets => "Datasets",
            Screen::Quality => "Quality",
            Screen::Logs => "Project",
        }
    }
}

/// Application entry point. `LaunchBuilder::desktop()` picks the desktop
/// renderer (only the `desktop` feature is enabled, so there is exactly one).
fn main() {
    dioxus::LaunchBuilder::desktop().launch(App);
}

/// Root component: brand header + navigation rail + the active screen.
///
/// ## State here vs state in screens
///
/// [`App`] holds exactly one signal: which screen is visible. Everything else
/// (previews, reports, inputs) belongs to the screen that uses it and lives
/// inside that screen's component — see `sources.rs`. This keeps state local
/// and makes components reusable.
#[component]
fn App() -> Element {
    let active = use_signal(|| Screen::Sources);
    // Shared project directory. Lives here because two screens use it:
    // Project (create/open/list) and Schemas (save schema into it).
    let project = use_signal(|| Option::<PathBuf>::None);

    rsx! {
        // Inject the single global stylesheet (Dioxus renders it into the
        // document head; raw CSS as one const keeps a tiny app self-contained).
        document::Style { "{CSS}" }

        div { class: "app",
            header { class: "topbar",
                div { class: "brand",
                    h1 { "Strata" }
                    span { class: "tagline", "Data Engineering Workbench" }
                }
                span { class: "layers", "Raw (staging) · no business rules · rules at ODS" }
            }

            div { class: "body",
                // Navigation rail: one button per Screen variant.
                // `active` is passed down by value because Signal<Screen> is
                // Copy — cheap, and the rail can set it on click.
                NavRail { active }

                main { class: "content",
                    // Render exactly one screen. `match` over the enum means
                    // adding a screen = compiler reminder to handle it here.
                    match *active.read() {
                        Screen::Pipeline => rsx! { PipelineHome {} },
                        Screen::Sources => rsx! { SourcesScreen {} },
                        Screen::Schema => rsx! { SchemaScreen { project } },
                        Screen::Datasets => rsx! { DatasetScreen {} },
                        Screen::Quality => rsx! { PlaceholderScreen {
                            title: "Quality",
                            text: "Data quality rules (NOT NULL, UNIQUE, RANGE, REGEX, …) and the \
                                   quarantine of broken rows are the M3 milestone."
                        } },
                        Screen::Logs => rsx! { ProjectScreen { project } },
                    }
                }
            }
        }
    }
}

/// Props of [`NavRail`]: the one signal it may change.
#[derive(Props, Clone, PartialEq)]
struct NavRailProps {
    /// Currently active screen. Passed as a `Signal` (Copy), so the rail can
    /// both read it (to highlight) and write it (to navigate).
    active: Signal<Screen>,
}

/// The left navigation rail: one button per [`Screen`].
#[component]
fn NavRail(props: NavRailProps) -> Element {
    // Signal<Screen> is Copy: take it out of the props struct once so the
    // closures below can call `active.set(...)` (which needs `&mut`) without
    // making the whole props struct mutable.
    let mut active = props.active;
    rsx! {
        nav { class: "nav",
            for screen in Screen::ALL {
                // Dynamic class list: `active` highlight depends on current value.
                button {
                    class: if *active.read() == screen { "nav-item active" } else { "nav-item" },
                    onclick: move |_| active.set(screen),
                    span { class: "nav-icon", "{screen.icon()}" }
                    "{screen.label()}"
                }
            }
        }
    }
}

/// Props of [`PlaceholderScreen`] — a plain-text stand-in until a real screen
/// is implemented (keeps the shell navigable from the start).
#[derive(Props, Clone, PartialEq)]
struct PlaceholderScreenProps {
    title: &'static str,
    text: &'static str,
}

#[component]
fn PlaceholderScreen(props: PlaceholderScreenProps) -> Element {
    rsx! {
        div { class: "screen",
            h1 { class: "screen-title", "{props.title}" }
            p { class: "screen-sub", "{props.text}" }
        }
    }
}

/// The whole application stylesheet.
///
/// Dioxus desktop renders into a WebView, so this is regular CSS. Class names
/// used across `main.rs`, `sources.rs` and `preview.rs` are all defined here —
/// one place to tweak the look (M4 will split themes/tokens, not needed yet).
const CSS: &str = r#"
    :root {
        --bg: #0e1216; --panel: #151b22; --panel2: #1a2129; --line: #2a333d;
        --text: #d7dee6; --muted: #8b98a5; --accent: #4da3ff;
    }
    * { box-sizing: border-box; }
    body { margin: 0; background: var(--bg); color: var(--text);
           font-family: system-ui, sans-serif; font-size: 14px; }

    /* ---- app frame ---------------------------------------------------- */
    .app { display: flex; flex-direction: column; height: 100vh; }
    .topbar { display: flex; align-items: center; justify-content: space-between;
              padding: 10px 16px; border-bottom: 1px solid var(--line);
              background: var(--panel); }
    .brand { display: flex; align-items: baseline; gap: 10px; }
    .brand h1 { margin: 0; font-size: 18px; letter-spacing: 0.3px; }
    .tagline { color: var(--muted); font-size: 12px; }
    .layers { color: var(--muted); font-size: 12px; font-family: monospace; }
    .body { display: flex; flex: 1; min-height: 0; }

    /* ---- navigation rail ---------------------------------------------- */
    .nav { display: flex; flex-direction: column; gap: 4px; width: 200px;
           padding: 12px 8px; border-right: 1px solid var(--line);
           background: var(--panel); flex-shrink: 0; overflow-y: auto; }
    .nav-item { display: flex; gap: 8px; align-items: center; text-align: left;
                background: transparent; border: 1px solid transparent;
                border-radius: 6px; color: var(--text); padding: 8px 10px;
                cursor: pointer; font-size: 13px; }
    .nav-item:hover { background: var(--panel2); }
    .nav-item.active { background: var(--panel2); border-color: var(--line);
                       color: var(--accent); }
    .nav-icon { width: 18px; text-align: center; }

    /* ---- content area -------------------------------------------------- */
    .content { flex: 1; overflow-y: auto; padding: 16px 18px; }
    .screen { display: flex; flex-direction: column; gap: 10px;
              max-width: 1080px; }
    .screen-title { margin: 0; font-size: 20px; }
    .screen-sub { margin: 0; color: var(--muted); max-width: 760px;
                  line-height: 1.45; }

    /* ---- cards ---------------------------------------------------------- */
    .card { background: var(--panel); border: 1px solid var(--line);
            border-radius: 10px; overflow: hidden; }
    .card-head { display: flex; align-items: center; gap: 8px;
                 padding: 8px 12px; border-bottom: 1px solid var(--line);
                 background: var(--panel2); }
    .card-title { font-weight: 600; font-size: 13px; text-transform: uppercase;
                  letter-spacing: 0.4px; color: var(--muted); }
    .card-badge { font-family: monospace; font-size: 11px; color: var(--accent);
                  border: 1px solid var(--line); border-radius: 4px;
                  padding: 1px 6px; }
    .card-body { display: flex; flex-direction: column; gap: 8px; padding: 12px; }
    .provenance { font-family: monospace; font-size: 12px; color: var(--accent); }

    /* ---- toolbar/inputs ------------------------------------------------- */
    .toolbar { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
    .hints { margin-top: -2px; }
    .hint { color: var(--muted); font-size: 12px; }
    .path-input { flex: 1; min-width: 260px; background: var(--bg);
                  border: 1px solid var(--line); border-radius: 6px;
                  color: var(--text); padding: 6px 9px; font-family: monospace; }
    .path-input:focus { outline: none; border-color: var(--accent); }
    .mini-input { background: var(--bg); border: 1px solid var(--line);
                  border-radius: 6px; color: var(--text); padding: 3px 8px;
                  font-size: 12px; font-family: monospace; }
    .mini-input:focus { outline: none; border-color: var(--accent); }
    button { background: var(--panel2); border: 1px solid var(--line);
             border-radius: 6px; color: var(--text); padding: 6px 12px;
             cursor: pointer; font-size: 13px; }
    button:hover { border-color: var(--accent); }

    /* ---- tables ---------------------------------------------------------- */
    .table-wrap { overflow: auto; border: 1px solid var(--line);
                  border-radius: 8px; background: var(--bg); }
    table.grid { border-collapse: collapse; font-family: monospace; font-size: 12px;
                 width: 100%; }
    table.grid th, table.grid td { border-bottom: 1px solid var(--line);
                 padding: 4px 10px; text-align: left; white-space: nowrap; }
    table.grid th { position: sticky; top: 0; background: var(--panel2);
                 color: var(--muted); font-weight: 600; z-index: 1; }
    table.grid tbody tr:hover { background: var(--panel2); }
    table.grid.files td { white-space: nowrap; }
    .empty-note { color: var(--muted); font-style: italic; padding: 6px 2px;
                  font-size: 12px; }

    /* ---- reports/status --------------------------------------------------- */
    .report { background: var(--panel2); border: 1px solid var(--line);
              border-radius: 8px; padding: 8px 12px; font-size: 12px; }
    .report p { margin: 2px 0 0; font-family: monospace; }
    .report ul { margin: 6px 0 0; padding-left: 18px; color: #ef6a6a;
                 font-size: 12px; }
    .report strong { color: var(--muted); text-transform: uppercase;
                     font-size: 11px; letter-spacing: 0.4px; }
    .toolbar.opts { gap: 12px; }
    .opt { color: var(--muted); font-size: 12px; display: inline-flex;
           gap: 6px; align-items: center; }
    .opt.checkbox { gap: 4px; cursor: pointer; }
    .mono { font-family: monospace; }
    .schema-h { margin: 14px 0 6px; font-size: 13px; color: #e2b93d; }
    .entity-row { display: flex; align-items: center; justify-content: space-between;
                  gap: 8px; border: 1px solid var(--line); border-radius: 6px;
                  padding: 6px 10px; margin: 4px 0; background: var(--bg); }
    .entity-main { display: flex; align-items: baseline; gap: 10px; min-width: 0; }
    .entity-main strong { font-family: monospace; }
    .conflict { border: 1px solid var(--line); border-left: 3px solid #e2b93d;
                border-radius: 6px; padding: 8px 10px; background: var(--bg); }
    .conflict-line { font-family: monospace; font-size: 12px; }
    .conflict ul, .card-body ul { margin: 6px 0 0; padding-left: 20px;
                                  font-size: 12px; color: var(--muted); }
    select { background: var(--bg); border: 1px solid var(--line);
             border-radius: 6px; color: var(--text); padding: 3px 6px;
             font-size: 12px; }
    select:hover { border-color: var(--accent); }
    .status { color: var(--muted); font-size: 12px; min-height: 1.1em;
              font-family: monospace; white-space: pre-wrap; }
"#;
