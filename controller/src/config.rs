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
    /// Persistent HMAC key; the known fallback is allowed only in explicit dev mode.
    pub session_key: Vec<u8>,
    pub handler_timeout_secs: u64,
}

/// Empty-string env (compose passes unset vars as "") counts as unset.
fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let data_dir =
            PathBuf::from(std::env::var("PUSHIT_DATA").unwrap_or_else(|_| "./pushit-data".into()));
        let dev = std::env::var("PUSHIT_DEV").ok().as_deref() == Some("1");
        let session_key = session_key(dev, nonempty_env("PUSHIT_SESSION_KEY"))?;
        Ok(Config {
            data_dir,
            base_domain: std::env::var("PUSHIT_BASE_DOMAIN").unwrap_or_else(|_| "localhost".into()),
            bind: std::env::var("PUSHIT_BIND").unwrap_or_else(|_| "0.0.0.0:9100".into()),
            dev,
            oidc_issuer: nonempty_env("PUSHIT_OIDC_ISSUER"),
            oidc_client_id: nonempty_env("PUSHIT_OIDC_CLIENT_ID"),
            oidc_client_secret: nonempty_env("PUSHIT_OIDC_CLIENT_SECRET"),
            session_key,
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

const DEV_SESSION_KEY: &str = "pushit-dev-session-key";

fn session_key(dev: bool, configured: Option<String>) -> anyhow::Result<Vec<u8>> {
    let configured = configured.filter(|s| !s.trim().is_empty());
    if dev {
        return Ok(configured
            .unwrap_or_else(|| DEV_SESSION_KEY.into())
            .into_bytes());
    }
    let key = configured.ok_or_else(|| {
        anyhow::anyhow!(
        "PUSHIT_SESSION_KEY is required outside PUSHIT_DEV=1; generate with openssl rand -hex 32"
    )
    })?;
    anyhow::ensure!(key != DEV_SESSION_KEY && key.trim().len() >= 32,
        "PUSHIT_SESSION_KEY must be a randomly generated persistent secret of at least 32 bytes (not the dev default)");
    Ok(key.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_key_fails_closed() {
        for key in [
            None,
            Some(""),
            Some("   "),
            Some(DEV_SESSION_KEY),
            Some("short"),
        ] {
            assert!(session_key(false, key.map(str::to_owned)).is_err());
        }
        let key = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            session_key(false, Some(key.into())).unwrap(),
            key.as_bytes()
        );
        assert_eq!(session_key(true, None).unwrap(), DEV_SESSION_KEY.as_bytes());
    }
}
