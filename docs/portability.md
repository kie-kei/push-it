# Local code/config portability smoke

Run from the repository root after building the workspace and hello fixture:

```sh
cargo build --workspace --locked
cargo build --manifest-path examples/hello/fn/Cargo.toml --release --target wasm32-wasip2 --locked
python3 scripts/portability-smoke.py
```

Use CI's pinned Rust/toolchain and lockfiles ([CI](ci.md)). Python 3 needs only
its standard library, including `sqlite3`. The fixture uses the repository's
WASI component bindings/draft SQL host interface and PushIt's compatible runtime;
this is not a promise of compatibility with arbitrary WASM hosts.

The smoke writes one app manifest and one tiny static UI, reuses the **same
once-built** `hello_fn.wasm`, and freezes their SHA-256 hashes before deployment.
It invokes the actual `pushit --dev --server ... deploy --manifest ... --wasm ...
--dist ...` CLI twice, with no rebuild and no dependency on `plan` output. It
checks unchanged source artifacts, stored manifest/WASM/UI, generated `config.js`,
public `GREETING` env and real HTTP function behavior on both instances.

Two controllers use distinct loopback ports, base domains (`a.localhost` and
`b.localhost`), temporary data roots, session keys, HOME and XDG directories.
Requests connect directly to loopback with explicit Host headers; no DNS setup,
proxy, saved credentials or external service is used. Controller readiness
requires a successful empty-list API response, not a log message. Wrong-instance
app hosts return 404, and B remains usable after A stops.

`PORTABILITY_TEST_SECRET` is a required app secret, supplied independently with
synthetic values through `pushit secret set`; deployment without it fails without
creating an app. A's provisioning does not unblock B. Secret names/values do not
appear in browser config. The hello fixture does not consume this secret at
runtime: this checks provisioning/isolation, not arbitrary secret integrations.
The manifest grants only app-local `sql: [local]`, with no external SQL DSN,
blobstore grant, external IdP or auth-group dependency. Domain mapping is
instance configuration, so the same manifest routes to different hostnames.

SQL requests create fresh independent fixture data: A has two rows and B one.
The script reads only its own temporary SQLite files with stdlib `sqlite3`, since
the hello SQL response currently is not valid JSON. It copies **no** app DB,
controller DB, encryption key or data directory; separate encryption keys are
also checked. Everything is stopped and removed on exit.

This establishes a bounded **two-local-instance isolation and artifact/config
portability regression**, not the full roadmap acceptance criterion of a second
host. It does not verify two VMs, cross-OS/runtime compatibility, source rebuild
reproducibility, production TLS/OIDC/Keycloak, data migration or backup/restore.
`plan` is still a configuration diff, not evidence of artifact equivalence.
