use anyhow::{bail, Context, Result};
use axum::http::HeaderMap;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

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

pub struct Auth {
    pub cfg: Config,
    /// state nonce -> created; single-process so in-memory is fine
    states: Mutex<HashMap<String, String>>,
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

    pub fn dev_bypass(&self, headers: &HeaderMap) -> bool {
        self.cfg.dev
            || headers
                .get("x-pushit-dev")
                .and_then(|v| v.to_str().ok())
                == Some("1")
    }

    pub fn oidc_enabled(&self) -> bool {
        self.cfg.oidc_issuer.is_some()
            && self.cfg.oidc_client_id.is_some()
            && !self.cfg.dev
    }

    // ---- session cookie (HMAC-signed) ----

    pub fn sign_session(&self, u: &User) -> String {
        let exp = now() + 7 * 86400;
        let payload = format!("{exp}|{}|{}|{}", u.id, u.name, u.email);
        let mut mac = HmacSha256::new_from_slice(&self.cfg.session_key).unwrap();
        mac.update(payload.as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!("{payload}|{sig}"))
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
        )
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

    // ---- OIDC auth-code flow ----

    pub fn auth_url(&self, host: &str, return_to: &str) -> Result<String> {
        let issuer = self.cfg.oidc_issuer.as_ref().unwrap();
        let cid = self.cfg.oidc_client_id.as_ref().unwrap();
        let state = hex::encode({
            let mut b = [0u8; 16];
            getrandom_fill(&mut b);
            b
        });
        self.states
            .lock()
            .insert(state.clone(), return_to.to_string());
        Ok(format!(
            "{issuer}/protocol/openid-connect/auth?client_id={cid}&response_type=code&scope=openid%20profile%20email&redirect_uri={}&state={state}",
            urlencoding(&format!("http://{host}/auth/callback")),
        ))
    }

    pub async fn exchange_code(&self, host: &str, code: &str, state: &str) -> Result<(User, String)> {
        let return_to = self.states.lock().remove(state).unwrap_or_else(|| "/".into());
        let issuer = self.cfg.oidc_issuer.as_ref().unwrap();
        let cid = self.cfg.oidc_client_id.as_ref().unwrap();
        let secret = self.cfg.oidc_client_secret.clone().unwrap_or_default();
        let redirect = format!("http://{host}/auth/callback");
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
        Ok((user, return_to))
    }

    // ---- Bearer JWT for API ----

    pub async fn verify_bearer(&self, headers: &HeaderMap) -> Result<Option<User>> {
        let Some(h) = headers.get(axum::http::header::AUTHORIZATION) else {
            return Ok(None);
        };
        let Some(token) = h.to_str().ok().and_then(|s| s.strip_prefix("Bearer "))
        else {
            return Ok(None);
        };
        let header = jsonwebtoken::decode_header(token)?;
        let kid = header.kid.unwrap_or_default();
        let key = self.jwks_key(&kid).await?;
        let mut val = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        val.set_issuer(&[self
            .cfg
            .oidc_issuer
            .clone()
            .unwrap_or_default()]);
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

fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
