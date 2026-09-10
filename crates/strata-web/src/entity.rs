//! # Страница сущности: одна таблица от грязного файла до ODS
//!
//! Реализация фаз UI-1…UI-4 из `docs/design-ui-pipeline.md`:
//! `/w/{slug}/e/{entity}` — один экран со стадиями-табами
//! (Files · Schema · Rules · ODS · Logs).
//!
//! ```text
//!  файлы ──► схема ──► правила ──► ODS + карантин ──► логи
//!   │          │          │            │
//!   │          │          │            └─ фоновый прогон (реестр задач + поллинг)
//!   │          │          └─ черновик правил в памяти сервиса ──► validate_entity
//!   │          └─ черновик колонок в памяти сервиса ──► save_entity_schema
//!   └─ список файлов + превью чтения (preview_source_with)
//! ```
//!
//! ## Где живёт «состояние»
//!
//! Клиентского состояния нет вообще (правило проекта). Всё, что пользователь
//! накликал, но ещё не подтвердил, лежит в **черновике на сервере**
//! ([`Draft`] в `AppState.drafts`, ключ — пользователь + воркспейс + сущность).
//! Любой htmx-запрос возвращает готовый HTML-фрагмент; после подтверждения
//! черновик становится файлом `schemas/<entity>.toml` (источник истины).
//!
//! ## Почему фрагменты рендерятся в строку
//!
//! Страница собирает активный таб из тех же askama-фрагментов, которые
//! возвращают htmx-обработчики (`{{ body|safe }}`). Разметка таба описана ровно
//! один раз: и при первой отрисовке страницы, и при подмене фрагмента работает
//! один и тот же шаблон.

use std::path::Path;

use askama::Template;
use axum::Form;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use strata_core::api;
use strata_core::{ColumnDef, ColumnRule, Preview, ReaderOptions, RunManifest};

use crate::forms::{
    ColumnForm, DTYPE_LABELS, PreviewForm, RULE_TYPES, RuleForm, RunPickForm, SEVERITIES,
    draft_key, format_duration, human_size, parse_column_name, parse_dtype, parse_file_name,
    parse_path_token, parse_rule, severity_token, url_encode,
};
use crate::status::{
    HintView, PillView, PipelineProgress, StageStatus, TONE_BAD, TONE_OK, TONE_WARN, Tab, TabView,
    entity_status,
};
use crate::{
    AppState, ErrorFragment, JobRunningFragment, WebError, WebResult, render_error,
    require_workspace, signed_in,
};

/// Сколько строк показывать в превью файла.
const PREVIEW_ROWS: usize = 50;
/// Сколько строк образца прогонять при «проверить на образце».
const SAMPLE_ROWS: usize = 1000;
/// Сколько строк карантина и лога показывать в UI.
const TABLE_LIMIT: usize = 200;

// ---------------------------------------------------------------------------
// Черновик контракта в памяти сервиса
// ---------------------------------------------------------------------------

/// Черновик контракта сущности (колонки + правила), ещё не сохранённый в TOML.
///
/// Живёт в `AppState.drafts` под ключом `user_id:slug:entity`: один черновик на
/// пользователя и сущность, чужие правки не видны, перезагрузка страницы
/// ничего не теряет.
#[derive(Debug, Clone, Default)]
pub(crate) struct Draft {
    /// Колонки в порядке отображения.
    pub(crate) columns: Vec<ColumnDef>,
    /// Правила качества.
    pub(crate) rules: Vec<ColumnRule>,
}

/// Прочитать черновик, если он уже создан.
fn read_draft(state: &AppState, key: &str) -> WebResult<Option<Draft>> {
    let drafts = state
        .drafts
        .lock()
        .map_err(|_| WebError::internal("schema draft store poisoned"))?;
    Ok(drafts.get(key).cloned())
}

/// Записать черновик.
fn write_draft(state: &AppState, key: &str, draft: Draft) -> WebResult<()> {
    let mut drafts = state
        .drafts
        .lock()
        .map_err(|_| WebError::internal("schema draft store poisoned"))?;
    drafts.insert(key.to_string(), draft);
    Ok(())
}

/// Черновик, инициализированный текущим контрактом ядра при первом обращении.
///
/// «Текущий контракт» — это подтверждённая схема, а пока её нет — предложение
/// инференса (его делает ядро, `api::entity_schema_view`).
fn ensure_draft(state: &AppState, key: &str, dir: &Path, entity: &str) -> WebResult<Draft> {
    if let Some(draft) = read_draft(state, key)? {
        return Ok(draft);
    }
    let view = api::entity_schema_view(dir, entity)?;
    let draft = Draft {
        columns: view.columns,
        rules: view.rules,
    };
    write_draft(state, key, draft.clone())?;
    Ok(draft)
}

// ---------------------------------------------------------------------------
// Строка сущности в хабе
// ---------------------------------------------------------------------------

/// Строка списка сущностей: имя, папка и пилюли стадий.
#[derive(Debug, Clone)]
pub(crate) struct EntityRowView {
    /// Имя сущности.
    pub(crate) entity: String,
    /// Привязанная папка-источник.
    pub(crate) folder: String,
    /// Пилюли стадий (файлы, схема, правила, ODS, логи).
    pub(crate) pills: Vec<PillView>,
    /// Тот же снимок стадий в виде данных: из него считается шаг пайплайна
    /// (степпер хаба), а не только подписи пилюль.
    pub(crate) status: StageStatus,
}

/// Строки сущностей воркспейса для хаба (`GET /w/{slug}`).
pub(crate) fn entity_rows(dir: &Path) -> WebResult<Vec<EntityRowView>> {
    Ok(api::entities(dir)?
        .into_iter()
        .map(|info| {
            let status = entity_status(dir, &info.entity, info.has_schema);
            EntityRowView {
                pills: status.pills(),
                entity: info.entity,
                folder: info.folder.display().to_string(),
                status,
            }
        })
        .collect())
}

/// Текущий шаг пути и подсказка «что дальше» по строкам хаба.
///
/// Строки уже несут снимок стадий, поэтому шаг считается без обращений к диску:
/// сам расчёт — чистая функция [`PipelineProgress::from_statuses`].
pub(crate) fn pipeline_view(
    slug: &str,
    rows: &[EntityRowView],
) -> (PipelineProgress, Option<HintView>) {
    let pairs: Vec<(String, StageStatus)> = rows
        .iter()
        .map(|row| (row.entity.clone(), row.status.clone()))
        .collect();
    let progress = PipelineProgress::from_statuses(&pairs);
    let hint = progress.hint(slug);
    (progress, hint)
}

/// Найти привязку сущности (имя приходит из URL — сначала проверяем его).
fn require_entity(dir: &Path, entity: &str) -> WebResult<api::EntityInfo> {
    let entity = parse_path_token(entity, "entity").map_err(WebError::bad_request)?;
    api::entities(dir)?
        .into_iter()
        .find(|info| info.entity == entity)
        .ok_or_else(|| {
            WebError::not_found(format!("entity '{entity}' is not bound in this workspace"))
        })
}

/// Есть ли у сущности подтверждённая схема (свежий ответ из `workspace.toml`).
fn entity_has_schema(dir: &Path, entity: &str) -> bool {
    api::entities(dir)
        .map(|entities| {
            entities
                .iter()
                .find(|info| info.entity == entity)
                .map(|info| info.has_schema)
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Общие представления (простые структуры для шаблонов)
// ---------------------------------------------------------------------------

/// Пара «значение, подпись» — пункт `<select>`.
#[derive(Debug, Clone)]
struct OptionView {
    /// Значение поля формы.
    value: String,
    /// Что видит пользователь.
    label: String,
}

/// Строка списка файлов сущности.
#[derive(Debug, Clone)]
struct FileRowView {
    /// Имя файла.
    name: String,
    /// Размер в удобных единицах.
    size: String,
    /// Вид файла (`CSV`, `Parquet`, …).
    kind: String,
    /// Короткий статус строки (`ok`, `not a data file`).
    status: String,
    /// Классы Tailwind для статуса.
    status_class: String,
}

/// Часть Parquet (ODS или карантин).
#[derive(Debug, Clone)]
struct PartView {
    /// Путь относительно директории прогона.
    path: String,
    /// Строк в части.
    rows: u64,
    /// Размер в удобных единицах.
    size: String,
}

/// Строка карантина.
#[derive(Debug, Clone)]
struct QuarantineRowView {
    /// Исходный файл.
    file: String,
    /// Номер строки.
    row_index: usize,
    /// Колонка.
    column: String,
    /// Правило.
    rule: String,
    /// Значение «как есть».
    value: String,
}

/// Сводка нарушений по правилу.
#[derive(Debug, Clone)]
struct RuleSummaryView {
    /// Колонка.
    column: String,
    /// Правило.
    rule: String,
    /// Строгость.
    severity: String,
    /// Классы Tailwind для строгости.
    severity_class: String,
    /// Число нарушений.
    violations: usize,
}

/// Строка истории прогонов (вкладки ODS и Logs).
#[derive(Debug, Clone)]
struct RunRowView {
    /// Идентификатор прогона.
    run_id: String,
    /// Ссылка на страницу прогона.
    href: String,
    /// Когда стартовал (UTC).
    started: String,
    /// Длительность в удобном виде.
    duration: String,
    /// Прочитано / в ODS / в карантине.
    counts: String,
    /// Итог прогона текстом.
    status: String,
    /// Классы Tailwind для итога.
    status_class: String,
    /// Выбран ли этот прогон на вкладке Logs (отметка в `<select>`).
    selected: bool,
}

/// Преобразовать манифест прогона в строку истории.
fn run_row(slug: &str, entity: &str, manifest: &RunManifest) -> RunRowView {
    RunRowView {
        run_id: manifest.run_id.clone(),
        href: run_href(slug, entity, &manifest.run_id),
        started: manifest.started_utc.clone(),
        duration: format_duration(manifest.duration_ms),
        counts: format!(
            "{} read · {} in ODS · {} quarantine",
            manifest.rows_read, manifest.rows_valid, manifest.rows_quarantine
        ),
        status: if manifest.has_errors {
            String::from("errors")
        } else {
            String::from("ok")
        },
        status_class: if manifest.has_errors {
            TONE_BAD.to_string()
        } else {
            TONE_OK.to_string()
        },
        selected: false,
    }
}

/// Ссылка на страницу прогона.
fn run_href(slug: &str, entity: &str, run_id: &str) -> String {
    format!("/w/{slug}/e/{entity}/runs/{}", url_encode(run_id))
}

/// Плашка итога прогона: «готов к отдаче» — это состояние, а не кнопка.
fn run_badge(manifest: &RunManifest) -> (String, String) {
    if manifest.has_errors {
        (
            format!(
                "Has errors — {} row(s) in quarantine",
                manifest.rows_quarantine
            ),
            TONE_BAD.to_string(),
        )
    } else {
        (
            format!(
                "Ready to serve — {} row(s) · {} part(s)",
                manifest.rows_valid,
                manifest.parts.len()
            ),
            TONE_OK.to_string(),
        )
    }
}

/// Описание опций чтения для строки «как прочитан файл».
fn reader_options_text(options: ReaderOptions) -> String {
    let encoding = options
        .encoding
        .map(|choice| choice.label().to_string())
        .unwrap_or_else(|| String::from("auto"));
    let delimiter = match options.delimiter {
        Some(ch) => format!("'{ch}'"),
        None => String::from("auto"),
    };
    format!(
        "encoding {encoding} · delimiter {delimiter} · header {}",
        if options.has_header { "yes" } else { "no" }
    )
}

/// Опции чтения для превью и пояснение к ним.
///
/// Если схема подтверждена — берём её опции; иначе читаем автоопределением.
/// Инференс здесь **намеренно не запускается**: вкладка Files должна открываться
/// мгновенно даже на папке из сотен файлов (что движок определил сам, видно в
/// строке «Read as» прямо над превью).
fn reader_options(dir: &Path, info: &api::EntityInfo) -> (ReaderOptions, String) {
    if info.has_schema {
        if let Ok(view) = api::entity_schema_view(dir, &info.entity) {
            if view.confirmed {
                return (
                    view.options,
                    format!(
                        "Reader options from the confirmed schema: {}",
                        reader_options_text(view.options)
                    ),
                );
            }
        }
    }
    (
        ReaderOptions::default(),
        String::from(
            "Schema is not confirmed yet — the preview reads with auto-detected options and shows below what it detected.",
        ),
    )
}

/// Части Parquet манифеста в представление шаблона.
fn part_views(parts: &[strata_core::PartInfo]) -> Vec<PartView> {
    parts
        .iter()
        .map(|part| PartView {
            path: part.path.clone(),
            rows: part.rows,
            size: human_size(part.bytes),
        })
        .collect()
}

/// Сводка правил манифеста в представление шаблона.
fn rule_summary_views(rules: &[strata_core::RuleSummary]) -> Vec<RuleSummaryView> {
    rules
        .iter()
        .map(|rule| RuleSummaryView {
            column: rule.column.clone(),
            rule: rule.rule.clone(),
            severity: severity_token(rule.severity).to_string(),
            severity_class: match rule.severity {
                strata_core::Severity::Error => TONE_BAD.to_string(),
                strata_core::Severity::Warning => TONE_WARN.to_string(),
            },
            violations: rule.violations,
        })
        .collect()
}

/// Строки карантина в представление шаблона.
fn quarantine_views(rows: Vec<strata_core::QuarantineRow>) -> Vec<QuarantineRowView> {
    rows.into_iter()
        .map(|row| QuarantineRowView {
            file: row.file,
            row_index: row.row_index,
            column: row.column,
            rule: row.rule,
            value: row.value,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Стадия 1: файлы и превью
// ---------------------------------------------------------------------------

/// Панель стадии Files (файлы + превью) — фрагмент и часть страницы.
#[derive(Template)]
#[template(path = "fragments/files_panel.html")]
struct FilesPanelFragment {
    slug: String,
    entity: String,
    folder: String,
    total_size: String,
    reader_note: String,
    files: Vec<FileRowView>,
    has_error: bool,
    error: String,
    /// Готовый HTML превью (рендерится отдельным фрагментом).
    preview_html: String,
}

/// Превью одного файла: колонки, строки и «как он прочитан».
#[derive(Template)]
#[template(path = "fragments/file_preview.html")]
struct FilePreviewFragment {
    file: String,
    source: String,
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
    has_error: bool,
    error: String,
    csv_href: String,
    max_rows: usize,
}

impl FilePreviewFragment {
    /// Превью-заглушка: файла нет, он вне папки сущности или не читается.
    fn failed(file: String, error: String) -> Self {
        FilePreviewFragment {
            file,
            source: String::new(),
            columns: Vec::new(),
            rows: Vec::new(),
            has_error: true,
            error,
            csv_href: String::new(),
            max_rows: PREVIEW_ROWS,
        }
    }

    /// Собрать превью из ответа ядра.
    fn success(file: &str, preview: Preview, csv_href: String) -> Self {
        FilePreviewFragment {
            file: file.to_string(),
            source: preview.source.summary(),
            columns: preview
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect(),
            rows: preview.rows,
            has_error: false,
            error: String::new(),
            csv_href,
            max_rows: PREVIEW_ROWS,
        }
    }
}

/// Собрать панель Files: список файлов и превью выбранного (по умолчанию — первого).
fn files_panel(
    slug: &str,
    dir: &Path,
    info: &api::EntityInfo,
    preview_file: &str,
) -> WebResult<FilesPanelFragment> {
    let listed = api::entity_files(dir, &info.entity)?;
    let total = listed.iter().map(|(_, size, _)| *size).sum::<u64>();
    let (options, reader_note) = reader_options(dir, info);

    let files = listed
        .iter()
        .map(|(name, size, kind)| {
            let data_file = kind != "Other";
            FileRowView {
                name: name.clone(),
                size: human_size(*size),
                kind: kind.clone(),
                status: if data_file {
                    String::from("ok")
                } else {
                    String::from("not a data file")
                },
                status_class: if data_file {
                    TONE_OK.to_string()
                } else {
                    TONE_WARN.to_string()
                },
            }
        })
        .collect();

    // Превью: явно выбранный файл или первый файл данных.
    let preview_html = match choose_preview_file(&listed, preview_file) {
        Ok(name) => preview_fragment(slug, info, &name, options)
            .render()
            .map_err(render_error)?,
        Err(problem) => problem.render().map_err(render_error)?,
    };

    Ok(FilesPanelFragment {
        slug: slug.to_string(),
        entity: info.entity.clone(),
        folder: info.folder.display().to_string(),
        total_size: human_size(total),
        reader_note,
        files,
        has_error: false,
        error: String::new(),
        preview_html,
    })
}

/// Какой файл показывать в превью и не ошиблись ли с выбором.
fn choose_preview_file(
    listed: &[(String, u64, String)],
    preview_file: &str,
) -> Result<String, FilePreviewFragment> {
    if preview_file.trim().is_empty() {
        return match listed.iter().find(|(_, _, kind)| kind != "Other") {
            Some((name, _, _)) => Ok(name.clone()),
            None => Err(FilePreviewFragment::failed(
                String::from("—"),
                String::from("no readable data files in the entity folder"),
            )),
        };
    }
    let wanted = parse_file_name(preview_file)
        .map_err(|message| FilePreviewFragment::failed(preview_file.to_string(), message))?;
    if listed.iter().any(|(name, _, _)| *name == wanted) {
        Ok(wanted)
    } else {
        Err(FilePreviewFragment::failed(
            wanted,
            String::from("this file is not part of the entity folder"),
        ))
    }
}

/// Превью файла через ядро: `preview_source_with` с опциями чтения схемы.
fn preview_fragment(
    slug: &str,
    info: &api::EntityInfo,
    file: &str,
    options: ReaderOptions,
) -> FilePreviewFragment {
    let path = info.folder.join(file);
    let csv_href = format!(
        "/w/{slug}/e/{}/files/preview.csv?file={}",
        info.entity,
        url_encode(file)
    );
    match strata_core::preview_source_with(&path, PREVIEW_ROWS, options) {
        Ok(preview) => FilePreviewFragment::success(file, preview, csv_href),
        Err(error) => {
            FilePreviewFragment::failed(file.to_string(), format!("cannot read this file: {error}"))
        }
    }
}

/// `POST /w/{slug}/e/{entity}/files/reload` — перечитать файлы и превью.
pub(crate) async fn files_reload(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = require_entity(&dir, &entity)?;
    let panel = files_panel(&slug, &dir, &info, "")?;
    Ok(Html(panel.render().map_err(render_error)?))
}

/// `POST /w/{slug}/e/{entity}/files/preview` — превью выбранного файла.
pub(crate) async fn file_preview_fragment(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
    Form(form): Form<PreviewForm>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = require_entity(&dir, &entity)?;
    let name = parse_file_name(&form.file).map_err(WebError::bad_request)?;
    let known = api::entity_files(&dir, &info.entity)?
        .into_iter()
        .any(|(listed, _, _)| listed == name);
    if !known {
        return Err(WebError::not_found(format!(
            "file '{name}' is not part of entity '{entity}'"
        )));
    }
    let (options, _) = reader_options(&dir, &info);
    let preview = preview_fragment(&slug, &info, &name, options);
    Ok(Html(preview.render().map_err(render_error)?))
}

/// `GET /w/{slug}/e/{entity}/files/preview.csv` — «сохранить превью как CSV».
///
/// Мелочь, но она снимает главный вопрос сырого слоя: «а точно так прочиталось?».
pub(crate) async fn preview_csv(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
    Query(query): Query<EntityQuery>,
) -> WebResult<Response> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = require_entity(&dir, &entity)?;
    let name = parse_file_name(&query.file).map_err(WebError::bad_request)?;
    let known = api::entity_files(&dir, &info.entity)?
        .into_iter()
        .any(|(listed, _, _)| listed == name);
    if !known {
        return Err(WebError::not_found(format!(
            "file '{name}' is not part of entity '{entity}'"
        )));
    }
    let (options, _) = reader_options(&dir, &info);
    let preview = strata_core::preview_source_with(&info.folder.join(&name), PREVIEW_ROWS, options)
        .map_err(|error| WebError::bad_request(format!("cannot read this file: {error}")))?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"preview.csv\"",
            ),
        ],
        preview_to_csv(&preview),
    )
        .into_response())
}

/// Превью в CSV (кавычки удваиваются — обычное правило CSV).
fn preview_to_csv(preview: &Preview) -> String {
    let mut out = String::new();
    let header_row: Vec<String> = preview
        .columns
        .iter()
        .map(|column| csv_cell(&column.name))
        .collect();
    out.push_str(&header_row.join(","));
    out.push('\n');
    for row in &preview.rows {
        let cells: Vec<String> = row.iter().map(|cell| csv_cell(cell)).collect();
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    out
}

/// Одна ячейка CSV: кавычим, если значение содержит разделитель, кавычку или перевод строки.
fn csv_cell(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

// ---------------------------------------------------------------------------
// Стадия 2: схема
// ---------------------------------------------------------------------------

/// Панель стадии Schema: черновик колонок, переинференс и подтверждение.
#[derive(Template)]
#[template(path = "fragments/schema_panel.html")]
struct SchemaPanelFragment {
    slug: String,
    entity: String,
    confirmed: bool,
    dirty: bool,
    has_notice: bool,
    notice: String,
    has_error: bool,
    error: String,
    columns: Vec<ColumnRowView>,
    rules_count: usize,
}

/// Пункт `<select>` типа колонки: метка ядра и признак «выбрано сейчас».
///
/// `selected` считается в Rust, а не в шаблоне: сравнение `&String` с `String`
/// внутри выражений askama — источник тонких ошибок компиляции.
#[derive(Debug, Clone)]
struct DtypeOption {
    /// Метка типа (`i64`, `str`, …) — значение поля формы.
    value: String,
    /// Совпадает ли с текущим типом колонки.
    selected: bool,
}

/// Строка таблицы схемы: имя колонки и варианты её типа.
#[derive(Debug, Clone)]
struct ColumnRowView {
    /// Имя колонки.
    name: String,
    /// Варианты `<select>`: метки, которые понимает ядро.
    dtype_options: Vec<DtypeOption>,
}

impl ColumnRowView {
    /// Строка таблицы по колонке черновика.
    fn new(column: &ColumnDef) -> Self {
        ColumnRowView {
            name: column.name.clone(),
            dtype_options: DTYPE_LABELS
                .iter()
                .map(|label| DtypeOption {
                    value: (*label).to_string(),
                    selected: *label == column.dtype,
                })
                .collect(),
        }
    }
}

/// Собрать панель Schema из черновика + текущего контракта ядра.
fn schema_panel(
    state: &AppState,
    user_id: i64,
    slug: &str,
    dir: &Path,
    info: &api::EntityInfo,
    notice: Option<String>,
    error: Option<String>,
) -> WebResult<SchemaPanelFragment> {
    let key = draft_key(user_id, slug, &info.entity);
    let view = api::entity_schema_view(dir, &info.entity)?;
    let draft = ensure_draft(state, &key, dir, &info.entity)?;
    Ok(SchemaPanelFragment {
        slug: slug.to_string(),
        entity: info.entity.clone(),
        confirmed: view.confirmed,
        dirty: draft.columns != view.columns || draft.rules != view.rules,
        has_notice: notice.is_some(),
        notice: notice.unwrap_or_default(),
        has_error: error.is_some(),
        error: error.unwrap_or_default(),
        rules_count: draft.rules.len(),
        columns: draft.columns.iter().map(ColumnRowView::new).collect(),
    })
}

/// Ответ на «Confirm schema»: обновлённая панель + таб-полоска (htmx OOB).
#[derive(Template)]
#[template(path = "fragments/schema_saved.html")]
struct SchemaSavedFragment {
    slug: String,
    entity: String,
    tabs: Vec<TabView>,
    /// Готовый HTML панели схемы.
    panel: String,
}

/// Таб-полоска после изменения контракта (статусы могли поменяться).
///
/// `active` — стадия, на которой пользователь сейчас: полоска подменяется целиком,
/// поэтому подсветка должна остаться на месте (подтвердили схему — остались в
/// Schema, сохранили правила — в Rules).
fn tabs_for(dir: &Path, slug: &str, entity: &str, active: Tab) -> Vec<TabView> {
    let _ = slug;
    let status = entity_status(dir, entity, entity_has_schema(dir, entity));
    status.tabs(active)
}

/// Сохранить черновик в `schemas/<entity>.toml` (единственная запись схемы).
fn persist_draft(
    dir: &Path,
    info: &api::EntityInfo,
    options: ReaderOptions,
    draft: &Draft,
) -> WebResult<()> {
    api::save_entity_schema(
        dir,
        &info.entity,
        &info.folder,
        draft.columns.clone(),
        draft.rules.clone(),
        options,
    )?;
    Ok(())
}

/// `POST /w/{slug}/e/{entity}/schema/infer` — сбросить черновик к контракту ядра.
pub(crate) async fn schema_infer(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = require_entity(&dir, &entity)?;
    let key = draft_key(user_id, &slug, &info.entity);

    // «Re-infer» = заново взять предложение движка: черновик сбрасывается, а
    // панель тут же читает свежий `entity_schema_view` (и снова его положит).
    let view = api::entity_schema_view(&dir, &info.entity)?;
    write_draft(
        &state,
        &key,
        Draft {
            columns: view.columns,
            rules: view.rules,
        },
    )?;
    let panel = schema_panel(
        &state,
        user_id,
        &slug,
        &dir,
        &info,
        Some(String::from(
            "Draft reset to the engine's current view (confirmed schema, or a fresh inference).",
        )),
        None,
    )?;
    Ok(Html(panel.render().map_err(render_error)?))
}

/// `POST /w/{slug}/e/{entity}/schema/column` — правка колонки в черновике.
///
/// Действия: `rename`, `type`, `delete`, `add`. На диск ничего не пишется:
/// контракт фиксируется только кнопкой «Confirm schema».
pub(crate) async fn schema_column(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
    Form(form): Form<ColumnForm>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = require_entity(&dir, &entity)?;
    let key = draft_key(user_id, &slug, &info.entity);
    let mut draft = ensure_draft(&state, &key, &dir, &info.entity)?;
    let mut notice = None;
    let mut error = None;

    match form.action.trim() {
        "rename" => match parse_column_name(&form.name) {
            Ok(name) => match draft.columns.get_mut(form.index) {
                Some(column) => {
                    column.name = name;
                    notice = Some(String::from("Column renamed in the draft."));
                }
                None => error = Some(String::from("column no longer exists — reload the page")),
            },
            Err(message) => error = Some(message),
        },
        "type" => match parse_dtype(&form.dtype) {
            Ok(dtype) => match draft.columns.get_mut(form.index) {
                Some(column) => {
                    column.dtype = dtype;
                    notice = Some(String::from("Column type changed in the draft."));
                }
                None => error = Some(String::from("column no longer exists — reload the page")),
            },
            Err(message) => error = Some(message),
        },
        "delete" => {
            if form.index < draft.columns.len() {
                draft.columns.remove(form.index);
                // Правила удалённой колонки больше не к чему применять.
                let names: Vec<String> = draft
                    .columns
                    .iter()
                    .map(|column| column.name.clone())
                    .collect();
                draft.rules.retain(|rule| names.contains(&rule.column));
                notice = Some(String::from(
                    "Column removed from the draft (its rules went with it).",
                ));
            } else {
                error = Some(String::from("column no longer exists — reload the page"));
            }
        }
        "add" => {
            let mut index = draft.columns.len() + 1;
            let mut name = format!("column_{index}");
            while draft.columns.iter().any(|column| column.name == name) {
                index += 1;
                name = format!("column_{index}");
            }
            draft.columns.push(ColumnDef {
                name,
                dtype: String::from("str"),
            });
            notice = Some(String::from(
                "Column added to the draft — rename it and pick a type.",
            ));
        }
        other => error = Some(format!("unknown schema action '{other}'")),
    }

    if error.is_none() {
        write_draft(&state, &key, draft)?;
    }
    let panel = schema_panel(&state, user_id, &slug, &dir, &info, notice, error)?;
    Ok(Html(panel.render().map_err(render_error)?))
}

/// `POST /w/{slug}/e/{entity}/schema/confirm` — записать контракт в TOML.
pub(crate) async fn schema_confirm(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = require_entity(&dir, &entity)?;
    let key = draft_key(user_id, &slug, &info.entity);
    let view = api::entity_schema_view(&dir, &info.entity)?;
    let draft = ensure_draft(&state, &key, &dir, &info.entity)?;

    let (notice, error) = match persist_draft(&dir, &info, view.options, &draft) {
        Ok(()) => (
            Some(format!(
                "Schema saved to schemas/{}.toml — the contract is frozen.",
                info.entity
            )),
            None,
        ),
        Err(failure) => (None, Some(failure.message().to_string())),
    };

    let panel = schema_panel(&state, user_id, &slug, &dir, &info, notice, error)?;
    let fragment = SchemaSavedFragment {
        tabs: tabs_for(&dir, &slug, &info.entity, Tab::Schema),
        panel: panel.render().map_err(render_error)?,
        slug,
        entity: info.entity,
    };
    Ok(Html(fragment.render().map_err(render_error)?))
}

// ---------------------------------------------------------------------------
// Стадия 3: правила и проверка на образце
// ---------------------------------------------------------------------------

/// Строка таблицы правил.
#[derive(Debug, Clone)]
struct RuleRowView {
    /// Позиция правила в черновике (нужна кнопке удаления).
    index: usize,
    /// Колонка.
    column: String,
    /// Тип правила.
    kind: String,
    /// Параметры правила.
    params: String,
    /// Строгость.
    severity: String,
    /// Классы Tailwind для строгости.
    severity_class: String,
}

/// Панель стадии Rules: таблица правил и конструктор «колонка → тип → параметры».
#[derive(Template)]
#[template(path = "fragments/rules_panel.html")]
struct RulesPanelFragment {
    slug: String,
    entity: String,
    columns: Vec<OptionView>,
    rules: Vec<RuleRowView>,
    rule_types: Vec<OptionView>,
    severities: Vec<OptionView>,
    has_columns: bool,
    has_notice: bool,
    notice: String,
    has_error: bool,
    error: String,
}

/// Ответ на «Save rules»: обновлённая панель + таб-полоска (htmx OOB).
#[derive(Template)]
#[template(path = "fragments/rules_saved.html")]
struct RulesSavedFragment {
    slug: String,
    entity: String,
    tabs: Vec<TabView>,
    /// Готовый HTML панели правил.
    panel: String,
}

/// Собрать панель Rules из черновика.
fn rules_panel(
    state: &AppState,
    user_id: i64,
    slug: &str,
    dir: &Path,
    info: &api::EntityInfo,
    notice: Option<String>,
    error: Option<String>,
) -> WebResult<RulesPanelFragment> {
    let key = draft_key(user_id, slug, &info.entity);
    let draft = ensure_draft(state, &key, dir, &info.entity)?;
    let columns: Vec<OptionView> = draft
        .columns
        .iter()
        .map(|column| OptionView {
            value: column.name.clone(),
            label: format!("{} · {}", column.name, column.dtype),
        })
        .collect();
    let rules = draft
        .rules
        .iter()
        .enumerate()
        .map(|(index, rule)| RuleRowView {
            index,
            column: rule.column.clone(),
            kind: crate::forms::rule_kind_token(&rule.kind).to_string(),
            params: crate::forms::rule_params_text(&rule.kind),
            severity: severity_token(rule.severity).to_string(),
            severity_class: match rule.severity {
                strata_core::Severity::Error => TONE_BAD.to_string(),
                strata_core::Severity::Warning => TONE_WARN.to_string(),
            },
        })
        .collect();
    Ok(RulesPanelFragment {
        slug: slug.to_string(),
        entity: info.entity.clone(),
        has_columns: !columns.is_empty(),
        columns,
        rules,
        rule_types: RULE_TYPES
            .iter()
            .map(|(value, label)| OptionView {
                value: (*value).to_string(),
                label: (*label).to_string(),
            })
            .collect(),
        severities: SEVERITIES
            .iter()
            .map(|(value, label)| OptionView {
                value: (*value).to_string(),
                label: (*label).to_string(),
            })
            .collect(),
        has_notice: notice.is_some(),
        notice: notice.unwrap_or_default(),
        has_error: error.is_some(),
        error: error.unwrap_or_default(),
    })
}

/// `POST /w/{slug}/e/{entity}/rules` — добавить или удалить правило в черновике.
pub(crate) async fn rules_change(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
    Form(form): Form<RuleForm>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = require_entity(&dir, &entity)?;
    let key = draft_key(user_id, &slug, &info.entity);
    let mut draft = ensure_draft(&state, &key, &dir, &info.entity)?;
    let mut notice = None;
    let mut error = None;

    match form.action.trim() {
        "add" => match parse_rule(
            &form.column,
            &form.severity,
            &form.rule_type,
            &form.min,
            &form.max,
            &form.pattern,
            &form.format,
            &form.values,
        ) {
            Ok(rule) => {
                // Правило живёт в контексте колонки: колонка должна существовать в
                // черновике, иначе правило никогда ни к чему не применится.
                if draft
                    .columns
                    .iter()
                    .any(|column| column.name == rule.column)
                {
                    draft.rules.push(rule);
                    notice = Some(String::from(
                        "Rule added to the draft — check it on the sample, then save.",
                    ));
                } else {
                    error = Some(format!(
                        "column '{}' is not in the schema draft",
                        rule.column
                    ));
                }
            }
            Err(message) => error = Some(message),
        },
        "delete" => {
            if form.index < draft.rules.len() {
                draft.rules.remove(form.index);
                notice = Some(String::from("Rule removed from the draft."));
            } else {
                error = Some(String::from("rule no longer exists — reload the page"));
            }
        }
        other => error = Some(format!("unknown rules action '{other}'")),
    }

    if error.is_none() {
        write_draft(&state, &key, draft)?;
    }
    let panel = rules_panel(&state, user_id, &slug, &dir, &info, notice, error)?;
    Ok(Html(panel.render().map_err(render_error)?))
}

/// `POST /w/{slug}/e/{entity}/rules/save` — сохранить правила (и колонки) в TOML.
pub(crate) async fn rules_save(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = require_entity(&dir, &entity)?;
    let key = draft_key(user_id, &slug, &info.entity);
    let view = api::entity_schema_view(&dir, &info.entity)?;
    let draft = ensure_draft(&state, &key, &dir, &info.entity)?;

    let (notice, error) = match persist_draft(&dir, &info, view.options, &draft) {
        Ok(()) => (
            Some(format!(
                "Rules saved to schemas/{}.toml ({} rule(s)).",
                info.entity,
                draft.rules.len()
            )),
            None,
        ),
        Err(failure) => (None, Some(failure.message().to_string())),
    };

    let panel = rules_panel(&state, user_id, &slug, &dir, &info, notice, error)?;
    let fragment = RulesSavedFragment {
        tabs: tabs_for(&dir, &slug, &info.entity, Tab::Rules),
        panel: panel.render().map_err(render_error)?,
        slug,
        entity: info.entity,
    };
    Ok(Html(fragment.render().map_err(render_error)?))
}

/// Проверка одного файла на образце.
#[derive(Debug, Clone)]
struct VFileView {
    /// Имя файла.
    name: String,
    /// Сколько строк проверено.
    rows: usize,
    /// Проблема схемы (расхождение колонок/типов), если есть.
    has_issue: bool,
    /// Текст проблемы.
    issue: String,
    /// Строк с ошибками.
    errors: usize,
    /// Строк с предупреждениями.
    warnings: usize,
    /// Статистика по правилам файла.
    stats: Vec<String>,
}

/// Статистика одного правила в отчёте.
#[derive(Debug, Clone)]
struct VRuleView {
    /// Колонка.
    column: String,
    /// Правило.
    rule: String,
    /// Строгость.
    severity: String,
    /// Классы Tailwind.
    severity_class: String,
    /// Нарушений.
    violations: usize,
    /// Примеры значений.
    examples: String,
}

/// Фрагмент отчёта «проверить на образце».
#[derive(Template)]
#[template(path = "fragments/validation_report.html")]
struct ValidationFragment {
    slug: String,
    entity: String,
    tabs: Vec<TabView>,
    has_error: bool,
    error: String,
    checked: usize,
    errors: usize,
    warnings: usize,
    files: Vec<VFileView>,
    rules: Vec<VRuleView>,
}

/// `POST /w/{slug}/e/{entity}/validate` — «проверить на образце» (1000 строк).
///
/// Важная деталь: ядро проверяет **сохранённый** контракт (`validate_entity`
/// читает `schemas/<entity>.toml`), поэтому перед проверкой черновик
/// записывается — иначе пользователь проверял бы не то, что видит. Об этом
/// честно сказано в самом отчёте.
pub(crate) async fn validate_sample(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = require_entity(&dir, &entity)?;
    let key = draft_key(user_id, &slug, &info.entity);
    let view = api::entity_schema_view(&dir, &info.entity)?;
    let draft = ensure_draft(&state, &key, &dir, &info.entity)?;

    // Черновик → диск, чтобы проверялся именно он.
    if let Err(failure) = persist_draft(&dir, &info, view.options, &draft) {
        let fragment = ValidationFragment {
            slug,
            entity: info.entity,
            tabs: Vec::new(),
            has_error: true,
            error: format!(
                "cannot check the sample: {} (fix the schema draft first)",
                failure.message()
            ),
            checked: 0,
            errors: 0,
            warnings: 0,
            files: Vec::new(),
            rules: Vec::new(),
        };
        return Ok(Html(fragment.render().map_err(render_error)?));
    }

    let report = api::validate_entity(&dir, &info.entity, SAMPLE_ROWS)?;
    let files = report
        .files
        .iter()
        .map(|file| VFileView {
            name: file.name.clone(),
            rows: file.rows_checked,
            has_issue: file.column_issue.is_some(),
            issue: file.column_issue.clone().unwrap_or_default(),
            errors: file.rows_with_errors,
            warnings: file.rows_with_warnings,
            stats: file
                .rule_stats
                .iter()
                .map(|stat| {
                    format!(
                        "{} ({}) — {} violation(s)",
                        stat.rule, stat.column, stat.violations
                    )
                })
                .collect(),
        })
        .collect();
    let rules = report
        .rules
        .iter()
        .map(|stat| VRuleView {
            column: stat.column.clone(),
            rule: stat.rule.clone(),
            severity: severity_token(stat.severity).to_string(),
            severity_class: match stat.severity {
                strata_core::Severity::Error => TONE_BAD.to_string(),
                strata_core::Severity::Warning => TONE_WARN.to_string(),
            },
            violations: stat.violations,
            examples: stat.examples.join(", "),
        })
        .collect();

    let fragment = ValidationFragment {
        entity: info.entity.clone(),
        tabs: tabs_for(&dir, &slug, &info.entity, Tab::Rules),
        slug,
        has_error: false,
        error: String::new(),
        checked: report.rows_checked,
        errors: report.rows_with_errors,
        warnings: report.rows_with_warnings,
        files,
        rules,
    };
    Ok(Html(fragment.render().map_err(render_error)?))
}

// ---------------------------------------------------------------------------
// Стадия 4: ODS, карантин и история прогонов
// ---------------------------------------------------------------------------

/// Панель стадии ODS: манифест последнего прогона, карантин и история.
#[derive(Template)]
#[template(path = "fragments/ods_panel.html")]
struct OdsPanelFragment {
    slug: String,
    entity: String,
    can_run: bool,
    run_hint: String,
    has_run: bool,
    run_id: String,
    run_href: String,
    run_badge: String,
    run_badge_class: String,
    started: String,
    duration: String,
    rows_read: u64,
    rows_valid: u64,
    rows_quarantine: u64,
    rows_warning: u64,
    schema_hash: String,
    parts: Vec<PartView>,
    quarantine_parts: Vec<PartView>,
    rule_summaries: Vec<RuleSummaryView>,
    has_quarantine: bool,
    quarantine: Vec<QuarantineRowView>,
    quarantine_note: String,
    history: Vec<RunRowView>,
}

/// Собрать панель ODS по последнему прогону сущности.
fn ods_panel(slug: &str, dir: &Path, info: &api::EntityInfo) -> WebResult<OdsPanelFragment> {
    let runs = api::entity_runs(dir, &info.entity)?;
    let latest = runs.first().cloned();
    let history = runs
        .iter()
        .map(|manifest| run_row(slug, &info.entity, manifest))
        .collect();

    // Прогон требует подтверждённой схемы: кнопка выключена, пока её нет, и рядом
    // написано, что делать (правило «ошибка = что делать»).
    let can_run = info.has_schema;
    let run_hint = if can_run {
        String::from(
            "A run writes data/<entity>/runs/<run_id>/ — ODS parts, quarantine, manifest and log.",
        )
    } else {
        String::from("Confirm the schema first: a run needs a contract (Schema tab).")
    };

    let Some(manifest) = latest else {
        return Ok(OdsPanelFragment {
            slug: slug.to_string(),
            entity: info.entity.clone(),
            can_run,
            run_hint,
            has_run: false,
            run_id: String::new(),
            run_href: String::new(),
            run_badge: String::new(),
            run_badge_class: String::new(),
            started: String::new(),
            duration: String::new(),
            rows_read: 0,
            rows_valid: 0,
            rows_quarantine: 0,
            rows_warning: 0,
            schema_hash: String::new(),
            parts: Vec::new(),
            quarantine_parts: Vec::new(),
            rule_summaries: Vec::new(),
            has_quarantine: false,
            quarantine: Vec::new(),
            quarantine_note: String::from("No run yet — nothing has been read into ODS."),
            history,
        });
    };

    let (badge, badge_class) = run_badge(&manifest);
    let quarantine = api::entity_quarantine(dir, &info.entity, &manifest.run_id, TABLE_LIMIT)
        .map(quarantine_views)
        .unwrap_or_default();
    let quarantine_note = if quarantine.is_empty() {
        String::from("Quarantine is empty for this run.")
    } else {
        format!(
            "First {} row(s) of the quarantine — every rejected row keeps its reason.",
            quarantine.len()
        )
    };

    Ok(OdsPanelFragment {
        slug: slug.to_string(),
        entity: info.entity.clone(),
        can_run,
        run_hint,
        has_run: true,
        run_href: run_href(slug, &info.entity, &manifest.run_id),
        run_id: manifest.run_id.clone(),
        run_badge: badge,
        run_badge_class: badge_class,
        started: manifest.started_utc.clone(),
        duration: format_duration(manifest.duration_ms),
        rows_read: manifest.rows_read,
        rows_valid: manifest.rows_valid,
        rows_quarantine: manifest.rows_quarantine,
        rows_warning: manifest.rows_warning,
        schema_hash: manifest.schema_hash.clone(),
        parts: part_views(&manifest.parts),
        quarantine_parts: part_views(&manifest.quarantine_parts),
        rule_summaries: rule_summary_views(&manifest.rules),
        has_quarantine: !quarantine.is_empty(),
        quarantine,
        quarantine_note,
        history,
    })
}

/// Финальный фрагмент прогона — то, что видит браузер после поллинга.
#[derive(Template)]
#[template(path = "fragments/run_result.html")]
pub(crate) struct RunResultFragment {
    pub(crate) slug: String,
    pub(crate) entity: String,
    pub(crate) run_id: String,
    pub(crate) run_href: String,
    pub(crate) run_badge: String,
    pub(crate) run_badge_class: String,
    pub(crate) duration: String,
    pub(crate) rows_read: u64,
    pub(crate) rows_valid: u64,
    pub(crate) rows_quarantine: u64,
    pub(crate) rows_warning: u64,
    pub(crate) parts: usize,
    pub(crate) quarantine_parts: usize,
    pub(crate) files: usize,
}

/// Собрать финальный фрагмент прогона из результата ядра.
pub(crate) fn run_result_fragment(
    slug: &str,
    outcome: &strata_core::RunOutcome,
) -> WebResult<RunResultFragment> {
    let manifest = &outcome.manifest;
    let (badge, badge_class) = run_badge(manifest);
    Ok(RunResultFragment {
        slug: slug.to_string(),
        entity: manifest.entity.clone(),
        run_id: manifest.run_id.clone(),
        run_href: run_href(slug, &manifest.entity, &manifest.run_id),
        run_badge: badge,
        run_badge_class: badge_class,
        duration: format_duration(manifest.duration_ms),
        rows_read: manifest.rows_read,
        rows_valid: manifest.rows_valid,
        rows_quarantine: manifest.rows_quarantine,
        rows_warning: manifest.rows_warning,
        parts: manifest.parts.len(),
        quarantine_parts: manifest.quarantine_parts.len(),
        files: manifest.files.len(),
    })
}

/// `POST /w/{slug}/e/{entity}/runs` — запустить прогон **в фоне**.
///
/// Прогон читает все файлы, применяет правила и пишет Parquet — долгая
/// CPU/IO-операция. Она уезжает в реестр фоновых задач, а браузер получает
/// фрагмент прогресса с поллингом (`/w/{slug}/jobs/{job_id}`).
pub(crate) async fn start_run(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = require_entity(&dir, &entity)?;

    // Кнопка в UI выключена, но проверяем и здесь: это граница доверия.
    if !info.has_schema {
        let fragment = ErrorFragment {
            message: format!(
                "entity '{}' has no confirmed schema — confirm it on the Schema tab before running",
                info.entity
            ),
        };
        return Ok(Html(fragment.render().map_err(render_error)?));
    }

    let task_dir = dir.clone();
    let task_entity = info.entity.clone();
    let job_id = crate::spawn_job(&state, info.entity.clone(), "run…", move |progress| {
        let outcome =
            api::run_entity_now(&task_dir, &task_entity, |done, total| progress(done, total))?;
        Ok(crate::JobResult::Run(Box::new(outcome)))
    })?;

    let fragment = JobRunningFragment {
        slug,
        job_id,
        entity: info.entity,
        label: String::from("run…"),
        done: 0,
        total: 0,
        percent: 0,
    };
    Ok(Html(fragment.render().map_err(render_error)?))
}

/// Панель стадии Logs: список прогонов и хвост лога выбранного.
#[derive(Template)]
#[template(path = "fragments/logs_panel.html")]
struct LogsPanelFragment {
    slug: String,
    entity: String,
    runs: Vec<RunRowView>,
    selected: String,
    has_runs: bool,
    has_log: bool,
    log_error: String,
    lines: Vec<String>,
}

/// Собрать панель Logs: `selected` — `run_id` или пустая строка (значит последний).
fn logs_panel(
    dir: &Path,
    slug: &str,
    info: &api::EntityInfo,
    selected: &str,
) -> WebResult<LogsPanelFragment> {
    let runs = api::entity_runs(dir, &info.entity)?;

    // Выбранный прогон должен существовать: иначе показываем последний.
    let chosen = if selected.trim().is_empty() {
        runs.first().map(|manifest| manifest.run_id.clone())
    } else {
        let wanted = parse_path_token(selected, "run").map_err(WebError::bad_request)?;
        runs.iter()
            .find(|manifest| manifest.run_id == wanted)
            .map(|manifest| manifest.run_id.clone())
    };

    // Список прогонов строим сразу с отметкой выбранного: сравнение значений
    // делает Rust, а шаблон только рисует `selected`.
    let history: Vec<RunRowView> = runs
        .iter()
        .map(|manifest| {
            let mut row = run_row(slug, &info.entity, manifest);
            row.selected = chosen.as_deref() == Some(manifest.run_id.as_str());
            row
        })
        .collect();

    let Some(run_id) = chosen else {
        return Ok(LogsPanelFragment {
            slug: slug.to_string(),
            entity: info.entity.clone(),
            runs: history,
            selected: String::new(),
            has_runs: false,
            has_log: false,
            log_error: String::from("No runs yet — the log appears after the first run."),
            lines: Vec::new(),
        });
    };

    match api::entity_run_log(dir, &info.entity, &run_id, TABLE_LIMIT) {
        Ok(lines) => Ok(LogsPanelFragment {
            slug: slug.to_string(),
            entity: info.entity.clone(),
            runs: history,
            selected: run_id,
            has_runs: true,
            has_log: true,
            log_error: String::new(),
            lines,
        }),
        Err(error) => Ok(LogsPanelFragment {
            slug: slug.to_string(),
            entity: info.entity.clone(),
            runs: history,
            selected: run_id,
            has_runs: true,
            has_log: false,
            log_error: error.message().to_string(),
            lines: Vec::new(),
        }),
    }
}

/// `POST /w/{slug}/e/{entity}/logs` — переключить прогон на вкладке Logs.
pub(crate) async fn logs_select(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
    Form(form): Form<RunPickForm>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = require_entity(&dir, &entity)?;
    let panel = logs_panel(&dir, &slug, &info, &form.run_id)?;
    Ok(Html(panel.render().map_err(render_error)?))
}

// ---------------------------------------------------------------------------
// Страница прогона
// ---------------------------------------------------------------------------

/// Строка таблицы файлов внутри прогона.
#[derive(Debug, Clone)]
struct FileRunView {
    /// Имя исходного файла.
    name: String,
    /// Прочитано / в ODS / в карантин / с предупреждениями.
    counts: String,
    /// Есть ли ошибка обработки файла.
    has_error: bool,
    /// Текст ошибки.
    error: String,
}

/// Страница одного прогона: манифест целиком + карантин + хвост лога.
#[derive(Template)]
#[template(path = "run.html")]
struct RunPageTemplate {
    slug: String,
    entity: String,
    ws_name: String,
    run_id: String,
    run_badge: String,
    run_badge_class: String,
    started: String,
    finished: String,
    duration: String,
    rows_read: u64,
    rows_valid: u64,
    rows_quarantine: u64,
    rows_warning: u64,
    schema_hash: String,
    ods_dir: String,
    files: Vec<FileRunView>,
    parts: Vec<PartView>,
    quarantine_parts: Vec<PartView>,
    rule_summaries: Vec<RuleSummaryView>,
    has_quarantine: bool,
    quarantine: Vec<QuarantineRowView>,
    has_log: bool,
    log_error: String,
    lines: Vec<String>,
}

/// `GET /w/{slug}/e/{entity}/runs/{run_id}` — страница прогона.
///
/// Прогон неизменяем: страница читает `runs/<run_id>/` (манифест, карантин, лог),
/// поэтому на неё можно ссылаться из тикета и месяцы спустя.
pub(crate) async fn run_page(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    AxumPath((slug, entity, run_id)): AxumPath<(String, String, String)>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = require_entity(&dir, &entity)?;
    let run_id = parse_path_token(&run_id, "run").map_err(WebError::bad_request)?;
    let workspace = api::open_workspace_at(&dir)?;

    let manifest = api::entity_run_manifest(&dir, &info.entity, &run_id)
        .map_err(|error| WebError::not_found(error.message().to_string()))?;
    let (badge, badge_class) = run_badge(&manifest);
    let quarantine = api::entity_quarantine(&dir, &info.entity, &run_id, TABLE_LIMIT)
        .map(quarantine_views)
        .unwrap_or_default();
    let (has_log, log_error, lines) =
        match api::entity_run_log(&dir, &info.entity, &run_id, TABLE_LIMIT) {
            Ok(lines) => (true, String::new(), lines),
            Err(error) => (false, error.message().to_string(), Vec::new()),
        };
    let ods_dir = workspace.data_dir.join(&info.entity).display().to_string();
    let files = manifest
        .files
        .iter()
        .map(|file| FileRunView {
            name: file.name.clone(),
            counts: format!(
                "{} read · {} in ODS · {} quarantine · {} warning",
                file.rows_read, file.rows_valid, file.rows_quarantine, file.rows_warning
            ),
            has_error: file.error.is_some(),
            error: file.error.clone().unwrap_or_default(),
        })
        .collect();

    let page = RunPageTemplate {
        slug,
        entity: info.entity,
        ws_name: workspace.name,
        run_id: manifest.run_id,
        run_badge: badge,
        run_badge_class: badge_class,
        started: manifest.started_utc,
        finished: manifest.finished_utc,
        duration: format_duration(manifest.duration_ms),
        rows_read: manifest.rows_read,
        rows_valid: manifest.rows_valid,
        rows_quarantine: manifest.rows_quarantine,
        rows_warning: manifest.rows_warning,
        schema_hash: manifest.schema_hash,
        ods_dir,
        files,
        parts: part_views(&manifest.parts),
        quarantine_parts: part_views(&manifest.quarantine_parts),
        rule_summaries: rule_summary_views(&manifest.rules),
        has_quarantine: !quarantine.is_empty(),
        quarantine,
        has_log,
        log_error,
        lines,
    };
    Ok(Html(page.render().map_err(render_error)?))
}

// ---------------------------------------------------------------------------
// Страница сущности
// ---------------------------------------------------------------------------

/// Параметры страницы сущности: активный таб, выбранный прогон и файл превью.
#[derive(Debug, Deserialize)]
pub(crate) struct EntityQuery {
    /// `?tab=files|schema|rules|ods|logs`.
    #[serde(default)]
    pub(crate) tab: String,
    /// `?run=<run_id>` — какой прогон открыть на вкладке Logs.
    #[serde(default)]
    pub(crate) run: String,
    /// `?file=<name>` — какой файл показать в превью (и выгрузить как CSV).
    #[serde(default)]
    pub(crate) file: String,
}

/// Страница сущности: крошки, таб-полоска со статусами и активная стадия.
#[derive(Template)]
#[template(path = "entity.html")]
struct EntityPageTemplate {
    slug: String,
    entity: String,
    ws_name: String,
    folder: String,
    tab: String,
    tabs: Vec<TabView>,
    /// Готовый HTML активного таба (тот же фрагмент, что возвращает htmx).
    body: String,
}

/// `GET /w/{slug}/e/{entity}` — главный экран работы с одной таблицей.
///
/// Без `?tab=` открывается **первая незавершённая стадия**: пользователь не
/// выбирает таб вручную, а сразу видит, где пайплайн застрял.
pub(crate) async fn entity_page(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    AxumPath((slug, entity)): AxumPath<(String, String)>,
    Query(query): Query<EntityQuery>,
) -> WebResult<Html<String>> {
    let user_id = signed_in(&session).await?;
    let dir = require_workspace(&state, user_id, &slug)?;
    let info = require_entity(&dir, &entity)?;
    let workspace = api::open_workspace_at(&dir)?;
    let status = entity_status(&dir, &info.entity, info.has_schema);
    let active = Tab::parse(&query.tab).unwrap_or_else(|| status.first_unfinished());
    let tabs = status.tabs(active);

    let body = match active {
        Tab::Files => files_panel(&slug, &dir, &info, &query.file)?
            .render()
            .map_err(render_error)?,
        Tab::Schema => schema_panel(&state, user_id, &slug, &dir, &info, None, None)?
            .render()
            .map_err(render_error)?,
        Tab::Rules => rules_panel(&state, user_id, &slug, &dir, &info, None, None)?
            .render()
            .map_err(render_error)?,
        Tab::Ods => ods_panel(&slug, &dir, &info)?
            .render()
            .map_err(render_error)?,
        Tab::Logs => logs_panel(&dir, &slug, &info, &query.run)?
            .render()
            .map_err(render_error)?,
    };

    let page = EntityPageTemplate {
        slug,
        entity: info.entity,
        ws_name: workspace.name,
        folder: info.folder.display().to_string(),
        tab: active.key().to_string(),
        tabs,
        body,
    };
    Ok(Html(page.render().map_err(render_error)?))
}

// ---------------------------------------------------------------------------
// Тесты: чистые помощники этого модуля
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Временный CSV с кавычками и запятыми — крайние случаи для выгрузки CSV.
    fn write_temp_csv() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "strata_web_preview_{}_{}.csv",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&path, "id,note\n1,\"he said \"\"hi\"\"\"\n").expect("csv");
        path
    }

    /// Превью из ядра + строка с запятой внутри значения.
    fn sample_preview() -> Preview {
        let path = write_temp_csv();
        let mut preview =
            strata_core::preview_source_with(&path, 10, ReaderOptions::default()).expect("превью");
        preview
            .rows
            .push(vec![String::from("2"), String::from("with,comma")]);
        let _ = std::fs::remove_file(&path);
        preview
    }

    #[test]
    fn csv_cells_are_quoted_only_when_needed() {
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell("with,comma"), "\"with,comma\"");
        assert_eq!(csv_cell("with \"quote\""), "\"with \"\"quote\"\"\"");
        assert_eq!(csv_cell("two\nlines"), "\"two\nlines\"");
    }

    #[test]
    fn preview_export_has_header_and_rows() {
        let csv = preview_to_csv(&sample_preview());
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "id,note");
        assert!(lines.len() >= 2, "ожидали хотя бы одну строку данных");
        assert!(csv.ends_with('\n'));
        assert!(
            csv.contains("\"with,comma\""),
            "значение с запятой закавычено"
        );
    }

    #[test]
    fn reader_options_are_described_for_the_files_tab() {
        let auto = reader_options_text(ReaderOptions::default());
        assert!(auto.contains("encoding auto"));
        assert!(auto.contains("delimiter auto"));
        assert!(auto.contains("header yes"));

        let forced = reader_options_text(ReaderOptions {
            encoding: None,
            delimiter: Some(';'),
            has_header: false,
        });
        assert!(forced.contains("delimiter ';'"));
        assert!(forced.contains("header no"));
    }

    /// Манифест-заготовка: остальные поля не влияют на проверяемую логику.
    fn manifest(has_errors: bool) -> RunManifest {
        RunManifest {
            run_id: String::from("run-20260101-120000.000"),
            entity: String::from("sales"),
            schema_hash: String::from("abc123"),
            started_utc: String::from("2026-01-01T12:00:00Z"),
            finished_utc: String::from("2026-01-01T12:00:02Z"),
            duration_ms: 2140,
            files: Vec::new(),
            rows_read: 5,
            rows_valid: 3,
            rows_quarantine: 2,
            rows_warning: 1,
            parts: Vec::new(),
            quarantine_parts: Vec::new(),
            rules: Vec::new(),
            has_errors,
        }
    }

    #[test]
    fn badge_says_ready_only_without_errors() {
        let (badge, class) = run_badge(&manifest(true));
        assert!(badge.contains("Has errors"), "{badge}");
        assert_eq!(class, TONE_BAD);

        let clean = RunManifest {
            has_errors: false,
            rows_quarantine: 0,
            ..manifest(true)
        };
        let (badge, class) = run_badge(&clean);
        assert!(badge.contains("Ready to serve"), "{badge}");
        assert_eq!(class, TONE_OK);
    }

    #[test]
    fn run_rows_link_to_the_run_page() {
        let row = run_row("sales", "orders", &manifest(false));
        assert_eq!(row.href, "/w/sales/e/orders/runs/run-20260101-120000.000");
        assert_eq!(row.duration, "2.14 s");
        assert_eq!(row.status, "ok");
        assert!(row.counts.contains("5 read"));
    }
}
