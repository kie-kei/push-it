---
name: pushit-app
description: Build, deploy, and debug PushIt platform apps with the pushit CLI. Use whenever the user wants to create, build, deploy, update, roll back, or debug an app on PushIt — internal WASM-component apps with server functions (`/_fn/*`) plus a static UI, host capabilities (wasi:keyvalue, wasi:sql, wasi:blobstore, wasi:logging), and configured platform auth. Also use when the user asks about pushit.yaml, pushit deploy, app logs, app secrets, or "the internal platform" generally.
---

# PushIt app development with the `pushit` CLI

PushIt deploys apps as **WASM components + static UI** to a single-VM controller. An app = a Rust crate built with `cargo component` into a WASI 0.2 component (exporting `wasi:http/incoming-handler@0.2`) plus a `dist/` of static assets. With cargo-component 0.21.1, Rust compilation uses `wasm32-wasip1`; cargo-component adapts that output into the final component. The compile target is not the deployed ABI: do not deploy a bare WASI 0.1 core module. The CLI orchestrates local builds; it does not replace the Rust toolchain or tools named in the manifest.

## Prerequisites

Rust/rustup + Cargo (CI pins **1.99.1**), the `wasm32-wasip1`
target, **cargo-component 0.21.1**, and `curl`. The Rust crate registry (and
any custom dependency sources) must be reachable. Node.js/npm are needed only when a custom `build.ui` uses them;
Git is needed for root-manifest repository deployments. The generated UI is
plain static HTML and the scaffold declares no secrets. The scaffold vendors
pinned WASI 0.2 WIT dependencies under `fn/wit/deps/` with local Cargo metadata
path references, so those interfaces do not require a WIT registry. Preserve
the included NOTICE and license notices when copying or updating these files.

For registry TLS trust failures (for example, `UnknownIssuer`), keep certificate
verification enabled. Have the operator configure the correct trusted CA and
registry access using supported tool configuration; do not use insecure TLS
bypasses. Report the blocked dependency-fetch/build stage rather than claiming
the full path has passed.

## Standalone owner auth

An operator may configure builtin single-owner auth behind a same-host HTTPS
reverse proxy instead of Keycloak. Use the operator-provided **HTTPS hub URL**
(including its external port) in the profile, then `pushit --profile <name> login
--builtin` (hidden owner-password prompt). Normal certificate/hostname verification
is mandatory; install a trusted internal CA rather than disabling TLS checks.
Never send an owner password to remote HTTP or to an app subdomain. Existing
app deployment/version/secret/log commands work unchanged; browser sessions remain
host-isolated from management bearer tokens. The original `--local` flow accepts
only HTTP `hub.localhost` on a loopback controller. Configuring/starting a server
requires the operator's deployment authorization; CLI use does not authorize it.

## Golden path: installed CLI and an existing instance

Use the installed `pushit` CLI and an **authorized existing instance**; a source
checkout and a local controller are not required. Confirm the deployment target
and choose a unique app name with the user before creating or replacing an app.
The commands below assume an existing authenticated profile; replace `myapp`
with the agreed name throughout if it is already in use.

```bash
set -euo pipefail
[ ! -f "$HOME/.cargo/env" ] || source "$HOME/.cargo/env"
command -v pushit
rustup toolchain install 1.99.1 --profile minimal
rustup target add wasm32-wasip1 --toolchain 1.99.1
cargo +1.99.1 install cargo-component --version 0.21.1 --locked
# Use the selected profile's credentials, not inherited environment overrides.
unset PUSHIT_TOKEN PUSHIT_PROFILE PUSHIT_DEV PUSHIT_SERVER
read -r -p 'Existing authorized profile name: ' APP_PROFILE
pushit profile show "$APP_PROFILE"  # verify its server before proceeding
# If authentication is missing/expired: pushit --profile "$APP_PROFILE" login
pushit --profile "$APP_PROFILE" list  # read-only authentication/target check
cd "$(mktemp -d /tmp/pushit-app.XXXXXX)"
export RUSTUP_TOOLCHAIN=1.99.1
pushit init myapp
cd myapp
pushit build && pushit --profile "$APP_PROFILE" deploy
pushit --profile "$APP_PROFILE" verify myapp   # HTTP checks from `checks:`; nonzero exit = failure
pushit --profile "$APP_PROFILE" status myapp
pushit --profile "$APP_PROFILE" logs myapp
```

If no profile exists, first register the operator-provided server with
`pushit profile add <name> --server <authorized-https-url>` and authenticate via
`pushit --profile <name> login`; do not guess a production endpoint or use
`--dev`. Explicit `--server` changes only the destination, not the resolved token.

Verify the deployed app over HTTP using the operator-confirmed app URL:
`pushit verify myapp` runs the `checks:` from pushit.yaml (scaffold: `/` and
`/_fn/hello` with the expected body) against the controller with the app Host
header, `--json` for machine-readable output. On an authenticated instance a
CLI Bearer token authorizes management APIs, **not** app UI/function routes:
a redirect (status < 400) then proves routing, not app content. For a content
check in production, open `/` and `/_fn/hello` in a browser authenticated to
that app host. Expect a `myapp` heading and `{"msg":"hello"}`, then check
status and logs. Production app routes require the browser session cookie.
Do not treat an unauthenticated redirect/login page as a successful content
check. If using curl, use an authorized app-host session cookie jar and
`curl -fsS -b <cookie-jar> <app-url>` (and `<app-url>/_fn/hello`); validate
the expected body, not just the exit status.
Never print or commit cookies. Report any auth/HTTP check still pending.

A nonzero build, deploy, or verification exit is failure: diagnose the error,
fix it, and repeat checks. Never claim success from an attempted command alone.

## Optional: source-based local dev sandbox

Use this only to test locally without production authentication. It requires a
source checkout and builds both CLI and controller. For the source quickstart,
follow these steps instead of the existing-instance path above.

From the cloned PushIt repository root, in Bash:

```bash
set -euo pipefail
[ ! -f "$HOME/.cargo/env" ] || source "$HOME/.cargo/env"
rustup toolchain install 1.99.1 --profile minimal
rustup override set 1.99.1
rustup target add wasm32-wasip1 --toolchain 1.99.1
cargo +1.99.1 install cargo-component --version 0.21.1 --locked
cargo build --locked -p pushit -p pushit-controller
export PUSHIT_REPO="$PWD"
export PATH="$PUSHIT_REPO/target/debug:$PATH"
```

In a separate terminal at the repository root, start the controller and leave
it running; wait until it is listening:

```bash
PUSHIT_DEV=1 PUSHIT_BIND=127.0.0.1:9100 PUSHIT_BASE_DOMAIN=localhost \
  PUSHIT_DATA="$(mktemp -d /tmp/pushit-data.XXXXXX)" \
  ./target/debug/pushit-controller
```

Back in the terminal with the exported `PATH`, create the app **outside the
repository Cargo workspace**:

```bash
set -euo pipefail
# Isolate this sandbox from saved profiles/tokens and inherited auth flags.
export XDG_CONFIG_HOME="$(mktemp -d /tmp/pushit-config.XXXXXX)"
unset PUSHIT_TOKEN PUSHIT_PROFILE PUSHIT_DEV PUSHIT_SERVER
cd "$(mktemp -d /tmp/pushit-app.XXXXXX)"
export RUSTUP_TOOLCHAIN=1.99.1  # repository rustup override no longer applies
pushit init myapp
cd myapp
pushit build && pushit --dev --server http://127.0.0.1:9100 deploy
pushit --dev --server http://127.0.0.1:9100 verify   # checks: from pushit.yaml; nonzero exit = failure
pushit --dev --server http://127.0.0.1:9100 status myapp
pushit --dev --server http://127.0.0.1:9100 logs myapp
```

Verify `verify` reports all checks passed (UI `myapp` heading and function
`{"msg":"hello"}` come from the scaffold `checks:`), and status shows the
active revision. A nonzero build/deploy/check exit is a
failure, not success: inspect the error and logs, fix the cause, then repeat the
checks. Never report success based only on an attempted command. Stop the
controller with Ctrl-C when done; temporary directories remain for inspection.

For a **single-app manifest**, deploy uploads existing artifacts; it does not
build them. The normal loop is `pushit build && pushit --dev --server http://127.0.0.1:9100 deploy`. Only a **root manifest** (`apps:`) automatically
syncs repositories and builds before deploying. `build.fn`/`build.ui` are local
shell commands and their tools must be installed.

Dev mode bypasses auth: keep the controller loopback-only. The controller's
inline `PUSHIT_DEV=1` does not configure a separately invoked CLI; pass `--dev`
and an explicit `--server` for every local controller command. `--server` alone
does not prevent sending an active production token: the temporary
`XDG_CONFIG_HOME` and unset auth variables above isolate sandbox credentials.
Use a dedicated terminal and close it before returning to normal profiles.
Dev checks cannot validate production OIDC/auth.

**Local iteration without a controller:** `pushit dev` runs the component on the *same* wasmtime host as the platform (wasi:keyvalue/sql/blobstore/logging, `local` key-value store, 30s timeout) at `http://127.0.0.1:9101` — `/_fn/*` goes to the component, everything else serves `ui_dist`. Secrets/env: `--env KEY=value`; DSNs for sql connections: `--dsn name='postgres://…'`; outgoing HTTP: `--http-out`. Use it for fast function testing; deploy to a controller to test hub/revisions, and an authenticated non-dev instance to test auth.

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
entry: fn/target/wasm32-wasip1/release/myapp.wasm
env:                             # non-secret config; reaches fn as env vars
  GREETING: hi
secrets:                         # names only — values are set via `pushit secret set`
  - API_TOKEN                    # deploy FAILS if a named secret is not set
capabilities:                    # what the component is allowed to import/use
  http_out: ["api.example.com"]  # presence grants outgoing HTTP (MVP: unrestricted)
  env: [API_TOKEN]
  kv: [local]                    # 'local' = built-in per-app key-value store (NoSQL)
  sql: [db]                      # logical names, bound to secret sources at deploy (root manifest sql:)
  blobstore: [files]             # logical stores from blobstores.yaml
build:
  fn: cd fn && cargo component build --release --target wasm32-wasip1
  ui: cd ui && npm ci && npm run build   # optional, only if UI needs a build step
  ui_dist: ui/dist
```

Rules that bite when violated:

- **fn/ must be a standalone cargo project** — `build.fn` runs `cd fn && cargo component build ...`. If `fn/` has no `Cargo.toml` of its own (or is inside another cargo workspace), the build may compile the wrong workspace for a wasm target and fail with confusing tokio/wasm errors. `pushit init` generates the right layout; don't restructure it.
- **Server functions live under `/_fn/*`**. The component's `handle()` receives requests already routed: `/` and everything else → `dist/` statics; `/_fn/*` → component. Inside the component, match on `request.path()` — it still contains `/_fn/...`.
- **env and secrets reach the component as env vars** (wasi:cli/environment). Secrets exist on the platform only via `pushit secret set`; never commit values.
- **`config.js` is generated by the controller** on deploy into `ui_dist` (`window.__ENV__ = {...}`) — frontend reads runtime config from it, never bakes env into the bundle.
- **User identity**: after platform authentication, the controller sets `x-user-id`, `x-user-name`, `x-user-email` headers on every `/_fn` request. Read them; don't re-implement auth.

## Capabilities — host interfaces instead of raw HTTP

| Manifest | Guest imports | What it gives |
|---|---|---|
| `capabilities.kv: [local]` | `wasi:keyvalue` `store.open("local")`, bucket `get`/`set`/`delete`/`exists`/`list-keys` | Built-in NoSQL store: app-scoped key-value database at `$PUSHIT_DATA/appdata/<app>/local.kv`, no config, data survives redeploys/rollbacks. Values are bytes — JSON documents by convention. No query language; scan keys or keep key-prefix conventions |
| `capabilities.sql: [name]` | `wasi:sql` `connection.open(name)`, `statement.prepare`, `query`, `exec` | Real DB access (postgres/mysql/sqlite via sqlx, ClickHouse via HTTP). Names are logical and bound at deploy time in the root manifest (`sql: {name: secret:<app-secret-key> \| platform:<global-secret-name>}`). Credentials never reach the component. For the built-in store use `kv`, not sql |
| `capabilities.blobstore: [name]` | `wasi:blobstore` `get-container`, `get-data`, `write-data`, `list-objects`… | Object storage via opendal (s3/fs). Streams objects — fine for large files |
| (always available) | `wasi:logging` `log(level, ctx, msg)` | Structured lines into `pushit logs <app>` |
| `capabilities.http_out: [...]` | `wasi:http` outgoing-handler | Raw outgoing HTTP — prefer kv/sql/blobstore interfaces when they cover the service |

If a component imports an interface NOT granted in `capabilities`, the request fails — check the manifest first when a call errors with "not granted".

## Deep-dive references (read as needed)

- [references/kv.md](references/kv.md) — `wasi:keyvalue` full contract: the built-in per-app NoSQL store, bucket semantics, pagination, key conventions for document data
- [references/sql.md](references/sql.md) — `wasi:sql` full contract: connection/statement resources, drivers (postgres/mysql/sqlite/ClickHouse), flattened rows, binding sources
- [references/blobstore.md](references/blobstore.md) — `wasi:blobstore` full contract: containers, object read/write, listing, cross-store copy/move
- [references/logging.md](references/logging.md) — `wasi:logging` levels and where lines land

## Everyday commands

```bash
pushit build                          # run build steps; require wasm (missing UI only warns)
pushit dev                            # local runtime, no controller (see Testing)
pushit --dev --server http://127.0.0.1:9100 deploy                   # upload manifest + app.wasm + dist/
pushit --dev --server http://127.0.0.1:9100 list                     # all apps
pushit --dev --server http://127.0.0.1:9100 status myapp             # detail + revision history
pushit --dev --server http://127.0.0.1:9100 logs myapp               # last 200 lines
pushit --dev --server http://127.0.0.1:9100 logs -f myapp            # live stream (SSE) — run in background while reproducing
pushit --dev --server http://127.0.0.1:9100 secret set myapp K=V     # set/update secret value
pushit --dev --server http://127.0.0.1:9100 secret ls myapp          # names only
pushit --dev --server http://127.0.0.1:9100 rollback myapp           # previous revision
pushit --dev --server http://127.0.0.1:9100 rollback myapp --revision <id>
pushit profile add prod --server https://pushit.example.com   # register instance
pushit login --profile prod           # auth flow for that profile
pushit profile use prod               # make active (default for all commands)
pushit --profile stage <cmd>          # one-off via another profile, no switching
```

`--dev` sends `X-Pushit-Dev: 1` — use only against a local dev controller (`PUSHIT_DEV=1`), with `--server http://127.0.0.1:9100`. For production: run `pushit login` once (browser → long-lived token in `~/.config/pushit/token`, reused automatically, ~1 year; `--token <jwt>` / `PUSHIT_TOKEN` still work and take precedence), plus `--server <url>` / `PUSHIT_SERVER` (default `http://localhost:9100`). Token resolution is `--token` > `PUSHIT_TOKEN` > nonempty selected/active profile token > legacy saved token. `--dev` is independent: a dev header and a resolved Bearer token can both be sent. Server resolution is `--server` > `PUSHIT_SERVER` > selected/active profile server > `http://localhost:9100`. Multiple push-it instances: `pushit profile add <name> --server <url>` → `pushit login --profile <name>` → `pushit profile use <name>` (active), `pushit --profile <name> <cmd>` for a one-off against another instance, `pushit profile list|show|remove` to manage.

## Testing and iterating

**Test server functions locally first** — `pushit dev` runs the app on the same wasmtime host as the platform, no controller needed:

```bash
pushit dev                                          # build + serve http://127.0.0.1:9101
pushit dev --env KEY=value --dsn db='postgres://…' --http-out
curl http://127.0.0.1:9101/_fn/hello                # exercise the function
```

This is the default way to develop and debug `/_fn/*` logic: same host functions (wasi:keyvalue/sql/blobstore/logging), same `local` key-value store, same 30s timeout — local success is useful evidence, not a guarantee of a successful platform deployment (configuration, capabilities, secrets, auth, and routing can differ). Go to the controller (`pushit build && pushit --dev --server http://127.0.0.1:9100 deploy`) to verify the hub, revisions/rollback, or bound secrets; use an authenticated non-dev instance to verify production auth.

## Managing immutable app versions

Every successful deploy creates and immediately activates an immutable version.
Version IDs are existing revision IDs (not Git SHAs); no migration is needed.
At most one version is active; zero active versions is valid.

```sh
pushit version list myapp
pushit version activate myapp <id>    # any retained version with a snapshot
pushit version deactivate myapp       # functions + UI/static/config now return 404
pushit version activate myapp <id>    # reactivation is supported
pushit version delete myapp <id>      # permanent; inactive only, asks confirmation
# --yes skips the deletion confirmation; use only with deletion authorization.
```

Active-version deletion returns 409: first activate another version or deactivate
the app. An inactive version still used by an in-flight request also returns 409;
retry after it finishes. Other versions/apps keep serving. Legacy inactive
versions without manifest snapshots can be deleted, but require redeploy before
activation. Hub controls expose the same lifecycle; the API is documented in
`API.md` under “Immutable app versions”. Existing `rollback myapp [--revision ID]`
and revision history remain compatible.

Activation restores WASM, UI, manifest/env/capabilities and domain atomically,
**not database/file contents or secret values**. Deactivation/version deletion
preserve app data, secrets and other versions. Validate old code against current
data/schema before activation. Metadata deletion is authoritative; filesystem
cleanup failures are logged and may leave unserved orphan artifact directories.

## Debugging a deployed app

1. **Deploy-time failure** (400 from `pushit deploy`): read the error — it names the cause: bad manifest, missing secret, component missing `incoming-handler` export, wasm not a component.
2. **Runtime failure**: `pushit logs -f myapp` in one shell (`--grep <substr>` / `--since 30m` to narrow), reproduce in another/browser. The log gets: component stdout/stderr, `wasi:logging` lines, `handler failed: …` (trap/panic), `handler timed out after 30s`, platform events (deploys, rollbacks).
3. **Capability errors**: `connection 'x' not granted by capabilities.sql`, `no DSN for connection 'x'`, `bucket 'x' not granted by capabilities.kv` — grant missing in the manifest, or the deploy manifest's `sql:` binding points at a missing secret; fix the manifest/binding or set the secret (`pushit secret set` / `pushit secret global set`).
4. **UI loads but `/_fn` 404/500**: check the handler actually matches the request path (it includes `/_fn` prefix) and the component didn't trap — `handler failed` in logs.
5. **Rollback when broken**: `pushit rollback myapp` — instant revert to previous revision; investigate at leisure.

## Making changes to an existing app

Edit code → verify with `pushit dev` (fast, local), then `pushit build && pushit --dev --server http://127.0.0.1:9100 deploy` followed by `pushit --dev --server http://127.0.0.1:9100 verify` when the change should land (rebuild + redeploy = new revision; previous revisions stay for rollback). Secrets and env changes also need a redeploy to take effect.

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
