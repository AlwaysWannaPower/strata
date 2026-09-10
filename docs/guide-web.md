# Guide: веб-сервис strata (axum + htmx + Tailwind)

> Почему ушли с Dioxus и как теперь всё устроено. Для человека, который учит
> бэкенд на Rust и htmx.

## 1. Почему htmx, а не Dioxus

Dioxus-десктоп (удалён, см. `docs/archive/`) давал «нативную» утилиту, но:

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
- **Dioxus-десктоп удалён** из репозитория (последний коммит с ним — `0838e06`,
  учебные заметки — `docs/archive/`). В Docker собирается только `strata-web`,
  поэтому WebKit и GUI-библиотеки в образ не попадают.

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

## 6.1 Аккаунты и приватные воркспейсы (платформенный слой)

Стек авторизации выбран из готовых, «взрослых» крейтов:

| Задача | Крейт | Почему он |
|---|---|---|
| Сессии | **`tower-sessions` 0.15** | серверные сессии: cookie хранит только id, данные — на сервере; logout реально удаляет сессию (в отличие от JWT) |
| Пароли | **`argon2` 0.6** (RustCrypto) | Argon2id; API сам генерирует соль и отдаёт PHC-строку |
| Пользователи | **`sqlx` 0.9 + SQLite** | один процесс — одна БД, без внешнего сервера; переезд на Postgres = смена URL/feature |
| (позже) вход через провайдера | `openidconnect` 4.x | Google/GitHub/корпоративный SSO |
| (позже) passkeys | `webauthn-rs` | сейчас в dev-версии (0.6.1-dev) — не берём в прод |

Как это устроено у нас (`crates/strata-web/src/auth.rs`):

```text
POST /register → validate → Argon2 hash (spawn_blocking!) → INSERT users → session
POST /login    → SELECT user → Argon2 verify (spawn_blocking!) → session + cycle_id
POST /logout   → session.clear()
GET  /w/...    → middleware require_login → redirect /login, если не вошёл
```

Важные детали, которые легко сделать неправильно:

* **хэширование и проверка пароля — в `spawn_blocking`**: Argon2 намеренно
  дорогой, в async-рантайме он бы застопорил другие запросы;
* **одинаковое сообщение** на «нет пользователя» и «неверный пароль»;
* **`cycle_id()` после входа** — защита от session fixation;
* **изоляция тенантов — путь**: у каждого пользователя свой корень
  `workspaces/<user_id>/`, и все маршруты `/w/{slug}` резолвятся только внутри
  него. Это грубее, чем tenant-колонка в БД, зато совпадает с тем, как движок
  работает с файлами (path jail);
* маршруты разделены на публичные и `protected`-роутер с одним
  `middleware::from_fn_with_state(..., require_login)` — новая защищённая ручка
  не забудет проверку по неосторожности.

Проверка вручную (curl): анонимный `/w/x` → 303 на `/login`; `POST /register`
создаёт аккаунт и сессию; `POST /workspaces` создаёт папку именно этого
пользователя; после `/logout` защищённый маршрут снова редиректит.

## 7. Фоновые задачи и прогресс (stage не блокирует запрос)

Staging — CPU/IO-тяжёлая операция, поэтому она **не выполняется в обработчике
запроса**:

```text
POST /…/stage
   1. создаём запись задачи в реестре (Arc<Mutex<JobEntry>>)
   2. tokio::task::spawn_blocking → движок (strata_core::api::stage_entity_with_progress)
   3. сразу отдаём HTML-фрагмент с hx-get="/w/{slug}/jobs/{job_id}"
      hx-trigger="every 1s" hx-swap="outerHTML"

GET /w/{slug}/jobs/{job_id}
   Running { done, total } → тот же поллинг-фрагмент с прогресс-баром
   Done(outcome)           → финальный фрагмент (БЕЗ hx-атрибутов → поллинг сам останавливается)
   Failed(message)         → красная плашка
```

Прогресс приходит из движка: `stage_folder_with_schema_progress` вызывает
колбэк `(done, total)` после каждого файла (включая пропущенные — «done» значит
«этот файл обработан»). Реестр задач крошечный: финальный результат отдаётся
один раз и запись сразу удаляется.

Проверено вживую: сущность из 60 файлов → POST отдал прогресс-фрагмент, первый
поллинг показал «staging…», второй — финал «60 file(s), 480 row(s) → 60 part(s)».

## 8. Ресурсы сервиса (RAM/CPU) и `/metrics`

* `GET /metrics` — текстовый Prometheus-подобный вывод:
  `strata_process_rss_bytes`, `strata_process_cpu_percent`,
  `strata_host_memory_total_bytes`, `strata_host_memory_used_bytes`,
  `strata_uptime_seconds`, `strata_workspaces_total`.
* `GET /fragments/resources` — маленький HTML-фрагмент для виджета в правом
  нижнем углу: `RAM 16 MB · CPU 4% · host 5.0 GB/8.0 GB · workspaces 2`.
  В `base.html` он подтягивается через `hx-trigger="load, every 2s"`.

Замеры идут через `sysinfo` (кросс-платформенно, включая macOS — в отличие от
десктопного `monitor.rs`, который читает `/proc` и на маке показывал «—»).
Инстанс `System` живёт в состоянии сервиса под `Mutex`: CPU% считается как
дельта между двумя обновлениями, поэтому состояние нужно сохранять между
запросами. Под блокировкой нет I/O — только мгновенные чтения.

## 9. Осознанные компромиссы и что дальше

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
