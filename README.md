# PushIt

Внутренняя платформа для деплоя небольших приложений на одну ВМ. Приложения — WASM-компоненты (`wasi:http`) + статичный UI; никаких контейнеров/процессов на приложение. Архитектура: [ARCHITECTURE.md](ARCHITECTURE.md), API: [API.md](API.md).

## Установка CLI

```bash
curl -fsSL https://raw.githubusercontent.com/kie-kei/push-it/master/scripts/install.sh | sh
```

Требуется Rust (`cargo`); бинарник ставится в `~/.cargo/bin/pushit`.

Удаление:

```bash
curl -fsSL https://raw.githubusercontent.com/kie-kei/push-it/master/scripts/uninstall.sh | sh
```

## Quickstart (dev)

```bash
source ~/.cargo/env

# 1. Контроллер
PUSHIT_DEV=1 PUSHIT_DATA=/tmp/pushit cargo run -p pushit-controller

# 2. Новое приложение
pushit init myapp && cd myapp
pushit secret set myapp MY_TOKEN=secret123   # если манифест требует
pushit deploy

# 3. Проверка
curl -H 'Host: myapp.localhost' http://localhost:9100/          # UI
curl -H 'Host: myapp.localhost' http://localhost:9100/_fn/hello # функция
pushit logs myapp
```

### Локальная отладка без контроллера

```bash
pushit dev            # в каталоге приложения: сборка + рантайм на http://127.0.0.1:9101
pushit dev --env KEY=value --dsn db='postgres://…' --http-out
```

Тот же wasmtime-рантайм, что и на платформе (wasi:sql/blobstore/logging,
`local` sqlite, таймауты). Секреты локально передаются через `--env`,
DSN не-`local` подключений — через `--dsn имя=DSN`.

## Структура

- `controller/` — Rust-хост: wasmtime, роутинг, OIDC, SQLite, хаб
- `cli/` — `pushit` CLI
- `wit/` — `pushit:app` WIT-world (`wasi:http/incoming-handler`)
- `examples/hello/` — эталонное приложение
- `infra/` — docker-compose (Caddy + Keycloak), realm
- `scripts/e2e.sh` — end-to-end проверка

## SQL

Компоненты могут импортировать `wasi:sql@0.2.0-draft` — SQL через хост-функции.
Доступ: `capabilities.sql` в pushit.yaml (логические имена; `local` — встроенная
sqlite приложения). Откуда DSN — решает деплоер в root-манифесте (`sql:`:
`secret:<ключ>` — секрет приложения, `platform:<имя>` — общий платформенный
секрет). Драйверы: postgres/mysql/sqlite через sqlx, ClickHouse через
HTTP-эндпоинт. См. `docs/sql.md`.
Также `wasi:blobstore@0.2.0-draft` (opendal: s3/fs, `capabilities.blobstore` +
`$PUSHIT_DATA/blobstores.yaml`, см. `docs/blobstore.md`) и
`wasi:logging@0.1.0-draft` (структурный лог в per-app файл, `docs/logging.md`).
