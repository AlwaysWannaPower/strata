//! # Прогон сущности: ODS + карантин + манифест + лог
//!
//! Один прогон — одна **неизменяемая** директория:
//!
//! ```text
//! data/<entity>/
//! ├── runs/<run_id>/
//! │   ├── ods/part-0000-<файл>.parquet        строки, прошедшие контракт
//! │   ├── quarantine/rows-0000-<файл>.parquet строки-нарушители (как есть)
//! │   ├── violations.jsonl                    по одному нарушению на строку
//! │   ├── run.jsonl                           фазы и решения (машинночитаемый лог)
//! │   └── manifest.json                       итоги, части, версия схемы
//! └── latest.json                             указатель на последний удачный прогон
//! ```
//!
//! Почему так: ODS должен быть воспроизводим («что именно мы отдали 3 дня
//! назад»), карантин — разбираемым, а логи — тем, на что можно ссылаться в
//! тикете. Поэтому ни один прогон не перезаписывает другой, а «последний
//! удачный» — это отдельный маленький указатель.
//!
//! Правила качества применяются построчно (`crate::quality`), после приведения
//! типов к подтверждённой схеме: сначала каст (числовое расширение и т.п.),
//! потом проверки, потом разделение строк.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use polars::prelude::ParquetWriter;
use serde::{Deserialize, Serialize};

use crate::quality::{RowViolation, RuleStats, Severity};
use crate::{ColumnDef, ReaderOptions, SchemaFile, StrataError};

/// Итог по одному исходному файлу внутри прогона.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRun {
    /// Имя исходного файла.
    pub name: String,
    /// Прочитано строк.
    pub rows_read: u64,
    /// Записано в ODS.
    pub rows_valid: u64,
    /// Отправлено в карантин.
    pub rows_quarantine: u64,
    /// Строк с предупреждениями (прошли, но помечены).
    pub rows_warning: u64,
    /// Ошибка файла (если он не обработан) — тогда остальные счётчики нулевые.
    pub error: Option<String>,
}

/// Описание записанной части Parquet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartInfo {
    /// Путь относительно директории прогона.
    pub path: String,
    /// Строк в части.
    pub rows: u64,
    /// Размер файла в байтах.
    pub bytes: u64,
}

/// Сводка нарушений по правилу (для UI и манифеста).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleSummary {
    /// Колонка.
    pub column: String,
    /// Метка правила.
    pub rule: String,
    /// Строгость.
    pub severity: Severity,
    /// Число нарушений.
    pub violations: usize,
}

/// Манифест прогона — то, что «отдают» вместе с ODS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunManifest {
    /// Идентификатор прогона.
    pub run_id: String,
    /// Сущность.
    pub entity: String,
    /// Маркер версии схемы (хеш колонок и правил; не криптография).
    pub schema_hash: String,
    /// Время старта (UTC, ISO-8601).
    pub started_utc: String,
    /// Время завершения.
    pub finished_utc: String,
    /// Длительность, мс.
    pub duration_ms: u128,
    /// Файлы и их счётчики.
    pub files: Vec<FileRun>,
    /// Прочитано строк всего.
    pub rows_read: u64,
    /// В ODS.
    pub rows_valid: u64,
    /// В карантине.
    pub rows_quarantine: u64,
    /// С предупреждениями.
    pub rows_warning: u64,
    /// Части ODS.
    pub parts: Vec<PartInfo>,
    /// Части карантина.
    pub quarantine_parts: Vec<PartInfo>,
    /// Сводка по правилам.
    pub rules: Vec<RuleSummary>,
    /// Есть ли нарушения уровня error (иначе прогон можно считать «готов к отдаче»).
    pub has_errors: bool,
}

/// Результат прогона, возвращаемый наружу (UI/CLI).
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    /// Манифест прогона.
    pub manifest: RunManifest,
    /// Директория прогона.
    pub run_dir: PathBuf,
}

/// Оценка прогресса: `(обработано файлов, всего файлов)`.
pub type ProgressFn<'a> = &'a mut dyn FnMut(usize, usize);

/// Выполнить прогон сущности: прочитать файлы, привести к схеме, проверить
/// правила, разложить на ODS и карантин, записать манифест и логи.
///
/// `schema` — подтверждённый контракт (колонки + правила + опции чтения).
/// Файлы, которые не читаются, не роняют прогон: они попадают в манифест с
/// текстом ошибки (ровно то поведение, которого мы требуем от UI).
pub fn run_entity(
    source_dir: &Path,
    dataset_dir: &Path,
    entity: &str,
    schema: &SchemaFile,
    options: ReaderOptions,
    on_progress: ProgressFn<'_>,
) -> crate::Result<RunOutcome> {
    let started = Instant::now();
    let started_utc = chrono::Utc::now().to_rfc3339();
    let run_id = format!("run-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f"));

    let run_dir = dataset_dir.join("runs").join(&run_id);
    let ods_dir = run_dir.join("ods");
    let quarantine_dir = run_dir.join("quarantine");
    std::fs::create_dir_all(&ods_dir)?;
    std::fs::create_dir_all(&quarantine_dir)?;

    let mut log = RunLog::create(&run_dir.join("run.jsonl"))?;
    log.write("start", &format!("прогон {run_id}: entity={entity}"))?;

    let files = crate::scan_folder(source_dir)?;
    let total_files = files.files.len();
    let mut file_runs: Vec<FileRun> = Vec::new();
    let mut parts: Vec<PartInfo> = Vec::new();
    let mut quarantine_parts: Vec<PartInfo> = Vec::new();
    let mut all_stats: Vec<RuleStats> = Vec::new();
    let mut violations_file = std::fs::File::create(run_dir.join("violations.jsonl"))?;

    for (index, meta) in files.files.iter().enumerate() {
        let path = source_dir.join(&meta.name);
        log.write("file", &format!("{}: начало", meta.name))?;

        // Читаем и приводим к схеме: типобезопасность до правил.
        let run = process_one_file(
            &path,
            &meta.name,
            index,
            schema,
            options,
            &ods_dir,
            &quarantine_dir,
            &mut parts,
            &mut quarantine_parts,
            &mut all_stats,
            &mut violations_file,
        );

        match run {
            Ok(file_run) => {
                log.write(
                    "file",
                    &format!(
                        "{}: прочитано {}, в ODS {}, в карантин {}",
                        meta.name,
                        file_run.rows_read,
                        file_run.rows_valid,
                        file_run.rows_quarantine
                    ),
                )?;
                file_runs.push(file_run);
            }
            Err(error) => {
                log.write("error", &format!("{}: {error}", meta.name))?;
                file_runs.push(FileRun {
                    name: meta.name.clone(),
                    rows_read: 0,
                    rows_valid: 0,
                    rows_quarantine: 0,
                    rows_warning: 0,
                    error: Some(error.to_string()),
                });
            }
        }
        on_progress(index + 1, total_files);
    }

    let rows_read: u64 = file_runs.iter().map(|f| f.rows_read).sum();
    let rows_valid: u64 = file_runs.iter().map(|f| f.rows_valid).sum();
    let rows_quarantine: u64 = file_runs.iter().map(|f| f.rows_quarantine).sum();
    let rows_warning: u64 = file_runs.iter().map(|f| f.rows_warning).sum();

    let rules: Vec<RuleSummary> = all_stats
        .iter()
        .map(|stats| RuleSummary {
            column: stats.column.clone(),
            rule: stats.rule.clone(),
            severity: stats.severity,
            violations: stats.violations,
        })
        .collect();
    let has_errors = rows_quarantine > 0;

    let duration_ms = started.elapsed().as_millis();
    let manifest = RunManifest {
        run_id: run_id.clone(),
        entity: entity.to_string(),
        schema_hash: schema_hash(schema),
        started_utc,
        finished_utc: chrono::Utc::now().to_rfc3339(),
        duration_ms,
        files: file_runs,
        rows_read,
        rows_valid,
        rows_quarantine,
        rows_warning,
        parts,
        quarantine_parts,
        rules,
        has_errors,
    };

    // Манифест + указатель «последний удачный прогон».
    let manifest_path = run_dir.join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| StrataError::Run(format!("манифест: {e}")))?,
    )?;
    if !manifest.has_errors {
        let pointer = dataset_dir.join("latest.json");
        std::fs::write(
            &pointer,
            serde_json::to_string_pretty(&serde_json::json!({
                "run_id": manifest.run_id,
                "rows": manifest.rows_valid,
                "parts": manifest.parts.len(),
                "finished_utc": manifest.finished_utc,
            }))
            .map_err(|e| StrataError::Run(format!("указатель: {e}")))?,
        )?;
    }

    log.write(
        "finish",
        &format!(
            "готово: прочитано {rows_read}, в ODS {rows_valid}, в карантин {rows_quarantine}, предупреждений {rows_warning}, {} мс",
            duration_ms
        ),
    )?;

    Ok(RunOutcome { manifest, run_dir })
}

/// Обработать один файл: прочитать → привести к схеме → проверить правила →
/// записать часть ODS и часть карантина.
#[allow(clippy::too_many_arguments)]
fn process_one_file(
    path: &Path,
    name: &str,
    index: usize,
    schema: &SchemaFile,
    options: ReaderOptions,
    ods_dir: &Path,
    quarantine_dir: &Path,
    parts: &mut Vec<PartInfo>,
    quarantine_parts: &mut Vec<PartInfo>,
    all_stats: &mut Vec<RuleStats>,
    violations_file: &mut std::fs::File,
) -> crate::Result<FileRun> {
    let frame = crate::read_frame(path, options, None)?;
    let frame = crate::cast_frame_to_schema(&frame, &schema.columns)?;
    let rows_read = frame.height() as u64;

    // Правила: считаем нарушения и собираем строки для карантина.
    let outcome = crate::quality::evaluate(&frame, &schema.quality, 20)?;
    let quarantine_indices = crate::quality::quarantine_row_indices(&outcome);
    all_stats.extend(outcome.stats.iter().cloned());

    for violation in &outcome.errors {
        write_violation(violations_file, name, violation)?;
    }

    let (valid, quarantine) = crate::quality::split_frame(&frame, &quarantine_indices)?;

    let write_part = |frame: &mut polars::prelude::DataFrame,
                      dir: &Path,
                      prefix: &str|
     -> crate::Result<Option<PartInfo>> {
        if frame.height() == 0 {
            return Ok(None);
        }
        let file_name = format!("{prefix}-{index:04}-{name}.parquet");
        let target = dir.join(&file_name);
        let mut file = std::fs::File::create(&target)?;
        let writer = ParquetWriter::new(&mut file);
        writer.finish(frame)?;
        let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        Ok(Some(PartInfo {
            path: format!(
                "{}/{}",
                dir.file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default(),
                file_name
            ),
            rows: frame.height() as u64,
            bytes,
        }))
    };

    let mut valid = valid;
    let mut quarantine = quarantine;
    let valid_part = write_part(&mut valid, ods_dir, "part")?;
    let quarantine_part = write_part(&mut quarantine, quarantine_dir, "rows")?;
    let rows_valid = valid_part.as_ref().map(|p| p.rows).unwrap_or(0);
    let rows_quarantine = quarantine_part.as_ref().map(|p| p.rows).unwrap_or(0);
    if let Some(part) = valid_part {
        parts.push(part);
    }
    if let Some(part) = quarantine_part {
        quarantine_parts.push(part);
    }

    Ok(FileRun {
        name: name.to_string(),
        rows_read,
        rows_valid,
        rows_quarantine,
        rows_warning: outcome.warning_rows as u64,
        error: None,
    })
}

/// Записать одно нарушение в `violations.jsonl`.
fn write_violation(
    file: &mut std::fs::File,
    source_name: &str,
    violation: &RowViolation,
) -> crate::Result<()> {
    let line = serde_json::json!({
        "file": source_name,
        "row_index": violation.row_index,
        "column": violation.column,
        "rule": violation.rule,
        "value": violation.value,
        "severity": match violation.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        },
    });
    writeln!(file, "{line}").map_err(|e| StrataError::Run(format!("violations.jsonl: {e}")))?;
    Ok(())
}

/// Маркер версии схемы: хеш колонок и правил (не криптографический).
fn schema_hash(schema: &SchemaFile) -> String {
    let mut hasher = DefaultHasher::new();
    for column in &schema.columns {
        column.name.hash(&mut hasher);
        column.dtype.hash(&mut hasher);
    }
    for rule in &schema.quality {
        rule.label().hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Простой построчный лог прогона (`run.jsonl`).
struct RunLog {
    file: std::fs::File,
    started: Instant,
}

impl RunLog {
    fn create(path: &Path) -> crate::Result<Self> {
        let file = std::fs::File::create(path)?;
        Ok(RunLog {
            file,
            started: Instant::now(),
        })
    }

    fn write(&mut self, phase: &str, message: &str) -> crate::Result<()> {
        let line = serde_json::json!({
            "t_ms": self.started.elapsed().as_millis() as u64,
            "phase": phase,
            "message": message,
        });
        writeln!(self.file, "{line}").map_err(|e| StrataError::Run(format!("run.jsonl: {e}")))?;
        Ok(())
    }
}

/// Список прогонов сущности (по манифестам), свежие сверху.
pub fn list_runs(dataset_dir: &Path) -> crate::Result<Vec<RunManifest>> {
    let runs_dir = dataset_dir.join("runs");
    if !runs_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut manifests = Vec::new();
    for entry in std::fs::read_dir(&runs_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("manifest.json");
        if let Ok(text) = std::fs::read_to_string(&manifest_path) {
            if let Ok(manifest) = serde_json::from_str::<RunManifest>(&text) {
                manifests.push(manifest);
            }
        }
    }
    manifests.sort_by(|a, b| b.run_id.cmp(&a.run_id));
    Ok(manifests)
}

/// Прочитать манифест конкретного прогона.
pub fn read_manifest(dataset_dir: &Path, run_id: &str) -> crate::Result<RunManifest> {
    let path = dataset_dir.join("runs").join(run_id).join("manifest.json");
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text).map_err(|e| StrataError::Run(format!("манифест {run_id}: {e}")))
}

/// Прочитать строки карантина конкретного прогона (для UI).
pub fn read_quarantine(
    dataset_dir: &Path,
    run_id: &str,
    limit: usize,
) -> crate::Result<Vec<QuarantineRow>> {
    let run_dir = dataset_dir.join("runs").join(run_id);
    let mut rows = Vec::new();

    // 1) человекочитаемые нарушения (файл, строка, колонка, правило, значение)
    let violations_path = run_dir.join("violations.jsonl");
    if let Ok(text) = std::fs::read_to_string(&violations_path) {
        for line in text.lines().take(limit) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                rows.push(QuarantineRow {
                    file: value["file"].as_str().unwrap_or_default().to_string(),
                    row_index: value["row_index"].as_u64().unwrap_or(0) as usize,
                    column: value["column"].as_str().unwrap_or_default().to_string(),
                    rule: value["rule"].as_str().unwrap_or_default().to_string(),
                    value: value["value"].as_str().unwrap_or_default().to_string(),
                });
            }
        }
    }
    Ok(rows)
}

/// Одна строка отчёта карантина.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuarantineRow {
    /// Исходный файл.
    pub file: String,
    /// Номер строки в файле.
    pub row_index: usize,
    /// Колонка.
    pub column: String,
    /// Правило.
    pub rule: String,
    /// Значение «как есть».
    pub value: String,
}

/// Колонки схемы, приведённые к парам «имя, тип» (для UI и хеша).
pub fn columns_summary(columns: &[ColumnDef]) -> Vec<(String, String)> {
    columns
        .iter()
        .map(|c| (c.name.clone(), c.dtype.clone()))
        .collect()
}
