# Strata — Data Engineering Workbench (сырой слой: файлы → Parquet)

Strata принимает **папки с грязными табличными файлами** (CSV/TSV/XLSX/Parquet),
сама предлагает схему, проверяет типы, пишет **Parquet-датасет** частями и
показывает проблемы — это первый (raw/staging) слой будущего ODS, а не «красивый
файловый менеджер».

```text
Файлы (CSV/TSV/XLSX/Parquet) → чтение по правилам → схема → проверка типов → Parquet-части
                                     ↑                                              ↓
                              кодировки, разделители                     data/<сущность>/part-*.parquet
```

## Что уже работает

| Возможность | Где |
|---|---|
| Чтение CSV/TSV (авто-разделитель `, ; \t \|`), Parquet, **Excel (XLSX)** | `strata-core` |
| Кодировки: UTF-8 (±BOM), UTF-16 LE/BE, windows-1251/1252 + ручной выбор | `strata-core` |
| Инференс схемы, конфликты типов между файлами, правило «одна папка = одна схема» | `strata-core` |
| **Правила совместимости типов** (числовое расширение `i64→f64`) и каст при записи | `strata-core` |
| Валидированный stage: несоответствующий файл не пишется молча, а попадает в отчёт | `strata-core` |
| Партиционирование датасета (`col=value/…`), рекурсивный обзор частей | `strata-core` |
| Аккаунты, сессии, приватные воркспейсы (`workspaces/<user_id>/…`) | `strata-web` |
| Веб-UI: пайплайн (scan → confirm → stage) на axum + **htmx** + Tailwind | `strata-web` |
| Фоновые задачи с прогресс-баром (htmx-поллинг), метрики `/metrics`, виджет RAM/CPU | `strata-web` |
| Docker: multi-stage + **cargo-chef**, non-root, healthcheck, compose | корень репо |

Тесты: **39 в ядре + 2 в веб-сервисе**. Проверки: `./scripts/check.sh`.

## Архитектура (кратко)

```text
        браузер (htmx, без JS-состояния)
                │  HTML-фрагменты
        ┌───────▼───────────────────────────────┐
        │  strata-web (axum + askama)           │  ← только HTTP: формы, сессии, задачи
        └───────┬───────────────────────────────┘
                │  вызовы фасада (стабильный API)
        ┌───────▼───────────────────────────────┐
        │  strata_core::api  (DTO + ApiError)   │  ← «свой API ядра»
        └───────┬───────────────────────────────┘
                │
        ┌───────▼───────────────────────────────┐
        │  движок: Polars / Arrow / Parquet / ФС │
        └───────────────────────────────────────┘
```

Правило слоёв: **веб-слой не знает про Polars**, а ядро не знает про веб.
Подробнее — `docs/guide-web.md`.

Дальше по платформе — модель «командный пункт + ноды» (обработка там, где лежат
данные, вместо загрузки гигабайтов на сервер), с критической оценкой и фазами:
`docs/design-platform-nodes.md`.

## Быстрый старт

### Локально

```bash
export STRATA_WORKSPACE_ROOT=$PWD/workspaces      # где хранятся воркспейсы
export STRATA_SOURCE_ROOTS=/path/to/your/data     # allowlist папок-источников (':'-разделитель)
export STRATA_DB_URL="sqlite://$PWD/workspaces/strata.db?mode=rwc"
cargo run -p strata-web
# http://127.0.0.1:8080 → регистрация → создать воркспейс → указать корень → Scan → Confirm → Stage
```

### Docker

```bash
docker compose up --build        # http://localhost:8080
# том strata-workspaces (схемы + части Parquet), ./data монтируется read-only как sources
```

## Репозиторий

```text
crates/strata-core/   движок + фасад strata_core::api (без UI и без web)
crates/strata-web/    сервис: axum + askama-шаблоны + htmx + Tailwind, auth, задачи, метрики
docs/                 дизайны и учебные гайды (design-*, guide-*); docs/archive — легаси
scripts/check.sh      fmt + test + check
scripts/bench.sh      бенчмарк ingest (release, thin-LTO)
```

Dioxus-десктоп удалён: сервисом он стать не мог, htmx закрывает те же сценарии
дешевле. Последняя версия — коммит `0838e06`, учебные заметки — `docs/archive/`.

## Скриншоты

`screenshot/screenshot1.png` — **легаси-скриншот удалённого Dioxus-десктопа**
(оставлен как история). Актуальный веб-интерфейс: запустите
`cargo run -p strata-web` и откройте <http://127.0.0.1:8080>.

## Планы

- **P1:** интерфейс `ExecutionTarget` + backend `Local` (заморозить API исполнения);
- **P2:** продуктовая ценность — редактор/подтверждение схем, валидация и карантин (M3), каталог датасетов;
- **P3:** `RemoteNode` — CLI `strata-node`, реестр нод, подписанные задачи, long-poll;
- **P4:** пул воркеров, квоты — только при реальном спросе.

Документация:
- **`docs/design-ui-pipeline.md`** — спецификация интерфейса: стадии, экраны, тексты, маршруты htmx;
- `docs/design-platform-nodes.md` — платформа (командный пункт + ноды) и критическая оценка с фазами;
- `docs/design-workspace-pipeline.md` — модель workspace и пайплайна;
- `docs/guide-web.md` — как устроен веб-стек (axum/htmx/Tailwind, auth, задачи, метрики);
- `PLAN.md` — актуальный план и статусы.
