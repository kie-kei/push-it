# Архитектура платформы (decision record, 2026-10-22)

Внутренняя платформа для деплоя небольших приложений команд. **Одна ВМ. Контейнеры — только для инфраструктуры платформы (Caddy, Keycloak), приложения — WASM-компоненты внутри контроллера. Никаких дочерних процессов/контейнеров на запрос.**

## Контекст и требования

- Приложения — внутренние, код доверенный, публичной регистрации нет.
- Аутентификация — через Keycloak realm (OIDC).
- Функционал платформы:
  1. Серверные функции (запросы к внешним сервисам с аутентификацией: ClickHouse HTTP, S3 API, REST).
  2. Рендер интерфейса с внешними зависимостями (npm-пакеты, сборка фронта).
  3. IaC — деплой через манифест в репозитории (env, secrets, capabilities).
- Типовые кейсы: данные в ClickHouse → UI + выборки; S3-бакет → чтение/трансформация под формат. Stateless HTTP-обёртки.

## Ключевые решения

| Решение | Выбор | Причина |
|---|---|---|
| Модель исполнения | **In-process WASM-компоненты** (wasi:http) | Приложение = `.wasm` компонент, инстанцируется на запрос или держится тёплым; нет процессов, supervisor, портов, healthcheck, reboot-recovery |
| Изоляция | **Capability-based WASI** | Компоненту выдаются только интерфейсы из манифеста: нет `wasi:http/outgoing-handler` → физически нет сети; нет `wasi:filesystem` → нет ФС |
| Runtime | **Wasmtime** (WASI 0.2/0.3, Component Model) | Эталонная реализация, встраивается в Rust-хост; fuel/epoch interruption — зависший handler прерывается |
| Язык приложений | **Rust** (`wasm32-wasip2`, `cargo component`) | Лучшая поддержка WASI 0.2/0.3. Опционально позже: TinyGo, componentize-py, TS→component |
| Язык контроллера | **Rust** | wasmtime нативно встраивается; один бинарь; router + host functions + lifecycle |
| UI | Обычный фронтенд: любой фреймворк + npm, собирается на деплое → `dist/` статика | WASM не рендерит UI — фронт остаётся JS |
| Reverse proxy + TLS | **Caddy** | TLS из коробки, admin API для динамических роутов |
| Auth | **OIDC внутри контроллера**: auth-code flow → Keycloak realm, подписанная сессионная кука (`Domain=.<base>`), Bearer JWT для API/CLI | Без oauth2-proxy: контроллер сам делает редирект/валидацию, `PUSHIT_DEV=1` — bypass
| Состояние | SQLite | apps, env refs, история деплоев, warm-instance state |
| Секреты | **Гибрид**: имена в `pushit.yaml`, значения через CLI/UI → шифрованные в SQLite (age) | В git — только имена. Деплой отклоняется, если секрет не заведён. Компонент получает секреты через env-хост, не через ФС |
| Логи | stdout/stderr компонента → per-app файл контроллером | `GET /apps/x/logs` API, `tail -f` в CLI |
| CLI | **С первой версии** | Тонкий клиент к API контроллера: `secret set`, `logs`, `deploy`, `rollback`, `list` |
| Хаб | **Системная страница контроллера** на `hub.*` | Server-rendered список приложений из SQLite: имя, домен, ревизия, статус, ссылки logs/rollback. UI платформы = хаб, отдельной админки нет |
| Адресация | **Wildcard-субдомены**: `*.apps.internal.lan → IP ВМ`, `name` → `<name>.apps.internal.lan` | Одна DNS-запись + один wildcard-серт; новое приложение не требует DNS/TLS-изменений. Caddy: один маршрут `*.apps → controller`, роутинг по `Host` внутри |

## Состав платформы

- **Controller** — Rust-бинарь на хосте (не в контейнере), единственный долгоживущий процесс кроме инфра-контейнеров:
  - принимает артефакты от клиента (`pushit deploy`: архив `app.wasm` + `dist/` + `pushit.yaml`) → регистрация ревизии → роут. Сборка на ВМ отсутствует — тулчейнов (rustup/node) на ней нет
  - HTTP-роутер: домен → приложение; `/_fn/*` → инстанс компонента; остальное → статика
  - host-функции: подаёт env/secrets, ограничивает capabilities по манифесту, fuel/timeout на вызов
  - сам под systemd (единственный unit платформы)
- **Caddy** — контейнер: reverse proxy, TLS, forward-auth к Keycloak.
- **Keycloak** — контейнер: realm, OIDC.
- **SQLite** — файл, стейт контроллера.

## Контракт приложения

```
repo/
  pushit.yaml      # манифест (IaC)
  fn/              # Rust-функции → cargo component → app.wasm
    Cargo.toml
    src/lib.rs     # impl wasi:http/incoming-handler
  ui/              # фронтенд (любой: Vite/React/Svelte/…)
```

```yaml
# pushit.yaml
name: reporting-bot
entry: fn/target/wasm32-wasip2/release/app.wasm
domain: reporting-bot   # → reporting-bot.apps.internal.lan; кастомный FQDN — опционально
# имя валидируется против блок-листа: hub, api, auth — системные субдомены
env:
  CLICKHOUSE_URL: http://clickhouse.internal:8123
secrets:              # значения через CLI/UI, в git — только имена
  - CLICKHOUSE_TOKEN
  - S3_SECRET_KEY
capabilities:         # какие WASI-интерфейсы выдать компоненту
  http_out: ["clickhouse.internal", "s3.internal"]   # allowlist хостов, если реализуем фильтр
  env: ["CLICKHOUSE_URL", "CLICKHOUSE_TOKEN"]
build:
  fn: cargo component build --release
  ui: cd ui && npm ci && npm run build
  ui_dist: ui/dist
```

Функция:

```rust
// fn/src/lib.rs — impl wasi:http/incoming-handler
fn handle(req: IncomingRequest) -> Response {
    // env/secrets разрешены хостом до вызова
    // исходящий HTTP — через wasi:http/outgoing-handler (если выдан в capabilities)
}
```

## Контракт серверной функции (WIT)

Контракт — не конвенция, а типизированный world, проверяемый при линковке компонента: деплой компонента без нужного экспорта падает на валидации, не на первом запросе.

```wit
// wit/pushit.wit — пакет платформы, распространяется как шаблон репозитория
world app {
    export wasi:http/incoming-handler@0.3.0;
    // импорты — только разрешённые capabilities из манифеста:
    // wasi:http/outgoing-handler, pushit:blobstore, … (линковка откажет на неразрешённых)
}
```

- **`wasi:http/incoming-handler`** — сторона «компонент = обработчик»: хост создаёт `IncomingRequest`, вызывает `handle()`, получает `Response`. Компонент не может открыть listener/порт — отвечает только на переданные ему запросы.
- **`wasi:http/outgoing-handler`** — исходящие запросы через хост; allowlist хостов применяется здесь.
- Маршрутизация внутри `/_fn/*` — дело приложения (`req.path()`), платформа доставляет весь префикс.
- Контекст: env/secrets (по `capabilities.env`), заголовки `X-User-*` после forward-auth — через стандартные поля запроса и env.
- Стартовый шаблон: `cargo component` + каталог `wit/` платформы — команды клонируют, а не пишут WIT с нуля.

## Фронтенд: контракт и флоу

Стек не фиксируется — любой фреймворк/бандлер, который собирается в статический каталог. Фиксируются только три соглашения:

1. **API-префикс**: серверные функции зовутся по относительному `/_fn/...` — тот же origin, без CORS, auth-кука проходит после forward-auth.
2. **Runtime-конфиг**: контроллер генерирует `<ui_dist>/config.js` (`window.__ENV__ = {...}`) на каждый деплой из `env:` манифеста; фронт читает конфиг оттуда. Работает и при сборке на контроллере, и с CI-артефактами.
3. **Идентичность пользователя**: стандартный `GET /_fn/me` (или проксирование `/auth/userinfo` в Keycloak) — одна конвенция для всех фронтов.

Роутинг на запрос: `app.domain/* → /apps/<name>/<rev>/dist/` (статика, Caddy file-server), `app.domain/_fn/* → wasm handler`. Кэш: `assets/*` immutable, `index.html` no-cache.

SSR не поддерживается (нет живого фронт-процесса). Если команде нужен серверный рендер — их wasm-handler сам отдаёт HTML, платформа ничего особого не делает.

## IaC-поток

```
pushit deploy (на машине разработчика или в CI):
  собрать app.wasm + dist/ → POST /apps/<name>/deploy (архив + pushit.yaml)
  controller: распаковать → /apps/<name>/<rev> → валидация WIT-линковки и секретов → активировать ревизию
```

- Запрос: `Caddy → controller → match domain → instantiate component (или тёплый инстанс) → handler(req) → ответ`. Cold start ~1–5мс.
- Rollback = переключить роут на предыдущую ревизию (артефакты ревизий на диске, история в SQLite).
- Стейт самой платформы — тоже манифестом в git, тот же git-sync loop.

## Платформенные host-интерфейсы

Контроллер реализует WASI-интерфейсы как host-функции; компонент импортирует только то, что разрешено в `capabilities`. Чем больше покрыто интерфейсами — тем меньше приложению нужно сырых HTTP/env.

| Интерфейс | Статус WASI | Наша реализация | Когда |
|---|---|---|---|
| `wasi:http` (in/out) | Phase 3, стабилен | wasmtime из коробки + allowlist хостов в outgoing-handler | **MVP** |
| env/secrets через хост | — | env подаётся при instantiate, секреты из SQLite | **MVP** |
| `wasi:blobstore` | Phase 1 (draft, `0.2.0-draft`) | **Реализовано**: opendal (`services-s3`, `services-fs`), `capabilities.blobstore: [name]` → `blobstores.yaml`, whole-object буферы в MVP | **Done** |
| `wasi:kv-store` | Phase 2 | host → Redis/Redis-кластер при появлении потребности | По потребности |
| `wasi:sql` | Phase 1 | **Реализовано**: `sqlx::AnyPool` (postgres/mysql/sqlite) + ClickHouse через HTTP-эндпоинт; `capabilities.sql: [name]` → `databases.yaml` (name→DSN), креды у хоста; `local` — зарезервированное имя: встроенная sqlite-БД приложения `$PUSHIT_DATA/appdata/<app>/local.db`, переживает ревизии/rollback | **Done** |
| `wasi:logging` | Phase 1 | **Реализовано**: `log(level, ctx, msg)` → строка в per-app лог-файл (`LogStore`), структурные уровни | **Done** |
| `wasi:messaging` | Phase 2 | — | Не планируется до запроса |

Принцип: берём WIT-спеки draft-пропозалов как есть (не выдумываем свой API) — если спека стандартизируется, компоненты совместимы бесплатно. Побочный эффект: приложение с `blobstore`/`sql` не нуждается в `http_out` к S3/БД и кредах в env — поверхность секретов сужается.

## Домены и TLS

- Wildcard DNS `*.apps.internal.lan → IP ВМ`, субдомен = `name` из манифеста.
- TLS: wildcard-сертификат — ACME DNS-01, если DNS с API, иначе внутренний CA (серт файлом в Caddy). Допустимо и plain HTTP внутри периметра.
- Caddy: один site `*.apps.internal.lan → reverse_proxy controller` — реконфигурация не нужна при новых приложениях; admin API используется только для кастомных доменов вне wildcard.
- OIDC: callback на одном домене (`auth.apps.internal.lan`), cookie-domain `*.apps.internal.lan` — сессии разделены между приложениями, но общий логин.

## Хаб

`hub.apps.internal.lan` — системная страница, рендерится самим контроллером (server-rendered, без фронт-стека):

- Список приложений из SQLite: имя → ссылка на субдомен, ревизия, дата деплоя, статус.
- Страница приложения: логи, форма секретов, кнопки deploy/rollback — мини-админка.

- Auth: тот же Keycloak realm; на старте список виден всем, фильтр по группам — при необходимости.

- Стек хаба: **Askama** (компайл-тайм шаблоны Rust) + SSE для стрима логов + **Pico.css**. Node/фронт-тулчейна в платформе нет.

## Решённые вопросы

- Исполнение: **in-process WASM** — нет процессов вообще.
- Изоляция: **WASI capabilities** из манифеста.
- Язык приложений: **Rust** (wasm32-wasip2); UI — любой JS-стек со сборкой в статику.
- Контроллер: **Rust + wasmtime**.
- Секреты: **гибрид** — имена в манифесте, значения через CLI/UI, валидация на деплое.
- CLI: **сразу** — `secret set`, `logs`, `deploy`, `rollback`, `list`.

## MVP-граница

Входит: деплой артефактов через CLI (`pushit deploy`), роутинг по `Host`, instantiate-per-request, env/secrets, логи в per-app файл, `hub.*` со списком приложений, Keycloak-логин, wildcard-субдомены.

Не входит: allowlist в `http_out` (пока «всё или ничего»), blobstore/sql host-интерфейсы, rollback-кнопки в хабе (rollback только через CLI), тёплый пул, cron, TLS через ACME (внутренний CA или HTTP).

## Открытые вопросы

- Гранулярность `capabilities.http_out`: allowlist хостов — фильтровать в host-функции outgoing-handler (реализуемо: хост видит URL запроса до выполнения).
- Тёплые инстансы vs instantiate-per-request: начать с instantiate-per-request (проще, ~1–5мс), тёплый пул — при появлении latency-проблем.
- Fuel/limit: значения по умолчанию для прерывания зависших вызовов; таймаут ответа.
- Cron/фоновые джобы: манифест `cron:` → контроллер дёргает handler по расписанию. Дешёвое расширение позже.
- Сборка на контроллере vs CI: cargo/npm toolchain на ВМ против готовых артефактов из CI. На старте — сборка на контроллере (проще, нет CI-зависимости).

## Риски

- Toolchain: на ВМ нужен rustup+cargo component+node для сборок — тяжёлые зависимости билда; альтернатива — CI собирает, контроллер только кладёт артефакты.
- Экосистема WASI моложе нодовской: ClickHouse/S3 клиенты внутри wasm — HTTP-уровень руками или wasm-совместимые либы; для наших кейсов (HTTP API) достаточно.
- Один процесс на всё: паника контроллера роняет все приложения — митигация systemd-restart + инстансы stateless.
- Одна ВМ — единая точка отказа; бэкап = SQLite + `/apps/`.
