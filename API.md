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

### Immutable app versions

Version IDs are the existing opaque revision IDs, not Git SHAs. No migration or
new identifier is required. Every successful deploy immediately activates its new
version. An app has at most one active version, and may have none.

- `GET /api/apps/{name}/versions` → `[{"id":"<id>","created_at":"...","active":true}]`, newest first, ties by ID descending. Missing app: 404.
- `POST /api/apps/{name}/versions/{id}/activate` → `{"app":"name","version":"<id>"}`. Atomically switches code, static UI, manifest/env/capabilities and domain to the existing immutable snapshot. Missing version: 404; missing/legacy manifest, artifact or required secrets: 400, leaving the prior active version unchanged.
- `POST /api/apps/{name}/deactivate` → 204 (idempotent); missing app: 404. With no active version, functions **and** UI/static/config routes return 404. All versions remain available for later activation.
- `DELETE /api/apps/{name}/versions/{id}` → 204. Deletes only this inactive version and its artifacts/cache entries. Active version: 409; missing version: 404. If an in-flight request still uses an inactive version, returns 409: retry after the request finishes. Unrelated apps/versions remain available. Legacy inactive versions without manifest snapshots can also be deleted.

Metadata deletion is authoritative. Filesystem cleanup failures are logged and
may leave unserved orphan artifact directories; deleted versions cannot be
reactivated even if cleanup failed.

Browser session-authenticated lifecycle mutations require `X-PushIt-CSRF: 1`;
when `Origin` is supplied it must match the request Host. Cross-origin CORS access
is not enabled. Bearer CLI clients need no browser CSRF header.

Activation/deactivation/deletion never restore or remove app secrets, local
SQL data, blob data or other versions. Activation/rollback restores **configuration
references**, not secret values or database contents. Older versions must be
compatible with the current data/schema. The existing rollback API and revision
history remain compatible; rollback without a target still selects the newest
inactive version with a known snapshot.

`DELETE /api/apps/{name}` — removes app, revisions, route

## Logs

`GET /api/apps/{name}/logs?tail=200` — last N lines, JSON lines
`GET /api/apps/{name}/logs/stream` — SSE, `data: <line>\n\n`

## Hub

`GET /` on `hub.<base-domain>` → server-rendered app list (Askama)
`GET /apps/{name}` on hub → app page: status, revisions, log tail, secrets form

## SQL capability

Components may import `wasi:sql@0.2.0-draft` to run SQL through host
functions. Grant access via `capabilities.sql` in pushit.yaml: `local`
(built-in app sqlite) or plain logical names, each bound at deploy time
in the root manifest to `secret:<app-secret-key>` or
`platform:<global-secret-name>`.
Drivers: postgres/mysql/sqlite (sqlx), ClickHouse (HTTP endpoint).
See `docs/sql.md`.

Components may import `wasi:blobstore@0.2.0-draft` for object storage through
host functions (opendal backend; `capabilities.blobstore` + `$PUSHIT_DATA/blobstores.yaml`;
see `docs/blobstore.md`) and `wasi:logging/logging@0.1.0-draft` for structured
log lines into the per-app log (`docs/logging.md`).
