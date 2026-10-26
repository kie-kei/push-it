# HANDOFF: что осталось сделать

Цель сессии: реализовать предложения по развитию PushIt, **кроме** экспорта/бэкапа
данных (он отложен сознательно — не делать). Этот файл — точка продолжения.
Спецификации задач агентов лежат рядом: `.agent-tasks/cron-task.md`,
`.agent-tasks/cli-tooling-task.md`.

## Окружение

- Rust 1.99.1 (`rustup override set 1.99.1`), cargo-component 0.21.1;
  реестр крейтов прогрет, для wasm-фикстур использовать `--offline`.
- Тесты: `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 cargo test --workspace --locked`.
- e2e-паттерн: `scripts/http-out-e2e.py`, `scripts/p1-reliability-e2e.py`
  (контроллер на эфемерном порту, `PUSHIT_DEV=1`, tmp-директория).
- Прогрев wasm-фикстуры: `cargo build --manifest-path examples/hello/fn/Cargo.toml
  --release --target wasm32-wasip2 --locked` (нужен для security/p1/portability e2e).

## УЖЕ СДЕЛАНО (в origin/main)

1. **Allowlist `http_out`** (0dcdfd2 / 731ef49 и раньше):
   `capabilities.http_out` — список хостов (точный хост, `*.suffix`, `'*'`;
   порт игнорируется; регистр не важен), проверка в host-outgoing-handler
   (`controller/src/runtime.rs::send_request`), fail-closed на кривом гранте
   при парсинге манифеста. `pushit dev --http-out` = разрешить всё.
   Тесты: юнит в manifest.rs + `scripts/http-out-e2e.py` (шаг в CI добавлен).
2. **P1-reliability e2e** (`scripts/p1-reliability-e2e.py`, 731ef49):
   инъекции отказов деплоя (битый архив/манифест/компонент/секрет/пустой
   http_out) не трогают работающую ревизию; rollback A↔B дважды + невалидная
   цель; консистентность активной ревизии после двух рестартов контроллера.
   **Шаг в CI для него ещё НЕ добавлен** — см. чеклист.

## ВЕТКИ В ОРИДЖИНЕ (продолжать здесь)

### `feat/artifact-digests` — переносимость (начато, wip-коммит 9814680)

Готово: `controller/src/digest.rs` (sha256 по app.wasm + отсортированным файлам
`dist/`, исключая генерируемый контроллером `dist/config.js`; 2 юнит-теста),
колонка `revisions.digest` (миграция + `backfill_digests()` для легаси-строк),
`Db::deploy_revision_with_digest`, `Revision.digest`. Ветка компилируется,
все тесты зелёные.

Осталось:

1. `server.rs::deploy_inner`: посчитать digest из распакованной ревизии
   (`crate::digest::artifact_digest(&dir.join("app.wasm"), dist-если-есть)`)
   и записать через `deploy_revision_with_digest`; добавить `"digest"` в JSON
   ответа deploy.
2. `api_versions` + `api_app_detail`: включить `digest` в версии (db.rs уже
   возвращает; JSON в server.rs собирает поля вручную — добавить).
3. CLI `pushit plan --artifacts` (`cli/src/main.rs::plan_root`): для каждого
   приложения сравнивать digest свежей локальной сборки с digest активной
   версии на платформе; вывод `ARTIFACT-MATCH` / `ARTIFACT-DRIFT` (и в
   human-выводе, и в будущем `--json`). Digest локально считается тем же
   алгоритмом — переиспользовать `pushit_controller::digest::artifact_digest`
   (CLI уже зависит от pushit-controller).
4. Расширить `scripts/portability-smoke.py`: после деплоя на оба инстанса
   проверить через API, что digest версий совпадает между инстансами.
5. Док `docs/portability.md` — абзац про digest (что покрывает, что нет:
   toolchain-версии не входят в digest, их фиксирует `rust-toolchain.toml`).
6. ROADMAP.md приоритет 3: отметить digest-механизм как реализованный.

### `feat/cron-scheduler` — cron (полная спецификация: `.agent-tasks/cron-task.md')

Работа не начата (агент упал по сети). Ветка пустая (= main). Выполнять строго
по `.agent-tasks/cron-task.md`: `cron:` в манифесте, планировщик в
`controller/src/cron.rs`, Db::active_apps, рефакторинг общего
resolve_call_env_caps, юнит-тесты + `scripts/cron-e2e.py` + `docs/cron.md`.

### `feat/cli-agent-tooling` — CLI-инструменты (спецификация: `.agent-tasks/cli-tooling-task.md`)

Работа не начата. `pushit logs --json`, `pushit plan --json`, `pushit doctor`
(--json), `pushit init --template` (hello / clickhouse-dashboard /
s3-browser) с обязательной реальной сборкой новых шаблонов. Выполнять строго
по `.agent-tasks/cli-tooling-task.md`.

## Порядок интеграции

1. cron и cli-tooling независимы друг от друга, но ОБА трогают `cli/src/main.rs`
   (cron — только e2e-скрипт, cli-tooling — команды) → ожидаемый конфликт с
   `plan --artifacts` из digest-ветки в `plan_root`. Порядок: digest → cli-tooling
   → cron (cron ничего не меняет в CLI, кроме нового e2e).
2. После каждой ветки: `cargo fmt --all`, clippy, полный `cargo test
   --workspace --locked`, соответствующий e2e локально.
3. Мерж в main обычным merge-коммитом (стиль репо), пуш.

## Финальный чеклист (после всех веток)

- [ ] CI `.github/workflows/ci.yml`: добавить шаги `p1-reliability-e2e.py`,
      `cron-e2e.py` (после security-e2e; портABILITY/http-out уже есть).
- [ ] README.md: новые команды (`doctor`, `init --template`, `plan --artifacts`,
      `logs --json`), cron в списке возможностей.
- [ ] ARCHITECTURE.md: cron в таблицу host-интерфейсов/состав платформы
      (планировщик в контроллере), digest в раздел IaC.
- [ ] API.md: digest в ответах deploy/versions, cron-семантика.
- [ ] skills/pushit-app/SKILL.md: cron-секция манифеста, шаблоны.
- [ ] ROADMAP.md: «Текущая база» + снять реализованные пункты backlog'а
      (cron), приоритет 3 (digest).
- [ ] `scripts/scaffold-smoke.sh` — убедиться, что `init --template` не сломал
      дефолтный scaffold.
- [ ] Удалить HANDOFF.md и `.agent-tasks/` финальным коммитом, когда всё
      сделано.
- [ ] **НЕ делать**: экспорт/бэкап данных (отложено пользователем).

## Примечания по состоянию репо

- origin/main = локальный main (allowlist + p1-reliability уже там).
- Worktrees `/home/kkm/Projects/push-it-wt-cron` и `...-wt-cli` можно
  переиспользовать (там лежат невскрытые AGENT_TASK.md — уже скопированы в
  `.agent-tasks/`).
- `.humanlayer/` в gitignored, не коммитить.
