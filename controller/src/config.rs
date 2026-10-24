use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    Local,
    Oidc,
    Dev,
}

#[derive(Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub base_domain: String,
    pub bind: String,
    pub dev: bool,
    pub auth_mode: AuthMode,
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
        let explicit = nonempty_env("PUSHIT_AUTH_MODE");
        let auth_mode = match explicit.as_deref() {
            Some("local") => AuthMode::Local,
            Some("oidc") => AuthMode::Oidc,
            Some("dev") => AuthMode::Dev,
            None if dev => AuthMode::Dev,
            None => AuthMode::Oidc,
            _ => anyhow::bail!("PUSHIT_AUTH_MODE must be local, oidc or dev"),
        };
        anyhow::ensure!(
            !dev || auth_mode == AuthMode::Dev,
            "PUSHIT_DEV conflicts with PUSHIT_AUTH_MODE"
        );
        let oidc_issuer = nonempty_env("PUSHIT_OIDC_ISSUER");
        let oidc_client_id = nonempty_env("PUSHIT_OIDC_CLIENT_ID");
        let oidc_client_secret = nonempty_env("PUSHIT_OIDC_CLIENT_SECRET");
        anyhow::ensure!(
            auth_mode != AuthMode::Local
                || (oidc_issuer.is_none()
                    && oidc_client_id.is_none()
                    && oidc_client_secret.is_none()),
            "local auth cannot be combined with OIDC settings"
        );
        let session_key = if auth_mode == AuthMode::Local {
            Vec::new()
        } else {
            session_key(
                auth_mode == AuthMode::Dev,
                nonempty_env("PUSHIT_SESSION_KEY"),
            )?
        };
        let base_domain =
            std::env::var("PUSHIT_BASE_DOMAIN").unwrap_or_else(|_| "localhost".into());
        let bind = std::env::var("PUSHIT_BIND").unwrap_or_else(|_| {
            if auth_mode == AuthMode::Local {
                "127.0.0.1:9100".into()
            } else {
                "0.0.0.0:9100".into()
            }
        });
        if auth_mode == AuthMode::Local {
            validate_local(&base_domain, &bind)?;
        }
        if explicit.as_deref() == Some("oidc") {
            anyhow::ensure!(
                oidc_issuer.is_some() && oidc_client_id.is_some(),
                "OIDC mode requires issuer and client ID"
            );
        }
        Ok(Config {
            data_dir,
            base_domain,
            bind,
            dev: auth_mode == AuthMode::Dev,
            auth_mode,
            oidc_issuer,
            oidc_client_id,
            oidc_client_secret,
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

/// Stage 1 deliberately excludes proxies, public binds, custom domains and TLS.
pub fn validate_local(base_domain: &str, bind: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        base_domain == "localhost",
        "local auth requires PUSHIT_BASE_DOMAIN=localhost"
    );
    let address: std::net::SocketAddr = bind
        .parse()
        .map_err(|_| anyhow::anyhow!("local auth requires a numeric loopback PUSHIT_BIND"))?;
    anyhow::ensure!(
        address.port() != 0,
        "local auth requires a nonzero controller port"
    );
    anyhow::ensure!(
        address.ip() == std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            || address.ip() == std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        "local auth requires PUSHIT_BIND on 127.0.0.1 or ::1"
    );
    Ok(())
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
