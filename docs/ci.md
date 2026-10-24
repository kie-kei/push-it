# CI

GitHub Actions запускает один job на pull request, push в `main` и вручную:

- `cargo fmt --all -- --check` и fmt отдельного WASM-fixture;
- `python3 scripts/test-oidc-config.py` — статическая регрессия Compose/realm;
- `cargo clippy --workspace --all-targets --locked`;
- `cargo test --workspace --locked`;
- `cargo build --workspace --locked`;
- сборка `examples/hello/fn` для `wasm32-wasip2` с `--release --locked`;
- `cargo install cargo-component --version 0.21.1 --locked` и
  `bash scripts/scaffold-smoke.sh` — сборка свежего шаблона без WIT registry;
- `python3 scripts/security-e2e.py` — production-отказы и реальный WASM
  deploy A → B → rollback A, маршруты, config.js и SQL;
- `python3 scripts/portability-smoke.py` — настоящий CLI deploy одинаковых
  артефактов на два чистых локальных инстанса, отдельные секреты/SQL и
  HTTP-проверки ([границы проверки](portability.md)).

Версия Rust закреплена на `1.99.1`, оба `Cargo.lock` хранятся в Git.
Для повторения используйте эту версию с компонентами `rustfmt`, `clippy`
и targets `wasm32-wasip2`, `wasm32-wasip1`, затем команды выше из корня репозитория.
Smoke ожидает обычные пути `target/` без переопределения `CARGO_TARGET_DIR`.
Существующие Clippy warnings видны, но не превращены в ошибки через `-D warnings`.

Job использует только read-only `contents`, checkout без сохранённых credentials,
закреплённые SHA официальных actions, cache зависимостей/артефактов и отмену
устаревшего запуска той же ветки. Debug symbols и incremental compilation
отключены, параллелизм сборки ограничен двумя процессами. Это уменьшает расход
диска и памяти для Wasmtime на обычном hosted runner; отдельной матрицы нет.

Секреты, внешний IdP, публичные DNS/сертификаты и production-инстанс CI не нужны.
Тест использует временные данные и loopback. Его deploy/rollback идут в dev-режиме;
он **не заменяет** production OIDC/full-stack проверку из
[security-upgrade.md](security-upgrade.md). Workflow ничего не деплоит,
не делает merge и не отключает проверку TLS.

### Standalone local authentication

After the workspace and real `examples/hello/fn` WASM build, CI also runs
`python3 scripts/local-auth-e2e.py`. It uses only temporary synthetic owner data
and loopback: configuration rejection, stdin setup/login, proxy-isolated CLI,
real WASM deployment, unauthenticated app-to-hub login/handoff, identity and
credential forwarding, version changes, restart persistence and durable token
revocation. `cargo test --workspace --locked` additionally covers local session
purpose/authority isolation, expiry, replay, CSRF, logout families and attempt
limits. This is not live OIDC-provider or production deployment verification.
