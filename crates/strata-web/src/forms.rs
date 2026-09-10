//! Формы и «чистые» преобразования ввода/вывода веб-слоя.
//!
//! Здесь живёт всё, что не зависит ни от HTTP, ни от состояния сервиса:
//! разбор полей формы в типы ядра ([`strata_core::ColumnRule`], метки типов),
//! проверка имён, пришедших из URL, и человекочитаемое форматирование чисел для
//! шаблонов. Благодаря этому правила конструктора правил и разбор меток типов
//! тестируются без поднятия сервера (см. тесты внизу файла).
//!
//! Правило проекта: **пользовательский текст — английский, комментарии — русские**.

use serde::Deserialize;
use strata_core::{ColumnRule, RuleKind, Severity};

// ---------------------------------------------------------------------------
// Справочники для форм (тот же набор, что понимает ядро)
// ---------------------------------------------------------------------------

/// Метки типов колонок, которые принимает ядро (`cast_frame_to_schema`).
/// Список — единственный источник для `<select>` в таблице схемы.
pub(crate) const DTYPE_LABELS: [&str; 12] = [
    "i64", "i32", "i16", "i8", "u64", "u32", "u16", "u8", "f64", "f32", "bool", "str",
];

/// Типы правил конструктора: `(токен, подпись для пользователя)`.
/// Позднее (UI-5) сюда добавятся правила уровня таблицы (`row_count`, `freshness`).
pub(crate) const RULE_TYPES: [(&str, &str); 6] = [
    ("not_null", "not null"),
    ("unique", "unique"),
    ("range", "range (min/max)"),
    ("regex", "regex (pattern)"),
    ("date_format", "date format (%Y-%m-%d)"),
    ("allowed_values", "allowed values"),
];

/// Строгость правила: `(токен, подпись)`.
pub(crate) const SEVERITIES: [(&str, &str); 2] = [
    ("error", "error → quarantine"),
    ("warning", "warning → flagged"),
];

// ---------------------------------------------------------------------------
// Полезные нагрузки форм
// ---------------------------------------------------------------------------

/// Правка одной колонки в черновике схемы.
#[derive(Deserialize)]
pub(crate) struct ColumnForm {
    /// `rename` | `type` | `delete` | `add` — действие над строкой таблицы.
    pub(crate) action: String,
    /// Позиция колонки в черновике (для `rename` / `type` / `delete`).
    #[serde(default)]
    pub(crate) index: usize,
    /// Новое имя (для `rename`).
    #[serde(default)]
    pub(crate) name: String,
    /// Новая метка типа (для `type`).
    #[serde(default)]
    pub(crate) dtype: String,
}

/// Действие над списком правил: добавление или удаление.
#[derive(Deserialize)]
pub(crate) struct RuleForm {
    /// `add` | `delete`.
    pub(crate) action: String,
    /// Позиция правила в черновике (для `delete`).
    #[serde(default)]
    pub(crate) index: usize,
    /// Колонка правила (для `add`).
    #[serde(default)]
    pub(crate) column: String,
    /// Токен типа правила (для `add`).
    #[serde(default)]
    pub(crate) rule_type: String,
    /// Токен строгости (для `add`).
    #[serde(default)]
    pub(crate) severity: String,
    /// Параметры: границы, шаблон, формат, список значений. Форма всегда
    /// показывает все поля сразу (динамических полей без JS не бывает), поэтому
    /// лишние для выбранного типа молча игнорируются.
    #[serde(default)]
    pub(crate) min: String,
    #[serde(default)]
    pub(crate) max: String,
    #[serde(default)]
    pub(crate) pattern: String,
    #[serde(default)]
    pub(crate) format: String,
    #[serde(default)]
    pub(crate) values: String,
}

/// Выбор файла для превью.
#[derive(Deserialize)]
pub(crate) struct PreviewForm {
    /// Имя файла внутри папки сущности.
    pub(crate) file: String,
}

/// Выбор прогона для вкладки Logs.
#[derive(Deserialize)]
pub(crate) struct RunPickForm {
    /// `run_id` выбранного прогона.
    #[serde(default)]
    pub(crate) run_id: String,
}

// ---------------------------------------------------------------------------
// Разбор полей формы в типы ядра
// ---------------------------------------------------------------------------

/// Привести метку типа из формы к канонической (`i64`, `str`, …).
///
/// Регистр не важен (браузер присылает ровно то, что мы положили в `<select>`,
/// но форму можно отправить и руками через `curl`).
pub(crate) fn parse_dtype(raw: &str) -> Result<String, String> {
    let token = raw.trim().to_ascii_lowercase();
    if DTYPE_LABELS.contains(&token.as_str()) {
        Ok(token)
    } else {
        Err(format!(
            "unknown column type '{raw}' — expected one of {}",
            DTYPE_LABELS.join(", ")
        ))
    }
}

/// Разобрать строгость правила.
pub(crate) fn parse_severity(raw: &str) -> Result<Severity, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "error" => Ok(Severity::Error),
        "warning" | "warn" => Ok(Severity::Warning),
        other => Err(format!(
            "unknown severity '{other}' — expected error or warning"
        )),
    }
}

/// Токен строгости: он же — подпись в таблицах и отчётах, он же — значение
/// поля формы (`error` / `warning`).
pub(crate) fn severity_token(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    }
}

/// Токен типа правила (совпадает с `serde`-тегом ядра).
pub(crate) fn rule_kind_token(kind: &RuleKind) -> &'static str {
    match kind {
        RuleKind::NotNull => "not_null",
        RuleKind::Unique => "unique",
        RuleKind::Range { .. } => "range",
        RuleKind::Regex { .. } => "regex",
        RuleKind::DateFormat { .. } => "date_format",
        RuleKind::AllowedValues { .. } => "allowed_values",
    }
}

/// Параметры правила одной строкой — колонка «Parameters» в таблице правил.
pub(crate) fn rule_params_text(kind: &RuleKind) -> String {
    match kind {
        RuleKind::NotNull | RuleKind::Unique => String::from("—"),
        RuleKind::Range { min, max } => match (min, max) {
            (Some(lo), Some(hi)) => format!("{lo} … {hi}"),
            (Some(lo), None) => format!("≥ {lo}"),
            (None, Some(hi)) => format!("≤ {hi}"),
            (None, None) => String::from("any"),
        },
        RuleKind::Regex { pattern } => pattern.clone(),
        RuleKind::DateFormat { format } => format.clone(),
        RuleKind::AllowedValues { values } => values.join(", "),
    }
}

/// Список допустимых значений: через запятую, пробелы обрезаются, пустые
/// элементы игнорируются (пользователь часто ставит запятую «на всякий случай»).
pub(crate) fn split_values(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect()
}

/// Необязательное число из текстового поля: пусто = «граница не задана».
fn optional_number(raw: &str, field: &str) -> Result<Option<f64>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<f64>()
        .map(Some)
        .map_err(|_| format!("{field}: '{trimmed}' is not a number"))
}

/// Собрать вид правила из полей конструктора.
///
/// Ошибки — это сообщения для пользователя (они уезжают в htmx-фрагмент), а не
/// паника: единственный источник плохого ввода здесь — форма.
pub(crate) fn parse_rule_kind(
    rule_type: &str,
    min: &str,
    max: &str,
    pattern: &str,
    format: &str,
    values: &str,
) -> Result<RuleKind, String> {
    let token = rule_type.trim().to_ascii_lowercase();
    match token.as_str() {
        "not_null" => Ok(RuleKind::NotNull),
        "unique" => Ok(RuleKind::Unique),
        "range" => {
            let lo = optional_number(min, "min")?;
            let hi = optional_number(max, "max")?;
            if lo.is_none() && hi.is_none() {
                return Err(String::from("range needs min and/or max"));
            }
            if let (Some(lo), Some(hi)) = (lo, hi) {
                if lo > hi {
                    return Err(String::from("range: min must not exceed max"));
                }
            }
            Ok(RuleKind::Range { min: lo, max: hi })
        }
        "regex" => {
            let pattern = pattern.trim();
            if pattern.is_empty() {
                return Err(String::from("regex needs a pattern"));
            }
            Ok(RuleKind::Regex {
                pattern: pattern.to_string(),
            })
        }
        "date_format" => {
            let format = format.trim();
            if format.is_empty() {
                return Err(String::from(
                    "date_format needs a chrono format, e.g. %Y-%m-%d",
                ));
            }
            Ok(RuleKind::DateFormat {
                format: format.to_string(),
            })
        }
        "allowed_values" => {
            let values = split_values(values);
            if values.is_empty() {
                return Err(String::from(
                    "allowed_values needs a comma-separated list, e.g. draft,paid",
                ));
            }
            Ok(RuleKind::AllowedValues { values })
        }
        other => Err(format!(
            "unknown rule type '{other}' — expected one of {}",
            RULE_TYPES
                .iter()
                .map(|(token, _)| *token)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Собрать правило целиком: колонка + строгость + вид.
pub(crate) fn parse_rule(
    column: &str,
    severity: &str,
    rule_type: &str,
    min: &str,
    max: &str,
    pattern: &str,
    format: &str,
    values: &str,
) -> Result<ColumnRule, String> {
    let column = column.trim();
    if column.is_empty() {
        return Err(String::from("choose a column for the rule"));
    }
    Ok(ColumnRule {
        column: column.to_string(),
        severity: parse_severity(severity)?,
        kind: parse_rule_kind(rule_type, min, max, pattern, format, values)?,
    })
}

// ---------------------------------------------------------------------------
// Имена из URL и памяти сервиса
// ---------------------------------------------------------------------------

/// Проверить имя, пришедшее из URL или формы, прежде чем подставлять его в путь.
///
/// Это **не** дублирование проверок ядра, а защита уровня веба: имена попадают
/// не только в файлы схем (`schemas/<entity>.toml`), но и в пути данных
/// (`data/<entity>/runs/<run_id>/…`), где ядро своих проверок не делает.
pub(crate) fn parse_path_token(raw: &str, what: &str) -> Result<String, String> {
    let token = raw.trim();
    let ok = !token.is_empty()
        && token.len() <= 128
        && !token.contains("..")
        && token
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.');
    if ok {
        Ok(token.to_string())
    } else {
        Err(format!("invalid {what} name"))
    }
}

/// Имя файла внутри папки сущности.
///
/// В отличие от имени сущности, имя файла — это то, что лежит на диске: пробелы,
/// запятые и кириллица тут обычное дело. Поэтому запрещаем только то, что может
/// увести из папки (разделители пути и `..`) и управляющие символы. Дополнительно
/// вызывающий обязан сверить имя со списком файлов сущности — это и есть главная
/// защита, а это правило лишь второе.
pub(crate) fn parse_file_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    let ok = !name.is_empty()
        && name.len() <= 255
        && name != ".."
        && !name.contains("..")
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(|c| c.is_control());
    if ok {
        Ok(name.to_string())
    } else {
        Err(format!("invalid file name '{raw}'"))
    }
}

/// Имя колонки из формы: это текст из заголовка CSV, поэтому пробелы и знаки
/// препинания допустимы — отвергаем только пустое, слишком длинное и
/// управляющие символы (они ломают и TOML, и таблицу).
pub(crate) fn parse_column_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    let ok = !name.is_empty() && name.len() <= 128 && !name.chars().any(|c| c.is_control());
    if ok {
        Ok(name.to_string())
    } else {
        Err(format!("invalid column name '{raw}'"))
    }
}

/// Процентное кодирование значения для query-строки (`?file=…`).
///
/// Имена файлов бывают с пробелами и кириллицей, а вставляются они в `href`
/// напрямую — кодируем всё, кроме «незарезервированных» символов RFC 3986.
pub(crate) fn url_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Ключ черновика в памяти сервиса: пользователь + воркспейс + сущность.
///
/// Разделитель `:` — чтобы два пользователя с одинаковыми slug/entity не делили
/// один черновик.
pub(crate) fn draft_key(user_id: i64, slug: &str, entity: &str) -> String {
    format!("{user_id}:{slug}:{entity}")
}

// ---------------------------------------------------------------------------
// Человекочитаемое форматирование (для шаблонов)
// ---------------------------------------------------------------------------

/// Размер файла в удобных единицах (`512 B`, `12 KB`, `1.4 MB`).
pub(crate) fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.2} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Длительность прогона (`420 ms`, `2.14 s`, `1m 05s`).
pub(crate) fn format_duration(ms: u128) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.2} s", ms as f64 / 1000.0)
    } else {
        let total_seconds = ms / 1000;
        format!("{}m {:02}s", total_seconds / 60, total_seconds % 60)
    }
}

// ---------------------------------------------------------------------------
// Тесты: чистые функции без HTTP и без файловой системы
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dtype_labels_are_parsed_strictly() {
        assert_eq!(parse_dtype("i64").expect("i64"), "i64");
        assert_eq!(parse_dtype("  F64 ").expect("F64"), "f64");
        assert_eq!(parse_dtype("STR").expect("STR"), "str");
        // Всё, что ядро не умеет кастить, отвергаем до сохранения схемы.
        assert!(parse_dtype("date").is_err());
        assert!(parse_dtype("int64").is_err());
        assert!(parse_dtype("").is_err());
        assert!(DTYPE_LABELS.contains(&parse_dtype("bool").expect("bool").as_str()));
    }

    #[test]
    fn severity_is_parsed_and_printed_back() {
        assert_eq!(parse_severity("error").expect("error"), Severity::Error);
        assert_eq!(
            parse_severity(" Warning ").expect("warning"),
            Severity::Warning
        );
        assert_eq!(parse_severity("warn").expect("warn"), Severity::Warning);
        assert!(parse_severity("fatal").is_err());
        assert_eq!(severity_token(Severity::Error), "error");
        assert_eq!(severity_token(Severity::Warning), "warning");
    }

    #[test]
    fn not_null_and_unique_need_no_parameters() {
        let rule = parse_rule("id", "error", "not_null", "", "", "", "", "").expect("правило");
        assert_eq!(rule.column, "id");
        assert_eq!(rule.kind, RuleKind::NotNull);
        assert_eq!(rule_kind_token(&rule.kind), "not_null");
        assert_eq!(rule_params_text(&rule.kind), "—");

        let unique = parse_rule_kind("unique", "1", "2", "x", "%Y", "a").expect("unique");
        assert_eq!(unique, RuleKind::Unique);
    }

    #[test]
    fn range_parses_bounds_and_rejects_bad_input() {
        let both = parse_rule_kind("range", "0", "1000", "", "", "").expect("range");
        assert_eq!(
            both,
            RuleKind::Range {
                min: Some(0.0),
                max: Some(1000.0)
            }
        );
        assert_eq!(rule_params_text(&both), "0 … 1000");

        let lower = parse_rule_kind("range", "-1.5", "", "", "", "").expect("только min");
        assert_eq!(
            lower,
            RuleKind::Range {
                min: Some(-1.5),
                max: None
            }
        );
        assert_eq!(rule_params_text(&lower), "≥ -1.5");

        // Пустые границы — правило без смысла; перепутанные — сразу видно.
        assert!(parse_rule_kind("range", "", "", "", "", "").is_err());
        assert!(parse_rule_kind("range", "10", "1", "", "", "").is_err());
        assert!(parse_rule_kind("range", "abc", "", "", "", "").is_err());
    }

    #[test]
    fn regex_and_date_format_require_text() {
        let regex = parse_rule_kind("regex", "", "", "^[A-Z]{2}$", "", "").expect("regex");
        assert_eq!(
            regex,
            RuleKind::Regex {
                pattern: String::from("^[A-Z]{2}$")
            }
        );
        assert_eq!(rule_params_text(&regex), "^[A-Z]{2}$");
        assert!(parse_rule_kind("regex", "", "", "   ", "", "").is_err());

        let date = parse_rule_kind("date_format", "", "", "", "%Y-%m-%d", "").expect("формат");
        assert_eq!(
            date,
            RuleKind::DateFormat {
                format: String::from("%Y-%m-%d")
            }
        );
        assert!(parse_rule_kind("date_format", "", "", "", "", "").is_err());
    }

    #[test]
    fn allowed_values_split_on_commas_and_skip_empties() {
        assert_eq!(
            split_values(" draft, paid ,, refunded "),
            vec!["draft", "paid", "refunded"]
        );
        assert!(split_values("  ,  , ").is_empty());

        let kind =
            parse_rule_kind("allowed_values", "", "", "", "", "draft, paid").expect("список");
        assert_eq!(
            kind,
            RuleKind::AllowedValues {
                values: vec![String::from("draft"), String::from("paid")]
            }
        );
        assert_eq!(rule_params_text(&kind), "draft, paid");
        assert!(parse_rule_kind("allowed_values", "", "", "", "", "").is_err());
    }

    #[test]
    fn unknown_rule_type_and_empty_column_are_reported() {
        assert!(parse_rule_kind("foreign_key", "", "", "", "", "").is_err());
        let error = parse_rule("", "error", "not_null", "", "", "", "", "").unwrap_err();
        assert!(
            error.contains("column"),
            "сообщение должно говорить о колонке"
        );
    }

    #[test]
    fn path_tokens_reject_traversal_and_separators() {
        assert_eq!(
            parse_path_token(" sales-2026 ", "entity").expect("ok"),
            "sales-2026"
        );
        assert_eq!(
            parse_path_token("run-20260101-120000.123", "run").expect("ok"),
            "run-20260101-120000.123"
        );
        assert!(parse_path_token("..", "entity").is_err());
        assert!(parse_path_token("../etc/passwd", "entity").is_err());
        assert!(parse_path_token("a/b", "entity").is_err());
        assert!(parse_path_token("", "entity").is_err());
        assert!(parse_path_token("a b", "entity").is_err());
    }

    #[test]
    fn column_names_allow_csv_text_but_not_control_chars() {
        assert_eq!(parse_column_name(" amount ").expect("ok"), "amount");
        assert_eq!(parse_column_name("Сумма, руб.").expect("ok"), "Сумма, руб.");
        assert!(parse_column_name("").is_err());
        assert!(parse_column_name("   ").is_err());
        assert!(parse_column_name("bad\nname").is_err());
    }

    #[test]
    fn file_names_allow_real_world_names_but_not_paths() {
        assert_eq!(
            parse_file_name(" sales_01.csv ").expect("ok"),
            "sales_01.csv"
        );
        assert_eq!(
            parse_file_name("Итоги 2026, май.csv").expect("ok"),
            "Итоги 2026, май.csv"
        );
        assert!(parse_file_name("").is_err());
        assert!(parse_file_name("..").is_err());
        assert!(parse_file_name("../secret.csv").is_err());
        assert!(parse_file_name("sub/dir.csv").is_err());
        assert!(parse_file_name("sub\\dir.csv").is_err());
    }

    #[test]
    fn url_encoding_escapes_everything_but_unreserved() {
        assert_eq!(url_encode("sales_01.csv"), "sales_01.csv");
        assert_eq!(url_encode("a b"), "a%20b");
        assert_eq!(url_encode("a/b"), "a%2Fb");
        assert_eq!(url_encode("да"), "%D0%B4%D0%B0");
    }

    #[test]
    fn draft_keys_are_scoped_to_user_and_workspace() {
        assert_eq!(draft_key(7, "sales", "orders"), "7:sales:orders");
        assert_ne!(
            draft_key(7, "sales", "orders"),
            draft_key(8, "sales", "orders")
        );
        assert_ne!(
            draft_key(7, "sales", "orders"),
            draft_key(7, "other", "orders")
        );
    }

    #[test]
    fn sizes_and_durations_are_human_readable() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(1_572_864), "1.5 MB");
        assert_eq!(human_size(5_368_709_120), "5.00 GB");

        assert_eq!(format_duration(0), "0 ms");
        assert_eq!(format_duration(420), "420 ms");
        assert_eq!(format_duration(2140), "2.14 s");
        assert_eq!(format_duration(65_000), "1m 05s");
        assert_eq!(format_duration(3_600_000), "60m 00s");
    }
}
