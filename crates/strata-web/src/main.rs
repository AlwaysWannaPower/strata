//! # strata-web — the Strata web service (axum + htmx + Tailwind)
//!
//! We moved off the Dioxus desktop shell: server-rendered HTML with **htmx**
//! gives us the same pipeline UI with far less client code, and it runs as a
//! service (Docker, `docker compose up`) instead of a per-machine app.
//!
//! ## Layering (the important part)
//!
//! ```text
//!  templates/*.html  ──►  handlers (this file)  ──►  strata_core::api  ──►  engine
//!      ▲                        │
//!      └──── htmx swaps ◄───────┘  (HTML fragments, no JSON, no client state)
//! ```
//!
//! * Handlers are **thin**: parse a form, call one `strata_core::api` function,
//!   render a fragment. They never touch Polars, Parquet or `workspace.toml`.
//! * htmx does the "reactivity": a form posts, the server returns a fragment,
//!   htmx swaps it into the page. No client-side state machine at all.
//! * Two hard rules live here as middleware-ish helpers:
//!   1. **`source_roots` allowlist** — a web user may only point us at folders
//!      the operator allowed (`STRATA_SOURCE_ROOTS`), never at arbitrary paths
//!      of the server (that would be a file-disclosure hole).
//!   2. **workspace resolution** — every workspace route resolves a slug under
//!      `STRATA_WORKSPACE_ROOT`, again with the "must stay inside" check.
//!
//! ## Why htmx instead of Dioxus (short version)
//!
//! Long staging runs, tables, forms and status panels are server-side data with
//! small UI deltas — exactly htmx's sweet spot. The one thing htmx does *not*
//! give us is a virtualized million-row grid; that is a later, opt-in island of
//! JavaScript (AG Grid/ TanStack) talking to a paginated endpoint.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use askama::Template;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use sysinfo::{Pid, ProcessesToUpdate, System};

use strata_core::api;

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

/// Shared, immutable service state. `Arc` so handlers can clone it cheaply;
/// everything in it is read-only, which keeps the web layer free of locks.
#[derive(Clone)]
struct AppState {
    /// Where workspaces live (`workspace.toml`, `schemas/`, `data/` per slug).
    workspace_root: PathBuf,
    /// Folders the service is allowed to read data from (allowlist).
    source_roots: Vec<PathBuf>,
    /// Process start time, for the "uptime" chip.
    started: Instant,
    /// Background staging jobs: id → (entity, state). Entries are removed as
    /// soon as the job finishes and its final fragment has been served, so the
    /// map stays tiny (a handful of in-flight runs).
    jobs: Arc<Mutex<HashMap<String, Arc<Mutex<JobEntry>>>>>,
    /// Monotonic job id source (`j1`, `j2`, …).
    job_seq: Arc<AtomicU64>,
    /// Shared resource sampler (`sysinfo`). Kept in a `Mutex` because CPU%
    /// needs two refreshes separated by time, so the instance must persist
    /// between requests. This is the only lock in the service and it is held
    /// for microseconds (no I/O inside).
    system: Arc<Mutex<System>>,
}

impl AppState {
    /// Build state from environment variables.
    ///
    /// * `STRATA_WORKSPACE_ROOT` — workspace storage (default `./workspaces`)
    /// * `STRATA_SOURCE_ROOTS` — `:`-separated allowlist (default: workspace root)
    /// * `STRATA_ADDR` — listen address (default `0.0.0.0:8080`)
    fn from_env() -> (Self, SocketAddr) {
        let workspace_root = std::env::var("STRATA_WORKSPACE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./workspaces"));
        let source_roots: Vec<PathBuf> = std::env::var("STRATA_SOURCE_ROOTS")
            .map(|value| value.split(':').map(PathBuf::from).collect())
            .unwrap_or_else(|_| vec![workspace_root.clone()]);
        let addr: SocketAddr = std::env::var("STRATA_ADDR")
            .unwrap_or_else(|_| String::from("0.0.0.0:8080"))
            .parse()
            .unwrap_or_else(|_| "0.0.0.0:8080".parse().expect("valid fallback addr"));

        (
            AppState {
                workspace_root,
                source_roots,
                started: Instant::now(),
                jobs: Arc::new(Mutex::new(HashMap::new())),
                job_seq: Arc::new(AtomicU64::new(1)),
                system: Arc::new(Mutex::new(System::new())),
            },
            addr,
        )
    }

    /// Resolve a workspace slug to its directory, refusing path escapes.
    fn workspace_dir(&self, slug: &str) -> Result<PathBuf, WebError> {
        // Slugs come from the URL; validate before touching the file system.
        let ok = !slug.is_empty()
            && slug.len() <= 64
            && slug
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.');
        if !ok || slug.contains("..") {
            return Err(WebError::bad_request("invalid workspace name"));
        }
        Ok(self.workspace_root.join(slug))
    }
}

// ---------------------------------------------------------------------------
// Error handling: one small type → HTTP status + human message
// ---------------------------------------------------------------------------

/// A web-layer error. Engine errors arrive as [`api::ApiError`] and become a
/// 400 with a readable message (they are user-caused: bad folder, bad schema).
#[derive(Debug)]
struct WebError {
    status: StatusCode,
    message: String,
}

impl WebError {
    fn bad_request(message: impl Into<String>) -> Self {
        WebError {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        WebError {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        WebError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl From<api::ApiError> for WebError {
    fn from(error: api::ApiError) -> Self {
        WebError::bad_request(error.message())
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        // Plain text is enough: htmx shows it inside the swapped fragment
        // target, and `curl` users get a readable reason.
        (self.status, self.message).into_response()
    }
}

type WebResult<T> = Result<T, WebError>;

// ---------------------------------------------------------------------------
// Background jobs: staging runs out-of-band, the browser polls a fragment
// ---------------------------------------------------------------------------

/// Which entity a job belongs to (needed to render its final fragment).
#[derive(Debug)]
struct JobEntry {
    entity: String,
    state: JobState,
}

/// Lifecycle of one staging run.
#[derive(Debug)]
enum JobState {
    /// Work in progress: `total == 0` means "not counted yet".
    Running { done: usize, total: usize },
    /// Finished successfully; the outcome carries the report.
    Done(Box<api::StageOutcome>),
    /// Finished with an engine/user error.
    Failed(String),
}

/// Fragment: progress of a running job. It **polls itself** (`hx-get` pointing
/// at the job endpoint, `hx-trigger="every 1s"`, `hx-swap="outerHTML"`), so the
/// page keeps updating without a single line of JavaScript.
#[derive(Template)]
#[template(path = "fragments/job_running.html")]
struct JobRunningFragment {
    slug: String,
    job_id: String,
    entity: String,
    done: usize,
    total: usize,
    percent: usize,
}

/// Ensure `candidate` is inside one of the allowed roots (after symlink-free
/// canonicalization). This is the single guard against "read any file on the
/// server" requests.
fn ensure_allowed(state: &AppState, candidate: &Path) -> WebResult<PathBuf> {
    let canonical = candidate
        .canonicalize()
        .map_err(|_| WebError::bad_request(format!("path not found: {}", candidate.display())))?;

    for root in &state.source_roots {
        if let Ok(root_canonical) = root.canonicalize() {
            if canonical.starts_with(&root_canonical) {
                return Ok(canonical);
            }
        }
    }
    Err(WebError::bad_request(format!(
        "path {} is outside the allowed source roots",
        candidate.display()
    )))
}

/// Resolve a workspace slug to an existing workspace directory.
///
/// Missing workspace → 404 (a route-level concern), while *invalid* slugs are
/// rejected earlier by [`AppState::workspace_dir`].
fn require_workspace(state: &AppState, slug: &str) -> WebResult<PathBuf> {
    let dir = state.workspace_dir(slug)?;
    if !dir.join("workspace.toml").exists() {
        return Err(WebError::not_found(format!("workspace '{slug}' not found")));
    }
    api::open_workspace_at(&dir)?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// Templates (askama compiles these into the binary — no runtime files needed)
// ---------------------------------------------------------------------------

/// One row of the workspace list (pre-formatted strings keep the template dumb).
struct WsRow {
    slug: String,
    name: String,
    data_dir: String,
}

/// Index page: workspace list + create form.
#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    workspaces: Vec<WsRow>,
    workspace_root: String,
    source_roots: Vec<String>,
}

/// Workspace "pipeline hub": entities, scan form, stage buttons.
#[derive(Template)]
#[template(path = "hub.html")]
struct HubTemplate {
    slug: String,
    name: String,
    data_dir: String,
    entities: Vec<api::EntityInfo>,
    uptime_secs: u64,
}

/// Fragment: entity cards (also returned after confirm/stage actions).
#[derive(Template)]
#[template(path = "fragments/entities.html")]
struct EntitiesFragment {
    slug: String,
    entities: Vec<api::EntityInfo>,
}

/// Fragment: scan candidates with "confirm & bind" forms.
#[derive(Template)]
#[template(path = "fragments/candidates.html")]
struct CandidatesFragment {
    slug: String,
    candidates: Vec<api::CandidateInfo>,
}

/// Fragment: result of a staging run.
#[derive(Template)]
#[template(path = "fragments/stage.html")]
struct StageFragment {
    entity: String,
    outcome: api::StageOutcome,
}

/// Fragment: a red error box (used by every htmx action on failure).
#[derive(Template)]
#[template(path = "fragments/error.html")]
struct ErrorFragment {
    message: String,
}

// ---------------------------------------------------------------------------
// Form payloads (axum's Form extractor + serde)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateWorkspaceForm {
    name: String,
}

#[derive(Deserialize)]
struct ScanForm {
    root: String,
}

#[derive(Deserialize)]
struct EntityForm {
    entity: String,
    folder: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /` — the workspace list.
async fn index(State(state): State<AppState>) -> WebResult<Html<String>> {
    let workspaces = api::list_workspaces(&state.workspace_root)?
        .into_iter()
        .map(|ws| WsRow {
            slug: ws
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            name: ws.name,
            data_dir: ws.data_dir.display().to_string(),
        })
        .collect();
    let page = IndexTemplate {
        workspaces,
        workspace_root: state.workspace_root.display().to_string(),
        source_roots: state
            .source_roots
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
    };
    Ok(Html(page.render().map_err(render_error)?))
}

/// `POST /workspaces` — create a workspace, then redirect to its hub.
async fn create_workspace(
    State(state): State<AppState>,
    Form(form): Form<CreateWorkspaceForm>,
) -> WebResult<Redirect> {
    let created = api::create_workspace_in(&state.workspace_root, &form.name)?;
    let slug = created
        .dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("workspace"));
    Ok(Redirect::to(&format!("/w/{slug}")))
}

/// `GET /w/{slug}` — the pipeline hub of one workspace.
async fn hub(
    State(state): State<AppState>,
    AxumPath(slug): AxumPath<String>,
) -> WebResult<Html<String>> {
    let dir = require_workspace(&state, &slug)?;
    let info = api::open_workspace_at(&dir)?;
    let page = HubTemplate {
        slug,
        name: info.name,
        data_dir: info.data_dir.display().to_string(),
        entities: api::entities(&dir)?,
        uptime_secs: state.started.elapsed().as_secs(),
    };
    Ok(Html(page.render().map_err(render_error)?))
}

/// `POST /w/{slug}/scan` — inspect a root's direct subfolders (mode B).
///
/// Returns the *candidates fragment*; nothing is written to the workspace
/// (proposal stage — the user still has to confirm each entity).
async fn scan_root(
    State(state): State<AppState>,
    AxumPath(slug): AxumPath<String>,
    Form(form): Form<ScanForm>,
) -> WebResult<Html<String>> {
    // The workspace must exist; the scan itself only needs the root path.
    let _dir = require_workspace(&state, &slug)?;
    let root = ensure_allowed(&state, Path::new(form.root.trim()))?;
    let candidates = api::scan_candidates(&root)?;
    let fragment = CandidatesFragment { slug, candidates };
    Ok(Html(fragment.render().map_err(render_error)?))
}

/// `POST /w/{slug}/entities` — confirm a candidate: schema + binding.
async fn add_entity(
    State(state): State<AppState>,
    AxumPath(slug): AxumPath<String>,
    Form(form): Form<EntityForm>,
) -> WebResult<Html<String>> {
    let dir = require_workspace(&state, &slug)?;
    let folder = ensure_allowed(&state, Path::new(form.folder.trim()))?;

    // Any engine refusal (empty folder, unreadable files) is surfaced to the
    // user as an htmx-swappable error fragment instead of a bare 400 page.
    if let Err(error) = api::confirm_entity(&dir, &form.entity, &folder) {
        return Ok(Html(
            ErrorFragment {
                message: error.message().to_string(),
            }
            .render()
            .map_err(render_error)?,
        ));
    }
    let fragment = EntitiesFragment {
        slug,
        entities: api::entities(&dir)?,
    };
    Ok(Html(fragment.render().map_err(render_error)?))
}

/// `POST /w/{slug}/entities/{entity}/stage` — start staging in the background.
///
/// Staging is CPU- and IO-bound (Parquet writing, type checks), so it must not
/// block the request thread: we spawn it on a blocking task and immediately
/// return a *progress fragment* that polls the job endpoint. The browser sees
/// the bar move; the server stays responsive for other users.
async fn stage_entity(
    State(state): State<AppState>,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
) -> WebResult<Html<String>> {
    let dir = require_workspace(&state, &slug)?;

    let job_id = format!("j{}", state.job_seq.fetch_add(1, Ordering::Relaxed));
    let entry = Arc::new(Mutex::new(JobEntry {
        entity: entity.clone(),
        state: JobState::Running { done: 0, total: 0 },
    }));
    state
        .jobs
        .lock()
        .map_err(|_| WebError::internal("job registry poisoned"))?
        .insert(job_id.clone(), entry.clone());

    // The engine call runs on a blocking thread; progress is pushed into the
    // shared job state, which the polling endpoint reads.
    let task_entry = entry.clone();
    let task_dir = dir.clone();
    let task_entity = entity.clone();
    tokio::task::spawn_blocking(move || {
        let result = api::stage_entity_with_progress(&task_dir, &task_entity, |done, total| {
            if let Ok(mut guard) = task_entry.lock() {
                guard.state = JobState::Running { done, total };
            }
        });
        if let Ok(mut guard) = task_entry.lock() {
            guard.state = match result {
                Ok(outcome) => JobState::Done(Box::new(outcome)),
                Err(error) => JobState::Failed(error.message().to_string()),
            };
        }
    });

    let fragment = JobRunningFragment {
        slug,
        job_id,
        entity,
        done: 0,
        total: 0,
        percent: 0,
    };
    Ok(Html(fragment.render().map_err(render_error)?))
}

/// `GET /w/{slug}/jobs/{job_id}` — progress (running) or the final result.
///
/// htmx swaps this fragment into `#stage`. Running fragments keep polling;
/// final fragments carry no polling attributes, so the loop stops by itself.
/// Finished jobs are removed from the registry right after their result is
/// served.
async fn job_status(
    State(state): State<AppState>,
    AxumPath((slug, job_id)): AxumPath<(String, String)>,
) -> WebResult<Html<String>> {
    let _dir = require_workspace(&state, &slug)?;

    let entry = {
        let jobs = state
            .jobs
            .lock()
            .map_err(|_| WebError::internal("job registry poisoned"))?;
        jobs.get(&job_id).cloned()
    };
    let Some(entry) = entry else {
        return Err(WebError::not_found(format!("job '{job_id}' not found")));
    };

    // Snapshot under the lock, then render outside it (never render holding a
    // lock: a template error must not poison shared state).
    enum Snapshot {
        Running { done: usize, total: usize },
        Final(JobState),
    }
    let (entity, snapshot) = {
        let mut guard = entry
            .lock()
            .map_err(|_| WebError::internal("job state poisoned"))?;
        let entity = guard.entity.clone();
        let snapshot = match &guard.state {
            JobState::Running { done, total } => Snapshot::Running {
                done: *done,
                total: *total,
            },
            // Final states are consumed exactly once: the polling loop ends
            // with this response, so there is nothing left to read afterwards.
            JobState::Done(_) | JobState::Failed(_) => {
                let taken = std::mem::replace(
                    &mut guard.state,
                    JobState::Failed(String::from("job result already served")),
                );
                Snapshot::Final(taken)
            }
        };
        (entity, snapshot)
    };

    let (html, is_final) = match snapshot {
        Snapshot::Running { done, total } => {
            let percent = if total == 0 {
                0
            } else {
                (done * 100 / total).min(100)
            };
            let fragment = JobRunningFragment {
                slug,
                job_id: job_id.clone(),
                entity,
                done,
                total,
                percent,
            };
            (fragment.render().map_err(render_error)?, false)
        }
        Snapshot::Final(JobState::Done(outcome)) => {
            let fragment = StageFragment {
                entity,
                outcome: *outcome,
            };
            (fragment.render().map_err(render_error)?, true)
        }
        Snapshot::Final(JobState::Failed(message)) => (
            ErrorFragment { message }.render().map_err(render_error)?,
            true,
        ),
        Snapshot::Final(JobState::Running { .. }) => unreachable!("running is not final"),
    };

    // Finished → drop the job from the registry (browser stops polling).
    if is_final {
        if let Ok(mut jobs) = state.jobs.lock() {
            jobs.remove(&job_id);
        }
    }

    Ok(Html(html))
}

// ---------------------------------------------------------------------------
// Resource metrics: "how much is the service eating?"
// ---------------------------------------------------------------------------

/// One resource snapshot for the status widget and `/metrics`.
#[derive(Clone, Copy)]
struct ResourceSample {
    /// Resident set size of this process (bytes) — the "RAM the app uses".
    rss_bytes: Option<u64>,
    /// Process CPU usage in percent. The first sample after start is 0.0:
    /// `sysinfo` needs two refreshes separated by time to compute a delta.
    cpu_pct: Option<f32>,
    /// Total physical memory of the host (bytes).
    mem_total_bytes: u64,
    /// Used physical memory of the host (bytes).
    mem_used_bytes: u64,
    /// Workspaces currently present.
    workspaces: usize,
}

/// Take a fresh sample. Cheap: one process refresh + one memory refresh.
fn sample_resources(state: &AppState) -> ResourceSample {
    let pid = Pid::from_u32(std::process::id());
    let workspaces = api::list_workspaces(&state.workspace_root)
        .map(|w| w.len())
        .unwrap_or(0);

    let mut system = match state.system.lock() {
        Ok(guard) => guard,
        // A poisoned mutex must not take the whole service down: report
        // "unknown" metrics instead.
        Err(_) => {
            return ResourceSample {
                rss_bytes: None,
                cpu_pct: None,
                mem_total_bytes: 0,
                mem_used_bytes: 0,
                workspaces,
            };
        }
    };

    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.refresh_memory();

    let process = system.process(pid);
    ResourceSample {
        rss_bytes: process.map(|p| p.memory()),
        cpu_pct: process.map(|p| p.cpu_usage()),
        mem_total_bytes: system.total_memory(),
        mem_used_bytes: system.used_memory(),
        workspaces,
    }
}

/// Human byte formatting for the widget (`123 MB`, `1.4 GB`).
fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.2} GB", value / GB)
    } else if value >= MB {
        format!("{:.0} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Fragment: the little live resource readout (htmx polls it every 2s).
#[derive(Template)]
#[template(path = "fragments/resources.html")]
struct ResourcesFragment {
    rss: String,
    cpu: String,
    host_mem: String,
    workspaces: usize,
}

async fn resources_fragment(State(state): State<AppState>) -> WebResult<Html<String>> {
    let sample = sample_resources(&state);
    let fragment = ResourcesFragment {
        rss: sample
            .rss_bytes
            .map(human_bytes)
            .unwrap_or_else(|| String::from("—")),
        cpu: sample
            .cpu_pct
            .map(|value| format!("{value:.0}%"))
            .unwrap_or_else(|| String::from("—")),
        host_mem: if sample.mem_total_bytes == 0 {
            String::from("—")
        } else {
            format!(
                "{}/{}",
                human_bytes(sample.mem_used_bytes),
                human_bytes(sample.mem_total_bytes)
            )
        },
        workspaces: sample.workspaces,
    };
    Ok(Html(fragment.render().map_err(render_error)?))
}

/// `GET /metrics` — plain-text metrics for Prometheus-style scrapers
/// (and for `curl`, which is how you debug it).
async fn metrics(State(state): State<AppState>) -> String {
    let sample = sample_resources(&state);
    let mut out = String::new();
    out.push_str("# HELP strata_process_rss_bytes Resident memory of the service process\n");
    out.push_str("# TYPE strata_process_rss_bytes gauge\n");
    out.push_str(&format!(
        "strata_process_rss_bytes {}\n",
        sample.rss_bytes.unwrap_or(0)
    ));
    out.push_str("# HELP strata_process_cpu_percent Process CPU usage (needs two samples)\n");
    out.push_str("# TYPE strata_process_cpu_percent gauge\n");
    out.push_str(&format!(
        "strata_process_cpu_percent {:.2}\n",
        sample.cpu_pct.unwrap_or(0.0)
    ));
    out.push_str(&format!(
        "strata_host_memory_total_bytes {}\n",
        sample.mem_total_bytes
    ));
    out.push_str(&format!(
        "strata_host_memory_used_bytes {}\n",
        sample.mem_used_bytes
    ));
    out.push_str(&format!(
        "strata_uptime_seconds {}\n",
        state.started.elapsed().as_secs()
    ));
    out.push_str(&format!("strata_workspaces_total {}\n", sample.workspaces));
    out
}

/// `GET /healthz` — container healthcheck (also handy in `curl`).
async fn healthz(State(state): State<AppState>) -> String {
    format!(
        "ok uptime={}s workspaces={}",
        state.started.elapsed().as_secs(),
        state.workspace_root.display()
    )
}

/// Map an askama render failure into a 500.
fn render_error(error: askama::Error) -> WebError {
    WebError::internal(format!("template error: {error}"))
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "strata_web=info,tower_http=info".into()),
        )
        .init();

    let (state, addr) = AppState::from_env();
    std::fs::create_dir_all(&state.workspace_root).ok();

    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/fragments/resources", get(resources_fragment))
        .route("/workspaces", post(create_workspace))
        .route("/w/{slug}", get(hub))
        .route("/w/{slug}/scan", post(scan_root))
        .route("/w/{slug}/entities", post(add_entity))
        .route("/w/{slug}/entities/{entity}/stage", post(stage_entity))
        .route("/w/{slug}/jobs/{job_id}", get(job_status))
        .with_state(state.clone());

    tracing::info!(
        "strata-web listening on http://{addr} (workspaces: {}, sources: {:?})",
        state.workspace_root.display(),
        state.source_roots
    );

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!("cannot bind {addr}: {error}");
            std::process::exit(1);
        }
    };

    // Graceful shutdown keeps `docker compose down` clean.
    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!("server error: {error}");
    }
}

/// Wait for Ctrl-C / SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Unused import guard for `Arc` (kept for the future shared-state extension:
/// background job registry). Referencing it in a doc-test-free way keeps the
/// dependency explicit without dead-code warnings.
#[allow(dead_code)]
type SharedState = Arc<AppState>;
