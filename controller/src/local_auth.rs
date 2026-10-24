//! Standalone local/proxied, single-owner authentication. No OIDC wire formats are accepted here.
use crate::{auth::User, config::AuthMode, server::AppState};
use anyhow::{Context, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;
use parking_lot::Mutex;
use rand::{rngs::OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const SESSION_TTL: u64 = 7 * 86400;
const TOKEN_TTL: u64 = 30 * 86400;
const FLOW_TTL: u64 = 120;
const OWNER: &str = "pushit_local_owner";
const APP: &str = "pushit_local_app";
const CHALLENGE: &str = "pushit_local_challenge";
const LOGIN: &str = "pushit_local_login";

pub struct LocalAuth {
    db: Mutex<Connection>,
    secure: bool,
    work: Arc<tokio::sync::Semaphore>,
}
#[derive(Clone)]
struct Session {
    family: String,
    token: String,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn random() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
fn hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
fn owner() -> User {
    User {
        id: "local-owner".into(),
        name: "Owner".into(),
        email: "owner@localhost".into(),
    }
}

impl LocalAuth {
    pub fn open(data: &Path) -> Result<Self> {
        Self::open_mode(data, false)
    }
    pub fn open_mode(data: &Path, secure: bool) -> Result<Self> {
        std::fs::create_dir_all(data)?;
        let path = data.join("local-auth.db");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            anyhow::ensure!(
                std::fs::metadata(&path)?.permissions().mode() & 0o077 == 0,
                "local-auth.db must be private (chmod 600)"
            );
        }
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(10))?;
        // DELETE journal inherits the database's restrictive permissions, unlike a
        // long-lived WAL shared with application state.
        db.execute_batch("PRAGMA journal_mode=DELETE;
          CREATE TABLE IF NOT EXISTS owner(id INTEGER PRIMARY KEY CHECK(id=1), password TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS credentials(hash TEXT PRIMARY KEY, kind TEXT NOT NULL, authority TEXT NOT NULL, family TEXT NOT NULL, expires INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS flows(request TEXT PRIMARY KEY, challenge TEXT NOT NULL, authority TEXT NOT NULL, path TEXT NOT NULL, expires INTEGER NOT NULL, grant_hash TEXT UNIQUE, family TEXT);
          CREATE TABLE IF NOT EXISTS attempts(at INTEGER NOT NULL);
          CREATE INDEX IF NOT EXISTS credential_family ON credentials(family,kind,expires);")?;
        Ok(Self {
            db: Mutex::new(db),
            secure,
            work: Arc::new(tokio::sync::Semaphore::new(2)),
        })
    }
    /// Operator-only bootstrap: never exposed over HTTP and cannot overwrite an owner.
    pub fn setup(data: &Path, password: &str) -> Result<()> {
        anyhow::ensure!(
            password.len() >= 12 && password.len() <= 1024 && !password.trim().is_empty(),
            "password must be 12–1024 bytes and not blank"
        );
        let auth = Self::open(data)?;
        let salt = SaltString::generate(&mut OsRng);
        let encoded = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("password hashing failed: {e}"))?
            .to_string();
        match auth
            .db
            .lock()
            .execute("INSERT INTO owner(id,password) VALUES(1,?1)", [&encoded])
        {
            Ok(_) => (),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                anyhow::bail!("owner already initialized; setup never replaces credentials")
            }
            Err(error) => return Err(error).context("persist owner credentials"),
        }
        Ok(())
    }
    fn verify_password(&self, password: &str) -> Result<bool> {
        anyhow::ensure!(password.len() <= 1024, "invalid credentials");
        let encoded: Option<String> = {
            let mut db = self.db.lock();
            let tx = db.transaction()?;
            tx.execute(
                "DELETE FROM attempts WHERE at<=?1",
                [now().saturating_sub(300)],
            )?;
            let count: u64 = tx.query_row("SELECT count(*) FROM attempts", [], |r| r.get(0))?;
            anyhow::ensure!(
                count < 10,
                "too many login attempts; retry after five minutes"
            );
            tx.execute("INSERT INTO attempts(at) VALUES(?1)", [now()])?;
            let encoded = tx
                .query_row("SELECT password FROM owner WHERE id=1", [], |r| r.get(0))
                .optional()?;
            tx.commit()?;
            encoded
        };
        let encoded = encoded
            .context("owner not initialized; run pushit auth setup on the controller computer")?;
        let parsed =
            PasswordHash::new(&encoded).map_err(|_| anyhow::anyhow!("invalid password store"))?;
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok())
    }
    async fn password(self: &Arc<Self>, value: String) -> Result<bool> {
        let permit = self
            .work
            .clone()
            .try_acquire_owned()
            .context("login busy; retry later")?;
        let auth = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            auth.verify_password(&value)
        })
        .await?
    }
    fn issue(&self, kind: &str, authority: &str, family: &str, ttl: u64) -> Result<String> {
        let token = random();
        let db = self.db.lock();
        db.execute("DELETE FROM credentials WHERE expires<=?1", [now()])?;
        db.execute(
            "INSERT INTO credentials(hash,kind,authority,family,expires) VALUES(?1,?2,?3,?4,?5)",
            params![hash(&token), kind, authority, family, now() + ttl],
        )?;
        Ok(token)
    }
    fn session(&self, headers: &HeaderMap, authority: &str, hub: bool) -> Option<Session> {
        let token = cookie(
            headers,
            &cookie_name(if hub { OWNER } else { APP }, self.secure),
        )?;
        let family = self.db.lock().query_row("SELECT family FROM credentials WHERE hash=?1 AND kind=?2 AND authority=?3 AND expires>?4 AND (kind!='app' OR EXISTS(SELECT 1 FROM credentials AS parent WHERE parent.kind='owner' AND parent.family=credentials.family AND parent.expires>?4))", params![hash(&token),if hub {"owner"} else {"app"},authority,now()], |r| r.get(0)).optional().ok()??;
        Some(Session { family, token })
    }
    pub fn browser_user(&self, headers: &HeaderMap, authority: &str, hub: bool) -> Option<User> {
        self.session(headers, authority, hub).map(|_| owner())
    }
    /// HTTPS tokens are scoped to the configured hub authority; retain the original
    /// authority-free token semantics only for the separate local HTTP mode.
    pub fn bearer_user(&self, headers: &HeaderMap, authority: &str) -> Option<User> {
        let token = headers
            .get(header::AUTHORIZATION)?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")?;
        self.db
            .lock()
            .query_row(
                "SELECT 1 FROM credentials WHERE hash=?1 AND kind='token' AND expires>?2 AND (?3=0 OR authority=?4)",
                params![hash(token), now(), self.secure, authority],
                |_| Ok(()),
            )
            .optional()
            .ok()??;
        Some(owner())
    }
    fn begin(&self, authority: &str, path: &str) -> Result<(String, String)> {
        let request = random();
        let challenge = random();
        let db = self.db.lock();
        db.execute("DELETE FROM flows WHERE expires<=?1", [now()])?;
        let count: u64 = db.query_row("SELECT count(*) FROM flows", [], |r| r.get(0))?;
        anyhow::ensure!(count < 1000, "too many pending app logins");
        db.execute(
            "INSERT INTO flows(request,challenge,authority,path,expires) VALUES(?1,?2,?3,?4,?5)",
            params![
                hash(&request),
                hash(&challenge),
                authority,
                safe_path(path),
                now() + FLOW_TTL
            ],
        )?;
        Ok((request, challenge))
    }
    fn authorize(&self, request: &str, family: &str) -> Result<(String, String)> {
        let grant = random();
        let db = self.db.lock();
        let valid: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM credentials WHERE kind='owner' AND family=?1 AND expires>?2)", params![family, now()], |row| row.get(0))?;
        anyhow::ensure!(valid, "owner session expired");
        let authority: String = db.query_row(
            "SELECT authority FROM flows WHERE request=?1 AND expires>?2 AND grant_hash IS NULL",
            params![hash(request), now()],
            |r| r.get(0),
        )?;
        anyhow::ensure!(db.execute("UPDATE flows SET grant_hash=?1,family=?2 WHERE request=?3 AND grant_hash IS NULL AND expires>?4",params![hash(&grant),family,hash(request),now()])?==1,"login already authorized");
        Ok((authority, grant))
    }
    fn complete(&self, grant: &str, challenge: &str, authority: &str) -> Result<(String, String)> {
        let mut db = self.db.lock();
        let tx = db.transaction()?;
        let (path,family):(String,String)=tx.query_row("SELECT path,family FROM flows WHERE grant_hash=?1 AND challenge=?2 AND authority=?3 AND expires>?4",params![hash(grant),hash(challenge),authority,now()],|r|Ok((r.get(0)?,r.get(1)?)))?;
        // Logout during the handoff invalidates the grant as well.
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM credentials WHERE kind='owner' AND family=?1 AND expires>?2)",params![family,now()],|r|r.get(0))?;
        anyhow::ensure!(valid, "owner session expired");
        tx.execute("DELETE FROM flows WHERE grant_hash=?1", [hash(grant)])?;
        let token = random();
        tx.execute(
            "INSERT INTO credentials(hash,kind,authority,family,expires) VALUES(?1,'app',?2,?3,?4)",
            params![hash(&token), authority, family, now() + SESSION_TTL],
        )?;
        tx.commit()?;
        Ok((path, token))
    }
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut found = None;
    for line in headers.get_all(header::COOKIE) {
        for pair in line.to_str().ok()?.split(';') {
            let Some((key, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if key == name {
                if found.is_some() || value.len() != 43 {
                    return None;
                }
                found = Some(value.to_owned());
            }
        }
    }
    found
}
fn cookie_name(name: &str, secure: bool) -> String {
    if secure {
        format!("__Host-{name}")
    } else {
        name.into()
    }
}
fn set_cookie(s: &AppState, name: &str, value: &str, ttl: u64) -> String {
    let secure = s.cfg.auth_mode == AuthMode::Builtin;
    format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl}{}",
        cookie_name(name, secure),
        if secure { "; Secure" } else { "" }
    )
}
fn safe_path(path: &str) -> &str {
    if path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !path.starts_with("/api")
        && !path.starts_with("/auth")
        && !path.contains('%')
    {
        path
    } else {
        "/"
    }
}
fn safe_hub_return(path: &str) -> &str {
    if path
        .strip_prefix("/auth/local/authorize?request=")
        .is_some_and(|request| {
            request.len() == 43
                && request
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    {
        path
    } else {
        safe_path(path)
    }
}
fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn response(status: StatusCode, body: impl Into<Body>) -> Response {
    Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-store")
        .header("referrer-policy", "no-referrer")
        .header("x-content-type-options", "nosniff")
        .body(body.into())
        .unwrap()
}
fn redirect(url: &str, cookies: Vec<String>) -> Response {
    let mut r = response(StatusCode::SEE_OTHER, Body::empty());
    r.headers_mut()
        .insert(header::LOCATION, url.parse().unwrap());
    for c in cookies {
        r.headers_mut()
            .append(header::SET_COOKIE, c.parse().unwrap());
    }
    r
}
fn denied() -> Response {
    response(StatusCode::FORBIDDEN, "local auth request rejected")
}
fn origin(s: &AppState, headers: &HeaderMap, authority: &str) -> bool {
    headers.get_all(header::ORIGIN).iter().count() == 1
        && headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
            == Some(format!("{}://{authority}", s.cfg.auth_scheme()).as_str())
}
fn authority(headers: &HeaderMap, port: u16) -> Option<String> {
    if headers.get_all(header::HOST).iter().count() != 1 {
        return None;
    }
    let raw = headers.get(header::HOST)?.to_str().ok()?;
    let parsed: http::uri::Authority = raw.parse().ok()?;
    let host = parsed.host();
    if parsed.as_str().contains('@')
        || host != host.to_ascii_lowercase()
        || !(host == "localhost" || host.ends_with(".localhost"))
        || parsed.port_u16().unwrap_or(80) != port
    {
        return None;
    }
    if host.split('.').any(|label| {
        label.is_empty()
            || !label
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    }) {
        return None;
    }
    Some(raw.to_owned())
}
pub fn local_authority(s: &AppState, headers: &HeaderMap) -> Option<String> {
    if s.cfg.auth_mode == AuthMode::Local {
        return authority(headers, s.cfg.auth_port());
    }
    if headers.get_all(header::HOST).iter().count() != 1 {
        return None;
    }
    let raw = headers.get(header::HOST)?.to_str().ok()?;
    let parsed: http::uri::Authority = raw.parse().ok()?;
    let host = parsed.host();
    if !crate::config::valid_dns_name(host)
        || !(host == s.cfg.base_domain || host.ends_with(&format!(".{}", s.cfg.base_domain)))
        || raw != format!("{host}{}", s.cfg.auth_port_suffix())
    {
        return None;
    }
    Some(raw.into())
}
fn hub_authority(s: &AppState) -> String {
    s.cfg.hub_authority()
}
fn approved_app(s: &AppState, authority: &str) -> bool {
    let host = authority.split(':').next().unwrap_or("");
    host != s.cfg.base_domain
        && host != format!("hub.{}", s.cfg.base_domain)
        && s.db.pin_active_by_domain(host).ok().flatten().is_some()
}

/// All local routes pass this gate before the legacy router, including management
/// routes that would otherwise match on every host.
pub async fn gate(State(s): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if !s.cfg.owner_auth() {
        return next.run(req).await;
    }
    let Some(authority) = local_authority(&s, req.headers()) else {
        return denied();
    };
    let hub = authority == hub_authority(&s);
    let path = req.uri().path().to_owned();
    let auth = s.auth.local.as_ref().expect("local auth initialized");
    let headers = req.headers().clone();
    if path.starts_with("/auth/") || path == "/api/tokens" || path.starts_with("/api/tokens/") {
        return handle(&s, auth, &authority, hub, req).await;
    }
    if path.starts_with("/api/") {
        if !hub {
            return denied();
        }
        if auth.browser_user(&headers, &authority, true).is_some() {
            if !matches!(*req.method(), http::Method::GET | http::Method::HEAD)
                && (!origin(&s, &headers, &authority)
                    || headers.get("x-pushit-csrf").and_then(|v| v.to_str().ok()) != Some("1"))
            {
                return denied();
            }
        } else if auth.bearer_user(&headers, &hub_authority(&s)).is_none() {
            return response(StatusCode::UNAUTHORIZED, "unauthorized");
        }
    } else if !hub {
        if !approved_app(&s, &authority) {
            return response(StatusCode::NOT_FOUND, "no active app for this host");
        }
        if !matches!(
            *req.method(),
            http::Method::GET | http::Method::HEAD | http::Method::OPTIONS
        ) && auth.browser_user(&headers, &authority, false).is_some()
            && !origin(&s, &headers, &authority)
        {
            return denied();
        }
    }
    next.run(req).await
}

#[derive(Deserialize, Default)]
struct Query {
    #[serde(default)]
    return_to: String,
    #[serde(default)]
    request: String,
    #[serde(default)]
    grant: String,
}
#[derive(Deserialize)]
struct PasswordForm {
    password: String,
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    return_to: String,
}

async fn handle(
    s: &AppState,
    auth: &Arc<LocalAuth>,
    authority: &str,
    hub: bool,
    req: Request,
) -> Response {
    let result = handle_inner(s, auth, authority, hub, req).await;
    let mut r = match result {
        Ok(r) => r,
        Err(_) => denied(),
    };
    r.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    r.headers_mut()
        .insert("referrer-policy", "no-referrer".parse().unwrap());
    r.headers_mut().insert(
        "content-security-policy",
        "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"
            .parse()
            .unwrap(),
    );
    r
}

async fn handle_inner(
    s: &AppState,
    auth: &Arc<LocalAuth>,
    authority: &str,
    hub: bool,
    req: Request,
) -> Result<Response> {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();
    let headers = req.headers().clone();
    let q: Query = match axum::extract::Query::<Query>::try_from_uri(req.uri()) {
        Ok(q) => q.0,
        Err(error) => return Err(error.into()),
    };
    let session = auth.session(&headers, authority, hub);

    match (method.as_str(), path.as_str()) {
        ("GET", "/auth/mode") if hub => Ok(axum::Json(
            serde_json::json!({"mode": if auth.secure {"builtin"} else {"local"}}),
        )
        .into_response()),
        ("GET", "/auth/login") if !hub => {
            anyhow::ensure!(approved_app(s, authority), "not an active app");
            let (request, challenge) = auth.begin(authority, &q.return_to)?;
            Ok(redirect(
                &format!(
                    "{}://{}/auth/local/authorize?request={request}",
                    s.cfg.auth_scheme(),
                    hub_authority(s)
                ),
                vec![set_cookie(s, CHALLENGE, &challenge, FLOW_TTL)],
            ))
        }
        ("GET", "/auth/login") if hub => {
            let challenge = random();
            let return_to = safe_hub_return(&q.return_to);
            let body=format!("<!doctype html><title>PushIt owner login</title><h1>PushIt owner login</h1><p>Enter your owner password only at {}.</p><form method=post action=/auth/login><input type=hidden name=csrf value=\"{challenge}\"><input type=hidden name=return_to value=\"{}\"><label>Password <input type=password name=password autocomplete=current-password required maxlength=1024></label><button>Sign in</button></form>",html_escape(&hub_authority(s)),html_escape(return_to));
            let mut r = response(StatusCode::OK, body);
            r.headers_mut().insert(
                header::CONTENT_TYPE,
                "text/html; charset=utf-8".parse().unwrap(),
            );
            r.headers_mut().insert(
                header::SET_COOKIE,
                set_cookie(s, LOGIN, &challenge, 300).parse().unwrap(),
            );
            Ok(r)
        }
        ("POST", "/auth/login") if hub => {
            anyhow::ensure!(origin(s, &headers, authority), "wrong origin");
            let bytes = axum::body::to_bytes(req.into_body(), 4096).await?;
            let form: PasswordForm = serde_urlencoded::from_bytes(&bytes)?;
            anyhow::ensure!(
                cookie(&headers, &cookie_name(LOGIN, auth.secure)).as_deref()
                    == Some(form.csrf.as_str()),
                "invalid login challenge"
            );
            anyhow::ensure!(auth.password(form.password).await?, "invalid credentials");
            let family = random();
            let token = auth.issue("owner", authority, &family, SESSION_TTL)?;
            let return_to = safe_hub_return(&form.return_to);
            Ok(redirect(
                return_to,
                vec![
                    set_cookie(s, OWNER, &token, SESSION_TTL),
                    set_cookie(s, LOGIN, "", 0),
                ],
            ))
        }
        ("GET", "/auth/local/authorize") if hub => {
            let Some(session) = session else {
                return Ok(redirect(
                    &format!(
                        "/auth/login?return_to={}",
                        crate::auth::urlencoding(&format!(
                            "/auth/local/authorize?request={}",
                            q.request
                        ))
                    ),
                    vec![],
                ));
            };
            let (target, grant) = auth.authorize(&q.request, &session.family)?;
            anyhow::ensure!(approved_app(s, &target), "app no longer active");
            Ok(redirect(
                &format!(
                    "{}://{target}/auth/local/complete?grant={grant}",
                    s.cfg.auth_scheme()
                ),
                vec![],
            ))
        }
        ("GET", "/auth/local/complete") if !hub => {
            anyhow::ensure!(approved_app(s, authority), "not an active app");
            let challenge = cookie(&headers, &cookie_name(CHALLENGE, auth.secure))
                .context("missing browser challenge")?;
            let (path, token) = auth.complete(&q.grant, &challenge, authority)?;
            Ok(redirect(
                &path,
                vec![
                    set_cookie(s, APP, &token, SESSION_TTL),
                    set_cookie(s, CHALLENGE, "", 0),
                ],
            ))
        }
        ("GET", "/auth/logout") => {
            let session = session.context("not authenticated")?;
            let body=format!("<!doctype html><title>Sign out</title><form method=post action=/auth/logout><input type=hidden name=csrf value=\"{}\"><button>Sign out{}</button></form>",hash(&format!("csrf:{}",session.token)),if hub {" of hub and all linked apps"}else{" of this app"});
            let mut r = response(StatusCode::OK, body);
            r.headers_mut().insert(
                header::CONTENT_TYPE,
                "text/html; charset=utf-8".parse().unwrap(),
            );
            Ok(r)
        }
        ("POST", "/auth/logout") => {
            anyhow::ensure!(origin(s, &headers, authority), "wrong origin");
            let session = session.context("not authenticated")?;
            let bytes = axum::body::to_bytes(req.into_body(), 4096).await?;
            let form: std::collections::HashMap<String, String> =
                serde_urlencoded::from_bytes(&bytes)?;
            anyhow::ensure!(
                form.get("csrf") == Some(&hash(&format!("csrf:{}", session.token))),
                "invalid csrf"
            );
            let db = auth.db.lock();
            if hub {
                db.execute(
                    "DELETE FROM credentials WHERE family=?1 AND kind IN ('owner','app')",
                    [&session.family],
                )?;
                db.execute("DELETE FROM flows WHERE family=?1", [&session.family])?;
            } else {
                db.execute(
                    "DELETE FROM credentials WHERE hash=?1",
                    [hash(&session.token)],
                )?;
            }
            Ok(redirect(
                "/",
                vec![set_cookie(s, if hub { OWNER } else { APP }, "", 0)],
            ))
        }
        ("POST", "/auth/local/token") if hub => {
            anyhow::ensure!(
                headers.get("x-pushit-cli").and_then(|v| v.to_str().ok()) == Some("1")
                    && headers
                        .get(header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        == Some("application/json"),
                "CLI JSON request required"
            );
            anyhow::ensure!(
                !headers.contains_key(header::ORIGIN) || origin(s, &headers, authority),
                "wrong origin"
            );
            let bytes = axum::body::to_bytes(req.into_body(), 4096).await?;
            let form: PasswordForm = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(auth.password(form.password).await?, "invalid credentials");
            let token = auth.issue(
                "token",
                if auth.secure { authority } else { "" },
                "",
                TOKEN_TTL,
            )?;
            Ok(axum::Json(serde_json::json!({"id":hash(&token),"token":token,"expires_at":now()+TOKEN_TTL,"user":"owner@localhost"})).into_response())
        }
        ("POST", "/api/tokens") if hub => {
            anyhow::ensure!(
                session.is_some()
                    && origin(s, &headers, authority)
                    && headers.get("x-pushit-csrf").and_then(|v| v.to_str().ok()) == Some("1"),
                "owner browser required; bearer tokens cannot mint tokens"
            );
            let token = auth.issue(
                "token",
                if auth.secure { authority } else { "" },
                "",
                TOKEN_TTL,
            )?;
            Ok(axum::Json(serde_json::json!({"id":hash(&token),"token":token,"expires_at":now()+TOKEN_TTL,"user":"owner@localhost"})).into_response())
        }
        ("GET", "/api/tokens") if hub => {
            anyhow::ensure!(
                session.is_some() || auth.bearer_user(&headers, &hub_authority(s)).is_some(),
                "unauthorized"
            );
            let db = auth.db.lock();
            let mut stmt = db.prepare(
                "SELECT hash,expires FROM credentials WHERE kind='token' AND expires>?1",
            )?;
            let rows=stmt.query_map([now()],|r|Ok(serde_json::json!({"id":r.get::<_,String>(0)?,"expires_at":r.get::<_,u64>(1)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
            Ok(axum::Json(rows).into_response())
        }
        ("DELETE", p) if hub && p.starts_with("/api/tokens/") => {
            if session.is_some() {
                anyhow::ensure!(
                    origin(s, &headers, authority)
                        && headers.get("x-pushit-csrf").and_then(|v| v.to_str().ok()) == Some("1"),
                    "invalid csrf"
                )
            } else {
                anyhow::ensure!(
                    auth.bearer_user(&headers, &hub_authority(s)).is_some(),
                    "unauthorized"
                )
            }
            let id = p.trim_start_matches("/api/tokens/");
            anyhow::ensure!(
                id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid token ID"
            );
            auth.db.lock().execute(
                "DELETE FROM credentials WHERE hash=?1 AND kind='token'",
                [id],
            )?;
            Ok(response(StatusCode::NO_CONTENT, Body::empty()))
        }
        _ => Ok(response(StatusCode::NOT_FOUND, "not found")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PASSWORD: &str = "synthetic-test-password-only";
    fn headers(name: &str, token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, format!("{name}={token}").parse().unwrap());
        h
    }

    #[test]
    fn bootstrap_is_singleton_argon2id_and_private() {
        let dir = tempfile::tempdir().unwrap();
        assert!(LocalAuth::setup(dir.path(), "").is_err());
        assert!(LocalAuth::setup(dir.path(), "short").is_err());
        LocalAuth::setup(dir.path(), PASSWORD).unwrap();
        assert!(LocalAuth::setup(dir.path(), "replacement-password").is_err());
        let auth = LocalAuth::open(dir.path()).unwrap();
        let encoded: String = auth
            .db
            .lock()
            .query_row("SELECT password FROM owner", [], |r| r.get(0))
            .unwrap();
        assert!(encoded.starts_with("$argon2id$"));
        assert!(!encoded.contains(PASSWORD));
        assert!(auth.verify_password(PASSWORD).unwrap());
        assert!(!auth.verify_password("wrong-password").unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path().join("local-auth.db"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn credentials_are_hashed_typed_bound_expiring_and_revocable_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let auth = LocalAuth::open(dir.path()).unwrap();
        let owner = auth
            .issue("owner", "hub.localhost:9100", "family", SESSION_TTL)
            .unwrap();
        let app = auth
            .issue("app", "one.localhost:9100", "family", SESSION_TTL)
            .unwrap();
        let token = auth.issue("token", "", "", TOKEN_TTL).unwrap();
        let mut bearer = HeaderMap::new();
        bearer.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        assert!(auth.bearer_user(&bearer, "").is_some());
        assert!(auth
            .browser_user(&headers(OWNER, &owner), "hub.localhost:9100", true)
            .is_some());
        assert!(auth
            .browser_user(&headers(APP, &app), "one.localhost:9100", false)
            .is_some());
        for (name, value, authority, hub) in [
            (OWNER, &token, "hub.localhost:9100", true),
            (OWNER, &app, "hub.localhost:9100", true),
            (APP, &app, "two.localhost:9100", false),
            (OWNER, &owner, "hub.localhost:9101", true),
        ] {
            assert!(auth
                .browser_user(&headers(name, value), authority, hub)
                .is_none());
        }
        bearer.insert(
            header::AUTHORIZATION,
            format!("Bearer {owner}").parse().unwrap(),
        );
        assert!(auth.bearer_user(&bearer, "").is_none());
        let mut duplicate = headers(OWNER, &owner);
        duplicate.append(header::COOKIE, format!("{OWNER}={owner}").parse().unwrap());
        assert!(auth
            .browser_user(&duplicate, "hub.localhost:9100", true)
            .is_none());
        let values: Vec<String> = auth
            .db
            .lock()
            .prepare("SELECT hash FROM credentials")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(!values.contains(&token));
        assert!(values.contains(&hash(&token)));
        drop(auth);
        let auth = LocalAuth::open(dir.path()).unwrap();
        bearer.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        assert!(auth.bearer_user(&bearer, "").is_some());
        auth.db
            .lock()
            .execute("DELETE FROM credentials WHERE hash=?1", [hash(&token)])
            .unwrap();
        drop(auth);
        let auth = LocalAuth::open(dir.path()).unwrap();
        assert!(auth.bearer_user(&bearer, "").is_none());
        let expired = auth.issue("token", "", "", TOKEN_TTL).unwrap();
        auth.db
            .lock()
            .execute(
                "UPDATE credentials SET expires=?1 WHERE hash=?2",
                params![now(), hash(&expired)],
            )
            .unwrap();
        bearer.insert(
            header::AUTHORIZATION,
            format!("Bearer {expired}").parse().unwrap(),
        );
        assert!(auth.bearer_user(&bearer, "").is_none());
        auth.db
            .lock()
            .execute(
                "UPDATE credentials SET expires=?1 WHERE kind='owner'",
                [now()],
            )
            .unwrap();
        assert!(auth
            .browser_user(&headers(OWNER, &owner), "hub.localhost:9100", true)
            .is_none());
        assert!(auth
            .browser_user(&headers(APP, &app), "one.localhost:9100", false)
            .is_none());
    }
    #[test]
    fn app_handoff_is_challenge_host_bound_one_use_expiring_and_safe() {
        let dir = tempfile::tempdir().unwrap();
        let auth = LocalAuth::open(dir.path()).unwrap();
        auth.issue("owner", "hub.localhost:9100", "family", SESSION_TTL)
            .unwrap();
        let (request, challenge) = auth
            .begin("one.localhost:9100", "/dashboard?tab=1")
            .unwrap();
        let (_, grant) = auth.authorize(&request, "family").unwrap();
        assert!(auth.authorize(&request, "family").is_err());
        assert!(auth
            .complete(&grant, &random(), "one.localhost:9100")
            .is_err());
        assert!(auth
            .complete(&grant, &challenge, "two.localhost:9100")
            .is_err());
        assert!(auth
            .complete(&grant, &challenge, "one.localhost:9101")
            .is_err());
        let (path, app) = auth
            .complete(&grant, &challenge, "one.localhost:9100")
            .unwrap();
        assert_eq!(path, "/dashboard?tab=1");
        assert!(auth
            .browser_user(&headers(APP, &app), "one.localhost:9100", false)
            .is_some());
        assert!(auth
            .complete(&grant, &challenge, "one.localhost:9100")
            .is_err());
        for path in [
            "//evil.example",
            "/\\evil",
            "https://evil.example",
            "/api/apps",
            "/auth/local/token",
            "/%2f%2fevil",
            "/\r\nx",
        ] {
            let (request, challenge) = auth.begin("one.localhost:9100", path).unwrap();
            let (_, grant) = auth.authorize(&request, "family").unwrap();
            assert_eq!(
                auth.complete(&grant, &challenge, "one.localhost:9100")
                    .unwrap()
                    .0,
                "/"
            );
        }
        let (request, challenge) = auth.begin("one.localhost:9100", "/").unwrap();
        let (_, grant) = auth.authorize(&request, "family").unwrap();
        auth.db
            .lock()
            .execute("UPDATE flows SET expires=?1", [now()])
            .unwrap();
        assert!(auth
            .complete(&grant, &challenge, "one.localhost:9100")
            .is_err());
        let (request, challenge) = auth.begin("one.localhost:9100", "/").unwrap();
        let (_, grant) = auth.authorize(&request, "family").unwrap();
        auth.db
            .lock()
            .execute("DELETE FROM credentials WHERE kind='owner'", [])
            .unwrap();
        assert!(auth
            .complete(&grant, &challenge, "one.localhost:9100")
            .is_err());
    }
    #[test]
    fn attempts_are_bounded_and_authorities_strict() {
        let dir = tempfile::tempdir().unwrap();
        LocalAuth::setup(dir.path(), PASSWORD).unwrap();
        let auth = LocalAuth::open(dir.path()).unwrap();
        for _ in 0..10 {
            assert!(!auth.verify_password("wrong").unwrap());
        }
        assert!(auth.verify_password(PASSWORD).is_err());
        auth.db
            .lock()
            .execute("UPDATE attempts SET at=?1", [now() - 301])
            .unwrap();
        assert!(auth.verify_password(PASSWORD).unwrap());
        for host in [
            "evil.example:9100",
            "hub.localhost.evil:9100",
            "hub.localhost:9101",
            "user@hub.localhost:9100",
            "hub.localhost",
            "HUB.localhost:9100",
            "hub.localhost:9100/path",
        ] {
            let mut h = HeaderMap::new();
            h.insert(header::HOST, host.parse().unwrap());
            assert!(authority(&h, 9100).is_none(), "accepted {host}");
        }
        assert!(crate::config::validate_local("localhost", "127.0.0.1:9100").is_ok());
        for (domain, bind) in [
            ("example.com", "127.0.0.1:9100"),
            ("localhost", "0.0.0.0:9100"),
            ("localhost", "[::]:9100"),
            ("localhost", "localhost:9100"),
            ("localhost", "127.0.0.1:0"),
        ] {
            assert!(crate::config::validate_local(domain, bind).is_err());
        }
    }
}
