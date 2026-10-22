# Roadmap / backlog

Зафиксировано после фиксации MVP-архитектуры (см. ARCHITECTURE.md). Каждый пункт — отдельно выкатываемое расширение, триггер указан.

## Standalone-запуск приложений вне PushIt

**Статус:** бэклог, не критично сейчас.

**Цель:** тот же `.wasm` компонент + `pushit.yaml` должны запускаться без контроллера — в контейнере или на другой ВМ.

**Вариант A — `pushit-standalone` (предпочтительный).** Мини-бинарь = существующий host-код (`runtime.rs` + `sql_host.rs` + `blob_host.rs` + `log_host.rs`) + axum-вход на порт + чтение `pushit.yaml`/`databases.yaml`/`blobstores.yaml`. Полная совместимость, включая sql/blobstore — это склейка уже написанного. Теряется только платформа: домены, хаб, OIDC, секреты из SQLite (env-файл вместо).

**Вариант B — `wasmtime serve` / wasmCloud, когда догонят.** wasmtime уже покрывает `wasi:http`+env — приложения без платформенных capabilities запускаются им уже сегодня. wasmCloud имеет capability providers под те же draft-интерфейсы (`wasi:blobstore`, `wasi:keyvalue`) — переносимость растёт по мере стандартизации draft-спек.

**Правило для команд (в скилле):** если приложение может понадобиться вне PushIt — ограничиваться `wasi:http` + env/secrets; платформенные интерфейсы — когда изоляция важнее переносимости.

## Прочее из открытых вопросов

- allowlist хостов в `capabilities.http_out` (сейчас «всё или ничего»)
- Тёплый пул wasm-инстансов (если cold-start ~1–5мс станет проблемой)
- Cron/фоновые джобы (`cron:` в манифесте)
- `wasi:kv-store` (Redis) — по потребности
- Фильтр хаба по Keycloak-группам, RBAC на приложения
- Request-id correlation в логах + `--level` фильтр в `pushit logs`
- `pushit login` — OIDC flow из CLI с refresh-токеном в `~/.config/pushit/`
