//! # Движок правил качества (бизнес-проверки и ограничения)
//!
//! Модуль отвечает на вопрос «какие строки не соответствуют контракту данных».
//! Правила описываются декларативно (и хранятся в `*.schema.toml`), а проверка
//! выполняется построчно по `AnyValue`.
//!
//! ## Почему построчно, а не выражениями Polars
//!
//! Векторные выражения Polars были бы быстрее, но требуют отдельной ветки кода
//! под каждый тип колонки и каждое правило, и заметно сложнее в отладке. На
//! текущем объёме (десятки-сотни тысяч строк на файл) построчный проход даёт
//! приемлемую скорость при несопоставимо более простом коде. Когда появится
//! реальная потребность в миллионах строк — этот модуль заменяется на
//! векторную реализацию, а контракт (`evaluate`) остаётся тем же.
//!
//! ## Семантика (важно, чтобы UI и логи говорили одно и то же)
//!
//! * `Severity::Error` — строка **не попадает** в ODS, уходит в карантин;
//! * `Severity::Warning` — строка **попадает** в ODS, но нарушение считается и
//!   показывается в отчёте;
//! * для строковых колонок «пусто» = `null` **или** пустая строка (это то, что
//!   люди ожидают от правила «не пусто»);
//! * карантин формируется по строкам целиком (а не по отдельным ячейкам):
//!   одна плохая ячейка уводит строку в карантин вместе с контекстом.

use std::collections::HashMap;

use polars::prelude::*;
use serde::{Deserialize, Serialize};

use crate::StrataError;

/// Строгость правила.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Нарушение уводит строку в карантин.
    Error,
    /// Нарушение только считается и показывается в отчёте.
    Warning,
}

/// Вид правила (что именно проверяем).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuleKind {
    /// Значение не пусто (см. семантику модуля).
    NotNull,
    /// Значение уникально в пределах файла.
    Unique,
    /// Число в диапазоне `[min, max]` (границы опциональны).
    Range {
        /// Нижняя граница (включительно).
        min: Option<f64>,
        /// Верхняя граница (включительно).
        max: Option<f64>,
    },
    /// Строка соответствует регулярному выражению.
    Regex {
        /// Шаблон (синтаксис crate `regex`).
        pattern: String,
    },
    /// Дата разбирается форматом chrono, например `%Y-%m-%d`.
    DateFormat {
        /// Формат (chrono).
        format: String,
    },
    /// Значение входит в список допустимых.
    AllowedValues {
        /// Допустимые значения (сравнение по строке).
        values: Vec<String>,
    },
}

/// Правило для одной колонки.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnRule {
    /// Имя колонки.
    pub column: String,
    /// Строгость.
    pub severity: Severity,
    /// Что проверяем (плоско в TOML благодаря `#[serde(flatten)]`).
    #[serde(flatten)]
    pub kind: RuleKind,
}

impl ColumnRule {
    /// Человекочитаемая метка правила — попадает в отчёты, карантин и логи.
    pub fn label(&self) -> String {
        match &self.kind {
            RuleKind::NotNull => "not_null".to_string(),
            RuleKind::Unique => "unique".to_string(),
            RuleKind::Range { min, max } => match (min, max) {
                (Some(lo), Some(hi)) => format!("range[{lo}..{hi}]"),
                (Some(lo), None) => format!("range>={lo}"),
                (None, Some(hi)) => format!("range<={hi}"),
                (None, None) => "range".to_string(),
            },
            RuleKind::Regex { pattern } => format!("regex/{pattern}"),
            RuleKind::DateFormat { format } => format!("date_format/{format}"),
            RuleKind::AllowedValues { values } => format!("allowed_values[{}]", values.len()),
        }
    }
}

/// Итог проверки одного правила (для отчётов и UI).
#[derive(Debug, Clone, PartialEq)]
pub struct RuleStats {
    /// Колонка правила.
    pub column: String,
    /// Метка правила.
    pub rule: String,
    /// Строгость.
    pub severity: Severity,
    /// Сколько строк нарушили правило.
    pub violations: usize,
    /// Несколько примеров «как есть» (для панели проверки и карантина).
    pub examples: Vec<String>,
}

/// Нарушение на уровне строки (используется для карантина).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RowViolation {
    /// Индекс строки внутри файла (0-based).
    pub row_index: usize,
    /// Колонка, в которой нарушение.
    pub column: String,
    /// Метка правила.
    pub rule: String,
    /// Значение «как есть».
    pub value: String,
    /// Строгость (в карантин идут только `Error`).
    pub severity: Severity,
}

/// Итог проверки набора правил по одному кадру.
#[derive(Debug, Clone, PartialEq)]
pub struct QualityOutcome {
    /// Статистика по каждому правилу (в порядке правил).
    pub stats: Vec<RuleStats>,
    /// Нарушения уровня `Error` (по строкам) — их строки уходят в карантин.
    pub errors: Vec<RowViolation>,
    /// Сколько строк нарушили хотя бы одно правило уровня `Warning`.
    pub warning_rows: usize,
}

/// Проверить кадр всеми правилами.
///
/// `example_limit` ограничивает число примеров на правило (для UI), чтобы
/// отчёт оставался маленьким даже на плохих данных.
pub fn evaluate(
    frame: &DataFrame,
    rules: &[ColumnRule],
    example_limit: usize,
) -> crate::Result<QualityOutcome> {
    // Регулярные выражения компилируем один раз на правило.
    let compiled: Vec<Option<regex::Regex>> = rules
        .iter()
        .map(|rule| match &rule.kind {
            RuleKind::Regex { pattern } => regex::Regex::new(pattern)
                .map(Some)
                .map_err(|error| StrataError::Rule(format!("regex '{pattern}': {error}"))),
            _ => Ok(None),
        })
        .collect::<crate::Result<Vec<_>>>()?;

    let mut stats = Vec::with_capacity(rules.len());
    let mut errors = Vec::new();
    let mut warning_rows_seen: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for (index, rule) in rules.iter().enumerate() {
        let column = frame.column(&rule.column).map_err(|_| {
            StrataError::Rule(format!(
                "правило ссылается на отсутствующую колонку '{}'",
                rule.column
            ))
        })?;

        // Уникальность считается по всему файлу, остальные правила — построчно.
        let duplicates = match rule.kind {
            RuleKind::Unique => Some(count_duplicates(column)?),
            _ => None,
        };

        let mut violations = 0usize;
        let mut examples = Vec::new();

        for row_index in 0..column.len() {
            let value = column.get(row_index).unwrap_or(AnyValue::Null);
            let failed = match &rule.kind {
                RuleKind::NotNull => is_empty_value(&value),
                RuleKind::Unique => duplicates
                    .as_ref()
                    .and_then(|map| map.get(&value_to_text(&value)))
                    .is_some_and(|count| *count > 1),
                RuleKind::Range { min, max } => match numeric_value(&value) {
                    Some(number) => {
                        min.is_some_and(|lo| number < lo) || max.is_some_and(|hi| number > hi)
                    }
                    // Нечисловое значение в числовом правиле — это нарушение.
                    None => true,
                },
                RuleKind::Regex { .. } => compiled[index]
                    .as_ref()
                    .is_some_and(|regex| !regex.is_match(&value_to_text(&value))),
                RuleKind::DateFormat { format } => {
                    chrono::NaiveDate::parse_from_str(&value_to_text(&value), format).is_err()
                }
                RuleKind::AllowedValues { values } => !values
                    .iter()
                    .any(|allowed| allowed == &value_to_text(&value)),
            };

            if failed {
                violations += 1;
                if examples.len() < example_limit {
                    examples.push(value_to_text(&value));
                }
                match rule.severity {
                    Severity::Error => errors.push(RowViolation {
                        row_index,
                        column: rule.column.clone(),
                        rule: rule.label(),
                        value: value_to_text(&value),
                        severity: Severity::Error,
                    }),
                    Severity::Warning => {
                        warning_rows_seen.insert(row_index);
                    }
                }
            }
        }

        stats.push(RuleStats {
            column: rule.column.clone(),
            rule: rule.label(),
            severity: rule.severity,
            violations,
            examples,
        });
    }

    Ok(QualityOutcome {
        stats,
        errors,
        warning_rows: warning_rows_seen.len(),
    })
}

/// Строки, которые обязаны уйти в карантин (уникальные индексы из `errors`).
pub fn quarantine_row_indices(outcome: &QualityOutcome) -> Vec<usize> {
    let mut indices: Vec<usize> = outcome.errors.iter().map(|e| e.row_index).collect();
    indices.sort_unstable();
    indices.dedup();
    indices
}

/// Разделить кадр на «чистую» часть и карантин по индексам строк.
///
/// Строим две булевы маски одним проходом (никаких отрицаний и `clear()` —
/// только предсказуемые операции), затем фильтруем кадр.
pub fn split_frame(
    frame: &DataFrame,
    quarantine_indices: &[usize],
) -> crate::Result<(DataFrame, DataFrame)> {
    let mut sorted = quarantine_indices.to_vec();
    sorted.sort_unstable();

    let mut bad = BooleanChunkedBuilder::new("bad".into(), frame.height());
    let mut good = BooleanChunkedBuilder::new("good".into(), frame.height());
    let mut cursor = 0usize;
    for row in 0..frame.height() {
        while cursor < sorted.len() && sorted[cursor] < row {
            cursor += 1;
        }
        let is_bad = cursor < sorted.len() && sorted[cursor] == row;
        bad.append_value(is_bad);
        good.append_value(!is_bad);
    }

    let bad = bad.finish();
    let good = good.finish();
    let quarantine = frame.filter(&bad).map_err(StrataError::from)?;
    let valid = frame.filter(&good).map_err(StrataError::from)?;
    Ok((valid, quarantine))
}

// ---------------------------------------------------------------------------
// Вспомогательные функции значений
// ---------------------------------------------------------------------------

/// Текстовое представление значения (для регулярных выражений, списков, примеров).
fn value_to_text(value: &AnyValue<'_>) -> String {
    match value {
        AnyValue::String(text) => (*text).to_string(),
        AnyValue::StringOwned(text) => text.to_string(),
        AnyValue::Null => String::new(),
        other => format!("{other}"),
    }
}

/// «Пусто» ли значение: `null` или пустая строка.
fn is_empty_value(value: &AnyValue<'_>) -> bool {
    match value {
        AnyValue::Null => true,
        AnyValue::String(text) => text.trim().is_empty(),
        AnyValue::StringOwned(text) => text.trim().is_empty(),
        _ => false,
    }
}

/// Числовое значение ячейки: число как есть либо разобранная строка.
fn numeric_value(value: &AnyValue<'_>) -> Option<f64> {
    match value {
        AnyValue::Int8(v) => Some(*v as f64),
        AnyValue::Int16(v) => Some(*v as f64),
        AnyValue::Int32(v) => Some(*v as f64),
        AnyValue::Int64(v) => Some(*v as f64),
        AnyValue::UInt8(v) => Some(*v as f64),
        AnyValue::UInt16(v) => Some(*v as f64),
        AnyValue::UInt32(v) => Some(*v as f64),
        AnyValue::UInt64(v) => Some(*v as f64),
        AnyValue::Float32(v) => Some(*v as f64),
        AnyValue::Float64(v) => Some(*v),
        AnyValue::String(text) => text.trim().replace(',', ".").parse::<f64>().ok(),
        AnyValue::StringOwned(text) => text.trim().replace(',', ".").parse::<f64>().ok(),
        _ => None,
    }
}

/// Сколько раз встречается каждое значение колонки (для `unique`).
fn count_duplicates(column: &Column) -> crate::Result<HashMap<String, usize>> {
    let mut counts: HashMap<String, usize> = HashMap::with_capacity(column.len());
    for row_index in 0..column.len() {
        let value = column.get(row_index).unwrap_or(AnyValue::Null);
        *counts.entry(value_to_text(&value)).or_insert(0) += 1;
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use polars::prelude::*;

    /// Простой кадр для проверок: id, amount, email, date.
    fn sample_frame() -> DataFrame {
        df!(
            "id" => &[1i64, 2, 3, 3],
            "amount" => &[10.0f64, -5.0, 100.0, 7.5],
            "email" => &["a@b.c", "bad", "", "d@e.f"],
            "date" => &["2026-01-05", "2026-13-40", "2026-02-01", "2026-02-02"],
        )
        .expect("кадр собирается")
    }

    #[test]
    fn rules_report_counts_examples_and_quarantine_rows() {
        let frame = sample_frame();
        let rules = vec![
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
                column: "email".into(),
                severity: Severity::Error,
                kind: RuleKind::NotNull,
            },
            ColumnRule {
                column: "email".into(),
                severity: Severity::Error,
                kind: RuleKind::Regex {
                    pattern: r"^[^@\s]+@[^@\s]+\.[^@\s]+$".into(),
                },
            },
            ColumnRule {
                column: "date".into(),
                severity: Severity::Warning,
                kind: RuleKind::DateFormat {
                    format: "%Y-%m-%d".into(),
                },
            },
        ];

        let outcome = evaluate(&frame, &rules, 2).expect("проверка проходит");
        assert_eq!(outcome.stats.len(), 5);

        // amount: одна строка отрицательная.
        let amount = outcome.stats.iter().find(|s| s.column == "amount").unwrap();
        assert_eq!(amount.violations, 1);
        assert_eq!(amount.examples, vec!["-5.0"]);

        // email: пусто (строка 2) + «bad» (строка 1) = 2 нарушения по двум правилам.
        let not_null = outcome
            .stats
            .iter()
            .find(|s| s.column == "email" && s.rule == "not_null")
            .unwrap();
        assert_eq!(not_null.violations, 1);
        let regex = outcome
            .stats
            .iter()
            .find(|s| s.column == "email" && s.rule.starts_with("regex"))
            .unwrap();
        // Пустое значение тоже не проходит regex — это ожидаемо и полезно
        // (регулярка ловит и «bad», и «»), поэтому нарушений два.
        assert_eq!(regex.violations, 2);

        // date: warning не уводит строку в карантин, но считается.
        let date = outcome.stats.iter().find(|s| s.column == "date").unwrap();
        assert_eq!(date.severity, Severity::Warning);
        assert_eq!(date.violations, 1);
        assert_eq!(outcome.warning_rows, 1);

        // Карантин: строки 1 (bad email, отрицательный amount), 2 (пусто), 2 (дубликат id)…
        let indices = quarantine_row_indices(&outcome);
        assert!(indices.contains(&1));
        assert!(indices.contains(&2));

        let (valid, quarantine) = split_frame(&frame, &indices).expect("разделение");
        assert_eq!(valid.height() + quarantine.height(), frame.height());
        assert_eq!(quarantine.height(), indices.len());
    }

    #[test]
    fn empty_frame_and_no_rules_are_fine() {
        let frame = df!("a" => &[1i64, 2]).unwrap();
        let outcome = evaluate(&frame, &[], 5).expect("без правил");
        assert!(outcome.stats.is_empty());
        assert!(outcome.errors.is_empty());
        let (valid, quarantine) = split_frame(&frame, &[]).expect("разделение пустое");
        assert_eq!(valid.height(), 2);
        assert_eq!(quarantine.height(), 0);
    }

    #[test]
    fn missing_column_is_an_error_not_a_panic() {
        let frame = sample_frame();
        let rules = vec![ColumnRule {
            column: "nope".into(),
            severity: Severity::Error,
            kind: RuleKind::NotNull,
        }];
        let error = evaluate(&frame, &rules, 1).expect_err("нет колонки");
        assert!(matches!(error, StrataError::Rule(_)));
    }

    #[test]
    fn invalid_regex_is_reported() {
        let frame = sample_frame();
        let rules = vec![ColumnRule {
            column: "email".into(),
            severity: Severity::Error,
            kind: RuleKind::Regex {
                pattern: "([".into(),
            },
        }];
        let error = evaluate(&frame, &rules, 1).expect_err("плохой шаблон");
        assert!(matches!(error, StrataError::Rule(_)));
    }
}
