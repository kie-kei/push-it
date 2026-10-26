# Agent task: CLI agent tooling for PushIt (doctor, JSON outputs, templates)

You are implementing CLI features in the PushIt repository. Work EXCLUSIVELY in
this git worktree (branch `feat/cli-agent-tooling`, based on main). NEVER touch
/home/kkm/Projects/push-it (the main checkout) and never push to any remote.
Leave your work as commits on the `feat/cli-agent-tooling` branch. AGENT_TASK.md
itself must not be committed (it is untracked; leave it as is).

## Environment setup (do this first)

```
source ~/.cargo/env
cd /home/kkm/Projects/push-it-wt-cli
rustup override set 1.99.1
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
```

Rust 1.99.1 and cargo-component 0.21.1 are installed. The crates.io registry
cache is warm; use `--offline` where possible.

## Project context

PushIt runs small internal apps as WASM components + static UI; `pushit` is the
thin CLI. Coding agents are primary users, so machine-readable output and
self-diagnosis matter. Read first:

- cli/src/main.rs (entire file: command parsing, `show`/`check` helpers, `logs`
  with --since/--grep + `log_line_epoch`, `verify` with --json as the JSON
  style reference, `init` with embedded scaffold consts INIT_* — note how
  INIT_WIT_DEPS reuses files from examples/hello/fn/wit/deps via include_str)
- cli/src/profiles.rs, cli/src/dev.rs
- cli/tests/init.rs and cli/tests/skill_install.rs (integration test style)
- controller/src/manifest.rs (Manifest::parse — used by doctor/init templates)
- examples/hello/fn/wit/deps/ (vendored WIT: blobstore/, keyvalue/, logging/,
  cli/, http/, io/, clocks/) and wit/sql/{types.wit,readwrite.wit} at repo root
- examples/hello/fn/src/lib.rs (guest code style; has kv/blob/logging examples)
- docs/sql.md, docs/blobstore.md (capability docs; templates follow them)
- scripts/version-lifetime-e2e.py lines ~85-120 (how a fixture vendors wasi:sql
  wits and patches world.wit/Cargo.toml)

## Feature 1: `pushit logs --json`

Add `--json` to LogsArgs. With it, print each kept line as one JSON object per
line: `{"line": "<raw line>", "epoch": <int or null>}` where epoch comes from
the existing `log_line_epoch` (null when the line has no stamp). Filters
(--since/--grep) and --follow keep working with the same semantics. Add unit
tests in the style of `verify_logs_tests` (module `logs_json_tests`).

## Feature 2: `pushit plan --json`

Add `--json` to the Plan command. Default human output unchanged. With --json,
print a single JSON object summarizing the run:
`{"apps": [{"key": "<root key>", "app": "<app name or null>", "status":
"<create|drift|error|up-to-date|orphan>", "changes": ["..."]}], "drift": <n>}`
where `changes` carries the same strings the human path prints (env drift,
missing secrets, sql bindings, sync/manifest errors), and orphan apps appear
with status "orphan". Exit code semantics unchanged. Unit-test the
aggregation/serialization logic as a pure function where practical.

## Feature 3: `pushit doctor`

New subcommand `doctor` with `--json`. It diagnoses the local environment and
the target instance, printing `ok`/`warn`/`fail` lines and exiting non-zero
iff any check FAILS (warnings don't fail). Checks:

1. `cargo` — present (fail if missing); `rustc --version` contains "1.99.1"
   (warn on other versions; fail if rustc missing).
2. `cargo component --version` — fail if missing; warn if not "0.21.1".
3. `rustup target list --installed` includes wasm32-wasip1 — warn if missing.
4. `git --version` — warn if missing.
5. Server reachability: GET `<server>/auth/mode` — ok with {"mode":"local"|
   "builtin"} (report the mode); a 404/other response → warn "not a standalone
   owner controller (OIDC/dev login applies)"; connection error → fail.
6. Authenticated API: GET `<server>/api/apps` with the normal client headers —
   200 → ok; 401 → fail with hint "run `pushit login` (or use --dev with a
   PUSHIT_DEV=1 controller)".
7. Local manifest: if pushit.yaml exists in cwd, parse it, report `name`;
   warn on parse errors; if absent, info/skip (not a failure — doctor also
   runs outside app dirs).

`--json` prints `{"checks":[{"name","status":"ok|warn|fail","detail":"..."}],
"ok":<bool>}`. Use short timeouts (5s) for HTTP checks. Implementation notes:
the client is `client_for(&server, dev, token)` — but build a plain blocking
client for /auth/mode so the auth header is not leaked to an unknown mode
probe (mirror how `verify` uses a bare probe client). Unit-test the pure parts
(version parsing/comparison, JSON shape, check aggregation) without network.

## Feature 4: `pushit init --template <name>`

Add `template: Option<String>` to InitArgs (`--template`, default "hello").
Unknown names error and list the available templates. Templates:

- `hello` — exactly the current scaffold (no behavior change).
- `clickhouse-dashboard` — a minimal analytics UI over wasi:sql:
  - manifest: `capabilities: {sql: [db]}`, `sql_bindings: {db:
    'secret:CLICKHOUSE_DSN'}`, `secrets: [CLICKHOUSE_DSN]` (document in the
    template's pushit.yaml comments: set with `pushit secret set`), env {},
    checks for `/_fn/tables`.
  - world.wit: import wasi:sql/types@0.2.0-draft, wasi:sql/readwrite@0.2.0-draft,
    export wasi:http/incoming-handler@0.2.0. Vendor sql wits by copying from
    repo root wit/sql/{types.wit,readwrite.wit} via include_str (follow the
    INIT_WIT_DEPS pattern; add "wasi:sql" = { path = "wit/deps/sql" } to the
    component target dependencies in the generated Cargo.toml).
  - fn: route `/_fn/tables` opens connection "db", prepares
    `SELECT database, name FROM system.tables LIMIT 50` (works on ClickHouse
    via the platform's HTTP DSN; also fine on postgres/mysql with minimal
    edits — note that in a comment), runs query, returns a JSON array of
    objects built from the returned rows (row.field-name + data-type variant;
    serialize strings/numbers/null, other variants as strings). Any sql error →
    500 with {"error": "..."}.
  - ui/index.html: no build step; fetch('/_fn/tables') on load, render a
    simple table (vanilla JS), show the error message on failure.
- `s3-browser` — minimal object listing over wasi:blobstore:
  - manifest: `capabilities: {blobstore: [files]}` (note in comments: bind the
    store name in $PUSHIT_DATA/blobstores.yaml — see docs/blobstore.md),
    checks for `/_fn/objects`.
  - world.wit: import wasi:blobstore/types@0.2.0-draft,
    wasi:blobstore/container@0.2.0-draft,
    wasi:blobstore/blobstore@0.2.0-draft (wit files already vendored in
    examples/hello/fn/wit/deps/blobstore — include_str them; add the three
    "wasi:blobstore" path deps like hello's Cargo.toml does).
  - fn: route `/_fn/objects` opens container "files" and lists object names
    (copy the API usage style from examples/hello/fn/src/lib.rs
    blob_roundtrip) → JSON array of names; errors → 500 {"error": "..."}.
  - ui/index.html: fetch('/_fn/objects'), render a list.

CRITICAL quality gate: scaffold each new template into a temp dir and build it
for real: `cargo component build --manifest-path <dir>/fn/Cargo.toml
--release --target wasm32-wasip1 --locked --offline` (cwd = worktree root for
the rustup override; the generated fn/.cargo/config.toml pins the target).
Iterate until both templates compile cleanly. Also extend cli/tests/init.rs to
scaffold all templates into temp dirs and assert key files exist
(pushit.yaml mentions the right capability, fn/wit/world.wit imports, ui/
index.html) — follow the existing test's isolation patterns.

## Do NOT edit

.github/workflows/ci.yml, README.md, ROADMAP.md, skills/ — the coordinator
handles CI/docs centrally.

## Definition of done

- `cargo fmt --all` clean; `cargo clippy --workspace --all-targets --locked`
  no new errors; `cargo test --workspace --locked` green (including your new
  tests).
- Both new templates compile as real wasm components (verified locally).
- All work committed on feat/cli-agent-tooling (one or a few `feat:` commits,
  repo style — see `git log`).
- Final message: files changed, key decisions, template build verification
  output summary, test results.
