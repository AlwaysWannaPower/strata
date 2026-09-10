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

// ---------------------------------------------------------------------------
// Степпер пайплайна воркспейса: 1 Source · 2 Schema · 3 Rules · 4 ODS
// ---------------------------------------------------------------------------

/// Крупный шаг пути пользователя — то, что рисует степпер на `/w/{slug}`.
///
/// Это «внешний» уровень по отношению к стадиям сущности ([`Tab`]): у
/// воркспейса шагов четыре, потому что шаг 1 (источник) заканчивается, как
/// только появилась хотя бы одна сущность, и дальше работа идёт с ней.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipelineStep {
    /// Куда смотреть: папка с файлами ещё не привязана ни к одной сущности.
    Source,
    /// Схема: колонки и типы подтверждены.
    Schema,
    /// Правила: проверки, которым должны удовлетворять данные.
    Rules,
    /// Прогон: ODS + карантин + манифест.
    Ods,
}

impl PipelineStep {
    /// Шаги в порядке пути пользователя.
    pub(crate) const ALL: [PipelineStep; 4] = [
        PipelineStep::Source,
        PipelineStep::Schema,
        PipelineStep::Rules,
        PipelineStep::Ods,
    ];

    /// Номер шага (1…4) — рисуется перед подписью («2 Schema»).
    pub(crate) fn number(self) -> usize {
        match self {
            PipelineStep::Source => 1,
            PipelineStep::Schema => 2,
            PipelineStep::Rules => 3,
            PipelineStep::Ods => 4,
        }
    }

    /// Подпись шага.
    pub(crate) fn label(self) -> &'static str {
        match self {
            PipelineStep::Source => "Source",
            PipelineStep::Schema => "Schema",
            PipelineStep::Rules => "Rules",
            PipelineStep::Ods => "ODS",
        }
    }

    /// Что происходит на шаге — одно короткое пояснение (его же показывает
    /// полоса «как это работает» на главной, чтобы текст не дублировался).
    pub(crate) fn detail(self) -> &'static str {
        match self {
            PipelineStep::Source => "Point at a folder with your files",
            PipelineStep::Schema => "Confirm columns and types",
            PipelineStep::Rules => "Add the checks your data must pass",
            PipelineStep::Ods => "Run it and get clean ODS",
        }
    }

    /// Вкладка сущности, где живёт шаг. У шага 1 её нет: сущности ещё нет, и
    /// работать не над чем — сначала нужно её создать (пустое состояние хаба).
    pub(crate) fn tab(self) -> Option<Tab> {
        match self {
            PipelineStep::Source => Option::None,
            PipelineStep::Schema => Some(Tab::Schema),
            PipelineStep::Rules => Some(Tab::Rules),
            PipelineStep::Ods => Some(Tab::Ods),
        }
    }

    /// Что этот шаг добавит к данным — подпись для «Next: … for sales».
    fn action(self) -> Option<&'static str> {
        match self {
            PipelineStep::Source => Option::None,
            PipelineStep::Schema => Some("schema"),
            PipelineStep::Rules => Some("rules"),
            PipelineStep::Ods => Some("run"),
        }
    }
}

/// Один шаг степпера, готовый к отрисовке.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepView {
    /// `1 Source` — номер и подпись.
    pub(crate) title: String,
    /// Короткое пояснение шага.
    pub(crate) detail: String,
    /// Ссылка на вкладку сущности (шаги 2–4, когда сущность уже есть).
    pub(crate) href: Option<String>,
    /// Состояние шага: `done`, `now` или `todo` (нужно тестам и верстке).
    pub(crate) state: String,
    /// Классы Tailwind для рамки/текста.
    pub(crate) class: String,
}

/// Подсказка «что делать дальше» — одна строка со ссылкой.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HintView {
    /// Текст подсказки (`Next: schema for sales`).
    pub(crate) text: String,
    /// Куда ведёт ссылка (вкладка сущности).
    pub(crate) href: String,
}

/// Где пользователь находится на пути «источник → схема → правила → ODS».
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipelineProgress {
    /// Текущий шаг.
    pub(crate) current: PipelineStep,
    /// Сколько шагов уже пройдено (0…4) — из этого рисуются галочки.
    pub(crate) completed: usize,
    /// Есть успешный прогон: весь путь пройден, данные можно отдавать.
    pub(crate) ready_to_serve: bool,
    /// Сущность, на которой стоит работать (её открывает ссылка шага).
    pub(crate) focus_entity: Option<String>,
}

impl PipelineProgress {
    /// Вывести шаг из статусов сущностей воркспейса.
    ///
    /// Чистая функция: никаких файлов и БД, поэтому правила читаются прямо
    /// здесь, а тесты не трогают диск. Приоритет проверок — ровно как в
    /// постановке, сверху вниз:
    ///
    /// 1. ни одной сущности → шаг 1 (нужен источник);
    /// 2. есть сущность без подтверждённой схемы → шаг 2;
    /// 3. ни у одной подтверждённой схемы нет правил → шаг 3;
    /// 4. схемы подтверждены, но успешных прогонов нет → шаг 4 («готово к прогону»);
    /// 5. есть успешный прогон → весь путь пройден («ready to serve»).
    ///
    /// Вход — пары «имя сущности + её статус»: так функция остаётся независимой
    /// от структур веб-слоя (см. `entity::pipeline_progress`).
    pub(crate) fn from_statuses(rows: &[(String, StageStatus)]) -> PipelineProgress {
        // Шаг 1: работать не с чем — сначала папка с файлами.
        if rows.is_empty() {
            return PipelineProgress {
                current: PipelineStep::Source,
                completed: 0,
                ready_to_serve: false,
                focus_entity: Option::None,
            };
        }

        // Шаг 2: первая сущность, у которой схема ещё не подтверждена.
        if let Some((entity, _)) = rows.iter().find(|(_, status)| !status.schema_confirmed) {
            return PipelineProgress {
                current: PipelineStep::Schema,
                completed: 1,
                ready_to_serve: false,
                focus_entity: Some(entity.clone()),
            };
        }

        // Шаг 3: все схемы подтверждены, но правил ещё нет ни у одной.
        if rows.iter().all(|(_, status)| status.rules == 0) {
            return PipelineProgress {
                current: PipelineStep::Rules,
                completed: 2,
                ready_to_serve: false,
                focus_entity: rows.first().map(|(entity, _)| entity.clone()),
            };
        }

        // Шаги 4/5: правила есть. Смотрим на прогоны: успешный прогон закрывает
        // весь путь, иначе остаётся «готово к прогону».
        let pending = rows.iter().find(|(_, status)| !status.ods_ok);
        match pending {
            Some((entity, _)) => PipelineProgress {
                current: PipelineStep::Ods,
                completed: 3,
                ready_to_serve: false,
                focus_entity: Some(entity.clone()),
            },
            Option::None => PipelineProgress {
                current: PipelineStep::Ods,
                completed: PipelineStep::ALL.len(),
                ready_to_serve: true,
                focus_entity: Option::None,
            },
        }
    }

    /// Шаги степпера с подсветкой текущего. Ссылки ведут в ту же сущность,
    /// поэтому шаги 2–4 — обычные ссылки на её вкладку (`?tab=…`).
    pub(crate) fn steps(&self, slug: &str) -> Vec<StepView> {
        PipelineStep::ALL
            .iter()
            .map(|step| {
                let state = if step.number() <= self.completed {
                    "done"
                } else if *step == self.current {
                    "now"
                } else {
                    "todo"
                };
                StepView {
                    title: format!("{} {}", step.number(), step.label()),
                    detail: step.detail().to_string(),
                    href: self.step_href(slug, *step),
                    state: state.to_string(),
                    class: format!(
                        "flex-1 min-w-[160px] rounded-lg border px-3 py-2 text-xs {}",
                        match state {
                            "done" => TONE_OK,
                            "now" => "border-[#4da3ff] text-slate-100",
                            _ => TONE_NONE,
                        }
                    ),
                }
            })
            .collect()
    }

    /// Ссылка шага: только если шаг ведёт в сущность, а она уже есть.
    fn step_href(&self, slug: &str, step: PipelineStep) -> Option<String> {
        let (tab, entity) = (step.tab()?, self.focus_entity.as_deref()?);
        Some(format!("/w/{slug}/e/{entity}?tab={}", tab.key()))
    }

    /// Строка «Next: schema for sales» со ссылкой — чтобы следующий шаг был
    /// виден всегда, даже когда список сущностей длинный.
    ///
    /// Возвращает `None`, когда подсказывать нечего: пустой воркспейс (там своя
    /// карточка «Add your data») и полностью пройденный путь (там бейдж
    /// «ready to serve»).
    pub(crate) fn hint(&self, slug: &str) -> Option<HintView> {
        let action = self.current.action()?;
        let entity = self.focus_entity.as_deref()?;
        let href = self.step_href(slug, self.current)?;
        Some(HintView {
            text: format!("Next: {action} for {entity}"),
            href,
        })
    }

    /// Короткая сводка для шапки степпера.
    pub(crate) fn badge(&self) -> String {
        if self.ready_to_serve {
            String::from("ready to serve")
        } else {
            format!(
                "Step {} of {} · {}",
                self.current.number(),
                PipelineStep::ALL.len(),
                self.current.label()
            )
        }
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

    /// Статус сущности для тестов степпера (лишь то, что влияет на шаг).
    fn status(schema_confirmed: bool, rules: usize, runs: usize, ods_ok: bool) -> StageStatus {
        StageStatus {
            files: 1,
            schema_confirmed,
            rules,
            runs,
            ods_ok,
            ..StageStatus::default()
        }
    }

    /// Пара «сущность + статус» — вход чистой функции шага.
    fn row(entity: &str, status: StageStatus) -> (String, StageStatus) {
        (entity.to_string(), status)
    }

    #[test]
    fn pipeline_step_walks_source_schema_rules_ods() {
        // 1. Ни одной сущности — нужен источник.
        let empty = PipelineProgress::from_statuses(&[]);
        assert_eq!(empty.current, PipelineStep::Source);
        assert_eq!(empty.completed, 0);
        assert_eq!(empty.focus_entity, None);
        assert!(!empty.ready_to_serve);

        // 2. Есть сущность без подтверждённой схемы.
        let proposed = PipelineProgress::from_statuses(&[row("sales", status(false, 0, 0, false))]);
        assert_eq!(proposed.current, PipelineStep::Schema);
        assert_eq!(proposed.completed, 1);
        assert_eq!(proposed.focus_entity.as_deref(), Some("sales"));

        // Готовая схема у одной сущности не спасает, если у второй её нет:
        // ведём пользователя к незаполненной.
        let mixed = PipelineProgress::from_statuses(&[
            row("sales", status(true, 3, 0, false)),
            row("clients", status(false, 0, 0, false)),
        ]);
        assert_eq!(mixed.current, PipelineStep::Schema);
        assert_eq!(mixed.focus_entity.as_deref(), Some("clients"));

        // 3. Схемы подтверждены, но правил нет ни у одной.
        let no_rules = PipelineProgress::from_statuses(&[row("sales", status(true, 0, 0, false))]);
        assert_eq!(no_rules.current, PipelineStep::Rules);
        assert_eq!(no_rules.completed, 2);
        assert_eq!(no_rules.focus_entity.as_deref(), Some("sales"));

        // 4. Правила есть, успешных прогонов нет — «готово к прогону».
        // Прогон с ошибками успешным не считается, поэтому ведём к той сущности,
        // у которой ещё не было чистого прогона.
        let ready_to_run = PipelineProgress::from_statuses(&[
            row("sales", status(true, 5, 1, true)),
            row("clients", status(true, 2, 0, false)),
        ]);
        assert_eq!(ready_to_run.current, PipelineStep::Ods);
        assert_eq!(ready_to_run.completed, 3);
        assert_eq!(ready_to_run.focus_entity.as_deref(), Some("clients"));
        assert!(!ready_to_run.ready_to_serve);

        // 5. Есть успешный прогон — весь путь пройден.
        let served = PipelineProgress::from_statuses(&[
            row("sales", status(true, 5, 1, true)),
            row("clients", status(true, 2, 1, true)),
        ]);
        assert_eq!(served.current, PipelineStep::Ods);
        assert_eq!(served.completed, PipelineStep::ALL.len());
        assert!(served.ready_to_serve);
        assert_eq!(served.badge(), "ready to serve");
    }

    #[test]
    fn stepper_marks_done_now_and_links_the_entity_tab() {
        let progress = PipelineProgress::from_statuses(&[row("sales", status(true, 0, 0, false))]);
        let steps = progress.steps("ws-1");
        assert_eq!(steps.len(), PipelineStep::ALL.len());
        assert_eq!(steps[0].title, "1 Source");
        assert_eq!(steps[0].state, "done");
        assert!(
            steps[0].class.contains(TONE_OK),
            "пройденный шаг зелёный: {}",
            steps[0].class
        );
        // Шаг 1 некуда вести: сущность уже есть, но «источник» — про выбор папки.
        assert_eq!(steps[0].href, None);

        let current = &steps[2];
        assert_eq!(current.title, "3 Rules");
        assert_eq!(current.state, "now");
        assert!(current.class.contains("#4da3ff"), "{}", current.class);
        assert_eq!(
            current.href.as_deref(),
            Some("/w/ws-1/e/sales?tab=rules"),
            "шаг ведёт во вкладку сущности"
        );

        // Будущий шаг помечен как «не начат» и всё ещё ведёт в ODS.
        assert_eq!(steps[3].state, "todo");
        assert!(
            steps[3].class.contains(TONE_NONE),
            "будущий шаг серый: {}",
            steps[3].class
        );
        assert_eq!(steps[3].href.as_deref(), Some("/w/ws-1/e/sales?tab=ods"));

        // Пока сущности нет, ссылок нет вообще: шаги — просто подписи.
        let empty = PipelineProgress::from_statuses(&[]);
        let steps = empty.steps("ws-1");
        assert_eq!(steps[0].state, "now");
        assert!(steps.iter().all(|step| step.href.is_none()));
    }

    #[test]
    fn next_step_hint_names_the_action_and_the_entity() {
        let schema = PipelineProgress::from_statuses(&[row("sales", status(false, 0, 0, false))]);
        let hint = schema.hint("ws-1").expect("подсказка шага 2");
        assert_eq!(hint.text, "Next: schema for sales");
        assert_eq!(hint.href, "/w/ws-1/e/sales?tab=schema");

        let rules = PipelineProgress::from_statuses(&[row("sales", status(true, 0, 0, false))]);
        assert_eq!(
            rules.hint("ws-1").expect("подсказка шага 3").text,
            "Next: rules for sales"
        );

        let run = PipelineProgress::from_statuses(&[row("sales", status(true, 4, 0, false))]);
        let hint = run.hint("ws-1").expect("подсказка шага 4");
        assert_eq!(hint.text, "Next: run for sales");
        assert_eq!(hint.href, "/w/ws-1/e/sales?tab=ods");

        // Пустой воркспейс и пройденный путь подсказки не показывают: там
        // работает карточка «Add your data» и бейдж «ready to serve».
        assert!(PipelineProgress::from_statuses(&[]).hint("ws-1").is_none());
        let served = PipelineProgress::from_statuses(&[row("sales", status(true, 4, 1, true))]);
        assert!(served.hint("ws-1").is_none());
    }

    #[test]
    fn badge_counts_the_current_step() {
        assert_eq!(
            PipelineProgress::from_statuses(&[]).badge(),
            "Step 1 of 4 · Source"
        );
        assert_eq!(
            PipelineProgress::from_statuses(&[row("sales", status(true, 1, 0, false))]).badge(),
            "Step 4 of 4 · ODS"
        );
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
