# Standalone local owner authentication (Stage 1)

Run the **whole existing controller** on your computer without Keycloak or Caddy. This is not `pushit dev`: deployments, immutable versions, secrets, logs, the hub, and application subdomains still use the controller and its existing data directory.

## First use

Build the workspace (`cargo build --workspace --locked`). On the controller computer, initialize your owner once:

```sh
./target/debug/pushit auth setup --data-dir ./pushit-data
# Password and confirmation are hidden terminal prompts (minimum 12 bytes).

PUSHIT_AUTH_MODE=local PUSHIT_DATA=./pushit-data \
  PUSHIT_BIND=127.0.0.1:9100 ./target/debug/pushit-controller
```

Open **http://hub.localhost:9100** in a browser. Enter the owner password only on the hub's login page. Apps retain addresses such as `http://hello.localhost:9100`; their login redirects to the hub, never an app-origin password form. Modern browsers resolve `*.localhost` locally; the CLI explicitly resolves its local hub to loopback without using an HTTP proxy.

Configure the CLI once:

```sh
./target/debug/pushit profile add local --server http://hub.localhost:9100
./target/debug/pushit profile use local
./target/debug/pushit login --local
```

Then run the existing deploy golden path from your built app directory, using the installed CLI (or an absolute path to the built `pushit` binary) and this profile. The platform repository itself is not an app deploy directory.

`pushit login` also detects local mode on a configured `http://hub.localhost:<port>` server. `--local` makes the intended flow explicit. All existing deploy/version/secret/log commands remain available. Use `--server http://hub.localhost:9100` if not using profiles; the legacy default bare-domain server is unchanged for compatibility.

For automation, `auth setup --password-stdin` and `login --local --password-stdin` read exactly one nonempty UTF-8 password line. Pipe from a protected secret source; **do not put passwords in command arguments, shell history, a manifest, or a URL**. A password argument or HTTP bootstrap endpoint does not exist. Setup cannot replace an existing owner.

## Boundaries and storage

- Set `PUSHIT_AUTH_MODE=local` explicitly. Stage 1 accepts only `PUSHIT_BASE_DOMAIN=localhost` (the default) and a nonzero port bound to `127.0.0.1` or `::1`. Public binds, custom domains, reverse proxies, and public TLS are unsupported and rejected rather than weakening authentication.
- Do not set `PUSHIT_DEV=1` or any `PUSHIT_OIDC_*` setting in local mode. Dev bypass is never implied by missing OIDC configuration. Existing implicit OIDC/dev configurations retain their behavior; explicit `oidc` requires issuer/client ID. The existing OIDC session key requirements remain unchanged. Local opaque credentials do not use or require `PUSHIT_SESSION_KEY`.
- One owner has full platform administration rights. This is not multi-user identity, per-app role management, or production authentication. Processes running as your operating-system user remain trusted.
- The owner's salted Argon2id password hash and hashed opaque credentials live in private `PUSHIT_DATA/local-auth.db` (0600). No plaintext passwords, session values, or token values are persisted there. Retain and protect this file with the data directory: restarts preserve authentication and revocations. There is no remote password-reset or owner-replacement endpoint.
- Hub and app sessions last at most seven days. Cookies are host-only, HttpOnly, and SameSite=Lax; HTTP loopback cannot use Secure cookies. Sessions are also bound in the database to their exact host **and port** and to their purpose. An app session cannot administer the hub, authenticate another app, or act as a bearer token.
- App authentication uses a browser challenge and a separate 120-second, one-use handoff grant bound to the active registered app authority and a safe local return path. The handoff grant is **not** an API bearer token and requires the app-origin browser challenge. Grant responses are `no-store`/`no-referrer`; grants are never sent to WASM handlers. This deliberately avoids wildcard administrative cookies.
- Management APIs accept local credentials only on the hub authority. Cookie-authenticated API mutations require exact hub `Origin` and `X-PushIt-CSRF: 1`. Unsafe app requests authenticated by app cookies require exact app `Origin`, preventing same-site sibling-app CSRF. No CORS permission is granted. Authentication cookies, authorization headers, and forged `X-User-*` headers are removed before forwarding to local-mode WASM; unrelated app cookies remain available.
- Password verification runs off the async executor, with at most two concurrent verifications and ten total attempts (including successful logins) per five-minute window (persisted across restarts). Failed login returns a generic rejection; wait for the window after hitting the limit.

## Logout and API tokens

Visit `/auth/logout` on the hub or an app to confirm sign-out. Logout requires a same-origin POST and a session-bound CSRF value; GET alone cannot invalidate credentials. Hub logout revokes that hub session and all app sessions and pending handoffs linked to it. App logout revokes only that app session. Another independently logged-in hub session is not logged out. API tokens are independent and must be revoked separately.

Local CLI login issues a **30-day** all-admin token and saves it to the selected profile (or legacy private token file). Tokens travel only in an HTTP JSON response and subsequent Authorization headers, not URL callbacks, browser history, or terminal output. Re-login creates a new token; it does not silently revoke older tokens.

```sh
pushit auth tokens                 # IDs and expiry times only
pushit auth revoke <token-id>       # immediate, durable revocation
```

Bearer tokens cannot mint more tokens. A logged-in hub browser can POST `/api/tokens` with the required Origin/CSRF header; terminal login requires the owner password. `GET /api/tokens` and `DELETE /api/tokens/{id}` also support existing valid bearer credentials. There is no local-mode `/auth/device` URL-token flow.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked
cargo test --workspace --locked
python3 scripts/local-auth-e2e.py
```

The last check uses only temporary synthetic owner data, a loopback controller, and the built CLI. Existing OIDC/security/version/portability smoke tests remain applicable; nothing here deploys to production.
