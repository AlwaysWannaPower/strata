# Guide: веб-сервис strata (axum + htmx + Tailwind)

> Почему ушли с Dioxus и как теперь всё устроено. Для человека, который учит
> бэкенд на Rust и htmx.

## 1. Почему htmx, а не Dioxus

Dioxus-десктоп давал красивую «нативную» утилиту, но:

- весь UI жил на клиенте, а данные — локально: **сервисом это не станет**;
- каждое изменение интерфейса = пересборка Rust-бинаря (медленная итерация);
- для «форм + таблиц + статусов» мы писали реактивную машину там, где хватает
  серверного HTML.

htmx — это про то же самое, но наоборот: **сервер отдаёт HTML-фрагмент, htmx
вставляет его в нужное место страницы**. Никакого JSON, никакого состояния на
клиенте, никакого bundler'а. То, что для миллиона строк понадобится JS-грид —
отдельный маленький «остров», а не архитектура.

| | Dioxus | axum + htmx |
|---|---|---|
| Формы/списки/статусы | писать реактивность | HTML-фрагменты |
| Деплой | desktop-бинарь | контейнер, URL, доступ по сети |
| Многопользовательность | нет | да (позже — auth) |
| Итерация | пересборка клиента | правка шаблона |
| Большая таблица | свой виртуализированный грид | JS-грид (AG Grid/TanStack) |

## 2. Слои (главное правило)

```text
templates/*.html        ← HTML + htmx-атрибуты, никакой логики
        ▲
handlers (axum)         ← тонкие: форма → один вызов API → фрагмент
        ▼
strata_core::api        ← стабильный фасад движка (DTO + ApiError)
        ▼
engine (polars/parquet/fs)
```

- **Крейт `strata-core` не знает про веб.** Его фасад — `strata_core::api`
  (`list_workspaces`, `scan_candidates`, `confirm_entity`, `stage_entity`, …) —
  это и есть «свой API ядра», который дергает axum. Если завтра появится CLI
  или другой фронтенд, они используют тот же фасад.
- **Крейт `strata-web`** — только HTTP: маршруты, формы, шаблоны askama, htmx.
  Он не импортирует polars и не читает `workspace.toml` сам.
- **`strata-app`** (Dioxus) остаётся в репозитории как легаси-десктоп; он в
  Docker не собирается (`-p strata-web`), чтобы не тащить WebKit.

## 3. Как работает пайплайн в браузере

```text
GET  /                        список workspace + форма создания
POST /workspaces              создать → 303 на /w/<slug>
GET  /w/{slug}                хаб: скан-форма + сущности + результаты stage
POST /w/{slug}/scan           → фрагмент candidates   (htmx → #candidates)
POST /w/{slug}/entities       → фрагмент entities     (htmx → #entities)
POST /w/{slug}/entities/{e}/stage → фрагмент stage    (htmx → #stage)
GET  /healthz                 healthcheck контейнера
```

Ключевой htmx-приём в шаблоне (см. `templates/fragments/candidates.html`):

```html
<form hx-post="/w/{{ slug }}/entities"
      hx-target="#entities"
      hx-swap="innerHTML">
```

Кнопка «Confirm &amp; bind» отправляет форму, сервер делает добро движка
(инференс схемы + запись `schemas/<entity>.toml` + привязку в
`workspace.toml`) и возвращает **готовый HTML списка сущностей** — его htmx
подменяет в блоке `#entities`. Ни строчки JS.

## 4. Безопасность: allowlist путей (важно!)

Веб-пользователь **не должен** иметь возможность прочитать любой путь сервера.
Поэтому:

- `STRATA_SOURCE_ROOTS` — список разрешённых корней (через `:`);
- каждый путь из формы проходит `ensure_allowed`: canonicalize + `starts_with`
  одного из корней, иначе 400;
- слаги workspace валидируются, `..` запрещён;
- в Docker входные данные монтируются **read-only** (`:ro`), запись — только в
  том `workspaces`.

Проверено вручную: `POST /w/demo/scan root=/etc` → `400`.

## 5. Запуск локально

```bash
export STRATA_WORKSPACE_ROOT=$PWD/workspaces
export STRATA_SOURCE_ROOTS=/path/to/your/data     # можно несколько через ':'
export STRATA_ADDR=127.0.0.1:8080
cargo run -p strata-web
```

Открой <http://127.0.0.1:8080>, создай workspace, укажи корень — увидишь
кандидатов, подтверди сущность (появится `schema ✓`), нажми **Stage ▶**.

## 6. Docker (multi-stage + cargo-chef)

«Шеф» (`cargo-chef`) решает главную боль Rust-образов — время сборки:

```text
planner  → recipe.json          (только граф зависимостей)
builder  → cargo chef cook …    (кэшируемый слой компиляции зависимостей)
         → cargo build …        (пересобираются только наши крейты)
runtime  → debian-slim + бинарь + non-root + HEALTHCHECK
```

```bash
docker compose up --build      # http://localhost:8080
```

В `docker-compose.yml` данные монтируются так:

```yaml
volumes:
  - strata-workspaces:/data/workspaces   # schemas + staged parquet (rw)
  - ./data:/data/sources:ro              # сырьё (только чтение)
```

## 7. Осознанные компромиссы и что дальше

- **Tailwind пока с CDN** — быстрый старт без node. Для прода: отдельная
  стадия сборки (node или tailwind standalone CLI) → `/static/app.css`.
- **Stage выполняется синхронно** в запросе. Долгие прогоны нужно вынести в
  очередь задач с прогрессом (SSE + htmx-поллинг) — это следующий шаг, вместе
  с ограничением параллелизма (движок CPU/память-ёмкий).
- **Нет авторизации/мультитенантности**: текущая модель — self-hosted, один
  доверенный оператор. Для публичного сервиса нужны: пользователи, квоты,
  загрузка файлов вместо серверных путей, изоляция workspace'ов.
- **Одна схема на папку** остаётся правилом; строгая валидация типов выявила
  следующий движковый шаг — **правила совместимости типов** (например,
  `i64 → f64` — числовое расширение, а не ошибка). Сейчас такой файл честно
  попадает в отчёт как пропущенный.
