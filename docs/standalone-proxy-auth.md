# Standalone owner auth behind an HTTPS reverse proxy

Run the **whole PushIt platform** (hub, app subdomains, deploys, versions,
secrets, logs, WASM) without Keycloak or a mandatory Caddy installation.
`PUSHIT_AUTH_MODE=builtin` uses the same single-owner password and revocable opaque
credentials as [local owner auth](standalone-local-auth.md), but requires an
operator-managed **same-host HTTPS reverse proxy**. This is not `PUSHIT_DEV=1`.
There is no embedded TLS/ACME, external identity provider, SSO, RBAC, or path-prefix
hosting. This guide describes a deployment contract, not a production deployment.

## Configure the controller

Build with `cargo build --workspace --locked`, then initialize on the controller
computer (once, hidden password prompts; minimum 12 bytes):

```sh
./target/debug/pushit auth setup --data-dir /path/to/private/pushit-data

PUSHIT_AUTH_MODE=builtin \
PUSHIT_DATA=/path/to/private/pushit-data \
PUSHIT_BIND=127.0.0.1:9100 \
PUSHIT_BASE_DOMAIN=apps.example.com \
PUSHIT_EXTERNAL_ORIGIN=https://apps.example.com \
  ./target/debug/pushit-controller
```

`PUSHIT_EXTERNAL_ORIGIN` is the fixed **base** origin, not the hub URL. It must be
exactly `https://<PUSHIT_BASE_DOMAIN>[:port]`: lowercase ASCII DNS labels, no IP,
localhost, credentials, trailing slash, path, query, fragment or explicit `:443`.
`PUSHIT_BASE_DOMAIN` is required in builtin mode. Nonstandard public ports work:
`https://apps.example.com:8443` gives `https://hub.apps.example.com:8443` and
`https://hello.apps.example.com:8443`, independently of backend port 9100.
The public port is fixed and shared by the hub and all apps.

The backend speaks **HTTP only**, on numeric `127.0.0.1` or `::1` and a nonzero
port. Wildcard binds, other addresses (including `127.0.0.2`), DNS bind names,
public IPs and network-separated proxies are rejected. A container bridge or
proxy on another VM is outside this contract; use a same-host network namespace
rather than exposing the backend. Do not expose or tunnel its port remotely.
Do not set `PUSHIT_DEV` or any `PUSHIT_OIDC_*` setting. OIDC and existing local-only
mode remain separate. No `PUSHIT_SESSION_KEY` is needed for opaque owner auth.

## DNS, certificate and proxy contract

Arrange DNS for `hub.apps.example.com` and `*.apps.example.com` to reach your
proxy. Obtain/install a normally trusted certificate covering the hub and app
names (a wildcard for `*.apps.example.com` covers the hub and single-label app routes).
Dotted manifest routes, such as `team.api`, produce
`team.api.apps.example.com`: provision explicit/additional wildcard certificate
names and matching DNS for those deeper hosts; one wildcard does not cover them. If using an internal CA,
install that CA in the browser/OS/CLI trust store. Never disable TLS validation.
Certificate issuance/renewal, DNS, firewalling and proxy operation are the
operator's responsibility; no public certificate or paid resource is created by
PushIt. The base name itself does not serve a hub login.

Any HTTPS reverse proxy is suitable if it:

1. Terminates trusted HTTPS on exactly the configured public port, and forwards
   to the configured same-host numeric loopback HTTP socket.
2. Preserves the **external `Host` authority including a nondefault port**.
   Do not replace it with the upstream host, strip its port, or hardcode the hub
   Host for all requests. Default 443 is represented without `:443`.
3. Preserves paths, query strings, cookies and `Origin`; does not rewrite cookie
   Domain/Path, redirects, or authentication routes. No path prefix is supported.
4. Supports deployment body sizes and long-lived log streaming; disables response
   buffering for SSE and uses an appropriate read timeout.
5. Does not log passwords, Authorization, cookies or query-string handoff grants.
   Configure access logs with the path **without query strings**, or exclude
   auth endpoints. Apply the same restrictions to upstream/load-balancer logs.
6. Rejects unknown public hosts and plaintext traffic, and never caches auth
   responses. Do not redirect credential-bearing requests between authorities.

The controller **ignores `Forwarded` and all `X-Forwarded-*` headers** for security
decisions. Host validation, redirects, cookie security and exact Origin checking
come only from startup configuration. A forged forwarding header cannot enable
TLS, change a redirect, select another host/port, or bypass CSRF. The controller
rejects missing/duplicate/noncanonical Host and duplicate/wrong Origin headers.
It cannot prove that a loopback request traversed TLS: local processes and the
proxy are trusted, and this is not isolation against an untrusted same-host user.

### Tested nginx recipe

This recipe was exercised by `scripts/proxy-auth-e2e.py` using nginx 1.28.0,
a temporary synthetic CA, a real HTTPS client and a separate loopback backend.
Use your actual certificate paths and domain; the example public port is 443.
For public 8443, change `listen` **and** `PUSHIT_EXTERNAL_ORIGIN` to 8443;
`$http_host` retains that port.

```nginx
# Inside http {}; do not include $request, $args, cookies or Authorization.
log_format pushit_safe '$host $request_method $uri $status';

server {
    listen 443 ssl default_server;
    server_name _;
    ssl_certificate     /etc/nginx/tls/fullchain.pem;
    ssl_certificate_key /etc/nginx/tls/privkey.pem;
    return 444;
}
server {
    listen 443 ssl;
    server_name hub.apps.example.com *.apps.example.com;
    ssl_certificate     /etc/nginx/tls/fullchain.pem;
    ssl_certificate_key /etc/nginx/tls/privkey.pem;
    access_log /var/log/nginx/pushit.log pushit_safe;
    client_max_body_size 128m;

    location / {
        proxy_pass http://127.0.0.1:9100;
        proxy_set_header Host $http_host;
        proxy_set_header Forwarded "";
        proxy_set_header X-Forwarded-Host "";
        proxy_set_header X-Forwarded-Proto "";
        proxy_set_header X-Forwarded-Port "";
        proxy_buffering off;
        proxy_read_timeout 3600s;
    }
}
server {
    listen 80 default_server;
    server_name _;
    return 444;  # do not accept plaintext password/API requests
}
```

Validate with `nginx -t` before reloading. The default reject server deliberately
avoids rewriting unknown hosts into a valid hub request. The controller is a
second authority check, not a substitute for the proxy configuration.

## Browser and CLI use

Open **https://hub.apps.example.com**. Enter the owner password **only at this
hub authority**. An app such as `https://hello.apps.example.com` hands off to the
hub, then completes a 120-second, single-use, app-bound browser challenge. No
password form is served on app hosts. Handoff grants are not API bearer tokens.

```sh
pushit profile add standalone --server https://hub.apps.example.com
pushit profile use standalone
pushit login --builtin
# pushit login also automatically detects builtin mode on a valid HTTPS hub URL.
```

`--builtin` uses a hidden password prompt and normal HTTPS certificate/hostname
validation. `--password-stdin` reads one password line from a protected source;
never put a password in arguments, URLs, shell history or a manifest.
Remote plaintext owner login is rejected **before** sending the password.
Password and management bearer requests never follow HTTP redirects, including
307/308 credential-preserving redirects. There is no insecure TLS flag.
`--local` remains restricted to the original HTTP `hub.localhost` flow.

The CLI receives a 30-day token in JSON, saves it privately to its profile, and
uses existing deploy/version/secret/log commands unchanged. Tokens never appear
in callback URLs or normal terminal output. `pushit auth tokens` lists only IDs
and expiries; `pushit auth revoke <id>` durably revokes a token. Tokens cannot
mint tokens and cannot authenticate app UI/function routes.

## Isolation, persistence, and changing domains

One owner is a full platform admin; this is not multi-user authorization.
`PUSHIT_DATA/local-auth.db` (0600) persists Argon2id owner hashes, hashed credentials,
revocations and bounded login-attempt history. Protect/back up the whole data
directory. Setup cannot replace an owner; no remote reset/bootstrap exists.

HTTPS owner/app/challenge cookies are `Secure`, `HttpOnly`, `SameSite=Lax`,
`Path=/`, **host-only `__Host-` cookies**. Sibling apps cannot set valid Domain
cookies with these names. Database credentials are also purpose- and exact
host/port-bound: app cookies cannot administer the hub or authenticate another
app, even if copied manually. Builtin API tokens are bound to the configured hub
host/port. Cookie-authenticated API mutations require the hub's exact HTTPS
Origin and `X-PushIt-CSRF: 1`; unsafe authenticated app requests require the exact
app Origin, so same-site sibling origins do not bypass CSRF. There is no CORS
grant. Before WASM dispatch the controller strips platform cookies, Authorization
and spoofed X-User-* headers, then supplies its verified owner identity.

Hub logout requires a same-origin, CSRF-bound POST and revokes the hub session,
linked app sessions and pending handoffs. App logout revokes only that app session.
Independent API tokens must be revoked separately. Password checking retains the
local mode's two-work-item and ten-attempt/five-minute persisted bounds.

Keep configuration, backend routing and DNS/certificates consistent. When changing
base domain or public port, stop the controller, update the proxy/DNS/certificates
and both domain/origin settings, restart, update CLI profile server, and log in
again. Old cookies and builtin tokens do not authenticate the new authority;
revoke old tokens before moving if they should never work after a future revert.
Redeploy every app under the new base domain: domains are stored in immutable
revision snapshots and do **not** migrate automatically. Do not activate an old
domain revision expecting it to move; redeploy that code instead. Old-domain
active revisions are not accepted as new-domain app hosts. App databases, files
and secret values remain unchanged by redeploy or domain changes.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked
cargo test --workspace --locked
cargo build --workspace --locked
cargo build --manifest-path examples/hello/fn/Cargo.toml --release --target wasm32-wasip2 --locked
python3 scripts/local-auth-e2e.py
python3 scripts/proxy-auth-e2e.py  # nginx and openssl required
```

Rust integration tests simulate proxy HTTP backend requests at both default 443
and a distinct nonstandard external port; they cover host/port/Origin spoofing,
duplicate headers, forwarding metadata, cookie isolation, token revocation and
local/OIDC regressions. The separate nginx smoke performs **real TLS termination**
with a trusted synthetic CA, checks untrusted-certificate/plaintext/credential-
redirect rejection, deploys real WASM via CLI, verifies the browser-protocol handoff and Secure
cookies, credential stripping, sibling-app separation, versions, restart persistence and token revocation. It uses temporary
data and a process-local CONNECT tunnel for synthetic DNS, not public DNS,
`/etc/hosts`, real credentials or a production deployment. The HTTP test client
checks cookie attributes and explicit cross-host rejection; it is not a browser
engine test of cookie-prefix enforcement. `NGINX`,
`CARGO_TARGET_DIR` and `PUSHIT_TEST_WASM` may select local test tools/artifacts.
