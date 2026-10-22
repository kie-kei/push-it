# PushIt

Внутренняя платформа для деплоя небольших приложений на одну ВМ. Приложения — WASM-компоненты (`wasi:http`) + статичный UI; никаких контейнеров/процессов на приложение. Архитектура: [ARCHITECTURE.md](ARCHITECTURE.md), API: [API.md](API.md).

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

## Структура

- `controller/` — Rust-хост: wasmtime, роутинг, OIDC, SQLite, хаб
- `cli/` — `pushit` CLI
- `wit/` — `pushit:app` WIT-world (`wasi:http/incoming-handler`)
- `examples/hello/` — эталонное приложение
- `infra/` — docker-compose (Caddy + Keycloak), realm
- `scripts/e2e.sh` — end-to-end проверка
