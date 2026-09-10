//! Стадии сущности: табы, пилюли статусов и «умный вход».
//!
//! Одна сущность — одна страница со стадиями-табами (см.
//! `docs/design-ui-pipeline.md` §2). Здесь живёт всё, что нужно, чтобы показать
//! путь целиком: перечисление стадий [`Tab`], снимок их состояния
//! [`StageStatus`] и пилюли [`PillView`] для таб-полоски и хаба.
//!
//! Важно: снятие статуса — **дешёвые** чтения (листинг папки, файл схемы,
//! манифесты прогонов). Инференс схемы тут не запускается: иначе хаб читал бы
//! все файлы всех сущностей при каждой отрисовке.

use strata_core::api;

/// Стадия пайплайна — таб на странице `/w/{ws}/e/{entity}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    /// Файлы и превью чтения.
    Files,
    /// Схема: предложение инференса или подтверждённый контракт.
    Schema,
    /// Бизнес-правила и ограничения.
    Rules,
    /// ODS, карантин, передача.
    Ods,
    /// Логи прогонов.
    Logs,
}

impl Tab {
    /// Все стадии в порядке пайплайна.
    pub(crate) const ALL: [Tab; 5] = [Tab::Files, Tab::Schema, Tab::Rules, Tab::Ods, Tab::Logs];

    /// Ключ стадии в URL (`?tab=…`).
    pub(crate) fn key(self) -> &'static str {
        match self {
            Tab::Files => "files",
            Tab::Schema => "schema",
            Tab::Rules => "rules",
            Tab::Ods => "ods",
            Tab::Logs => "logs",
        }
    }

    /// Подпись таба.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Tab::Files => "Files",
            Tab::Schema => "Schema",
            Tab::Rules => "Rules",
            Tab::Ods => "ODS",
            Tab::Logs => "Logs",
        }
    }

    /// Разобрать `?tab=…`. Неизвестное значение — не ошибка: просто «выбора
    /// нет», и страница откроет первую незавершённую стадию.
    pub(crate) fn parse(raw: &str) -> Option<Tab> {
        let normalized = raw.trim().to_ascii_lowercase();
        Tab::ALL.iter().copied().find(|tab| tab.key() == normalized)
    }
}

/// Пилюля статуса стадии (рисуется рядом с табом и в строке сущности в хабе).
#[derive(Debug, Clone)]
pub(crate) struct PillView {
    /// Подпись стадии (`Files`, `Schema`, …).
    pub(crate) label: String,
    /// Короткий статус (`8 file(s)`, `confirmed`, `no run`).
    pub(crate) text: String,
    /// Классы Tailwind для рамки/текста пилюли.
    pub(crate) class: String,
}

/// Ссылка на таб с пилюлей статуса этой стадии.
#[derive(Debug, Clone)]
pub(crate) struct TabView {
    /// Ключ стадии (`files`, `schema`, …) — часть URL `?tab=`.
    pub(crate) key: String,
    /// Подпись таба.
    pub(crate) label: String,
    /// Классы Tailwind: активный таб подсвечивается.
    pub(crate) class: String,
    /// Пилюля статуса стадии.
    pub(crate) pill: PillView,
}

/// Снимок стадий сущности: из него строятся пилюли и выбирается таб по умолчанию.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StageStatus {
    /// Сколько файлов лежит в привязанной папке.
    pub(crate) files: usize,
    /// Подтверждена ли схема (`schemas/<entity>.toml` на диске).
    pub(crate) schema_confirmed: bool,
    /// Сколько правил в подтверждённой схеме (0, если схема ещё не подтверждена).
    pub(crate) rules: usize,
    /// Сколько прогонов у сущности.
    pub(crate) runs: usize,
    /// Есть ли успешный прогон (без нарушений уровня error).
    pub(crate) ods_ok: bool,
    /// Строк в ODS последнего успешного прогона.
    pub(crate) ods_rows: u64,
    /// Частей ODS в последнем успешном прогоне.
    pub(crate) ods_parts: usize,
}

impl StageStatus {
    /// Первая незавершённая стадия — «умный вход» на страницу сущности:
    /// пользователь не выбирает таб вручную.
    ///
    /// Когда всё сделано, открываем ODS (там лежит результат — то, что отдают).
    pub(crate) fn first_unfinished(&self) -> Tab {
        if self.files == 0 {
            Tab::Files
        } else if !self.schema_confirmed {
            Tab::Schema
        } else if self.rules == 0 {
            Tab::Rules
        } else {
            Tab::Ods
        }
    }

    /// Пилюля одной стадии.
    pub(crate) fn pill(&self, tab: Tab) -> PillView {
        match tab {
            Tab::Files => PillView {
                label: tab.label().to_string(),
                text: if self.files == 0 {
                    String::from("no files")
                } else {
                    format!("{} file(s)", self.files)
                },
                class: tone_class(self.files > 0).to_string(),
            },
            Tab::Schema => PillView {
                label: tab.label().to_string(),
                text: if self.schema_confirmed {
                    String::from("confirmed")
                } else {
                    String::from("proposal")
                },
                class: if self.schema_confirmed {
                    TONE_OK.to_string()
                } else {
                    TONE_WARN.to_string()
                },
            },
            Tab::Rules => PillView {
                label: tab.label().to_string(),
                text: if self.rules == 0 {
                    String::from("no rules")
                } else {
                    format!("{} rule(s)", self.rules)
                },
                class: tone_class(self.rules > 0).to_string(),
            },
            Tab::Ods => PillView {
                label: tab.label().to_string(),
                text: if self.ods_ok {
                    format!("{} row(s) · {} part(s)", self.ods_rows, self.ods_parts)
                } else if self.runs > 0 {
                    String::from("errors")
                } else {
                    String::from("no run")
                },
                class: if self.ods_ok {
                    TONE_OK.to_string()
                } else if self.runs > 0 {
                    TONE_BAD.to_string()
                } else {
                    TONE_NONE.to_string()
                },
            },
            Tab::Logs => PillView {
                label: tab.label().to_string(),
                text: if self.runs == 0 {
                    String::from("no runs")
                } else {
                    format!("{} run(s)", self.runs)
                },
                class: tone_class(self.runs > 0).to_string(),
            },
        }
    }

    /// Все пилюли по порядку стадий — строка сущности в хабе.
    pub(crate) fn pills(&self) -> Vec<PillView> {
        Tab::ALL.iter().map(|tab| self.pill(*tab)).collect()
    }

    /// Таб-полоска целиком: подпись, пилюля и признак активности на каждую
    /// стадию. Ссылки собирает шаблон (ему известны slug и сущность).
    pub(crate) fn tabs(&self, active: Tab) -> Vec<TabView> {
        Tab::ALL
            .iter()
            .map(|tab| {
                let active_class = if *tab == active {
                    "border-[#4da3ff] text-slate-100"
                } else {
                    "border-slate-700 text-slate-400 hover:text-slate-200"
                };
                TabView {
                    key: tab.key().to_string(),
                    label: tab.label().to_string(),
                    class: format!(
                        "flex items-center gap-2 rounded-lg border px-3 py-1.5 text-xs {active_class}"
                    ),
                    pill: self.pill(*tab),
                }
            })
            .collect()
    }
}

/// Тон «готово».
pub(crate) const TONE_OK: &str = "border-emerald-700 text-emerald-400";
/// Тон «есть проблема / не завершено».
pub(crate) const TONE_WARN: &str = "border-amber-700 text-amber-400";
/// Тон «ошибка».
pub(crate) const TONE_BAD: &str = "border-red-700 text-red-400";
/// Тон «не начато».
pub(crate) const TONE_NONE: &str = "border-slate-700 text-slate-500";

/// Класс пилюли по булеву признаку «есть/нет».
fn tone_class(present: bool) -> &'static str {
    if present { TONE_OK } else { TONE_NONE }
}

/// Снять статусы стадий сущности.
///
/// `has_schema` берётся из привязки (`api::EntityInfo`), чтобы **не** запускать
/// инференс: для неподтверждённой схемы число правил заведомо ноль.
pub(crate) fn entity_status(dir: &std::path::Path, entity: &str, has_schema: bool) -> StageStatus {
    let files = api::entity_files(dir, entity)
        .map(|files| files.len())
        .unwrap_or(0);
    let rules = if has_schema {
        api::entity_schema_view(dir, entity)
            .map(|view| view.rules.len())
            .unwrap_or(0)
    } else {
        0
    };
    let runs = api::entity_runs(dir, entity).unwrap_or_default();
    let ok_run = runs.iter().find(|run| !run.has_errors);
    StageStatus {
        files,
        schema_confirmed: has_schema,
        rules,
        runs: runs.len(),
        ods_ok: ok_run.is_some(),
        ods_rows: ok_run.map(|run| run.rows_valid).unwrap_or(0),
        ods_parts: ok_run.map(|run| run.parts.len()).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_keys_roundtrip_and_unknown_is_none() {
        for tab in Tab::ALL {
            assert_eq!(Tab::parse(tab.key()), Some(tab));
        }
        assert_eq!(Tab::parse(" ODS "), Some(Tab::Ods));
        assert_eq!(Tab::parse("quarantine"), None);
        assert_eq!(Tab::parse(""), None);
    }

    #[test]
    fn smart_entry_picks_first_unfinished_stage() {
        let mut status = StageStatus::default();
        assert_eq!(status.first_unfinished(), Tab::Files);

        status.files = 8;
        assert_eq!(status.first_unfinished(), Tab::Schema);

        status.schema_confirmed = true;
        assert_eq!(status.first_unfinished(), Tab::Rules);

        status.rules = 3;
        assert_eq!(status.first_unfinished(), Tab::Ods);

        // Всё сделано → показываем результат, а не логи.
        status.ods_ok = true;
        status.ods_rows = 23;
        status.ods_parts = 3;
        assert_eq!(status.first_unfinished(), Tab::Ods);
    }

    #[test]
    fn pills_describe_each_stage() {
        let status = StageStatus {
            files: 8,
            schema_confirmed: true,
            rules: 5,
            runs: 2,
            ods_ok: true,
            ods_rows: 23,
            ods_parts: 3,
        };
        let files = status.pill(Tab::Files);
        assert_eq!(files.text, "8 file(s)");
        assert_eq!(files.class, TONE_OK);
        assert_eq!(status.pill(Tab::Schema).text, "confirmed");
        assert_eq!(status.pill(Tab::Rules).text, "5 rule(s)");
        assert_eq!(status.pill(Tab::Ods).text, "23 row(s) · 3 part(s)");
        assert_eq!(status.pill(Tab::Logs).text, "2 run(s)");

        // Неподтверждённая схема и прогон с ошибками видны без открытия карточки.
        let draft = StageStatus {
            files: 3,
            schema_confirmed: false,
            runs: 1,
            ..StageStatus::default()
        };
        assert_eq!(draft.pill(Tab::Schema).text, "proposal");
        assert_eq!(draft.pill(Tab::Schema).class, TONE_WARN);
        assert_eq!(draft.pill(Tab::Files).text, "3 file(s)");
        assert_eq!(draft.pill(Tab::Rules).text, "no rules");
        assert_eq!(draft.pill(Tab::Ods).text, "errors");
        assert_eq!(draft.pill(Tab::Ods).class, TONE_BAD);
        assert_eq!(draft.pill(Tab::Logs).text, "1 run(s)");
    }

    #[test]
    fn tabs_mark_the_active_stage() {
        let status = StageStatus::default();
        let tabs = status.tabs(Tab::Schema);
        assert_eq!(tabs.len(), Tab::ALL.len());
        let active = tabs.iter().find(|tab| tab.key == "schema").expect("таб");
        assert!(active.class.contains("#4da3ff"), "активный таб подсвечен");
        let idle = tabs.iter().find(|tab| tab.key == "files").expect("таб");
        assert!(!idle.class.contains("#4da3ff"));
        assert_eq!(active.pill.label, "Schema");
    }
}
