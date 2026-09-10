//! # strata-web — веб-сервис Strata (axum + htmx + Tailwind)
//!
//! Dioxus-десктоп удалён (см. docs/archive), перешли на axum + htmx:
//! server-rendered HTML с **htmx** даёт тот же UI пайплайна при куда меньшем
//! объёме клиентского кода и работает как сервис (Docker, `docker compose up`),
//! а не как приложение на каждой машине.
//!
//! ## Слои (самое важное здесь)
//!
//! ```text
//!  templates/*.html  ──►  обработчики (этот файл)  ──►  strata_core::api  ──►  движок
//!      ▲                        │
//!      └──── htmx-подмены ◄─────┘  (HTML-фрагменты, без JSON, без клиентского состояния)
//! ```
//!
//! * Обработчики **тонкие**: разобрать форму, вызвать одну функцию
//!   `strata_core::api`, отрендерить фрагмент. Они никогда не трогают Polars,
//!   Parquet или `workspace.toml`.
//! * «Реактивность» делает htmx: форма отправляется, сервер возвращает фрагмент,
//!   htmx подменяет его на странице. Никакого клиентского автомата состояний.
//! * Два жёстких правила живут прямо здесь, в виде middleware-хелперов:
//!   1. **allowlist `source_roots`** — веб-пользователь может указать только те
//!      папки, что разрешил оператор (`STRATA_SOURCE_ROOTS`), и никогда —
//!      произвольные пути сервера (это была бы дыра на раскрытие файлов).
//!   2. **разрешение воркспейса** — каждый маршрут воркспейса разрешает slug
//!      внутри `STRATA_WORKSPACE_ROOT`, снова с проверкой «должно остаться внутри».
//!
//! ## Почему htmx, а не Dioxus (короткая версия)
//!
//! Долгие запуски staging, таблицы, формы и панели статуса — это серверные
//! данные с небольшими дельтами UI, то есть ровно то, для чего хорош htmx.
//! Единственное, чего htmx *не* даёт — виртуализированную таблицу на миллион
//! строк; это более поздний опциональный островок JavaScript (AG Grid/TanStack),
//! общающийся с постраничным эндпоинтом.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

mod auth;
mod entity;
mod forms;
mod status;

use askama::Template;
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use sqlx::SqlitePool;
use sysinfo::{Pid, ProcessesToUpdate, System};
use tower_sessions::{MemoryStore, Session, SessionManagerLayer};

use strata_core::api;

// ---------------------------------------------------------------------------
// Состояние приложения
// ---------------------------------------------------------------------------

/// Общее неизменяемое состояние сервиса. `Arc`, чтобы обработчики дёшево его
/// клонировали; всё внутри — только для чтения, поэтому веб-слой обходится без
/// блокировок.
#[derive(Clone)]
struct AppState {
    /// Где живут воркспейсы (`workspace.toml`, `schemas/`, `data/` на каждый slug).
    workspace_root: PathBuf,
    /// Папки, из которых сервису разрешено читать данные (allowlist).
    source_roots: Vec<PathBuf>,
    /// База пользователей (SQLite; см. `auth.rs`).
    db: SqlitePool,
    /// Время старта процесса — для чипа «uptime».
    started: Instant,
    /// Фоновые задачи staging: id → (сущность, состояние). Записи удаляются
    /// сразу после того, как задача завершится и её финальный фрагмент будет
    /// отдан, поэтому карта остаётся крошечной (несколько запусков в полёте).
    jobs: Arc<Mutex<HashMap<String, Arc<Mutex<JobEntry>>>>>,
    /// Монотонный источник id задач (`j1`, `j2`, …).
    job_seq: Arc<AtomicU64>,
    /// Черновики контракта (колонки + правила), ещё не подтверждённые в TOML.
    /// Ключ — `user_id:slug:entity` (см. `forms::draft_key`). Это единственное
    /// изменяемое состояние сервиса: клиентского состояния в UI нет вообще,
    /// поэтому всё, что пользователь накликал в таблицах схемы и правил, лежит
    /// здесь, на сервере, и переживает перерисовку htmx-фрагментов.
    drafts: Arc<Mutex<HashMap<String, entity::Draft>>>,
    /// Общий сэмплер ресурсов (`sysinfo`). Держим в `Mutex`, потому что CPU%
    /// требует двух обновлений, разделённых во времени, поэтому экземпляр должен
    /// переживать запросы. Это единственная блокировка в сервисе, и держится она
    /// микросекунды (никакого I/O внутри).
    system: Arc<Mutex<System>>,
}

impl AppState {
    /// Собрать состояние из переменных окружения.
    ///
    /// * `STRATA_WORKSPACE_ROOT` — хранилище воркспейсов (по умолчанию
    ///   `./workspaces`); каждый пользователь получает `workspaces/<user_id>/`.
    /// * `STRATA_SOURCE_ROOTS` — разделённый `:` allowlist (по умолчанию: корень
    ///   воркспейсов)
    /// * `STRATA_ADDR` — адрес прослушивания (по умолчанию `0.0.0.0:8080`)
    /// * `STRATA_DB_URL` — URL SQLite (по умолчанию `sqlite://<root>/strata.db?mode=rwc`)
    async fn from_env() -> Result<(Self, SocketAddr), Box<dyn std::error::Error>> {
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

        // `mode=rwc` = создать файл, если его нет (по умолчанию sqlx — read-only).
        let db_url = std::env::var("STRATA_DB_URL").unwrap_or_else(|_| {
            format!("sqlite://{}/strata.db?mode=rwc", workspace_root.display())
        });
        std::fs::create_dir_all(&workspace_root)?;
        let db = SqlitePool::connect(&db_url).await?;
        auth::init_db(&db).await.map_err(|e| e.0)?;

        Ok((
            AppState {
                workspace_root,
                source_roots,
                db,
                started: Instant::now(),
                jobs: Arc::new(Mutex::new(HashMap::new())),
                job_seq: Arc::new(AtomicU64::new(1)),
                drafts: Arc::new(Mutex::new(HashMap::new())),
                system: Arc::new(Mutex::new(System::new())),
            },
            addr,
        ))
    }

    /// Корень воркспейсов **одного пользователя**: каждая учётная запись владеет
    /// `workspaces/<user_id>/`.
    ///
    /// Мультитенантность здесь намеренно грубая — отдельный каталог на id
    /// пользователя — потому что движок работает с путями, а именно «тюрьма»
    /// для путей и разводит тенантов по-настоящему.
    fn user_root(&self, user_id: i64) -> PathBuf {
        self.workspace_root.join(user_id.to_string())
    }

    /// Разрешить slug воркспейса в его каталог, отказывая в побеге из пути.
    fn workspace_dir(&self, user_id: i64, slug: &str) -> Result<PathBuf, WebError> {
        // Slug приходит из URL; проверяем его до обращения к файловой системе.
        let ok = !slug.is_empty()
            && slug.len() <= 64
            && slug
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.');
        if !ok || slug.contains("..") {
            return Err(WebError::bad_request("invalid workspace name"));
        }
        Ok(self.user_root(user_id).join(slug))
    }
}

// ---------------------------------------------------------------------------
// Обработка ошибок: один маленький тип → HTTP-статус + понятное сообщение
// ---------------------------------------------------------------------------

/// Ошибка веб-слоя. Ошибки движка приходят как [`api::ApiError`] и становятся
/// 400 с читаемым сообщением (их причина — пользователь: плохая папка, плохая схема).
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

    /// Человекочитаемая причина — нужна, когда ошибку показывают как текст
    /// внутри фрагмента (например, отчёт «проверить на образце»).
    fn message(&self) -> &str {
        &self.message
    }
}

impl From<auth::AuthError> for WebError {
    fn from(error: auth::AuthError) -> Self {
        // Проблемы БД/валидации здесь видны пользователю (плохой ввод, занятое
        // имя); настоящие сбои всплывают как 500 в логах.
        WebError::bad_request(error.0)
    }
}

impl From<api::ApiError> for WebError {
    fn from(error: api::ApiError) -> Self {
        WebError::bad_request(error.message())
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        // Обычного текста достаточно: htmx покажет его внутри подменяемого
        // фрагмента, а пользователи `curl` получат читаемую причину.
        (self.status, self.message).into_response()
    }
}

type WebResult<T> = Result<T, WebError>;

// ---------------------------------------------------------------------------
// Фоновые задачи: staging и прогон идут вне запроса, браузер поллит фрагмент
// ---------------------------------------------------------------------------

/// К какой сущности относится задача (нужно, чтобы отрендерить её финальный фрагмент).
#[derive(Debug)]
struct JobEntry {
    entity: String,
    /// Подпись операции для фрагмента прогресса (`staging…`, `run…`).
    label: String,
    state: JobState,
}

/// Итог фоновой задачи — то, что показывает финальный фрагмент.
#[derive(Debug)]
enum JobResult {
    /// Staging (сырой слой: файл → Parquet).
    Stage(Box<api::StageOutcome>),
    /// Прогон (стадия ODS: части ODS, карантин, манифест, лог).
    Run(Box<strata_core::RunOutcome>),
}

/// Жизненный цикл одной фоновой задачи.
#[derive(Debug)]
enum JobState {
    /// Работа идёт: `total == 0` означает «ещё не посчитано».
    Running { done: usize, total: usize },
    /// Завершилась успешно; результат несёт отчёт.
    Done(Box<JobResult>),
    /// Завершилась ошибкой движка/пользователя.
    Failed(String),
}

/// Зарегистрировать фоновую задачу и выполнить `work` в блокирующем потоке.
///
/// Это единственное место, где живёт «долгая операция»: обработчик сразу
/// возвращает фрагмент прогресса, браузер поллит `/w/{slug}/jobs/{job_id}`, а
/// `work` сообщает `(done, total)` через колбэк. Реестр задач крошечный:
/// записи удаляются, как только финальный фрагмент отдан.
fn spawn_job<F>(state: &AppState, entity: String, label: &str, work: F) -> WebResult<String>
where
    F: FnOnce(&mut dyn FnMut(usize, usize)) -> Result<JobResult, api::ApiError> + Send + 'static,
{
    let job_id = format!("j{}", state.job_seq.fetch_add(1, Ordering::Relaxed));
    let entry = Arc::new(Mutex::new(JobEntry {
        entity,
        label: label.to_string(),
        state: JobState::Running { done: 0, total: 0 },
    }));
    state
        .jobs
        .lock()
        .map_err(|_| WebError::internal("job registry poisoned"))?
        .insert(job_id.clone(), entry.clone());

    // Вызов движка выполняется в блокирующем потоке; прогресс кладётся в общее
    // состояние задачи, откуда его читает эндпоинт поллинга.
    tokio::task::spawn_blocking(move || {
        let mut progress = |done: usize, total: usize| {
            if let Ok(mut guard) = entry.lock() {
                guard.state = JobState::Running { done, total };
            }
        };
        let result = work(&mut progress);
        if let Ok(mut guard) = entry.lock() {
            guard.state = match result {
                Ok(done) => JobState::Done(Box::new(done)),
                Err(error) => JobState::Failed(error.message().to_string()),
            };
        }
    });
    Ok(job_id)
}

/// Фрагмент: прогресс идущей задачи. Он **сам себя поллит** (`hx-get` на
/// эндпоинт задачи, `hx-trigger="every 1s"`, `hx-swap="outerHTML"`), поэтому
/// страница продолжает обновляться без единой строки JavaScript.
#[derive(Template)]
#[template(path = "fragments/job_running.html")]
struct JobRunningFragment {
    slug: String,
    job_id: String,
    entity: String,
    /// Что именно идёт (`staging…`, `run…`).
    label: String,
    done: usize,
    total: usize,
    percent: usize,
}

/// Убедиться, что `candidate` лежит внутри одного из разрешённых корней (после
/// канонизации, без симлинков). Это единственная защита от запросов в духе
/// «прочитай любой файл на сервере».
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
    Err(WebError::bad_request(outside_allowed_roots_message(
        candidate,
        &state.source_roots,
    )))
}

/// Сообщение об отказе по allowlist: что разрешено, почему и где это меняют.
///
/// Отказ в чтении папки — самая частая ошибка первого шага, поэтому сообщение
/// обязано быть действием: перечисляем корни и называем переменную окружения.
/// Отрисовку путей здесь не делаем — она дешёвая и чистая, поэтому проверяется
/// тестом без запуска сервиса.
fn outside_allowed_roots_message(candidate: &Path, roots: &[PathBuf]) -> String {
    let allowed = if roots.is_empty() {
        String::from("(none configured)")
    } else {
        roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "Path {} is outside the folders this server may read. Allowed roots: {allowed}. \
         The service only reads folders listed in STRATA_SOURCE_ROOTS.",
        candidate.display()
    )
}

/// Разобрать форму режима A («одна папка = одна сущность»): имя сущности и путь.
///
/// Пустые поля отсекаются здесь, а не в движке: иначе пользователь получил бы
/// «path not found: » вместо понятного «нужно имя». Пробелы по краям срезаем —
/// их легко набрать копипастом пути.
fn parse_entity_form(form: EntityForm) -> WebResult<(String, PathBuf)> {
    let entity = form.entity.trim();
    let folder = form.folder.trim();
    if entity.is_empty() {
        return Err(WebError::bad_request(
            "Entity name is required — it becomes the table name in ODS.",
        ));
    }
    if folder.is_empty() {
        return Err(WebError::bad_request(
            "Folder path is required — it is the folder this entity reads.",
        ));
    }
    Ok((entity.to_string(), PathBuf::from(folder)))
}

/// Разрешить slug воркспейса в существующий каталог воркспейса.
///
/// Отсутствующий воркспейс → 404 (забота уровня маршрутов), а *некорректные*
/// slug отбрасываются раньше, в [`AppState::workspace_dir`].
fn require_workspace(state: &AppState, user_id: i64, slug: &str) -> WebResult<PathBuf> {
    let dir = state.workspace_dir(user_id, slug)?;
    if !dir.join("workspace.toml").exists() {
        return Err(WebError::not_found(format!("workspace '{slug}' not found")));
    }
    api::open_workspace_at(&dir)?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// Шаблоны (askama компилирует их в бинарник — файлы в рантайме не нужны)
// ---------------------------------------------------------------------------

/// Одна строка списка воркспейсов (строки предформатированы, чтобы шаблон оставался простым).
struct WsRow {
    slug: String,
    name: String,
    data_dir: String,
}

/// Один шаг полосы «как это работает» на главной (`1 Source — Point at a folder…`).
///
/// Подписи берутся из [`status::PipelineStep`], то есть из того же места, что и
/// степпер хаба: текст шага описан в коде ровно один раз.
struct HowStepView {
    /// Готовая подпись шага: номер и название (`1 Source`).
    title: String,
    /// Короткое пояснение шага.
    detail: String,
}

/// Главная страница: список воркспейсов + форма создания.
#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    /// `Some(username)` когда пользователь вошёл; `None` показывает призыв войти.
    user: Option<String>,
    workspaces: Vec<WsRow>,
    workspace_root: String,
    source_roots: Vec<String>,
    /// Личная папка пользователя (`workspaces/<user_id>/`), куда он может
    /// положить файлы и указать её как scan root. `None` для анонима.
    files_area: Option<String>,
    /// Полоса «как это работает»: Source → Schema → Rules → ODS.
    steps: Vec<HowStepView>,
}

/// «Хаб пайплайна» воркспейса: степпер пути, сущности и формы добавления данных.
///
/// Степпер (1 Source · 2 Schema · 3 Rules · 4 ODS) стоит первым: он отвечает на
/// вопрос «где я и что дальше», из-за которого пользователь и терялся. Шаги 2–4
/// ведут прямо во вкладку сущности; шаг 1 закрывается карточкой «Add your data».
///
/// Поля степпера продублированы здесь и в [`EntitiesFragment`], потому что
/// `hub.html` подключает фрагмент через `{% include %}`: шаблон берёт значения
/// из контекста страницы, а не из отдельной структуры.
#[derive(Template)]
#[template(path = "hub.html")]
struct HubTemplate {
    slug: String,
    name: String,
    data_dir: String,
    /// Личная папка пользователя — куда положить файлы, если сервису не
    /// разрешено читать папки вне allowlist.
    files_area: String,
    rows: Vec<entity::EntityRowView>,
    /// Шаги степпера с подсветкой текущего.
    stepper: Vec<status::StepView>,
    /// `Step 2 of 4 · Schema` или `ready to serve`.
    progress_badge: String,
    /// Весь путь пройден — есть успешный прогон.
    ready_to_serve: bool,
    /// Строка «Next: … for sales» со ссылкой (когда подсказывать есть что).
    hint: Option<status::HintView>,
    /// Ошибка добавления: на самой странице хаба её нет (поле нужно, потому что
    /// `hub.html` подключает фрагмент сущностей через `{% include %}`).
    error: Option<String>,
    uptime_secs: u64,
}

/// Фрагмент: степпер + карточка «Add your data» или список сущностей.
///
/// Тот же фрагмент возвращается после подтверждения сущности, поэтому степпер и
/// подсказка «что дальше» обновляются вместе со списком.
#[derive(Template)]
#[template(path = "fragments/entities.html")]
struct EntitiesFragment {
    slug: String,
    files_area: String,
    rows: Vec<entity::EntityRowView>,
    stepper: Vec<status::StepView>,
    progress_badge: String,
    ready_to_serve: bool,
    hint: Option<status::HintView>,
    /// Ошибка добавления/подтверждения сущности, показанная внутри блока.
    error: Option<String>,
}

impl EntitiesFragment {
    /// Собрать фрагмент: строки + посчитанный по ним шаг пути.
    fn build(
        slug: String,
        files_area: String,
        rows: Vec<entity::EntityRowView>,
        error: Option<String>,
    ) -> Self {
        let (progress, hint) = entity::pipeline_view(&slug, &rows);
        EntitiesFragment {
            stepper: progress.steps(&slug),
            progress_badge: progress.badge(),
            ready_to_serve: progress.ready_to_serve,
            hint,
            slug,
            files_area,
            rows,
            error,
        }
    }
}

/// Фрагмент: кандидаты сканирования с формами «подтвердить и привязать».
#[derive(Template)]
#[template(path = "fragments/candidates.html")]
struct CandidatesFragment {
    slug: String,
    candidates: Vec<api::CandidateInfo>,
}

/// Страница входа.
#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: Option<String>,
}

/// Страница регистрации.
#[derive(Template)]
#[template(path = "register.html")]
struct RegisterTemplate {
    error: Option<String>,
}

/// Фрагмент: результат запуска staging.
#[derive(Template)]
#[template(path = "fragments/stage.html")]
struct StageFragment {
    entity: String,
    outcome: api::StageOutcome,
}

/// Фрагмент: красный блок ошибки (используется всеми htmx-действиями при сбое).
#[derive(Template)]
#[template(path = "fragments/error.html")]
struct ErrorFragment {
    message: String,
}

// ---------------------------------------------------------------------------
// Полезные нагрузки форм (extractor `Form` из axum + serde)
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
struct CredentialsForm {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct EntityForm {
    entity: String,
    folder: String,
}

// ---------------------------------------------------------------------------
// Обработчики
// ---------------------------------------------------------------------------

/// `GET /` — список воркспейсов.
async fn index(State(state): State<AppState>, session: Session) -> WebResult<Html<String>> {
    let user = auth::current_user(&state.db, &session).await;
    let root = match &user {
        Some(user) => state.user_root(user.id),
        None => state.workspace_root.clone(),
    };
    let workspaces = api::list_workspaces(&root)?
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
        files_area: user
            .as_ref()
            .map(|u| state.user_root(u.id).display().to_string()),
        user: user.map(|u| u.username),
        workspaces,
        workspace_root: root.display().to_string(),
        source_roots: state
            .source_roots
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        steps: status::PipelineStep::ALL
            .iter()
            .map(|step| HowStepView {
                title: format!("{} {}", step.number(), step.label()),
                detail: step.detail().to_string(),
            })
            .collect(),
    };
    Ok(Html(page.render().map_err(render_error)?))
}

/// `POST /workspaces` — создать воркспейс, затем перенаправить в его хаб.
async fn create_workspace(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<CreateWorkspaceForm>,
) -> WebResult<Redirect> {
    let user_id = auth::current_user_id(&session)
        .await
        .ok_or_else(|| WebError::bad_request("not signed in"))?;
    let created = api::create_workspace_in(&state.user_root(user_id), &form.name)?;
    let slug = created
        .dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("workspace"));
    Ok(Redirect::to(&format!("/w/{slug}")))
}

/// `GET /w/{slug}` — хаб пайплайна одного воркспейса.
///
/// Порядок блока сущностей — это и есть ответ на «что делать дальше»: степпер
/// текущего шага, строка «Next: … for {entity}» и список сущностей с пилюлями.
async fn hub(
    State(state): State<AppState>,
    session: Session,
    AxumPath(slug): AxumPath<String>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = api::open_workspace_at(&dir)?;
    let files_area = state.user_root(user_id).display().to_string();
    let rows = entity::entity_rows(&dir)?;
    let (progress, hint) = entity::pipeline_view(&slug, &rows);
    let page = HubTemplate {
        slug: slug.clone(),
        name: info.name,
        data_dir: info.data_dir.display().to_string(),
        files_area,
        stepper: progress.steps(&slug),
        progress_badge: progress.badge(),
        ready_to_serve: progress.ready_to_serve,
        hint,
        rows,
        error: None,
        uptime_secs: state.started.elapsed().as_secs(),
    };
    Ok(Html(page.render().map_err(render_error)?))
}

/// `POST /w/{slug}/scan` — посмотреть прямые подпапки корня (режим B).
///
/// Возвращает *фрагмент кандидатов*; в воркспейс ничего не пишется (стадия
/// предложения — пользователь всё ещё должен подтвердить каждую сущность).
async fn scan_root(
    State(state): State<AppState>,
    session: Session,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
    Form(form): Form<ScanForm>,
) -> WebResult<Html<String>> {
    // Воркспейс должен существовать; самому сканированию нужен только путь корня.
    let user_id = signed_in(&session).await?;
    let _dir = require_workspace(&state, user_id, &slug)?;
    // Пустая строка дала бы «path not found: » — подсказываем, чего не хватает.
    let root_input = form.root.trim();
    if root_input.is_empty() {
        return form_error(
            &headers,
            "Root folder path is required — scanning looks at its direct subfolders.",
        );
    }
    // Путь вне allowlist — самая частая ошибка; показываем её прямо в блоке
    // кандидатов (см. `form_error`), а не молчаливым 400.
    let root = match ensure_allowed(&state, Path::new(root_input)) {
        Ok(root) => root,
        Err(error) => return form_error(&headers, error.message()),
    };
    let candidates = match api::scan_candidates(&root) {
        Ok(candidates) => candidates,
        Err(error) => return form_error(&headers, error.message()),
    };
    let fragment = CandidatesFragment { slug, candidates };
    Ok(Html(fragment.render().map_err(render_error)?))
}

/// `POST /w/{slug}/entities` — подтвердить кандидата: схема + привязка.
///
/// Этим маршрутом пользуются оба пути добавления данных: карточка режима A
/// («одна папка = одна сущность») и кнопка «Confirm & bind» у кандидата скана.
/// Ответ — тот же фрагмент, что и у хаба, поэтому вместе со списком обновляются
/// степпер и подсказка «что дальше».
async fn add_entity(
    State(state): State<AppState>,
    session: Session,
    AxumPath(slug): AxumPath<String>,
    Form(form): Form<EntityForm>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let files_area = state.user_root(user_id).display().to_string();

    // Ошибки формы и allowlist показываем внутри блока сущностей: степпер и уже
    // добавленные сущности остаются на месте.
    let inline_error = |dir: &std::path::Path, message: String| -> WebResult<Html<String>> {
        let rows = entity::entity_rows(dir)?;
        let fragment =
            EntitiesFragment::build(slug.clone(), files_area.clone(), rows, Some(message));
        Ok(Html(fragment.render().map_err(render_error)?))
    };

    let (entity, raw_folder) = match parse_entity_form(form) {
        Ok(parsed) => parsed,
        Err(error) => return inline_error(&dir, error.message().to_string()),
    };
    let folder = match ensure_allowed(&state, &raw_folder) {
        Ok(folder) => folder,
        Err(error) => return inline_error(&dir, error.message().to_string()),
    };

    // Любой отказ движка (пустая папка, нечитаемые файлы) показывается внутри
    // блока сущностей: степпер и уже добавленные сущности остаются на месте,
    // а не превращаются в один красный блок.
    let error = api::confirm_entity(&dir, &entity, &folder)
        .err()
        .map(|error| error.message().to_string());

    let rows = entity::entity_rows(&dir)?;
    let fragment = EntitiesFragment::build(slug, files_area, rows, error);
    Ok(Html(fragment.render().map_err(render_error)?))
}

/// `POST /w/{slug}/entities/{entity}/stage` — запустить staging в фоне.
///
/// Staging упирается в CPU и IO (запись Parquet, проверки типов), поэтому он не
/// должен блокировать поток запроса: мы запускаем его блокирующей задачей и
/// сразу возвращаем *фрагмент прогресса*, который поллит эндпоинт задачи.
/// Браузер видит, как движется полоса; сервер остаётся отзывчивым для других
/// пользователей.
async fn stage_entity(
    State(state): State<AppState>,
    session: Session,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;

    let task_dir = dir.clone();
    let task_entity = entity.clone();
    let job_id = spawn_job(&state, entity.clone(), "staging…", move |progress| {
        let outcome = api::stage_entity_with_progress(&task_dir, &task_entity, |done, total| {
            progress(done, total)
        })?;
        Ok(JobResult::Stage(Box::new(outcome)))
    })?;

    let fragment = JobRunningFragment {
        slug,
        job_id,
        entity,
        label: String::from("staging…"),
        done: 0,
        total: 0,
        percent: 0,
    };
    Ok(Html(fragment.render().map_err(render_error)?))
}

/// `GET /w/{slug}/jobs/{job_id}` — прогресс (пока задача идёт) или финальный результат.
///
/// htmx подменяет этот фрагмент в `#stage`. Идущие фрагменты продолжают
/// поллинг; финальные фрагменты не несут атрибутов поллинга, поэтому цикл
/// останавливается сам. Завершённые задачи удаляются из реестра сразу после
/// того, как их результат отдан.
async fn job_status(
    State(state): State<AppState>,
    session: Session,
    AxumPath((slug, job_id)): AxumPath<(String, String)>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let _dir = require_workspace(&state, user_id, &slug)?;

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

    // Снимок берём под блокировкой, а рендерим уже вне неё (никогда не рендерим
    // с блокировкой в руках: ошибка шаблона не должна отравить общее состояние).
    enum Snapshot {
        Running { done: usize, total: usize },
        Final(JobState),
    }
    let (entity, label, snapshot) = {
        let mut guard = entry
            .lock()
            .map_err(|_| WebError::internal("job state poisoned"))?;
        let entity = guard.entity.clone();
        let label = guard.label.clone();
        let snapshot = match &guard.state {
            JobState::Running { done, total } => Snapshot::Running {
                done: *done,
                total: *total,
            },
            // Финальные состояния потребляются ровно один раз: цикл поллинга
            // заканчивается этим ответом, так что читать дальше уже нечего.
            JobState::Done(_) | JobState::Failed(_) => {
                let taken = std::mem::replace(
                    &mut guard.state,
                    JobState::Failed(String::from("job result already served")),
                );
                Snapshot::Final(taken)
            }
        };
        (entity, label, snapshot)
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
                label,
                done,
                total,
                percent,
            };
            (fragment.render().map_err(render_error)?, false)
        }
        Snapshot::Final(JobState::Done(result)) => match *result {
            // Staging: отчёт сырого слоя (файлы, строки, части).
            JobResult::Stage(outcome) => (
                StageFragment {
                    entity,
                    outcome: *outcome,
                }
                .render()
                .map_err(render_error)?,
                true,
            ),
            // Прогон: сводка манифеста + ссылка на страницу прогона.
            JobResult::Run(outcome) => (
                entity::run_result_fragment(&slug, &outcome)?
                    .render()
                    .map_err(render_error)?,
                true,
            ),
        },
        Snapshot::Final(JobState::Failed(message)) => (
            ErrorFragment { message }.render().map_err(render_error)?,
            true,
        ),
        Snapshot::Final(JobState::Running { .. }) => unreachable!("running is not final"),
    };

    // Завершено → убираем задачу из реестра (браузер прекращает поллинг).
    if is_final {
        if let Ok(mut jobs) = state.jobs.lock() {
            jobs.remove(&job_id);
        }
    }

    Ok(Html(html))
}

// ---------------------------------------------------------------------------
// Аутентификация: страницы, обработчики отправки, middleware-охранник маршрутов
// ---------------------------------------------------------------------------

/// id пользователя из сессии, или 400, если сессия его почему-то потеряла.
async fn signed_in(session: &Session) -> WebResult<i64> {
    auth::current_user_id(session)
        .await
        .ok_or_else(|| WebError::bad_request("not signed in"))
}

/// Middleware, охраняющая маршруты воркспейсов: аноним → редирект на страницу входа.
///
/// То, что это middleware (а не проверка внутри каждого обработчика), держит
/// правило в одном месте: любой новый маршрут, добавленный под защищённый
/// роутер, покрыт по умолчанию.
async fn require_login(
    State(state): State<AppState>,
    session: Session,
    request: Request,
    next: Next,
) -> Response {
    match auth::current_user(&state.db, &session).await {
        Some(_) => next.run(request).await,
        None => Redirect::to("/login").into_response(),
    }
}

/// `GET /login`
async fn login_page() -> WebResult<Html<String>> {
    let page = LoginTemplate { error: None };
    Ok(Html(page.render().map_err(render_error)?))
}

/// `POST /login` — проверить учётные данные, начать сессию.
///
/// Проверка Argon2 стоит десятки миллисекунд CPU, поэтому выполняется в
/// блокирующем потоке; иначе всплеск входов застопорит async-рантайм.
async fn login_submit(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<CredentialsForm>,
) -> WebResult<Response> {
    let username = form.username.trim().to_string();
    let password = form.password.clone();

    let user = auth::find_user(&state.db, &username).await?;
    let Some((id, _, phc)) = user else {
        // Одно и то же сообщение для «нет такого пользователя» и «неверный пароль».
        return Ok(login_failed("invalid username or password"));
    };

    let verified = tokio::task::spawn_blocking(move || auth::verify_password(&password, &phc))
        .await
        .map_err(|_| WebError::internal("password check panicked"))?;
    if !verified {
        return Ok(login_failed("invalid username or password"));
    }

    auth::login(&session, id)
        .await
        .map_err(|e| WebError::internal(e.0))?;
    Ok(Redirect::to("/").into_response())
}

/// Отрендерить страницу входа с ошибкой (200, чтобы браузер сохранил форму).
fn login_failed(message: &str) -> Response {
    let page = LoginTemplate {
        error: Some(message.to_string()),
    };
    match page.render() {
        Ok(html) => (StatusCode::UNAUTHORIZED, Html(html)).into_response(),
        Err(error) => WebError::internal(format!("template error: {error}")).into_response(),
    }
}

/// `GET /register`
async fn register_page() -> WebResult<Html<String>> {
    let page = RegisterTemplate { error: None };
    Ok(Html(page.render().map_err(render_error)?))
}

/// `POST /register` — создать учётную запись и войти под ней.
async fn register_submit(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<CredentialsForm>,
) -> WebResult<Response> {
    let username = form.username.trim().to_string();
    let password = form.password.clone();

    if let Err(error) = auth::validate_credentials(&username, &password) {
        return Ok(register_failed(&error.0));
    }

    // Хэширование намеренно дорогое (Argon2id) → блокирующий поток.
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(&password))
        .await
        .map_err(|_| WebError::internal("password hashing panicked"))?
        .map_err(|e| WebError::internal(e.0))?;

    match auth::create_user(&state.db, &username, &hash).await {
        Ok(id) => {
            auth::login(&session, id)
                .await
                .map_err(|e| WebError::internal(e.0))?;
            // Сразу выдаём новому пользователю его собственный корень воркспейсов.
            let _ = std::fs::create_dir_all(state.user_root(id));
            Ok(Redirect::to("/").into_response())
        }
        Err(error) => Ok(register_failed(&error.0)),
    }
}

/// Отрендерить страницу регистрации с ошибкой.
fn register_failed(message: &str) -> Response {
    let page = RegisterTemplate {
        error: Some(message.to_string()),
    };
    match page.render() {
        Ok(html) => (StatusCode::BAD_REQUEST, Html(html)).into_response(),
        Err(error) => WebError::internal(format!("template error: {error}")).into_response(),
    }
}

/// `POST /logout` — уничтожить серверную сессию.
async fn logout_submit(session: Session) -> Redirect {
    auth::logout(&session).await;
    Redirect::to("/login")
}

// ---------------------------------------------------------------------------
// Метрики ресурсов: «сколько сервис кушает?»
// ---------------------------------------------------------------------------

/// Один снимок ресурсов для виджета статуса и `/metrics`.
#[derive(Clone, Copy)]
struct ResourceSample {
    /// Resident set size этого процесса (байты) — «сколько RAM ест приложение».
    rss_bytes: Option<u64>,
    /// Использование CPU процессом в процентах. Первый снимок после старта —
    /// 0.0: `sysinfo` нужны два обновления, разделённых во времени, чтобы
    /// посчитать дельту.
    cpu_pct: Option<f32>,
    /// Всего физической памяти на хосте (байты).
    mem_total_bytes: u64,
    /// Использовано физической памяти на хосте (байты).
    mem_used_bytes: u64,
    /// Воркспейсов сейчас существует.
    workspaces: usize,
}

/// Снять свежий снимок. Дёшево: одно обновление процессов + одно обновление памяти.
fn sample_resources(state: &AppState) -> ResourceSample {
    let pid = Pid::from_u32(std::process::id());
    let workspaces = api::list_workspaces(&state.workspace_root)
        .map(|w| w.len())
        .unwrap_or(0);

    let mut system = match state.system.lock() {
        Ok(guard) => guard,
        // Отравленный мьютекс не должен ронять весь сервис: вместо этого
        // сообщаем метрики «неизвестно».
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

/// Человеческое форматирование байтов для виджета (`123 MB`, `1.4 GB`).
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

/// Фрагмент: маленький живой вывод ресурсов (htmx поллит его каждые 2 с).
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

/// `GET /metrics` — текстовые метрики для скрейперов в стиле Prometheus
/// (и для `curl`, которым это и отлаживают).
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

/// `GET /healthz` — healthcheck контейнера (тоже удобно через `curl`).
async fn healthz(State(state): State<AppState>) -> String {
    format!(
        "ok uptime={}s workspaces={}",
        state.started.elapsed().as_secs(),
        state.workspace_root.display()
    )
}

/// Пришёл ли запрос от htmx.
///
/// htmx по умолчанию **не подменяет** содержимое при ответах 4xx/5xx, поэтому
/// ошибку, которую пользователь должен увидеть прямо в интерфейсе, отдаём
/// кодом 200 с HTML-фрагментом. Для обычных клиентов (curl, скрипты) сохраняем
/// честный 400 с текстом.
fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

/// Ошибка формы/пути: htmx — фрагмент с пояснением (200), иначе — 400.
fn form_error(headers: &HeaderMap, message: impl Into<String>) -> WebResult<Html<String>> {
    let message = message.into();
    if is_htmx(headers) {
        let fragment = ErrorFragment { message };
        Ok(Html(fragment.render().map_err(render_error)?))
    } else {
        Err(WebError::bad_request(message))
    }
}

/// Превратить сбой рендера askama в 500.
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

    let (state, addr) = match AppState::from_env().await {
        Ok(pair) => pair,
        Err(error) => {
            tracing::error!("startup failed: {error}");
            std::process::exit(1);
        }
    };

    // Маршруты, требующие вошедшего пользователя. Всё, что связано с
    // воркспейсами, живёт здесь, под охраной одной middleware (`require_login`).
    let protected = Router::new()
        .route("/workspaces", post(create_workspace))
        .route("/w/{slug}", get(hub))
        .route("/w/{slug}/scan", post(scan_root))
        .route("/w/{slug}/entities", post(add_entity))
        .route("/w/{slug}/entities/{entity}/stage", post(stage_entity))
        .route("/w/{slug}/jobs/{job_id}", get(job_status))
        // --- Страница сущности: стадии пайплайна (UI-1…UI-4) --------------------
        // Стадии — это табы одной страницы (`?tab=`), а не пункты меню.
        .route("/w/{slug}/e/{entity}", get(entity::entity_page))
        .route("/w/{slug}/e/{entity}/runs/{run_id}", get(entity::run_page))
        // Файлы: перечитать список и показать превью (в т.ч. выгрузить его как CSV).
        .route(
            "/w/{slug}/e/{entity}/files/reload",
            post(entity::files_reload),
        )
        .route(
            "/w/{slug}/e/{entity}/files/preview",
            post(entity::file_preview_fragment),
        )
        .route(
            "/w/{slug}/e/{entity}/files/preview.csv",
            get(entity::preview_csv),
        )
        // Схема: черновик в памяти сервиса, подтверждение пишет schemas/<entity>.toml.
        .route(
            "/w/{slug}/e/{entity}/schema/infer",
            post(entity::schema_infer),
        )
        .route(
            "/w/{slug}/e/{entity}/schema/column",
            post(entity::schema_column),
        )
        .route(
            "/w/{slug}/e/{entity}/schema/confirm",
            post(entity::schema_confirm),
        )
        // Правила: конструктор в контексте колонки + проверка на образце.
        .route("/w/{slug}/e/{entity}/rules", post(entity::rules_change))
        .route("/w/{slug}/e/{entity}/rules/save", post(entity::rules_save))
        .route(
            "/w/{slug}/e/{entity}/validate",
            post(entity::validate_sample),
        )
        // Стадия ODS: прогон в фоне (прогресс поллит `/jobs/{job_id}`) и логи.
        .route("/w/{slug}/e/{entity}/runs", post(entity::start_run))
        .route("/w/{slug}/e/{entity}/logs", post(entity::logs_select))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_login));

    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/fragments/resources", get(resources_fragment))
        .route("/login", get(login_page).post(login_submit))
        .route("/register", get(register_page).post(register_submit))
        .route("/logout", post(logout_submit))
        .merge(protected)
        .with_state(state.clone())
        // Middleware сессий должна обёртывать всё (включая guard уровня маршрутов
        // выше), поэтому применяется последней = самой внешней.
        // `with_secure(false)` разрешает обычный HTTP локально и в dev; поставьте
        // сервис за TLS и включите её в продакшене.
        .layer(SessionManagerLayer::new(MemoryStore::default()).with_secure(false));

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

    // Аккуратное завершение, чтобы `docker compose down` проходил чисто.
    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!("server error: {error}");
    }
}

/// Дождаться Ctrl-C / SIGTERM.
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

/// Заглушка для неиспользуемого импорта `Arc` (сохранена ради будущего
/// расширения общего состояния: реестр фоновых задач). Ссылка на него без
/// doc-теста оставляет зависимость явной и без предупреждений о мёртвом коде.
#[allow(dead_code)]
type SharedState = Arc<AppState>;

// ---------------------------------------------------------------------------
// Тесты: чистые помощники веб-слоя (без сервера, БД и файлов)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_message_lists_every_root_and_the_env_var() {
        let message = outside_allowed_roots_message(
            Path::new("/tmp/secret"),
            &[PathBuf::from("/data/one"), PathBuf::from("/data/two")],
        );
        // Пользователь должен понять и что запрещено, и куда можно, и чем это меняют.
        assert!(message.contains("/tmp/secret"), "{message}");
        assert!(
            message.contains("Allowed roots: /data/one, /data/two"),
            "{message}"
        );
        assert!(message.contains("STRATA_SOURCE_ROOTS"), "{message}");
        assert!(
            message.contains("outside the folders this server may read"),
            "{message}"
        );
    }

    #[test]
    fn allowlist_message_survives_an_empty_root_list() {
        let message = outside_allowed_roots_message(Path::new("/tmp/secret"), &[]);
        assert!(
            message.contains("Allowed roots: (none configured)"),
            "{message}"
        );
        // Даже без корней сообщение остаётся предложением, а не обрывком.
        assert!(message.ends_with("STRATA_SOURCE_ROOTS."), "{message}");
    }

    #[test]
    fn entity_form_trims_the_name_and_the_folder() {
        let form = EntityForm {
            entity: String::from("  sales \n"),
            folder: String::from("  /data/sales  "),
        };
        let (entity, folder) = parse_entity_form(form).expect("форма режима A разобрана");
        assert_eq!(entity, "sales");
        assert_eq!(folder, PathBuf::from("/data/sales"));
    }

    #[test]
    fn entity_form_rejects_empty_fields_with_a_hint() {
        // Пустое имя: пользователь должен узнать, чего не хватает, а не получить
        // «path not found: » из движка.
        let no_name = EntityForm {
            entity: String::from("   "),
            folder: String::from("/data/sales"),
        };
        let error = parse_entity_form(no_name).expect_err("пустое имя — ошибка");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(
            error.message().contains("Entity name is required"),
            "{}",
            error.message()
        );

        let no_folder = EntityForm {
            entity: String::from("sales"),
            folder: String::new(),
        };
        let error = parse_entity_form(no_folder).expect_err("пустая папка — ошибка");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(
            error.message().contains("Folder path is required"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn htmx_requests_are_detected_by_header() {
        // htmx не подменяет контент при 4xx, поэтому по этому признаку решаем,
        // отдавать ошибку фрагментом (200) или честным 400.
        let mut headers = HeaderMap::new();
        assert!(!is_htmx(&headers));
        headers.insert("hx-request", "true".parse().expect("значение заголовка"));
        assert!(is_htmx(&headers));
    }
}
