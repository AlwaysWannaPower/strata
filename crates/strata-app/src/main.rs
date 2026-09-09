//! # strata-app — the Strata desktop shell (Dioxus 0.7, WebView)
//!
//! This binary is deliberately thin: every data operation lives in
//! [`strata_core`]. The UI only renders plain data structures and forwards
//! user intent.
//!
//! ## The shell (M1c)
//!
//! ```text
//! ┌ topbar ──────────────┐   brand + current workspace
//! ├ content ─────────────┤   the single Pipeline hub (pipeline_home.rs)
//! ├ statusbar ───────────┤   global status text · CPU/RAM/data meters (right)
//! └──────────────────────┘
//! ```
//!
//! There are no disconnected tabs anymore: everything happens in the context
//! of the open workspace on the Pipeline screen. That is why this file owns
//! the **global state** (the workspace must survive anything, see
//! [`workspace_state`]):
//!
//! * `ws` — the open workspace (dir + config). Lives here so no screen can
//!   lose it by unmounting; the last workspace is restored at startup.
//! * `status` — one global status line (rendered at the bottom-left).
//! * `refresh` — a tick bumped after every disk/config change; screens that
//!   project disk state onto the UI re-read when it changes.
//! * `sample` — the resource meter snapshot, refreshed once per second by a
//!   background thread ([`monitor`]) through a Dioxus coroutine.
//!
//! ## Layout of this crate (read top-down)
//!
//! ```text
//! src/
//! ├── main.rs            — App frame, topbar, status bar, global state, CSS
//! ├── pipeline_home.rs   — the Pipeline hub: entity cards, mode-B scan,
//! │                        candidate inspector
//! ├── entity_detail.rs   — per-entity Sources / Schema / Stage panel
//! ├── actions.rs         — engine operations shared by the UI (scan/confirm/
//! │                        stage/workspace open) over the shared signals
//! ├── workspace_state.rs — WsCtx wrapper (create/open/reload)
//! ├── recent.rs          — last-used workspace persistence
//! ├── monitor.rs         — process CPU/RAM sampling + data-dir size
//! ├── preview.rs         — presentational tables (preview grid, name/type)
//! └── util.rs            — reader-option mapping, byte formatting, dialogs
//! ```

mod actions;
mod entity_detail;
mod monitor;
mod pipeline_home;
pub(crate) mod preview;
mod recent;
mod util;
mod workspace_state;

use dioxus::prelude::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use monitor::Sample;
use pipeline_home::PipelineHome;

/// Application entry point. `LaunchBuilder::desktop()` picks the desktop
/// renderer (only the `desktop` feature is enabled, so there is exactly one).
fn main() {
    dioxus::LaunchBuilder::desktop().launch(App);
}

/// Root component: brand header + the pipeline hub + the status bar.
#[component]
fn App() -> Element {
    // --- global state (see module docs: survives every navigation) ---------
    // Start with the most recent workspace, if there is one.
    let ws = use_signal(workspace_state::restore_last);
    let mut status = use_signal(String::new);
    let refresh = use_signal(|| 0u64);
    // The workspace data dir the sampler should measure (Arc so the sampler
    // thread and an effect can share it without locking Dioxus state).
    let data_dir_ctl = use_signal(|| Arc::new(Mutex::new(None::<PathBuf>)));

    // --- one-time greeting when a workspace was restored at startup --------
    let mut greeted = use_signal(|| false);
    use_effect(move || {
        if *greeted.read() {
            return;
        }
        greeted.set(true);
        if let Some(current) = ws.read().clone() {
            status.set(format!(
                "restored last workspace: {} ({})",
                current.config.name,
                current.dir.display()
            ));
        }
    });

    // --- keep the sampler's target data dir in sync with the workspace -----
    let ctl_arc = data_dir_ctl.read().clone();
    use_effect(move || {
        let dir = ws.read().clone().map(|w| w.data_dir());
        if let Ok(mut guard) = ctl_arc.lock() {
            *guard = dir;
        }
    });

    // --- the sampler itself -------------------------------------------------
    // One std thread measures every second and sends [Sample]s into this
    // coroutine; the coroutine pushes them into the `sample` signal. No async
    // timers, no tokio — just a plain thread + a channel (see monitor.rs).
    let mut sample = use_signal(Sample::default);
    let sampler_rx = use_coroutine(
        move |mut rx: dioxus::prelude::UnboundedReceiver<Sample>| async move {
            // futures-channel receivers yield Result (Err = channel closed).
            while let Ok(next) = rx.recv().await {
                sample.set(next);
            }
        },
    );
    let sampler_tx = sampler_rx.tx();
    let sampler_ctl = data_dir_ctl.read().clone();
    use_effect(move || {
        // Clones: the effect body may run more than once (FnMut), so the
        // values moved into the thread must be per-run copies.
        let tx = sampler_tx.clone();
        let ctl = sampler_ctl.clone();
        std::thread::spawn(move || {
            let mut sampler = monitor::Sampler::new();
            loop {
                std::thread::sleep(monitor::TICK);
                let dir = ctl.lock().map(|guard| guard.clone()).unwrap_or(None);
                let snapshot = sampler.sample(dir.as_deref());
                if tx.unbounded_send(snapshot).is_err() {
                    break; // coroutine dropped (app closing)
                }
            }
        });
    });
    // Silence "unused" if the receiver handle is never read below.
    let _ = &sampler_rx;

    // --- snapshots for the topbar / status bar ------------------------------
    let ws_snapshot = ws.read().clone();
    let ws_name = ws_snapshot
        .as_ref()
        .map(|w| w.config.name.clone())
        .unwrap_or_default();
    let ws_dir_display = ws_snapshot
        .as_ref()
        .map(|w| w.dir.display().to_string())
        .unwrap_or_default();
    let sample_now = *sample.read();
    let status_text = status.read().clone();

    rsx! {
        document::Style { "{CSS}" }

        div { class: "app",
            header { class: "topbar",
                div { class: "brand",
                    h1 { "Strata" }
                    span { class: "tagline", "Data Engineering Workbench" }
                }
                if ws_snapshot.is_some() {
                    div { class: "ws-chip",
                        span { class: "ws-name", "{ws_name}" }
                        span { class: "ws-path", "{ws_dir_display}" }
                    }
                } else {
                    span { class: "layers", "no workspace · open one on the left" }
                }
            }

            main { class: "content",
                // The single pipeline hub — see pipeline_home.rs.
                PipelineHome {
                    ws: ws,
                    status: status,
                    refresh: refresh,
                }
            }

            StatusBar {
                text: status_text,
                sample: sample_now,
                has_workspace: ws_snapshot.is_some(),
                workspace_name: ws_name,
            }
        }
    }
}

/// Props of [`StatusBar`]: status text + the latest resource sample.
#[derive(Props, Clone, PartialEq)]
struct StatusBarProps {
    /// Global status line (last action) — shown on the left.
    text: String,
    /// The latest resource snapshot (updates once per second).
    sample: Sample,
    /// Whether a workspace is open (the data-size meter depends on it).
    has_workspace: bool,
    /// Workspace name for the trailing hint.
    workspace_name: String,
}

/// The bottom full-width status bar: status on the left, resource meters on
/// the right ("тулбар справа внизу").
#[component]
fn StatusBar(props: StatusBarProps) -> Element {
    // Format the meters once per sample (plain Rust, no state).
    let cpu = props
        .sample
        .cpu_pct
        .map(|pct| format!("CPU {pct:.0}%"))
        .unwrap_or_else(|| String::from("CPU —"));
    let ram = match (props.sample.rss_bytes, props.sample.mem_total_bytes) {
        (Some(rss), Some(total)) => {
            let mb = rss as f64 / 1_048_576.0;
            let share = if total > 0 {
                (rss as f64 * 100.0) / total as f64
            } else {
                0.0
            };
            format!("RAM {mb:.0} MB · {share:.1}%")
        }
        _ => String::from("RAM —"),
    };
    let data = if props.has_workspace {
        props
            .sample
            .data_bytes
            .map(|bytes| format!("data {}", util::format_bytes(bytes)))
            .unwrap_or_else(|| String::from("data …"))
    } else {
        String::from("data —")
    };
    let ws_hint = if props.has_workspace {
        props.workspace_name.clone()
    } else {
        String::from("no workspace")
    };

    rsx! {
        footer { class: "statusbar",
            div { class: "status-text", "{props.text}" }
            div { class: "metrics",
                span { class: "metric", "{cpu}" }
                span { class: "metric", "{ram}" }
                span { class: "metric", "{data}" }
                span { class: "metric ws", "{ws_hint}" }
            }
        }
    }
}

/// The whole application stylesheet.
///
/// Dioxus desktop renders into a WebView, so this is regular CSS. All class
/// names used across the crate are defined here — one place to tweak the look.
const CSS: &str = r#"
    :root {
        --bg: #0d1117; --panel: #141b23; --panel2: #1b2430; --line: #26303c;
        --text: #dbe2ea; --muted: #8b98a5; --accent: #4da3ff;
        --ok: #4ecb8d; --warn: #e2b93d; --bad: #ef6a6a;
    }
    * { box-sizing: border-box; }
    body { margin: 0; background: var(--bg); color: var(--text);
           font-family: system-ui, sans-serif; font-size: 14px; }

    /* ---- app frame ------------------------------------------------------ */
    .app { display: flex; flex-direction: column; height: 100vh; }
    .topbar { display: flex; align-items: center; justify-content: space-between;
              gap: 12px; padding: 8px 16px; border-bottom: 1px solid var(--line);
              background: var(--panel); }
    .brand { display: flex; align-items: baseline; gap: 10px; min-width: 0; }
    .brand h1 { margin: 0; font-size: 17px; letter-spacing: 0.3px; }
    .tagline { color: var(--muted); font-size: 12px; }
    .layers { color: var(--muted); font-size: 12px; font-family: monospace; }

    .ws-chip { display: flex; align-items: baseline; gap: 10px; min-width: 0;
               background: var(--panel2); border: 1px solid var(--line);
               border-radius: 8px; padding: 4px 12px; }
    .ws-name { color: var(--accent); font-weight: 600; white-space: nowrap; }
    .ws-path { color: var(--muted); font-family: monospace; font-size: 12px;
               overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

    /* ---- content ---------------------------------------------------------- */
    .content { flex: 1; overflow-y: auto; padding: 16px 20px; }
    .screen { display: flex; flex-direction: column; gap: 12px;
              max-width: 1180px; margin: 0 auto; }
    .screen-title { margin: 0; font-size: 20px; }
    .screen-sub { margin: 0; color: var(--muted); max-width: 820px;
                  line-height: 1.5; }

    /* ---- cards ------------------------------------------------------------ */
    .card { background: var(--panel); border: 1px solid var(--line);
            border-radius: 10px; overflow: hidden; }
    .card-head { display: flex; align-items: center; gap: 8px;
                 padding: 8px 12px; border-bottom: 1px solid var(--line);
                 background: var(--panel2); }
    .card-title { font-weight: 600; font-size: 12px; text-transform: uppercase;
                  letter-spacing: 0.5px; color: var(--muted); }
    .card-badge { font-family: monospace; font-size: 11px; color: var(--accent);
                  border: 1px solid var(--line); border-radius: 4px;
                  padding: 1px 6px; overflow: hidden; text-overflow: ellipsis;
                  white-space: nowrap; max-width: 60%; }
    .card-body { display: flex; flex-direction: column; gap: 10px; padding: 12px; }
    .provenance { font-family: monospace; font-size: 12px; color: var(--accent); }

    .welcome h3 { margin: 0 0 4px; }
    .ws-stats { gap: 18px; }

    /* ---- section headers --------------------------------------------------- */
    .section-head { margin-top: 6px; }
    .schema-h { margin: 10px 0 4px; font-size: 13px; color: #e2b93d;
                text-transform: uppercase; letter-spacing: 0.4px; }

    /* ---- toolbar/inputs ----------------------------------------------------- */
    .toolbar { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
    .toolbar.opts { gap: 14px; }
    .opt { color: var(--muted); font-size: 12px; display: inline-flex;
           gap: 6px; align-items: center; }
    .opt.checkbox { gap: 4px; cursor: pointer; }
    .hints { margin-top: -2px; }
    .hint { color: var(--muted); font-size: 12px; }
    .mono { font-family: monospace; }
    .path-input { flex: 1; min-width: 260px; background: var(--bg);
                  border: 1px solid var(--line); border-radius: 6px;
                  color: var(--text); padding: 6px 9px; font-family: monospace; }
    .path-input:focus { outline: none; border-color: var(--accent); }
    .mini-input { background: var(--bg); border: 1px solid var(--line);
                  border-radius: 6px; color: var(--text); padding: 3px 8px;
                  font-size: 12px; font-family: monospace; }
    button { background: var(--panel2); border: 1px solid var(--line);
             border-radius: 6px; color: var(--text); padding: 6px 12px;
             cursor: pointer; font-size: 13px; }
    button:hover { border-color: var(--accent); }
    .mini-btn { padding: 3px 10px; font-size: 12px; }
    select { background: var(--bg); border: 1px solid var(--line);
             border-radius: 6px; color: var(--text); padding: 3px 6px;
             font-size: 12px; }
    select:hover { border-color: var(--accent); }

    /* ---- entity grid / cards ------------------------------------------------ */
    .entity-grid { display: grid; grid-template-columns: repeat(auto-fill,
                   minmax(340px, 1fr)); gap: 12px; }
    .entity-card { display: flex; flex-direction: column; gap: 10px;
                   background: var(--panel); border: 1px solid var(--line);
                   border-radius: 10px; padding: 12px; }
    .entity-card:hover { border-color: var(--accent); }
    .entity-card.selected { border-color: var(--accent);
                            box-shadow: 0 0 0 1px var(--accent) inset; }
    .entity-main { display: flex; align-items: baseline; gap: 10px; min-width: 0; }
    .entity-main strong { font-family: monospace; font-size: 15px; }
    .entity-name { color: var(--text); }
    .entity-folder { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .entity-actions { justify-content: flex-end; }

    .entity-row { display: flex; align-items: center; justify-content: space-between;
                  gap: 8px; border: 1px solid var(--line); border-radius: 6px;
                  padding: 6px 10px; margin: 4px 0; background: var(--bg); }

    /* ---- stage chips ---------------------------------------------------------- */
    .stage-chips { display: flex; gap: 6px; flex-wrap: wrap; }
    .chip { display: inline-flex; flex-direction: column; gap: 1px;
            border: 1px solid var(--line); border-radius: 6px;
            padding: 3px 8px; background: var(--bg); min-width: 76px; }
    .chip-label { font-size: 10px; text-transform: uppercase;
                  letter-spacing: 0.5px; color: var(--muted); }
    .chip-note { font-family: monospace; font-size: 12px; color: var(--text); }
    .chip-ok { border-color: #2a5c42; }
    .chip-ok .chip-note { color: var(--ok); }
    .chip-todo { border-color: var(--line); }
    .chip-todo .chip-note { color: var(--warn); }
    .chip-off { opacity: 0.55; }

    /* ---- entity detail ---------------------------------------------------------- */
    .detail-bar { margin-top: 14px; }
    .detail-tabs { display: flex; gap: 0; border-bottom: 1px solid var(--line);
                   background: var(--panel); padding: 0 8px; }
    .tab-btn { border: none; border-bottom: 2px solid transparent;
               border-radius: 0; background: transparent; color: var(--muted);
               padding: 8px 14px; }
    .tab-btn:hover { color: var(--text); border-color: var(--line); }
    .tab-btn.active { color: var(--accent); border-bottom-color: var(--accent); }
    .detail-body { padding-top: 10px; }
    .detail-card .card-head { justify-content: space-between; }

    /* ---- inspector ----------------------------------------------------------- */
    .inspector { border: 1px solid var(--accent); margin-top: 8px; }

    /* ---- conflicts ----------------------------------------------------------- */
    .conflict { border: 1px solid var(--line); border-left: 3px solid var(--warn);
                border-radius: 6px; padding: 8px 10px; background: var(--bg); }
    .conflict-line { font-family: monospace; font-size: 12px; }
    .conflict ul, .card-body ul { margin: 6px 0 0; padding-left: 20px;
                                  font-size: 12px; color: var(--muted); }

    /* ---- tables --------------------------------------------------------------- */
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

    /* ---- bottom status bar ------------------------------------------------------ */
    .statusbar { display: flex; align-items: center; justify-content: space-between;
                 gap: 12px; border-top: 1px solid var(--line);
                 background: var(--panel); padding: 4px 14px;
                 font-family: monospace; font-size: 12px;
                 color: var(--muted); min-height: 26px; }
    .status-text { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .metrics { display: flex; gap: 16px; align-items: center; white-space: nowrap;
               flex-shrink: 0; }
    .metric { color: var(--text); }
    .metric.ws { color: var(--accent); max-width: 260px; overflow: hidden;
                 text-overflow: ellipsis; }
"#;
