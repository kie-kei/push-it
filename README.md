# PushIt

Внутренняя платформа для деплоя небольших приложений на одну ВМ. Приложения — WASM-компоненты (`wasi:http`) + статичный UI; никаких контейнеров/процессов на приложение. Для команд разработчиков и сотрудников, отвечающих за свой проект. Целевой сценарий: агент создаёт, проверяет и деплоит внутрянку, пользователь сразу ей пользуется. Цель — переносимый деплой кода и конфигурации через манифесты и CLI; миграция данных отложена.

[Видение](VISION.md) · [Приоритеты и критерии готовности](ROADMAP.md) · [Архитектура](ARCHITECTURE.md) · [API](API.md).

## Standalone platform

Want the full controller, hub, app subdomains, versions and secrets without Keycloak/Caddy? Use explicit **local owner auth** on loopback. It is separate from `pushit dev` and does not disable authentication. See [local setup, CLI login and security boundaries](docs/standalone-local-auth.md).

For a fixed external HTTPS domain behind any same-host reverse proxy, use explicit
**builtin owner auth**: [proxy contract and tested nginx setup](docs/standalone-proxy-auth.md).
No embedded TLS or mandatory Keycloak/Caddy; the controller remains loopback HTTP.

## Установка CLI

```bash
curl -fsSL https://raw.githubusercontent.com/kie-kei/push-it/master/scripts/install.sh | sh
```

Требуется Rust (`cargo`); бинарник ставится в `~/.cargo/bin/pushit`.

Удаление:

```bash
curl -fsSL https://raw.githubusercontent.com/kie-kei/push-it/master/scripts/uninstall.sh | sh
```

### Skill для coding agents

```bash
pushit skill install             # ~/.agents/skills/pushit/
pushit skill install --project   # ./.agents/skills/pushit/ в текущем каталоге
```

После подтверждения устанавливаются `SKILL.md` и три документа в `references/`;
они встроены в CLI, исходный checkout для установки не нужен. Повторная установка
перезаписывает эти файлы после подтверждения, остальные файлы не удаляет.
Старый каталог `.agents/skill/pushit/` автоматически не переносится и не удаляется:
проверьте свои изменения перед его ручной очисткой. `pushit skill` печатает гайд
без установки.

## Quickstart (dev)

Нужны Rust/rustup и Cargo (CI использует Rust **1.99.1**), `curl` и
`cargo-component` **0.21.1**. Команды ниже — для Bash, начиная из корня
клонированного репозитория PushIt. Сборка требует доступа к реестру Rust-крейтов
(и к источникам дополнительных зависимостей вашего приложения).
Node.js/npm для сгенерированного статичного UI не нужны. Scaffold включает
закреплённые WIT-зависимости WASI 0.2 в `fn/wit/deps/`: отдельный WIT-реестр
для них не нужен. Сохраняйте включённый файл NOTICE и лицензионные уведомления.
`cargo-component` 0.21.1 использует Rust-цель `wasm32-wasip1` и адаптирует
результат в WASI 0.2-компонент (`wasi:http`); это не деплой core-модуля WASI 0.1.

```bash
set -euo pipefail
[ ! -f "$HOME/.cargo/env" ] || source "$HOME/.cargo/env"
rustup toolchain install 1.99.1 --profile minimal
rustup override set 1.99.1
rustup target add wasm32-wasip1 --toolchain 1.99.1
cargo +1.99.1 install cargo-component --version 0.21.1 --locked
cargo build --locked -p pushit -p pushit-controller
export PUSHIT_REPO="$PWD"
# Абсолютный PATH сохраняет доступ к CLI после перехода в каталог приложения.
export PATH="$PUSHIT_REPO/target/debug:$PATH"
```

**Терминал 1:** из того же корня репозитория запустите контроллер и оставьте
его работать (дождитесь сообщения о прослушивании порта):

```bash
PUSHIT_DEV=1 PUSHIT_BIND=127.0.0.1:9100 PUSHIT_BASE_DOMAIN=localhost \
  PUSHIT_DATA="$(mktemp -d /tmp/pushit-data.XXXXXX)" \
  ./target/debug/pushit-controller
```

Dev-режим отключает аутентификацию: используйте его только локально, не
выставляйте этот контроллер в сеть. Префикс `PUSHIT_DEV=1` выше действует только
на процесс контроллера, не на CLI в другом терминале.

**Терминал 2:** продолжайте в терминале с подготовленным `PATH` из первого
блока. Создайте приложение вне Cargo workspace репозитория:

```bash
set -euo pipefail
# Isolate this sandbox from saved profiles/tokens and inherited auth flags.
export XDG_CONFIG_HOME="$(mktemp -d /tmp/pushit-config.XXXXXX)"
unset PUSHIT_TOKEN PUSHIT_PROFILE PUSHIT_DEV PUSHIT_SERVER
APP_PARENT="$(mktemp -d /tmp/pushit-app.XXXXXX)"
cd "$APP_PARENT"
# rustup override репозитория не действует вне него.
export RUSTUP_TOOLCHAIN=1.99.1
pushit init myapp
cd myapp
pushit build && pushit --dev --server http://127.0.0.1:9100 deploy

curl -fsS -H 'Host: myapp.localhost' http://127.0.0.1:9100/ | grep -F '<h1>myapp</h1>'
curl -fsS -H 'Host: myapp.localhost' http://127.0.0.1:9100/_fn/hello | grep -Fx '{"msg":"hello"}'
pushit --dev --server http://127.0.0.1:9100 status myapp
pushit --dev --server http://127.0.0.1:9100 logs myapp
```

Ожидается UI с заголовком `myapp`, ответ функции `{"msg":"hello"}` и активная
ревизия в `status`. Scaffold не требует секретов (`secrets: []`). Для одного
приложения `deploy` **не запускает сборку**: после изменений повторяйте
`pushit build && pushit --dev --server http://127.0.0.1:9100 deploy`.
Не считайте деплой успешным при ненулевом коде выхода или неуспешных проверках;
проверьте вывод команды и логи, исправьте причину и повторите проверку.
При ошибке доверия TLS при загрузке зависимостей не отключайте проверку
сертификатов: согласуйте доверенный CA и доступ к реестру с оператором через
поддерживаемую конфигурацию инструмента. Пока загрузка/сборка заблокирована,
так и сообщайте — это не успешный деплой.
Dev-проверки не проверяют production OIDC/auth. `--server` сам по себе не
отключает токен активного профиля: временный `XDG_CONFIG_HOME` и `unset` выше
изолируют CLI от production-учётных данных. Выполняйте этот sandbox в отдельном
терминале; закройте его перед возвратом к обычным профилям.
Остановить контроллер можно через Ctrl-C; временные каталоги оставлены для осмотра.

### Аутентификация и профили

```bash
pushit login    # один раз: браузер → Keycloak → долгоживущий токен (~1 год)
```

Токен хранится в `~/.config/pushit/token` (0600) и используется автоматически;
приоритет токена: `--token` > `PUSHIT_TOKEN` > непустой токен выбранного/активного
профиля > сохранённый legacy-токен. `--dev` — отдельный флаг dev-заголовка,
а не источник токена; он может отправляться одновременно с Bearer-токеном.
На production не используйте `--dev`: укажите сервер и выполните login
для нужного профиля.
Ротация — повторный `pushit login`.

Несколько инстансов — профили (файл `~/.config/pushit/profiles.toml`, 0600):

```bash
pushit profile add prod --server https://pushit.example.com
pushit login --profile prod        # флоу аутентификации для этого профиля
pushit profile use prod            # активировать (все команды идут в него)
pushit --profile stage list        # одна команда другим профилем, без переключения
pushit profile list | show stage | remove prod
```

### Локальная отладка без контроллера

```bash
pushit dev            # в каталоге приложения: сборка + рантайм на http://127.0.0.1:9101
pushit dev --env KEY=value --dsn db='postgres://…' --http-out
```

Тот же wasmtime-рантайм, что и на платформе (wasi:keyvalue/sql/blobstore/logging,
`local` key-value store, таймауты). Секреты локально передаются через `--env`,
DSN sql-подключений — через `--dsn имя=DSN`.

## Структура

- `controller/` — Rust-хост: wasmtime, роутинг, OIDC, SQLite, хаб
- `cli/` — `pushit` CLI
- `wit/` — `pushit:app` WIT-world (`wasi:http/incoming-handler`)
- `examples/hello/` — эталонное приложение
- `infra/` — docker-compose (Caddy + Keycloak), realm
- `scripts/e2e.sh` — end-to-end проверка

## Данные

Встроенная БД приложения — NoSQL: `wasi:keyvalue@0.2.0-draft` (`store`), embedded
redb, один файл на приложение `$PUSHIT_DATA/appdata/<app>/local.kv`. Доступ:
`capabilities.kv: [local]`; значения — байты (JSON по конвенции), запросов нет —
только ключи и пагинация `list-keys`. Данные переживают ревизии и rollback.
См. `docs/kv.md`.

Внешние БД: `wasi:sql@0.2.0-draft` — SQL через хост-функции. Доступ:
`capabilities.sql` в pushit.yaml (логические имена). Откуда DSN — решает деплоер
в root-манифесте (`sql:`: `secret:<ключ>` — секрет приложения,
`platform:<имя>` — общий платформенный секрет). Драйверы: postgres/mysql/sqlite
через sqlx, ClickHouse через HTTP-эндпоинт. См. `docs/sql.md`.
Также `wasi:blobstore@0.2.0-draft` (opendal: s3/fs, `capabilities.blobstore` +
`$PUSHIT_DATA/blobstores.yaml`, см. `docs/blobstore.md`) и
`wasi:logging@0.1.0-draft` (структурный лог в per-app файл, `docs/logging.md`).

## Безопасное обновление

Перед обновлением существующей установки прочитайте
[изменения авторизации, ключей сессий и rollback](docs/security-upgrade.md).
Production теперь требует явно заданный надёжный `PUSHIT_SESSION_KEY`;
исторические ревизии без сохранённого manifest нельзя безопасно откатывать.

### Кэш репозиториев корневого манифеста

Для приложений корневого манифеста `ref` задаёт ветку или тег, не SHA коммита.
Если ветка и тег имеют одинаковое имя, выбирается ветка — как при первом
клонировании, так и при повторной синхронизации. Без `ref` используется удалённый `HEAD`.

Настроенный `origin` кэша должен точно совпадать с `repo` в манифесте.
Обычные локальные пути перед клонированием и сравнением преобразуются в абсолютные;
для URL и scp-подобных адресов эквивалентность путей, учётных данных и протоколов
не угадывается. При несовпадении ошибка возникает до fetch/reset; PushIt не удаляет
и не перенастраивает кэш автоматически. Проверьте `.pushit-cache/<ключ-приложения>`
рядом с корневым манифестом, сохраните локальную работу, переместите каталог
в сторону и повторите команду для клонирования нового репозитория. Либо после
проверки восстановите правильный URL в манифесте. Диагностика Git при ошибках
синхронизации не выводится: она может содержать учётные данные из URL.

## Версии приложения

Каждый `pushit deploy` создаёт неизменяемую версию и сразу делает её активной.
ID версии — существующий ID ревизии, не Git SHA; миграция не нужна.
У приложения может быть одна активная версия или ни одной:

```sh
pushit version list hello
pushit version activate hello r123   # любой существующий ID из списка
pushit version deactivate hello     # функции, UI и статика больше не обслуживаются (404)
pushit version activate hello r123   # повторная активация сохранённой версии
pushit version delete hello r122    # только неактивная версия; есть подтверждение
# --yes пропускает подтверждение удаления
```

Удаление активной версии отклоняется (409): сначала активируйте другую или
деактивируйте приложение. Если версию ещё использует выполняющийся запрос,
удаление возвращает 409 — повторите после его завершения. Другие приложения и
версии не блокируются. Можно удалить и старую неактивную ревизию без снимка
манифеста; активировать её без повторного деплоя нельзя.

Активация атомарно переключает WASM, UI, env/capabilities и домен из снимка.
**Данные БД/файлы и значения секретов не откатываются**; используются текущие
данные и значения по ссылкам из конфигурации версии. Деактивация и удаление
версии сохраняют данные/секреты приложения и остальные версии.
`pushit rollback hello [--revision ID]` и история ревизий сохраняют прежнюю совместимость.
В хабе доступны те же действия (с подтверждением удаления/деактивации).
Контракт и ошибки API: [API.md](API.md#immutable-app-versions).
