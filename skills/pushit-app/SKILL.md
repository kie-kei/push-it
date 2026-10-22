---
name: pushit-app
description: Build, deploy, and debug PushIt platform apps with the pushit CLI. Use whenever the user wants to create, build, deploy, update, roll back, or debug an app on PushIt — internal WASM-component apps with server functions (`/_fn/*`) plus a static UI, host capabilities (wasi:sql, wasi:blobstore, wasi:logging), and Keycloak auth. Also use when the user asks about pushit.yaml, pushit deploy, app logs, app secrets, or "the internal platform" generally.
---

# PushIt app development with the `pushit` CLI

PushIt deploys apps as **WASM components + static UI** to a single-VM controller. An app = a Rust crate compiled with `cargo component` to `wasm32-wasip2` (exporting `wasi:http/incoming-handler@0.2`) plus a `dist/` of static assets. The `pushit` CLI is the only tool needed.

## Golden path for a new app

```bash
mkdir myapp && cd myapp
pushit init                # scaffold: pushit.yaml, fn/, ui/
pushit build               # runs build.fn/build.ui from the manifest + verifies artifacts
pushit --dev deploy        # upload and activate
```

The routine loop is exactly two commands: `pushit build` (or just `pushit deploy` when artifacts are already built) and `pushit --dev deploy`. No extra tooling — the CLI reads `build:` commands out of `pushit.yaml`, so the manifest is both the IaC unit and the build definition.

**Local iteration without a controller:** `pushit dev` runs the component on the *same* wasmtime host as the platform (wasi:sql/blobstore/logging, `local` sqlite, 30s timeout) at `http://127.0.0.1:9101` — `/_fn/*` goes to the component, everything else serves `ui_dist`. Secrets/env: `--env KEY=value`; DSNs for non-`local` sql connections: `--dsn name='postgres://…'`; outgoing HTTP: `--http-out`. Use it for fast function testing; deploy to a real controller to test auth, hub, and revisions.

## Deploying multiple apps from one manifest (repos + one command)

A **root manifest** lists apps as git repos; `pushit deploy` syncs, builds, and deploys every one of them:

```yaml
# pushit.yaml (root, e.g. platform/ or infra/ repo)
apps:
  reporting-bot:
    repo: git@internal:team/reporting-bot.git
    ref: main                # optional: branch or tag, default remote HEAD
    manifest: pushit.yaml    # optional: path inside the repo, default pushit.yaml
    env:                     # REQUIRED: deploy-time env, replaces the repo manifest's env wholesale
      API_URL: https://api.example.com
      LOG_LEVEL: info
  status-page:
    repo: https://internal.example/status-page.git
    env: {}                  # explicit empty — no env vars
```

```bash
pushit deploy --manifest /path/to/root/pushit.yaml   # one command → all apps
pushit plan --manifest /path/to/root/pushit.yaml     # read-only diff vs live platform
```

Behavior:

- Repos are shallow-cloned/pulled into `.pushit-cache/<name>/` **next to the root manifest** (add `.pushit-cache/` to .gitignore); `ref` is checked out on every deploy.
- **`env:` per app is mandatory** and replaces the repo manifest's `env:` entirely — runtime config is a deploy-time decision owned by the platform repo, not the app code. Missing `env:` fails that app with a clear error. `env: {}` means explicitly no vars. Secrets still live in the app's manifest as names + `pushit secret set` values.
- **Builds run locally** (the app's own `build.fn`/`build.ui`), never on the server — the controller stays toolchain-free.
- Apps deploy sequentially; a failed app doesn't stop the rest. Exit code is non-zero and a `N app(s) failed: a, b` line lists them — rerun the same command to retry; redeploys are idempotent.
- Per-app secrets must exist beforehand (`pushit secret set <app> KEY=V`); deploy fails fast with `missing secrets: …` otherwise.
- **`pushit plan`** previews without changing anything: per app it reports `CREATE` (new), `DRIFT` (env differs from root manifest, declared secret has no value on the platform), or `up to date`; live apps absent from the root manifest are flagged as orphans. Code changes are invisible to plan — a rebuilt artifact always deploys as a new revision. Run `plan` before `deploy` as the cheap "what will change" check.

## The app contract

```
myapp/
  pushit.yaml     # manifest — IaC unit AND build definition
  fn/             # Rust → cargo component → app.wasm
  ui/             # static UI (any stack that produces files)
```

```yaml
name: myapp                      # → myapp.<base-domain>
entry: fn/target/wasm32-wasip2/release/myapp.wasm
env:                             # non-secret config; reaches fn as env vars
  GREETING: hi
secrets:                         # names only — values are set via `pushit secret set`
  - API_TOKEN                    # deploy FAILS if a named secret is not set
capabilities:                    # what the component is allowed to import/use
  http_out: ["api.example.com"]  # presence grants outgoing HTTP (MVP: unrestricted)
  env: [API_TOKEN]
  sql: [local]                   # 'local' = built-in per-app sqlite; other names are bound to secret sources at deploy (root manifest sql:)
  blobstore: [files]             # logical stores from blobstores.yaml
build:
  fn: cd fn && cargo component build --release --target wasm32-wasip2
  ui: cd ui && npm ci && npm run build   # optional, only if UI needs a build step
  ui_dist: ui/dist
```

Rules that bite when violated:

- **fn/ must be a standalone cargo project** — `build.fn` runs `cd fn && cargo component build ...`. If `fn/` has no `Cargo.toml` of its own (or is inside another cargo workspace), the build may compile the wrong workspace for a wasm target and fail with confusing tokio/wasm errors. `pushit init` generates the right layout; don't restructure it.
- **Server functions live under `/_fn/*`**. The component's `handle()` receives requests already routed: `/` and everything else → `dist/` statics; `/_fn/*` → component. Inside the component, match on `request.path()` — it still contains `/_fn/...`.
- **env and secrets reach the component as env vars** (wasi:cli/environment). Secrets exist on the platform only via `pushit secret set`; never commit values.
- **`config.js` is generated by the controller** on deploy into `ui_dist` (`window.__ENV__ = {...}`) — frontend reads runtime config from it, never bakes env into the bundle.
- **User identity**: after OIDC, the controller sets `x-user-id`, `x-user-name`, `x-user-email` headers on every `/_fn` request. Read them; don't re-implement auth.

## Capabilities — host interfaces instead of raw HTTP

| Manifest | Guest imports | What it gives |
|---|---|---|
| `capabilities.sql: [name]` | `wasi:sql` `connection.open(name)`, `statement.prepare`, `query`, `exec` | Real DB access (postgres/mysql/sqlite via sqlx, ClickHouse via HTTP). Grants: `local` (app-scoped sqlite at `$PUSHIT_DATA/appdata/<app>/local.db`, no config, data survives redeploys/rollbacks) or plain logical names bound at deploy time in the root manifest (`sql: {name: secret:<app-secret-key> \| platform:<global-secret-name>}`). Credentials never reach the component |
| `capabilities.blobstore: [name]` | `wasi:blobstore` `get-container`, `get-data`, `write-data`, `list-objects`… | Object storage via opendal (s3/fs). Streams objects — fine for large files |
| (always available) | `wasi:logging` `log(level, ctx, msg)` | Structured lines into `pushit logs <app>` |
| `capabilities.http_out: [...]` | `wasi:http` outgoing-handler | Raw outgoing HTTP — prefer sql/blobstore interfaces when they cover the service |

If a component imports an interface NOT granted in `capabilities`, the request fails — check the manifest first when a call errors with "not granted".

## Everyday commands

```bash
pushit build                          # run build.fn/build.ui + verify artifacts exist
pushit --dev deploy                   # upload manifest + app.wasm + dist/
pushit --dev list                     # all apps
pushit --dev status myapp             # detail + revision history
pushit --dev logs myapp               # last 200 lines
pushit --dev logs -f myapp            # live stream (SSE) — run in background while reproducing
pushit --dev secret set myapp K=V     # set/update secret value
pushit --dev secret ls myapp          # names only
pushit --dev rollback myapp           # previous revision
pushit --dev rollback myapp --revision <id>
```

`--dev` sends `X-Pushit-Dev: 1` — use against a dev controller (`PUSHIT_DEV=1`). For production: `--token <jwt>` or `PUSHIT_TOKEN` env, and `--server <url>` / `PUSHIT_SERVER` (default `http://localhost:9100`).

## Debugging a deployed app

1. **Deploy-time failure** (400 from `pushit deploy`): read the error — it names the cause: bad manifest, missing secret, component missing `incoming-handler` export, wasm not a component.
2. **Runtime failure**: `pushit logs -f myapp` in one shell, reproduce in another/browser. The log gets: component stdout/stderr, `wasi:logging` lines, `handler failed: …` (trap/panic), `handler timed out after 30s`, platform events (deploys, rollbacks).
3. **Capability errors**: `connection 'x' not granted by capabilities.sql`, `no DSN for connection 'x'` — grant missing in the manifest or the deploy manifest's `sql:` binding points at a missing secret; fix the manifest/binding or set the secret (`pushit secret set` / `pushit secret global set`).
4. **UI loads but `/_fn` 404/500**: check the handler actually matches the request path (it includes `/_fn` prefix) and the component didn't trap — `handler failed` in logs.
5. **Rollback when broken**: `pushit rollback myapp` — instant revert to previous revision; investigate at leisure.

## Making changes to an existing app

Edit code → `pushit build && pushit --dev deploy` (rebuild + redeploy = new revision; previous revisions stay for rollback). Secrets and env changes also need a redeploy to take effect.

## Common failures and fixes

| Symptom | Cause | Fix |
|---|---|---|
| `no wasm found under fn/target/...` | build skipped or wrong target | `pushit build` — it runs build.fn and verifies; check `entry:` |
| tokio "features ... not supported on wasm" during `cargo component build` | `fn/` inside a parent workspace; cargo compiled the parent | ensure `fn/Cargo.toml` exists and build.fn does `cd fn`; don't nest under another cargo workspace |
| `build.fn failed (exit 1)` | the command in manifest failed | run it manually in the app dir, read the real toolchain error, fix, commit the working command in pushit.yaml |
| deploy 400 `secret X not set` | manifest declares a secret never created | `pushit secret set <app> X=value` before deploy |
| request to `/_fn/x` 404 | handler didn't match path | path includes `/_fn` prefix — match on it |
| logs show `handler timed out after 30s` | infinite loop / blocking call in guest | fix the code; epoch deadline is the platform limit |

## Constraints to respect

- Static UI only — **no SSR** (no live frontend process exists). If an app must render HTML server-side, the wasm handler returns HTML itself.
- App code is Rust + cargo-component. Other languages (TinyGo, componentize-py) are possible but not the supported path.
- Don't hand-edit generated bindings (`fn/src/bindings.rs`); regenerate via `cargo component build`.
