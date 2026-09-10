//! # `strata_core::api` — стабильный прикладной API движка
//!
//! Всё, что выше движка (сегодня — веб-сервис на axum; вчера — десктоп на
//! Dioxus), общается **только с этим модулем**. Это намеренный фасад: на входе
//! и выходе простые Rust-типы, один тип ошибки, а типы Polars/Arrow и файловые
//! дескрипторы наверх не протекают.
//!
//! ```text
//!  axum handlers (strata-web)  ──►  strata_core::api  ──►  внутренности движка
//!  (или любой будущий фронтенд)     (этот модуль)         (polars, fs, toml)
//! ```
//!
//! Зачем он нужен:
//! * **Заменяемые фронтенды.** Dioxus-десктоп удалён, см. `docs/archive`; если
//!   axum+htmx однажды заменят на что-то другое, стабильным должен остаться
//!   только этот фасад.
//! * **Одно место для политики.** Allowlist корневых источников, именование
//!   воркспейсов, именование файлов схем и семантика ошибок ETL живут здесь,
//!   а не в хендлерах.
//! * **Тестируемость без HTTP.** Всё, что ниже, — чистая функция от путей;
//!   веб-слой — лишь тонкий адаптер над этим.
//!
//! Правило именования в одном предложении: у *сущности* есть папка
//! (uploads/каталог-источник) и файл схемы `schemas/<entity>.schema.toml`;
//! застейдженные Parquet-части сущности лежат в `<workspace>/data/<entity>/`.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::quality::{ColumnRule, RuleStats};
use crate::run::{QuarantineRow, RunManifest, RunOutcome};
use crate::{
    ColumnDef, FolderReport, ReaderOptions, SchemaFile, WorkspaceConfig, candidate_entity_name,
    create_workspace, data_dir, delimiter_from_token, encoding_from_token, folder_to_parquet,
    list_entity_candidates, list_parts, open_workspace, save_schema, scan_folder,
    schema_from_folder, upsert_binding,
};

// ---------------------------------------------------------------------------
// Тип ошибки: одна плоская, отображаемая ошибка для вызывающих
// ---------------------------------------------------------------------------

/// Любая ошибка, которую может сообщить прикладной API.
///
/// Внутри он намеренно *строковый*: фронтенды только показывают сообщение или
/// превращают его в код статуса, а богатые типы ошибок движка остаются приватными.
#[derive(Debug, Clone)]
pub struct ApiError {
    message: String,
}

impl ApiError {
    /// Собирает ошибку из чего угодно, что умеет отображаться.
    pub fn new(message: impl Into<String>) -> Self {
        ApiError {
            message: message.into(),
        }
    }

    /// Человекочитаемая причина (безопасно показывать в UI).
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

/// Сокращение, используемое каждой функцией этого модуля.
pub type ApiResult<T> = Result<T, ApiError>;

/// Превращает любую ошибку движка в [`ApiError`].
fn err(context: &str, error: impl fmt::Display) -> ApiError {
    ApiError::new(format!("{context}: {error}"))
}

// ---------------------------------------------------------------------------
// DTO (data transfer objects) — то, что фронтенды реально отрисовывают
// ---------------------------------------------------------------------------

/// Воркспейс в том виде, в каком он нужен UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInfo {
    /// Директория, где лежат `workspace.toml`, `schemas/`, `data/`.
    pub dir: PathBuf,
    /// Человеческое имя из конфига.
    pub name: String,
    /// Абсолютная директория data, куда пишутся застейдженные Parquet-части.
    pub data_dir: PathBuf,
}

/// Привязка папка→сущность, с удобным флагом для UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityInfo {
    /// Имя сущности (логической таблицы).
    pub entity: String,
    /// Привязанная папка-источник.
    pub folder: PathBuf,
    /// `true`, когда подтверждённый файл схемы есть на диске.
    pub has_schema: bool,
}

/// Сущность-кандидат, найденная внутри scan root (режим B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateInfo {
    /// Предлагаемое имя сущности (имя папки).
    pub entity: String,
    /// Путь папки-кандидата.
    pub folder: PathBuf,
    /// Сколько файлов успешно просмотрено.
    pub files: usize,
    /// Сколько колонок в предложенной схеме.
    pub columns: usize,
    /// Сколько конфликтов типов найдено между файлами.
    pub conflicts: usize,
}

/// Итог staging одной сущности.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageOutcome {
    /// Сколько файлов застейджено в части.
    pub staged_files: usize,
    /// Всего записано строк данных.
    pub rows: u64,
    /// Сколько файлов отклонено (несоответствие схеме, нечитаемые, …).
    pub skipped_files: usize,
    /// Причины по пропущенным файлам (для панели отчёта).
    pub skipped_reasons: Vec<String>,
    /// Сколько файлов-частей Parquet сейчас лежит в директории датасета.
    pub parts: usize,
    /// Директория датасета.
    pub dataset_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Воркспейсы
// ---------------------------------------------------------------------------

/// Перечисляет воркспейсы внутри `root` (директории с `workspace.toml`).
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

/// Создаёт воркспейс с именем `name` под `root` и возвращает его описание.
///
/// Имя директории — это slug из `name`; если такая директория занята, к ней
/// добавляется числовой суффикс (`sales-2026-2`, …), чтобы создание никогда не
/// перезаписывало существующее.
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

/// Загружает воркспейс по его директории (используется всеми остальными вызовами).
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
// Сущности: scan roots, подтверждение, листинг
// ---------------------------------------------------------------------------

/// Смотрит на **прямые подпапки** `root` как на сущности-кандидаты.
///
/// Только чтение: в воркспейс ничего не пишется (правило «сначала предложить,
/// потом подтвердить»). Каждый кандидат несёт краткую сводку для карточки в UI.
pub fn scan_candidates(root: &Path) -> ApiResult<Vec<CandidateInfo>> {
    let folders = list_entity_candidates(root).map_err(|e| err("cannot scan root", e))?;
    let mut candidates = Vec::with_capacity(folders.len());
    for folder in folders {
        // Папка без читаемых файлов всё равно показывается (с нулями), чтобы
        // пользователь увидел «папка пуста или не та», а не тишину.
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

/// Подтверждает кандидата: выводит схему, сохраняет `schemas/<entity>.toml` и
/// привязывает папку к сущности в `workspace.toml`.
///
/// Это единственное место, которое *записывает* схему, и это всегда явное
/// действие пользователя (см. правило проекта «сначала предложить, потом
/// подтвердить»).
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

    // Движок называет файлы схем по *источнику*, поэтому сохраняем, а затем
    // переименовываем в имя сущности: поиск привязки должен быть предсказуемым
    // (`<entity>.toml`).
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

/// Сущности, привязанные в воркспейсе сейчас, с признаком наличия схемы.
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

/// Файлы, найденные в папке сущности (листинг для детального вида).
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

/// Стейджит сущность **по её подтверждённой схеме** (staging с валидацией схемы).
///
/// Если файл схемы есть, несоответствующие файлы отклоняются с причинами
/// (никогда не стейджатся молча). Без схемы мы откатываемся к обычному сырому
/// staging, чтобы пользователь всё равно получил Parquet из папки.
pub fn stage_entity(workspace_dir: &Path, entity: &str) -> ApiResult<StageOutcome> {
    stage_entity_with_progress(workspace_dir, entity, |_, _| {})
}

/// То же, что [`stage_entity`], но сообщает `(done_files, total_files)` по ходу
/// работы. Вызывающие, которые должны оставаться отзывчивыми (веб-сервис),
/// запускают это в фоновом потоке и стримят прогресс в браузер.
pub fn stage_entity_with_progress<F>(
    workspace_dir: &Path,
    entity: &str,
    mut on_progress: F,
) -> ApiResult<StageOutcome>
where
    F: FnMut(usize, usize),
{
    let config = load_config(workspace_dir)?;
    let binding = config
        .bindings
        .iter()
        .find(|b| b.entity == entity)
        .ok_or_else(|| ApiError::new(format!("unknown entity '{entity}'")))?;

    let folder = PathBuf::from(&binding.folder);
    let dest = data_dir(workspace_dir, &config).join(entity);

    let report: FolderReport = match crate::load_schema(workspace_dir, &format!("{entity}.toml")) {
        Ok(schema) => {
            crate::stage_folder_with_schema_progress(&folder, &dest, &schema, &mut on_progress)
                .map_err(|e| err("staging failed", e))?
        }
        // Схема ещё не подтверждена: обычный сырой staging (без колбэка на файл).
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
// Внутреннее
// ---------------------------------------------------------------------------

/// Читает конфиг воркспейса (используется несколькими сценариями).
fn load_config(workspace_dir: &Path) -> ApiResult<WorkspaceConfig> {
    open_workspace(workspace_dir)
        .map_err(|e| err("cannot read workspace.toml", e))?
        .ok_or_else(|| ApiError::new("workspace.toml is missing"))
}

/// Имена сущностей становятся именами файлов — всё похожее на путь отвергаем.
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

/// Превращает имя воркспейса в безопасный для файловой системы slug.
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

// ---------------------------------------------------------------------------
// Схема, правила и проверка на образце (стадии Schema и Rules)
// ---------------------------------------------------------------------------

/// Текущий контракт сущности: колонки, правила и опции чтения.
#[derive(Debug, Clone, PartialEq)]
pub struct EntitySchemaView {
    /// Подтверждена ли схема (есть ли `schemas/<entity>.toml`).
    pub confirmed: bool,
    /// Колонки по порядку.
    pub columns: Vec<ColumnDef>,
    /// Правила качества.
    pub rules: Vec<ColumnRule>,
    /// Опции чтения, сохранённые в схеме.
    pub options: ReaderOptions,
    /// Папка-источник, привязанная к сущности.
    pub folder: PathBuf,
}

/// Прочитать контракт сущности: подтверждённую схему или предложение инференса.
pub fn entity_schema_view(workspace_dir: &Path, entity: &str) -> ApiResult<EntitySchemaView> {
    let config = load_config(workspace_dir)?;
    let binding = config
        .bindings
        .iter()
        .find(|b| b.entity == entity)
        .ok_or_else(|| ApiError::new(format!("неизвестная сущность '{entity}'")))?;
    let folder = PathBuf::from(&binding.folder);

    if let Ok(schema) = crate::load_schema(workspace_dir, &format!("{entity}.toml")) {
        return Ok(EntitySchemaView {
            confirmed: true,
            columns: schema.columns,
            rules: schema.quality,
            options: ReaderOptions {
                encoding: encoding_from_token(&schema.encoding),
                delimiter: delimiter_from_token(&schema.delimiter),
                has_header: schema.has_header,
            },
            folder,
        });
    }

    let report = schema_from_folder(&folder, ReaderOptions::default())
        .map_err(|e| err("не удалось вывести схему", e))?;
    Ok(EntitySchemaView {
        confirmed: false,
        columns: report
            .columns
            .into_iter()
            .map(|c| ColumnDef {
                name: c.name,
                dtype: c.dtype,
            })
            .collect(),
        rules: Vec::new(),
        options: ReaderOptions::default(),
        folder,
    })
}

/// Сохранить подтверждённую схему: колонки + правила + опции чтения.
pub fn save_entity_schema(
    workspace_dir: &Path,
    entity: &str,
    folder: &Path,
    columns: Vec<ColumnDef>,
    rules: Vec<ColumnRule>,
    options: ReaderOptions,
) -> ApiResult<()> {
    let entity = validate_entity_name(entity)?;
    if columns.is_empty() {
        return Err(ApiError::new("схема без колонок не имеет смысла"));
    }

    let mut schema = SchemaFile::new(folder.to_path_buf(), options, columns);
    schema.quality = rules;

    let saved =
        save_schema(workspace_dir, &schema).map_err(|e| err("не удалось сохранить схему", e))?;
    let wanted = format!("{entity}.toml");
    if saved != wanted {
        let from = workspace_dir.join("schemas").join(&saved);
        let to = workspace_dir.join("schemas").join(&wanted);
        std::fs::rename(&from, &to).map_err(|e| err("не удалось переименовать схему", e))?;
    }
    Ok(())
}

/// Результат проверки одного файла на образце.
#[derive(Debug, Clone, PartialEq)]
pub struct FileSampleCheck {
    /// Имя файла.
    pub name: String,
    /// Сколько строк проверено.
    pub rows_checked: usize,
    /// Проблема схемы (тип/набор колонок), если есть.
    pub column_issue: Option<String>,
    /// Статистика по правилам.
    pub rule_stats: Vec<RuleStats>,
    /// Строк с нарушениями уровня error.
    pub rows_with_errors: usize,
    /// Строк с предупреждениями.
    pub rows_with_warnings: usize,
}

/// Отчёт «проверить на образце».
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationReport {
    /// По файлам.
    pub files: Vec<FileSampleCheck>,
    /// Сводка по правилам (суммарно).
    pub rules: Vec<RuleStats>,
    /// Всего проверено строк.
    pub rows_checked: usize,
    /// Строк уйдёт в карантин.
    pub rows_with_errors: usize,
    /// Строк с предупреждениями.
    pub rows_with_warnings: usize,
}

/// Проверить файлы сущности на образце: схема + правила. Ничего не пишет.
pub fn validate_entity(
    workspace_dir: &Path,
    entity: &str,
    sample_rows: usize,
) -> ApiResult<ValidationReport> {
    let view = entity_schema_view(workspace_dir, entity)?;
    let options = view.options;
    let scan = scan_folder(&view.folder).map_err(|e| err("не удалось прочитать папку", e))?;

    let mut files = Vec::new();
    let mut totals: Vec<RuleStats> = Vec::new();
    let mut rows_checked = 0usize;
    let mut rows_with_errors = 0usize;
    let mut rows_with_warnings = 0usize;

    for meta in &scan.files {
        if meta.kind == "Other" {
            continue;
        }
        let path = view.folder.join(&meta.name);
        let frame = match crate::read_frame(&path, options, Some(sample_rows)) {
            Ok(frame) => frame,
            Err(error) => {
                files.push(FileSampleCheck {
                    name: meta.name.clone(),
                    rows_checked: 0,
                    column_issue: Some(format!("не читается: {error}")),
                    rule_stats: Vec::new(),
                    rows_with_errors: 0,
                    rows_with_warnings: 0,
                });
                continue;
            }
        };

        let column_issue = check_columns(&view.columns, &frame);
        let frame = crate::cast_frame_to_schema(&frame, &view.columns).unwrap_or(frame);
        let outcome =
            crate::quality::evaluate(&frame, &view.rules, 5).map_err(|e| err("правила", e))?;

        let error_rows = crate::quality::quarantine_row_indices(&outcome).len();
        rows_checked += frame.height();
        rows_with_errors += error_rows;
        rows_with_warnings += outcome.warning_rows;
        merge_rule_stats(&mut totals, &outcome.stats);

        files.push(FileSampleCheck {
            name: meta.name.clone(),
            rows_checked: frame.height(),
            column_issue,
            rule_stats: outcome.stats,
            rows_with_errors: error_rows,
            rows_with_warnings: outcome.warning_rows,
        });
    }

    Ok(ValidationReport {
        files,
        rules: totals,
        rows_checked,
        rows_with_errors,
        rows_with_warnings,
    })
}

/// Сравнить колонки файла с подтверждённой схемой.
fn check_columns(expected: &[ColumnDef], frame: &polars::prelude::DataFrame) -> Option<String> {
    let actual: Vec<(String, String)> = frame
        .columns()
        .iter()
        .map(|c| (c.name().to_string(), c.dtype().to_string()))
        .collect();
    if expected.len() != actual.len() {
        return Some(format!(
            "{} колонок в схеме, {} в файле",
            expected.len(),
            actual.len()
        ));
    }
    for (expected, (name, dtype)) in expected.iter().zip(actual) {
        if expected.name != name {
            return Some(format!(
                "ожидали колонку '{}', в файле '{name}'",
                expected.name
            ));
        }
        if !crate::types_compatible(&expected.dtype, &dtype) {
            return Some(format!(
                "'{}': ожидали тип {}, в файле {dtype}",
                expected.name, expected.dtype
            ));
        }
    }
    None
}

/// Сложить статистику правил из разных файлов (ключ: колонка + правило).
fn merge_rule_stats(totals: &mut Vec<RuleStats>, stats: &[RuleStats]) {
    for stat in stats {
        match totals
            .iter_mut()
            .find(|t| t.column == stat.column && t.rule == stat.rule)
        {
            Some(existing) => existing.violations += stat.violations,
            None => totals.push(stat.clone()),
        }
    }
}

// ---------------------------------------------------------------------------
// Прогон и карантин (стадия ODS)
// ---------------------------------------------------------------------------

/// Запустить прогон сущности: ODS + карантин + манифест + логи.
pub fn run_entity_now<F>(
    workspace_dir: &Path,
    entity: &str,
    mut on_progress: F,
) -> ApiResult<RunOutcome>
where
    F: FnMut(usize, usize),
{
    let config = load_config(workspace_dir)?;
    let binding = config
        .bindings
        .iter()
        .find(|b| b.entity == entity)
        .ok_or_else(|| ApiError::new(format!("неизвестная сущность '{entity}'")))?;
    let folder = PathBuf::from(&binding.folder);

    let schema = crate::load_schema(workspace_dir, &format!("{entity}.toml")).map_err(|_| {
        ApiError::new(format!(
            "схема '{entity}' не подтверждена — сначала подтвердите схему"
        ))
    })?;

    let options = ReaderOptions {
        encoding: encoding_from_token(&schema.encoding),
        delimiter: delimiter_from_token(&schema.delimiter),
        has_header: schema.has_header,
    };
    let dataset_dir = data_dir(workspace_dir, &config).join(entity);

    crate::run_entity(
        &folder,
        &dataset_dir,
        entity,
        &schema,
        options,
        &mut |done, total| on_progress(done, total),
    )
    .map_err(|e| err("прогон не удался", e))
}

/// Прогоны сущности (свежие сверху).
pub fn entity_runs(workspace_dir: &Path, entity: &str) -> ApiResult<Vec<RunManifest>> {
    let config = load_config(workspace_dir)?;
    let dataset_dir = data_dir(workspace_dir, &config).join(entity);
    crate::list_runs(&dataset_dir).map_err(|e| err("не удалось прочитать прогоны", e))
}

/// Манифест конкретного прогона.
pub fn entity_run_manifest(
    workspace_dir: &Path,
    entity: &str,
    run_id: &str,
) -> ApiResult<RunManifest> {
    let config = load_config(workspace_dir)?;
    let dataset_dir = data_dir(workspace_dir, &config).join(entity);
    crate::read_manifest(&dataset_dir, run_id).map_err(|e| err("не удалось прочитать манифест", e))
}

/// Строки карантина конкретного прогона (для таблицы в UI).
pub fn entity_quarantine(
    workspace_dir: &Path,
    entity: &str,
    run_id: &str,
    limit: usize,
) -> ApiResult<Vec<QuarantineRow>> {
    let config = load_config(workspace_dir)?;
    let dataset_dir = data_dir(workspace_dir, &config).join(entity);
    crate::read_quarantine(&dataset_dir, run_id, limit)
        .map_err(|e| err("не удалось прочитать карантин", e))
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

        // То же имя ещё раз → директория с суффиксом, никогда не перезапись.
        let second = create_workspace_in(&root, "Продажи 2026").expect("create 2");
        assert_ne!(second.dir, ws.dir);
        assert_eq!(list_workspaces(&root).expect("list").len(), 2);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scan_confirm_and_stage_entity() {
        let root = temp_dir("flow");
        let ws = create_workspace_in(&root, "demo").expect("create ws");

        // Корень-источник с одной папкой сущности и двумя подходящими файлами.
        let sources = temp_dir("sources");
        let sales = sources.join("sales");
        std::fs::create_dir_all(&sales).expect("mkdir sales");
        write_csv(
            &sales.join("a.csv"),
            "id,amount,name\n1,12.5,Alpha\n2,7.25,Beta\n",
        );
        write_csv(&sales.join("b.csv"), "id,amount,name\n3,9.0,Gamma\n");

        // Скан в режиме B видит кандидата, но ничего не пишет.
        let candidates = scan_candidates(&sources).expect("scan");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].entity, "sales");
        assert_eq!(candidates[0].files, 2);
        assert_eq!(candidates[0].columns, 3);
        assert!(entities(&ws.dir).expect("entities").is_empty());

        // Подтверждение → файл схемы + привязка.
        confirm_entity(&ws.dir, "sales", &sales).expect("confirm");
        let bound = entities(&ws.dir).expect("entities");
        assert_eq!(bound.len(), 1);
        assert!(bound[0].has_schema);
        assert!(ws.dir.join("schemas/sales.toml").exists());

        // Staging → части в data/sales, строки посчитаны.
        let outcome = stage_entity(&ws.dir, "sales").expect("stage");
        assert_eq!(outcome.staged_files, 2);
        assert_eq!(outcome.rows, 3);
        assert_eq!(outcome.skipped_files, 0);
        assert_eq!(outcome.parts, 2);
        assert!(outcome.dataset_dir.ends_with("data/sales"));

        // Листинг файлов для детального вида.
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
        // Типы колонок тут совпадают, поэтому прогон стейджит оба файла; суть
        // теста — *форма* отчёта (skipped_reasons присутствует всегда).
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

    #[test]
    fn full_pipeline_schema_rules_run_and_quarantine() {
        use crate::quality::{ColumnRule, RuleKind, Severity};

        let root = temp_dir("pipeline_root");
        let ws = create_workspace_in(&root, "pipeline").expect("воркспейс");
        let sources = temp_dir("pipeline_sources");
        let sales = sources.join("sales");
        std::fs::create_dir_all(&sales).expect("папка сущности");

        // Две строки с нарушениями: отрицательный amount и пустой customer, плюс дубль id.
        write_csv(
            &sales.join("a.csv"),
            "id,amount,customer\n1,12.5,Alpha\n2,-3.0,Beta\n3,7.0,Gamma\n",
        );
        write_csv(
            &sales.join("b.csv"),
            "id,amount,customer\n4,9.5,Delta\n4,1.0,\n",
        );

        // Привязываем папку к сущности (как это делает confirm_entity, но схему задаём сами).
        let mut config = open_workspace(&ws.dir).expect("конфиг").expect("есть");
        upsert_binding(&ws.dir, &mut config, "sales".into(), sales.clone()).expect("привязка");

        // Контракт: колонки + правила (error → карантин, warning → пометка).
        save_entity_schema(
            &ws.dir,
            "sales",
            &sales,
            vec![
                crate::ColumnDef {
                    name: "id".into(),
                    dtype: "i64".into(),
                },
                crate::ColumnDef {
                    name: "amount".into(),
                    dtype: "f64".into(),
                },
                crate::ColumnDef {
                    name: "customer".into(),
                    dtype: "str".into(),
                },
            ],
            vec![
                ColumnRule {
                    column: "id".into(),
                    severity: Severity::Error,
                    kind: RuleKind::Unique,
                },
                ColumnRule {
                    column: "amount".into(),
                    severity: Severity::Error,
                    kind: RuleKind::Range {
                        min: Some(0.0),
                        max: None,
                    },
                },
                ColumnRule {
                    column: "customer".into(),
                    severity: Severity::Warning,
                    kind: RuleKind::NotNull,
                },
            ],
            crate::ReaderOptions::default(),
        )
        .expect("схема сохранена");

        // Схема теперь подтверждена, и предложение совпадает с сохранённым.
        let view = entity_schema_view(&ws.dir, "sales").expect("контракт");
        assert!(view.confirmed);
        assert_eq!(view.columns.len(), 3);
        assert_eq!(view.rules.len(), 3);

        // Проверка на образце видит нарушения.
        let report = validate_entity(&ws.dir, "sales", 100).expect("проверка");
        assert_eq!(report.files.len(), 2);
        assert!(report.rows_with_errors > 0, "ожидали строки-нарушители");
        assert!(
            report.rows_with_warnings > 0,
            "ожидали предупреждение по customer"
        );

        // Прогон: ODS + карантин + манифест + логи.
        let outcome = run_entity_now(&ws.dir, "sales", |_, _| {}).expect("прогон");
        let manifest = &outcome.manifest;
        assert_eq!(manifest.entity, "sales");
        assert_eq!(manifest.rows_read, 5);
        assert_eq!(manifest.rows_valid + manifest.rows_quarantine, 5);
        assert_eq!(
            manifest.rows_quarantine, 3,
            "строки 2 (amount), 4 и 5 (id/customer)"
        );
        assert!(manifest.has_errors);
        // a.csv даёт часть ODS (2 строки прошли), b.csv целиком ушёл в карантин
        // (дубликат id=4 в обеих строках) — поэтому частей ODS ровно одна,
        // а частей карантина две.
        assert_eq!(manifest.parts.len(), 1);
        assert_eq!(manifest.quarantine_parts.len(), 2);
        assert!(!manifest.schema_hash.is_empty());

        // Артефакты на диске.
        assert!(outcome.run_dir.join("manifest.json").exists());
        assert!(outcome.run_dir.join("run.jsonl").exists());
        assert!(outcome.run_dir.join("violations.jsonl").exists());
        // При ошибках указатель «последний удачный» не обновляется.
        let dataset_dir = ws.data_dir.join("sales");
        assert!(!dataset_dir.join("latest.json").exists());

        // Список прогонов и карантин читаются через API.
        let runs = entity_runs(&ws.dir, "sales").expect("прогоны");
        assert_eq!(runs.len(), 1);
        let quarantine =
            entity_quarantine(&ws.dir, "sales", &manifest.run_id, 10).expect("карантин");
        assert!(!quarantine.is_empty());
        assert!(quarantine.iter().any(|row| row.column == "amount"));
        let loaded = entity_run_manifest(&ws.dir, "sales", &manifest.run_id).expect("манифест");
        assert_eq!(loaded.run_id, manifest.run_id);

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(sources);
    }
}
