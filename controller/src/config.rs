use std::path::PathBuf;

#[derive(Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub base_domain: String,
    pub bind: String,
    pub dev: bool,
    pub oidc_issuer: Option<String>,
    pub oidc_client_id: Option<String>,
    pub oidc_client_secret: Option<String>,
    /// HMAC key for session cookies; falls back to the age key bytes.
    pub session_key: Vec<u8>,
    pub handler_timeout_secs: u64,
}

/// Empty-string env (compose passes unset vars as "") counts as unset.
fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let data_dir = PathBuf::from(
            std::env::var("PUSHIT_DATA").unwrap_or_else(|_| "./pushit-data".into()),
        );
        Ok(Config {
            data_dir,
            base_domain: std::env::var("PUSHIT_BASE_DOMAIN")
                .unwrap_or_else(|_| "localhost".into()),
            bind: std::env::var("PUSHIT_BIND").unwrap_or_else(|_| "0.0.0.0:9100".into()),
            dev: std::env::var("PUSHIT_DEV").ok().as_deref() == Some("1"),
            oidc_issuer: nonempty_env("PUSHIT_OIDC_ISSUER"),
            oidc_client_id: nonempty_env("PUSHIT_OIDC_CLIENT_ID"),
            oidc_client_secret: nonempty_env("PUSHIT_OIDC_CLIENT_SECRET"),
            session_key: nonempty_env("PUSHIT_SESSION_KEY")
                .map(|s| s.into_bytes())
                .unwrap_or_else(|| b"pushit-dev-session-key".to_vec()),
            handler_timeout_secs: 30,
        })
    }

    pub fn apps_dir(&self) -> PathBuf {
        self.data_dir.join("apps")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("pushit.db")
    }
    pub fn age_key_path(&self) -> PathBuf {
        self.data_dir.join("age.key")
    }
}
