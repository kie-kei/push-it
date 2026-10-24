use anyhow::{bail, Context, Result};
use axum::http::HeaderMap;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;

type HmacSha256 = Hmac<Sha256>;

/// Authenticated user propagated to wasm via X-User-* headers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: String,
    pub email: String,
}

impl User {
    pub fn dev() -> Self {
        User {
            id: "dev".into(),
            name: "Dev User".into(),
            email: "dev@localhost".into(),
        }
    }
}

const LOGIN_TTL: Duration = Duration::from_secs(300);

struct PendingLogin {
    created: Instant,
    browser: String,
    return_to: String,
    redirect_uri: String,
    verifier: String,
}

pub struct Auth {
    pub cfg: Config,
    /// In-memory, short-lived one-time login challenges; restart invalidates pending logins.
    states: Mutex<HashMap<String, PendingLogin>>,
    /// cached JWKS keys: kid -> DecodingKey
    jwks: Mutex<HashMap<String, jsonwebtoken::DecodingKey>>,
    http: reqwest::Client,
}

impl Auth {
    pub fn new(cfg: Config) -> Self {
        Auth {
            cfg,
            states: Mutex::new(HashMap::new()),
            jwks: Mutex::new(HashMap::new()),
            http: reqwest::Client::new(),
        }
    }

    pub fn dev_bypass(&self, _headers: &HeaderMap) -> bool {
        self.cfg.dev
    }

    pub fn oidc_enabled(&self) -> bool {
        self.cfg.oidc_issuer.is_some() && self.cfg.oidc_client_id.is_some() && !self.cfg.dev
    }

    // ---- session cookie (HMAC-signed) ----

    pub fn sign_session(&self, u: &User) -> String {
        self.sign_session_exp(u, 7 * 86400)
    }

    /// Long-lived bearer token for the CLI (same wire format as a session).
    /// One year: long enough not to annoy, short enough that a leaked token
    /// is not eternal; rotate by re-running `pushit login`.
    pub fn sign_api_token(&self, u: &User) -> String {
        self.sign_session_exp(u, 365 * 86400)
    }

    fn sign_session_exp(&self, u: &User, ttl_secs: u64) -> String {
        let exp = now() + ttl_secs;
        let payload = format!("{exp}|{}|{}|{}", u.id, u.name, u.email);
        let mut mac = HmacSha256::new_from_slice(&self.cfg.session_key).unwrap();
        mac.update(payload.as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{payload}|{sig}"))
    }

    pub fn verify_session(&self, cookie: &str) -> Option<User> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(cookie)
            .ok()?;
        let s = String::from_utf8(raw).ok()?;
        let (payload, sig) = s.rsplit_once('|')?;
        let mut mac = HmacSha256::new_from_slice(&self.cfg.session_key).unwrap();
        mac.update(payload.as_bytes());
        mac.verify_slice(&hex::decode(sig).ok()?).ok()?;
        let mut it = payload.splitn(4, '|');
        let exp: u64 = it.next()?.parse().ok()?;
        if exp < now() {
            return None;
        }
        Some(User {
            id: it.next()?.into(),
            name: it.next()?.into(),
            email: it.next()?.into(),
        })
    }

    pub fn cookie_header(&self, u: &User) -> String {
        format!(
            "pushit_session={}; Domain=.{domain}; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800",
            self.sign_session(u),
            domain = self.cfg.base_domain
        ) + if self.secure_cookies() {
            "; Secure"
        } else {
            ""
        }
    }

    pub fn user_from_headers(&self, headers: &HeaderMap) -> Option<User> {
        let c = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
        for pair in c.split(';') {
            let mut kv = pair.trim().splitn(2, '=');
            if kv.next()? == "pushit_session" {
                if let Some(u) = self.verify_session(kv.next()?) {
                    return Some(u);
                }
            }
        }
        None
    }

    // ---- long-lived API bearer (same HMAC format) ----

    pub fn verify_api_token(&self, headers: &HeaderMap) -> Option<User> {
        let h = headers
            .get(axum::http::header::AUTHORIZATION)?
            .to_str()
            .ok()?;
        let token = h.strip_prefix("Bearer ")?;
        self.verify_session(token)
    }

    // ---- OIDC auth-code flow ----

    fn secure_cookies(&self) -> bool {
        !matches!(
            self.cfg.base_domain.as_str(),
            "localhost" | "127.0.0.1" | "::1"
        )
    }

    fn redirect_uri(&self, host: &str) -> Result<String> {
        let authority: http::uri::Authority = host.parse().context("invalid callback authority")?;
        anyhow::ensure!(
            !authority.as_str().contains('@'),
            "invalid callback authority"
        );
        let hostname = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        let base_domain = self.cfg.base_domain.to_ascii_lowercase();
        anyhow::ensure!(
            hostname == base_domain || hostname.ends_with(&format!(".{base_domain}")),
            "callback host must belong to PUSHIT_BASE_DOMAIN"
        );
        let scheme = if self.secure_cookies() {
            "https"
        } else {
            "http"
        };
        let url = reqwest::Url::parse(&format!("{scheme}://{host}/auth/callback"))?;
        anyhow::ensure!(
            url.path() == "/auth/callback"
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "invalid callback host"
        );
        Ok(url.into())
    }

    fn login_cookie_name(&self) -> &'static str {
        if self.secure_cookies() {
            "__Host-pushit_login"
        } else {
            "pushit_login"
        }
    }

    fn login_cookie(&self, value: &str, max_age: u64) -> String {
        let name = self.login_cookie_name();
        format!(
            "{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{}",
            if self.secure_cookies() {
                "; Secure"
            } else {
                ""
            }
        )
    }

    pub fn clear_login_cookie(&self) -> String {
        self.login_cookie("", 0)
    }

    /// Start an authorization flow and return its URL and host-only browser cookie.
    pub fn auth_url(&self, host: &str, return_to: &str) -> Result<(String, String)> {
        let issuer = self
            .cfg
            .oidc_issuer
            .as_ref()
            .context("OIDC is not configured")?;
        let cid = self
            .cfg
            .oidc_client_id
            .as_ref()
            .context("OIDC client is not configured")?;
        let state = random_token();
        let browser = random_token();
        let verifier = random_token();
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        let redirect_uri = self.redirect_uri(host)?;
        let return_to = if return_to.starts_with('/')
            && !return_to.starts_with("//")
            && !return_to.contains('\\')
            && !return_to.chars().any(char::is_control)
        {
            return_to.to_string()
        } else {
            "/".into()
        };
        let mut url = reqwest::Url::parse(&format!("{issuer}/protocol/openid-connect/auth"))?;
        url.query_pairs_mut().extend_pairs([
            ("client_id", cid.as_str()),
            ("response_type", "code"),
            ("scope", "openid profile email"),
            ("redirect_uri", &redirect_uri),
            ("state", &state),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ]);
        let mut states = self.states.lock();
        states.retain(|_, pending| pending.created.elapsed() < LOGIN_TTL);
        anyhow::ensure!(states.len() < 10_000, "too many pending logins");
        states.insert(
            state,
            PendingLogin {
                created: Instant::now(),
                browser: browser.clone(),
                return_to,
                redirect_uri,
                verifier,
            },
        );
        Ok((url.into(), self.login_cookie(&browser, LOGIN_TTL.as_secs())))
    }

    fn consume_login(&self, host: &str, state: &str, headers: &HeaderMap) -> Result<PendingLogin> {
        anyhow::ensure!(!state.is_empty(), "missing login state");
        let browser = headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|cookies| {
                cookies.split(';').find_map(|pair| {
                    let (key, value) = pair.trim().split_once('=')?;
                    (key == self.login_cookie_name()).then_some(value)
                })
            })
            .context("missing login browser binding")?;
        let redirect_uri = self.redirect_uri(host)?;
        let mut states = self.states.lock();
        states.retain(|_, pending| pending.created.elapsed() < LOGIN_TTL);
        let pending = states
            .get(state)
            .context("unknown, expired or already used login state")?;
        anyhow::ensure!(
            pending.browser == browser && pending.redirect_uri == redirect_uri,
            "login state does not belong to this browser or callback host"
        );
        Ok(states.remove(state).expect("state checked under lock"))
    }

    pub async fn exchange_code(
        &self,
        host: &str,
        code: &str,
        state: &str,
        headers: &HeaderMap,
    ) -> Result<(User, String)> {
        let pending = self.consume_login(host, state, headers)?;
        let issuer = self.cfg.oidc_issuer.as_ref().unwrap();
        let cid = self.cfg.oidc_client_id.as_ref().unwrap();
        let secret = self.cfg.oidc_client_secret.clone().unwrap_or_default();
        let redirect = pending.redirect_uri;
        #[derive(Deserialize)]
        struct Tok {
            access_token: String,
        }
        let tok: Tok = self
            .http
            .post(format!("{issuer}/protocol/openid-connect/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", cid),
                ("client_secret", &secret),
                ("redirect_uri", &redirect),
                ("code_verifier", &pending.verifier),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        #[derive(Deserialize)]
        struct Ui {
            sub: Option<String>,
            preferred_username: Option<String>,
            name: Option<String>,
            email: Option<String>,
        }
        let ui: Ui = self
            .http
            .get(format!("{issuer}/protocol/openid-connect/userinfo"))
            .bearer_auth(&tok.access_token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let user = User {
            id: ui.sub.clone().unwrap_or_default(),
            name: ui
                .name
                .or(ui.preferred_username)
                .or_else(|| ui.sub.clone())
                .unwrap_or_else(|| "user".into()),
            email: ui.email.unwrap_or_default(),
        };
        Ok((user, pending.return_to))
    }

    // ---- Bearer JWT for API ----

    pub async fn verify_bearer(&self, headers: &HeaderMap) -> Result<Option<User>> {
        let Some(h) = headers.get(axum::http::header::AUTHORIZATION) else {
            return Ok(None);
        };
        let Some(token) = h.to_str().ok().and_then(|s| s.strip_prefix("Bearer ")) else {
            return Ok(None);
        };
        let header = jsonwebtoken::decode_header(token)?;
        let kid = header.kid.unwrap_or_default();
        let key = self.jwks_key(&kid).await?;
        let mut val = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        val.set_issuer(&[self.cfg.oidc_issuer.clone().unwrap_or_default()]);
        // audience varies (account, client_id); accept any
        val.validate_aud = false;
        let data: jsonwebtoken::TokenData<serde_json::Value> =
            jsonwebtoken::decode(token, &key, &val)?;
        let c = &data.claims;
        Ok(Some(User {
            id: c["sub"].as_str().unwrap_or("").into(),
            name: c["preferred_username"]
                .as_str()
                .or(c["name"].as_str())
                .unwrap_or("user")
                .into(),
            email: c["email"].as_str().unwrap_or("").into(),
        }))
    }

    async fn jwks_key(&self, kid: &str) -> Result<jsonwebtoken::DecodingKey> {
        if let Some(k) = self.jwks.lock().get(kid) {
            return Ok(k.clone());
        }
        let issuer = self
            .cfg
            .oidc_issuer
            .as_ref()
            .context("no OIDC issuer configured")?;
        #[derive(Deserialize)]
        struct Jwks {
            keys: Vec<serde_json::Value>,
        }
        let jwks: Jwks = self
            .http
            .get(format!("{issuer}/protocol/openid-connect/certs"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        for k in jwks.keys {
            if k["kid"].as_str().unwrap_or("") == kid {
                let key = jsonwebtoken::DecodingKey::from_jwk(
                    &serde_json::from_value(k).context("parsing JWK")?,
                )?;
                self.jwks.lock().insert(kid.to_string(), key.clone());
                return Ok(key);
            }
        }
        bail!("no JWKS key for kid '{kid}'")
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn getrandom_fill(buf: &mut [u8]) {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .expect("urandom");
}

pub(crate) fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom_fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            data_dir: std::path::PathBuf::new(),
            base_domain: "localhost".into(),
            bind: "127.0.0.1:0".into(),
            dev: false,
            oidc_issuer: Some("http://127.0.0.1:1".into()),
            oidc_client_id: Some("test-client".into()),
            oidc_client_secret: None,
            session_key: b"0123456789abcdef0123456789abcdef".to_vec(),
            handler_timeout_secs: 30,
        }
    }

    fn login(auth: &Auth) -> (String, HeaderMap, String) {
        let (url, cookie) = auth.auth_url("hub.localhost", "/apps/test").unwrap();
        let url = reqwest::Url::parse(&url).unwrap();
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["code_challenge_method"], "S256");
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            cookie.split(';').next().unwrap().parse().unwrap(),
        );
        (
            query["state"].clone(),
            headers,
            query["code_challenge"].clone(),
        )
    }

    #[test]
    fn client_dev_header_never_enables_production_bypass() {
        let mut headers = HeaderMap::new();
        headers.insert("x-pushit-dev", "1".parse().unwrap());
        assert!(!Auth::new(config()).dev_bypass(&headers));
        let mut cfg = config();
        cfg.dev = true;
        assert!(Auth::new(cfg).dev_bypass(&HeaderMap::new()));
    }

    #[test]
    fn persisted_key_preserves_sessions_and_tokens_after_restart() {
        let auth = Auth::new(config());
        let session = auth.sign_session(&User::dev());
        let token = auth.sign_api_token(&User::dev());
        let restarted = Auth::new(config());
        assert_eq!(restarted.verify_session(&session).unwrap().id, "dev");
        assert_eq!(restarted.verify_session(&token).unwrap().id, "dev");
        let mut cfg = config();
        cfg.session_key = vec![5; 32];
        assert!(Auth::new(cfg).verify_session(&session).is_none());
    }

    #[test]
    fn login_state_is_browser_bound_expiring_and_single_use() {
        let auth = Auth::new(config());
        let (state, headers, _) = login(&auth);
        assert!(auth.consume_login("hub.localhost", "", &headers).is_err());
        assert!(auth
            .consume_login("hub.localhost", "unknown", &headers)
            .is_err());
        assert!(auth
            .consume_login("hub.localhost", &state, &HeaderMap::new())
            .is_err());
        let (_, other_browser, _) = login(&auth);
        assert!(auth
            .consume_login("hub.localhost", &state, &other_browser)
            .is_err());
        assert!(auth
            .consume_login("another.localhost", &state, &headers)
            .is_err());
        assert_eq!(
            auth.consume_login("hub.localhost", &state, &headers)
                .unwrap()
                .return_to,
            "/apps/test"
        );
        assert!(auth
            .consume_login("hub.localhost", &state, &headers)
            .is_err());
        let (state, headers, _) = login(&auth);
        auth.states.lock().get_mut(&state).unwrap().created = Instant::now() - LOGIN_TTL;
        assert!(auth
            .consume_login("hub.localhost", &state, &headers)
            .is_err());
    }

    #[test]
    fn callback_authority_is_validated_and_preserves_port() {
        let auth = Auth::new(config());
        assert_eq!(
            auth.redirect_uri("hub.localhost:9100").unwrap(),
            "http://hub.localhost:9100/auth/callback"
        );
        for host in [
            "evil.example",
            "localhost.evil.example",
            "evil-localhost",
            "user@hub.localhost",
            "hub.localhost/path",
            "hub.localhost?x=1",
            "hub.localhost#fragment",
        ] {
            assert!(auth.redirect_uri(host).is_err(), "accepted {host}");
        }
    }

    #[test]
    fn login_cookies_and_redirects_are_safe() {
        let mut cfg = config();
        cfg.base_domain = "example.com".into();
        let auth = Auth::new(cfg);
        for destination in [
            "//evil.example",
            "/\\evil.example",
            "https://evil.example",
            "/\r\nLocation: bad",
        ] {
            let (url, cookie) = auth.auth_url("hub.example.com", destination).unwrap();
            assert!(cookie.contains("; Secure"));
            assert!(cookie.contains("HttpOnly; SameSite=Lax"));
            assert!(!cookie.contains("Domain="));
            let url = reqwest::Url::parse(&url).unwrap();
            let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(
                query["redirect_uri"],
                "https://hub.example.com/auth/callback"
            );
            assert_eq!(auth.states.lock()[&query["state"]].return_to, "/");
        }
    }

    #[tokio::test]
    async fn valid_oidc_flow_sends_pkce_and_consumes_state() {
        use axum::{
            routing::{get, post},
            Json, Router,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let mut cfg = config();
        cfg.oidc_issuer = Some(issuer);
        let auth = Auth::new(cfg);
        let (state, headers, challenge) = login(&auth);
        let app = Router::new()
            .route("/protocol/openid-connect/token", post(move |axum::extract::Form(form): axum::extract::Form<HashMap<String, String>>| async move {
                assert_eq!(form["code"], "test-code");
                assert_eq!(form["redirect_uri"], "http://hub.localhost/auth/callback");
                assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes())), challenge);
                Json(serde_json::json!({"access_token": "test-access"}))
            }))
            .route("/protocol/openid-connect/userinfo", get(|headers: HeaderMap| async move {
                assert_eq!(headers[axum::http::header::AUTHORIZATION], "Bearer test-access");
                Json(serde_json::json!({"sub":"test-user", "name":"Test", "email":"test@example.com"}))
            }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let result = auth
            .exchange_code("hub.localhost", "test-code", &state, &headers)
            .await
            .unwrap();
        assert_eq!(result.0.id, "test-user");
        assert_eq!(result.1, "/apps/test");
        assert!(auth
            .exchange_code("hub.localhost", "test-code", &state, &headers)
            .await
            .is_err());
        server.abort();
    }
}
