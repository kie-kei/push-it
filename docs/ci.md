# CI

GitHub Actions запускает один job на pull request, push в `main` и вручную:

- `cargo fmt --all -- --check` и fmt отдельного WASM-fixture;
- `python3 scripts/test-oidc-config.py` — статическая регрессия Compose/realm;
- `cargo clippy --workspace --all-targets --locked`;
- `cargo test --workspace --locked`;
- `cargo build --workspace --locked`;
- сборка `examples/hello/fn` для `wasm32-wasip2` с `--release --locked`;
- `python3 scripts/security-e2e.py` — production-отказы и реальный WASM
  deploy A → B → rollback A, маршруты, config.js и SQL.

Версия Rust закреплена на `1.99.1`, оба `Cargo.lock` хранятся в Git.
Для повторения используйте эту версию с компонентами `rustfmt`, `clippy`
и target `wasm32-wasip2`, затем команды выше из корня репозитория.
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
