# pushit-controller

Single-binary platform controller: deploys WASM components
(`wasi:http/incoming-handler@0.2`) + static `dist/`, routes by `Host`
wildcard, serves hub pages, stores state in SQLite.

## Run

```sh
source ~/.cargo/env
cargo build -p pushit-controller

PUSHIT_DEV=1 PUSHIT_DATA=/tmp/pushit-data \
  ./target/debug/pushit-controller          # listens on :9100
```

## Env vars

| Var | Default | Purpose |
|---|---|---|
| `PUSHIT_DATA` | `./pushit-data` | data dir (SQLite, apps/, logs/, age.key) |
| `PUSHIT_BIND` | `0.0.0.0:9100` | listen address |
| `PUSHIT_BASE_DOMAIN` | `localhost` | suffix for `<app>.<base>` host routing |
| `PUSHIT_DEV` | — | `1` disables auth (fixed dev user) |
| `PUSHIT_OIDC_ISSUER` | — | Keycloak realm URL (enables OIDC when set + not dev) |
| `PUSHIT_OIDC_CLIENT_ID` / `PUSHIT_OIDC_CLIENT_SECRET` | — | OIDC client |
| `PUSHIT_AGE_KEY` | `$PUSHIT_DATA/age.key` (auto-generated) | age X25519 identity for secrets-at-rest |
| `PUSHIT_SESSION_KEY` | dev default | HMAC key for session cookies — set in prod |

## Dev mode

`PUSHIT_DEV=1` (or request header `X-Pushit-Dev: 1`) bypasses auth with a
fixed `dev` user — no Keycloak needed.

## Deploy (example)

```sh
tar -czf app.tar.gz -C artifact .   # artifact: app.wasm + dist/
curl -F 'manifest=<pushit.yaml' -F 'archive=@app.tar.gz' \
  http://localhost:9100/api/apps/<name>/deploy
```

Then `Host: <name>.localhost` serves `dist/` statics, `/_fn/*` hits the
component, `GET /api/apps/<name>/logs` tails logs, SSE stream at
`/api/apps/<name>/logs/stream`. Hub: `Host: hub.localhost`.

## Notes / limits

- `capabilities.http_out` presence grants unrestricted outgoing HTTP (MVP).
- Components get env from manifest `env` + decrypted `secrets`; `capabilities.env`
  filtering is accepted but currently all manifest env + declared secrets are passed.
- Per-call deadline ~30s via epoch interruption; component stdout/stderr
  (up to 1 MiB/call) → `$PUSHIT_DATA/logs/<app>.log`.
- Session cookie `pushit_session`, `Domain=.<base-domain>`, HMAC-signed.
- API accepts `Authorization: Bearer <JWT>` validated against the realm JWKS.
