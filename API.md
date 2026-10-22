# PushIt control API (contract)

Base: `http://<controller>:9100` (dev default). All JSON unless noted.
Auth (prod): `Authorization: Bearer <Keycloak access token>` — controller validates the JWT against the realm. Dev mode: `PUSHIT_DEV=1` disables auth checks.

## Deploy

`POST /api/apps/{name}/deploy` — `multipart/form-data`:
- `manifest` — `pushit.yaml` text
- `archive` — tar.gz containing `app.wasm` (component, exports `wasi:http/incoming-handler@0.2`) and optional `dist/` (static UI)

200 → `{"app":"name","revision":"<id>","domain":"<name>.<base-domain>"}`
400 → validation error (bad manifest, missing export, unmet secrets)

## Secrets

`PUT /api/apps/{name}/secrets/{key}` — body: plain text value. Stored age-encrypted.
`DELETE /api/apps/{name}/secrets/{key}`
`GET /api/apps/{name}/secrets` → `{"keys":["A","B"]}` — names only, never values.

## Apps

`GET /api/apps` → `[{"name","domain","revision","deployed_at","status"}]`
`GET /api/apps/{name}` → app detail + manifest + revision history
`POST /api/apps/{name}/rollback` — body `{"revision":"<id>"}` (omit → previous revision)
`DELETE /api/apps/{name}` — removes app, revisions, route

## Logs

`GET /api/apps/{name}/logs?tail=200` — last N lines, JSON lines
`GET /api/apps/{name}/logs/stream` — SSE, `data: <line>\n\n`

## Hub

`GET /` on `hub.<base-domain>` → server-rendered app list (Askama)
`GET /apps/{name}` on hub → app page: status, revisions, log tail, secrets form

## SQL capability

Components may import `wasi:sql@0.2.0-draft` to run SQL through host
functions. Grant logical connection names via `capabilities.sql` in
pushit.yaml; the platform admin maps names to DSNs in
`$PUSHIT_DATA/databases.yaml`. Drivers: postgres/mysql/sqlite (sqlx),
ClickHouse (HTTP endpoint). See `docs/sql.md`.

Components may import `wasi:blobstore@0.2.0-draft` for object storage through
host functions (opendal backend; `capabilities.blobstore` + `$PUSHIT_DATA/blobstores.yaml`;
see `docs/blobstore.md`) and `wasi:logging/logging@0.1.0-draft` for structured
log lines into the per-app log (`docs/logging.md`).
