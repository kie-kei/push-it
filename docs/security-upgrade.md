# Обновление авторизации и ревизий

## Перед обновлением

1. Остановите контроллер и сохраните согласованную резервную копию всего
   `PUSHIT_DATA`: SQLite, каталоги артефактов, `age.key` и данные приложений.
   Сохраните внешние конфигурации и секреты отдельно. Не публикуйте резервные копии.
2. Для production задайте `PUSHIT_DEV=0` и постоянный `PUSHIT_SESSION_KEY`.
   Ключ должен содержать не менее 32 байт; генерируйте его криптографически случайно,
   например `openssl rand -hex 32`, и храните в секретах окружения, не в Git.
   Отсутствующий, пустой, слишком короткий ключ и известный
   `pushit-dev-session-key` теперь останавливают запуск вне dev-режима.
3. Настройте OIDC для браузерного входа. Отсутствие OIDC больше не означает
   вход от имени Dev User. Без OIDC production принимает только уже действительные
   сессии/токены; `/auth/login`, hub и device flow не предоставляют анонимный доступ.
4. Проверьте разрешённые redirect URI у провайдера: HTTPS в production,
   HTTP для явно настроенного локального base domain (`localhost`, `127.0.0.1`, `::1`).
   Провайдер должен поддерживать PKCE S256. Не полагайтесь на клиентские
   `X-Forwarded-*` заголовки для выбора схемы.

`PUSHIT_DEV=1` — явное серверное разрешение доступа без аутентификации.
Оно предназначено только для доверенной локальной среды. CLI `--dev` и заголовок
`X-Pushit-Dev: 1` сами по себе больше не дают доступ к production.

При сохранении ключа существующие корректно подписанные сессии и CLI-токены
работают после перезапуска. Замена ключа отзывает **все** такие сессии и токены:
пользователям нужно снова выполнить `pushit login`. Если ранее production работал
с fallback-ключом, замените его и считайте старые токены недоверенными.
Это изменение не устанавливает, был ли ваш инстанс скомпрометирован.

## OIDC

Каждый вход создаёт одноразовый `state`, действующий пять минут, и независимый
случайный браузерный идентификатор в host-only cookie (`HttpOnly`, `SameSite=Lax`,
`Path=/`; `Secure` и префикс `__Host-` для HTTPS). Callback проверяет наличие, срок,
браузерную привязку и происхождение challenge до обмена кода. Повторное использование
не допускается. Код обменивается с сохранённым PKCE verifier (`S256`).

Незавершённые входы хранятся в памяти одного процесса: перезапуск их отменяет.
Начните вход заново после перезапуска или истечения пяти минут. Параллельные входы
в одном браузере могут заменить cookie предыдущего входа; используйте последний.

## Деплой, rollback и миграция

Новая ревизия сохраняет неизменяемый снимок manifest и domain вместе с путём
к артефактам. Деплой публикует запись приложения, ревизию и её активацию одной
SQLite-транзакцией. Rollback использует ту же гарантию: предыдущая активная
ревизия остаётся активной при ошибке переключения. Уникальный частичный индекс
не допускает две активные ревизии одного приложения.

При rollback вместе восстанавливаются WASM, static dist/config.js, env,
ссылки на secrets, capabilities, SQL bindings и домен выбранной ревизии.
Обработка одного запроса использует один согласованный снимок активной ревизии.
При переключении запросы, уже начавшиеся на старой ревизии, могут завершиться
на ней; разные запросы, пересекающие переключение, могут видеть разные ревизии.

Для старой базы миграция может восстановить manifest/domain **только текущей
активной ревизии** из `apps`. Исторические неактивные ревизии не содержат этих
данных: rollback к ним отклоняется, а не смешивает старый код с новой конфигурацией.
Если до обновления уже выполнялся rollback, старый баг мог смешать текущий manifest
с другой ревизией WASM. Миграция не может восстановить утраченную историю:
проверьте текущую конфигурацию и при необходимости повторите деплой из исходников.
Если нужен такой rollback, заново задеплойте исходные артефакты с известным
историческим manifest. Не подставляйте текущий manifest в старые ревизии вручную.

### Что НЕ откатывается

- Значения app/global secrets остаются текущими. Снимок хранит их **имена и ссылки**,
  а не значения; например, старый manifest с `secret:DB` получает текущее значение
  `DB`, а не пароль на момент своего деплоя.
- Содержимое SQLite/Postgres/других баз, blobstores, файловые данные приложений,
  общая конфигурация платформы и внешние побочные эффекты не откатываются.
- Миграции схем данных автоматически не отменяются. Перед rollback проверьте
  совместимость старого приложения с текущими данными и текущими секретами.

Храните нужные секреты и bindings доступными, пока возможен rollback к использующим
их ревизиям. Не удаляйте каталоги исторических артефактов до окончания этого периода.

## Проверки

`cargo test -p pushit-controller` включает production-auth регрессии
(`controller/tests/security.rs`), OIDC state/PKCE и проверки транзакций/миграции.
`scripts/e2e.sh` сохраняет отдельный dev happy path с реальным WASM-компонентом;
одного dev smoke test недостаточно для проверки production-аутентификации.

Отдельный локальный smoke проверяет отказ production-запуска без ключа, поддельный
header и реальный WASM deploy A → B → rollback A с изменением маршрута/config.js:

```bash
rustup target add wasm32-wasip2
cargo build --workspace --locked
cargo build --manifest-path examples/hello/fn/Cargo.toml --release --target wasm32-wasip2
python3 scripts/security-e2e.py
```

Он использует временный каталог и loopback-порт; production-инстансы не затрагивает.
SQL pools учитывают текущий DSN (по хешу), поэтому после ротации секрета rollback
не возвращает прежние credentials. Уже выполняющиеся запросы могут закончиться
на ранее открытом соединении.

### Полный стек: Caddy + Keycloak + controller

Статическая проверка конфигурации без Docker и сторонних Python-зависимостей:

```bash
python3 scripts/test-oidc-config.py
```

Она проверяет передачу одного `PUSHIT_OIDC_CLIENT_SECRET` в controller и Keycloak
и placeholder секрета в импортируемом realm; это не проверка реального входа.
В Compose переменная остаётся необязательной, чтобы не требовать OIDC-секрет для
controller-only dev. Для `--profile edge` с production-аутентификацией обязательно
задайте непустой случайный `PUSHIT_OIDC_CLIENT_SECRET` вне Git: Keycloak подставляет
его из **своего** окружения при импорте `realm-pushit.json`.

Для запуска нужны Docker Engine с Compose v2, доступ к образам и сборке,
браузер и тестовые DNS/HTTPS-домены. Из корня репозитория подготовьте локальный
файл окружения (не перезаписывайте существующий), заполните параметры ниже,
затем запустите стек:

```bash
# Только если infra/.env ещё не существует:
cp -n infra/.env.example infra/.env
# Заполните infra/.env перед запуском; не добавляйте его в Git.
docker compose --env-file infra/.env -f infra/docker-compose.yml --profile edge up -d --build
docker compose --env-file infra/.env -f infra/docker-compose.yml --profile edge ps
```

Перед подтверждением полного стека в изолированной тестовой среде:

- Задайте `PUSHIT_DEV=0`, постоянный `PUSHIT_SESSION_KEY`, `KC_ADMIN_PASSWORD`,
  непустой OIDC client secret, issuer realm `pushit` и согласованные hostname,
  base domain, HTTPS redirect URI. Для встроенного realm используйте client ID
  `pushit-platform`; при других доменах обновите его разрешённые redirect URI.
- Проверьте профиль `edge`, DNS/TLS и discovery issuer через Caddy из браузера
  и controller. Не публикуйте вывод `docker compose config`: он содержит секреты.
- На свежем тестовом realm проверьте браузерный login → callback → hub,
  отклонение callback без cookie/с неверным state и повторного callback,
  затем CLI `pushit login` и авторизованный запрос. Без сессии/токена и с одним
  `X-Pushit-Dev: 1` production должен отказывать в доступе.
- Проверьте перезапуск с сохранёнными томами и ключом. Импорт при старте
  **не обновляет уже существующий realm**: при смене client secret согласованно
  обновите client в Keycloak и окружение controller; не удаляйте production-том
  ради повторного импорта.

Этот checklist требует настоящего тестового IdP и TLS. При подготовке этой
поправки реальный полный стек не запускался; успешная статическая проверка
или локальный dev smoke сами по себе не подтверждают полный стек.

#### Точные тестовые домены и доверие TLS

Для штатного `infra/Caddyfile` используйте следующие значения в `infra/.env`:

```dotenv
PUSHIT_DEV=0
PUSHIT_BASE_DOMAIN=apps.internal.lan
PUSHIT_OIDC_ISSUER=https://auth.apps.internal.lan/realms/pushit
PUSHIT_OIDC_CLIENT_ID=pushit-platform
KC_HOSTNAME=auth.apps.internal.lan
```

Секреты из предыдущих пунктов также обязательны. Направьте
`auth.apps.internal.lan`, `hub.apps.internal.lan`, `first.apps.internal.lan` и
`second.apps.internal.lan` на тестовый Caddy; имя issuer должно разрешаться и
из контейнера controller, не только с компьютера браузера.

Caddy использует локальный CA (`local_certs`), поэтому его корневому сертификату
должны доверять браузер, CLI и **TLS-клиент controller**. Корень находится в томе
Caddy по пути `/data/caddy/pki/authorities/local/root.crt`; импортируйте только
проверенный публичный сертификат CA, не приватный ключ. Не отключайте проверку TLS.
Важно: controller собран с reqwest `rustls-tls` (webpki roots); установка CA только
в системное хранилище контейнера не гарантирует доверие этого клиента. До реального
входа обеспечьте поддерживаемое доверие CA в TLS-клиенте controller либо тестовый
issuer с доверенным сертификатом и согласованными доменами/Caddyfile. Пока discovery
из controller не проходит проверку сертификата, полный стек **не подтверждён**.

#### Production: публичный домен и публично доверенный сертификат

Штатный `infra/Caddyfile` — **локальный пример**, а не готовая production-
конфигурация: домены в нём и `infra/realm-pushit.json` заданы явно. Одной смены
`PUSHIT_BASE_DOMAIN` в `.env` недостаточно. Для обычного публичного домена,
например `apps.example.com`, подготовьте отдельную конфигурацию перед запуском:

1. Направьте DNS всех используемых имён на Caddy. Из интернета должны быть
   доступны 80/443 для ACME; issuer должен разрешаться и быть доступен из
   контейнера controller. Публикуйте controller только через reverse proxy:
   удалите внешний port mapping `9100:9100` или ограничьте его loopback
   (`127.0.0.1:9100:9100`) в своей Compose-конфигурации.
2. Замените содержимое смонтированного Caddyfile на явные публичные имена,
   **без** глобального `local_certs` и без wildcard:

   ```caddyfile
   auth.apps.example.com {
       reverse_proxy {$KC_UPSTREAM}
   }

   hub.apps.example.com, first.apps.example.com, second.apps.example.com {
       reverse_proxy controller:9100
   }
   ```

   Caddy получает отдельные публичные сертификаты для перечисленных имён.
   Добавляйте каждое новое app-имя в этот список и перезагружайте Caddy.
   Просто заменить `*.apps.internal.lan` на `*.apps.example.com` нельзя:
   публичный wildcard требует DNS challenge и настроенного DNS-provider module
   либо отдельно управляемого wildcard-сертификата; стандартный образ `caddy:2`
   не содержит произвольных DNS-provider modules. Не ослабляйте TLS-проверку.
3. Согласуйте окружение (реальные секреты храните вне Git):

   ```dotenv
   PUSHIT_DEV=0
   PUSHIT_BASE_DOMAIN=apps.example.com
   PUSHIT_OIDC_ISSUER=https://auth.apps.example.com/realms/pushit
   PUSHIT_OIDC_CLIENT_ID=pushit-platform
   KC_HOSTNAME=https://auth.apps.example.com
   ```

   Также обязательны постоянный случайный `PUSHIT_SESSION_KEY`, непустой
   `PUSHIT_OIDC_CLIENT_SECRET` и `KC_ADMIN_PASSWORD`. Для внешнего Keycloak
   используйте его настоящий публичный issuer и соответствующий upstream.
4. Для свежего realm замените `redirectUris` клиента `pushit-platform` в
   импортируемом JSON на точные callbacks всех разрешённых app-hosts:

   ```json
   "redirectUris": [
     "https://hub.apps.example.com/auth/callback",
     "https://first.apps.example.com/auth/callback",
     "https://second.apps.example.com/auth/callback"
   ]
   ```

   Вход с app-домена возвращает callback на тот же host, а не всегда на hub.
   Для каждого нового app-имени обновляйте и Caddy, и разрешённые callbacks
   клиента Keycloak. Удалите локальные redirect URI. `webOrigins: ["+"]` означает origins
   настроенных redirect URI; не заменяйте его на `"*"`. Для уже существующего
   realm изменяйте client в Keycloak, а не рассчитывайте на повторный импорт.
5. Проверьте discovery issuer из controller с обычной проверкой сертификата,
   затем полный browser/CLI login и авторизованный deploy/rollback по checklist
   выше. Публично доверенная цепочка устраняет необходимость локального CA,
   но сама по себе не подтверждает правильность issuer, redirect URI и login.

В командах следующего раздела замените `apps.internal.lan` на свой согласованный
публичный base domain. Эти инструкции не выполняют deployment автоматически;
реальный публичный full-stack login не покрыт локальным smoke или CI.

Справка: [Caddy automatic HTTPS и ACME challenges](https://caddyserver.com/docs/automatic-https),
[Keycloak: разрешение web origins `+`](https://github.com/keycloak/keycloak/blob/26.0.0/services/src/main/java/org/keycloak/protocol/oidc/utils/WebOriginsUtils.java).

#### Авторизованный deploy A → B → rollback A

Выполняйте только в отдельном тестовом инстансе: команды изменяют приложение
`hello`. Нужны Rust toolchain/target и собранный CLI `pushit` в `PATH`, доверие TLS
и успешно завершённый production OIDC-вход. Здесь используется тот же WASM-fixture
`examples/hello/fn`, что в локальном smoke, но запросы отправляет авторизованный CLI,
без `--dev` и без `X-Pushit-Dev`.

```bash
rustup target add wasm32-wasip2
cargo build --manifest-path examples/hello/fn/Cargo.toml --release --target wasm32-wasip2
export PUSHIT_SERVER=https://hub.apps.internal.lan
pushit login
WORK=$(mktemp -d)
mkdir "$WORK/dist"
printf '<title>revision validation</title>\n' > "$WORK/dist/index.html"
printf 'name: hello\ndomain: first\nenv:\n  VERSION: A\n' > "$WORK/a.yaml"
printf 'name: hello\ndomain: second\nenv:\n  VERSION: B\n' > "$WORK/b.yaml"
WASM="$PWD/examples/hello/fn/target/wasm32-wasip2/release/hello_fn.wasm"
pushit deploy --manifest "$WORK/a.yaml" --wasm "$WASM" --dist "$WORK/dist"
pushit status hello
# Сохраните точный revision ID A из ответа deploy/status:
read -r REV_A
pushit deploy --manifest "$WORK/b.yaml" --wasm "$WASM" --dist "$WORK/dist"
pushit status hello
# Проверьте B в браузере перед rollback, затем:
pushit rollback hello --revision "$REV_A"
pushit status hello
```

После A откройте `https://first.apps.internal.lan/_fn/hello` и `/config.js`:
WASM должен отвечать, config содержать `VERSION: A`. После B на
`https://second.apps.internal.lan/config.js` ожидается `VERSION: B`, а прежний
домен должен возвращать 404. После rollback проверьте восстановление первого
домена, WASM и `VERSION: A`, 404 второго домена, manifest и активную ревизию
через `pushit status hello`. Сессионная cookie общая для поддоменов настроенного
base domain (`Domain=.apps.internal.lan`); при её отсутствии выполните вход
заново. Только временная cookie привязки OIDC-входа является host-only.

Локальный `scripts/security-e2e.py` отдельно проверяет production-отказы, но
его deploy/rollback выполняются в **dev-режиме**; это не production OIDC e2e.
Приведённые выше команды полного стека при подготовке поправки не выполнялись.
