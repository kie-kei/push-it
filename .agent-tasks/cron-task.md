# Agent task: cron scheduler for PushIt

You are implementing a feature in the PushIt repository. Work EXCLUSIVELY in this
git worktree (branch `feat/cron-scheduler`, based on main). NEVER touch
/home/kkm/Projects/push-it (the main checkout) and never push to any remote.
Leave your work as commits on the `feat/cron-scheduler` branch. AGENT_TASK.md
itself must not be committed (it is untracked; leave it as is).

## Environment setup (do this first)

```
source ~/.cargo/env
cd /home/kkm/Projects/push-it-wt-cron
rustup override set 1.99.1
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
```

Rust 1.99.1 and cargo-component 0.21.1 are installed. The crates.io registry
cache is warm; use `--offline` for fixture builds when possible.

## Project context

PushIt runs small internal apps as WASM components (wasi:http
incoming-handler) inside a Rust controller, plus static UI. Apps are described
by pushit.yaml. Read these files first:

- ARCHITECTURE.md, docs/kv.md (style example for the new doc)
- controller/src/manifest.rs (Manifest parse/validate; Capabilities; how sql
  grants are validated and how http_out allowlist is validated — follow these
  patterns)
- controller/src/server.rs (AppState, `call_fn` — env+secrets+HostCaps
  resolution, api_deploy/deploy_inner, router)
- controller/src/runtime.rs (Runtime::call, HostCaps)
- controller/src/db.rs (Db: deploy_revision, active_revision, revisions,
  pin_revision, pin_active_by_domain)
- controller/src/main.rs (startup, AppState construction)
- scripts/security-e2e.py and scripts/http-out-e2e.py (e2e script patterns:
  request/deploy/start/stop helpers, controller on an ephemeral port with
  PUSHIT_DEV=1)
- examples/hello/fn/src/lib.rs + wit/world.wit (guest fixture; already imports
  wasi:logging and has route /_fn/log)

## Feature: cron-scheduled requests from the app manifest

Goal: an app manifest can declare scheduled GET requests that the controller
fires against the app's own component, so internal report/batch apps have a
"morning report" trigger without external schedulers.

### Manifest section

```yaml
cron:
  - name: report              # required, DNS-label (a-z0-9-), unique within the app
    schedule: "0 9 * * *"     # required, standard 5-field cron (minute hour dom month dow), UTC
    path: /_cron/report       # optional, default /_cron/<name>; must start with '/'
```

- Add `pub cron: Vec<CronEntry>` to Manifest with serde default; validate in
  `Manifest::parse`: names valid+unique, schedule parses, path starts with '/'.
  Empty list is fine. A non-list `cron:` or entries missing required fields must
  fail with clear messages (mirror the sql-grant error style).
- Add the `cron` crate (0.15.x: `Schedule::from_str`, occurrences via
  `after(&DateTime<Utc>)`; pairs with chrono) to controller/Cargo.toml. chrono
  is already a dependency.

### Controller scheduler (new file controller/src/cron.rs)

- Pure, unit-testable core: jobs as structs {app, name, path, schedule};
  per-job state `next_due: DateTime<Utc>`. On each scan (tick) rebuild the job
  set from ALL apps that have an active revision (new Db method
  `active_apps() -> Result<Vec<(App, Revision)>>` — join apps+revisions where
  active=1, mirroring active_app_by_domain). Jobs of apps that disappeared or
  lost their active version are dropped. New jobs start with
  `next_due = first occurrence strictly after now` — NO retroactive catch-up
  of runs missed while the controller was down.
- A job fires when `now >= next_due` and the same job isn't still running
  (track in-flight job keys in a HashSet). On fire: advance `next_due` to the
  first occurrence strictly after now (skip missed occurrences), spawn the run.
- Tick interval: 30 seconds default, overridable via env PUSHIT_CRON_TICK_SECS
  (u64 seconds; lets tests tick fast). Read once at scheduler start;
  invalid/missing → 30.
- Firing a run: resolve the same env (manifest env + decrypted secrets) and
  HostCaps as server.rs `call_fn` — refactor a shared helper (e.g.
  `pub(crate) fn resolve_call_env_caps(s: &AppState, app: &App, manifest: &Manifest)
  -> Result<(Vec<(String,String)>, HostCaps)>`) out of call_fn and reuse it from
  both call sites; do NOT duplicate the sql-grant/blobstore/env logic. Add one
  env var: `PUSHIT_CRON=<name>`. Build a hyper GET request for the configured
  path with headers: `Host: <app domain>`, `x-user-id: cron`,
  `x-user-name: cron`, `x-user-email: cron@pushit.local`,
  `x-pushit-cron: <name>`. Pin the revision (db.pin_revision or
  pin_active_by_domain) so deletion can't race the run, call runtime.call, and
  append one line to the app log (s.logs.append):
  `cron '<name>' GET <path> -> <status or error summary>` plus any guest
  stdout/stderr (same as call_fn does). Timeout comes from the runtime as usual.
- Spawn the scheduler from controller/src/main.rs after AppState is built
  (tokio::spawn). It must never crash the process: log errors via
  tracing::warn and continue.
- Manifest changes (deploy/activate/deactivate) take effect within one tick via
  the rescan — document this.

### Tests (all must pass; English comments, existing style)

1. Unit tests in manifest.rs for cron parsing: valid minimal, default path,
   duplicate names, bad name, bad schedule string, path without leading '/',
   non-list cron.
2. Unit tests in cron.rs for the scheduling core with an injected clock:
   next_due initialization (strictly after now), firing when due, advancing
   past missed occurrences, dropping removed jobs, not firing again while a
   run is in flight (model runs as closures recorded into a Vec; no real
   spawning in unit tests).
3. E2E script scripts/cron-e2e.py following scripts/http-out-e2e.py structure:
   build a fixture by copying examples/hello/fn (ignore target); the manifest
   cron entry targets path `/_fn/log` (already exists, logs via wasi:logging —
   no guest patching needed). Deploy with manifest:
   `name: cron-smoke\ncron:\n  - name: tick\n    schedule: "* * * * *"\n    path: /_fn/log\n`.
   Start the controller with PUSHIT_DEV=1 and PUSHIT_CRON_TICK_SECS=1 on an
   ephemeral port (build the controller first: `cargo build -p
   pushit-controller`; build the fixture wasm with `cargo component build
   --manifest-path <fixture>/Cargo.toml --release --target wasm32-wasip1
   --locked --offline`, cwd=worktree root). Poll GET /api/apps/cron-smoke/logs
   until the wasi:logging line appears (timeout 75s — "*" fires within a
   minute). Also assert the log contains the `cron 'tick'` marker. Print a
   PASS line. Keep total runtime under ~90s.
4. Do NOT edit .github/workflows/ci.yml, README.md, ROADMAP.md, skills/ — the
   coordinator adds CI/docs centrally.

### Docs

Create docs/cron.md in the style of docs/kv.md (English, concise): manifest
syntax, UTC semantics, GET-only, default path, PUSHIT_CRON env var +
x-pushit-cron header, no catch-up after downtime, ≤1 tick activation delay,
one run at a time per job, timeout, where logs go, PUSHIT_CRON_TICK_SECS.

### Definition of done

- `cargo fmt --all` clean; `cargo clippy --workspace --all-targets --locked`
  with no new errors; `cargo test --workspace --locked` all green.
- `python3 scripts/cron-e2e.py` prints PASS.
- All work committed on feat/cron-scheduler as a single `feat:` commit (repo
  style: lowercase imperative subject + body explaining design decisions; see
  `git log`).
- Final message: files changed, key design decisions, test/e2e results.
