use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    Local,
    Builtin,
    Oidc,
    Dev,
}

#[derive(Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub base_domain: String,
    pub bind: String,
    /// Fixed public HTTPS base origin in builtin mode, independent of HTTP bind.
    pub external_origin: Option<String>,
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
            Some("builtin") => AuthMode::Builtin,
            Some("oidc") => AuthMode::Oidc,
            Some("dev") => AuthMode::Dev,
            None if dev => AuthMode::Dev,
            None => AuthMode::Oidc,
            _ => anyhow::bail!("PUSHIT_AUTH_MODE must be local, builtin, oidc or dev"),
        };
        anyhow::ensure!(
            !dev || auth_mode == AuthMode::Dev,
            "PUSHIT_DEV conflicts with PUSHIT_AUTH_MODE"
        );
        let oidc_issuer = nonempty_env("PUSHIT_OIDC_ISSUER");
        let oidc_client_id = nonempty_env("PUSHIT_OIDC_CLIENT_ID");
        let oidc_client_secret = nonempty_env("PUSHIT_OIDC_CLIENT_SECRET");
        anyhow::ensure!(
            !matches!(auth_mode, AuthMode::Local | AuthMode::Builtin)
                || (oidc_issuer.is_none()
                    && oidc_client_id.is_none()
                    && oidc_client_secret.is_none()),
            "builtin/local auth cannot be combined with OIDC settings"
        );
        let session_key = if matches!(auth_mode, AuthMode::Local | AuthMode::Builtin) {
            Vec::new()
        } else {
            session_key(
                auth_mode == AuthMode::Dev,
                nonempty_env("PUSHIT_SESSION_KEY"),
            )?
        };
        let external_origin = nonempty_env("PUSHIT_EXTERNAL_ORIGIN");
        anyhow::ensure!(
            auth_mode == AuthMode::Builtin || external_origin.is_none(),
            "PUSHIT_EXTERNAL_ORIGIN requires PUSHIT_AUTH_MODE=builtin"
        );
        let base_domain =
            std::env::var("PUSHIT_BASE_DOMAIN").unwrap_or_else(|_| "localhost".into());
        let bind = std::env::var("PUSHIT_BIND").unwrap_or_else(|_| {
            if matches!(auth_mode, AuthMode::Local | AuthMode::Builtin) {
                "127.0.0.1:9100".into()
            } else {
                "0.0.0.0:9100".into()
            }
        });
        if auth_mode == AuthMode::Local {
            validate_local(&base_domain, &bind)?;
        }
        if auth_mode == AuthMode::Builtin {
            validate_builtin(&base_domain, &bind, external_origin.as_deref())?;
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
            external_origin,
            dev: auth_mode == AuthMode::Dev,
            auth_mode,
            oidc_issuer,
            oidc_client_id,
            oidc_client_secret,
            session_key,
            handler_timeout_secs: 30,
        })
    }

    pub fn owner_auth(&self) -> bool {
        matches!(self.auth_mode, AuthMode::Local | AuthMode::Builtin)
    }
    pub fn auth_scheme(&self) -> &'static str {
        if self.auth_mode == AuthMode::Builtin {
            "https"
        } else {
            "http"
        }
    }
    pub fn auth_port(&self) -> u16 {
        if self.auth_mode == AuthMode::Builtin {
            reqwest::Url::parse(
                self.external_origin
                    .as_deref()
                    .expect("validated external origin"),
            )
            .expect("validated external origin")
            .port_or_known_default()
            .unwrap()
        } else {
            self.bind
                .parse::<std::net::SocketAddr>()
                .expect("validated local bind")
                .port()
        }
    }
    pub fn auth_port_suffix(&self) -> String {
        let port = self.auth_port();
        if port
            == if self.auth_scheme() == "https" {
                443
            } else {
                80
            }
        {
            String::new()
        } else {
            format!(":{port}")
        }
    }
    pub fn hub_authority(&self) -> String {
        format!("hub.{}{}", self.base_domain, self.auth_port_suffix())
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

/// This mode trusts only a same-host TLS reverse proxy, never forwarded metadata.
pub fn validate_builtin(base_domain: &str, bind: &str, origin: Option<&str>) -> anyhow::Result<()> {
    validate_local("localhost", bind)?;
    anyhow::ensure!(
        valid_dns_name(base_domain)
            && base_domain.len() <= 249
            && base_domain.contains('.')
            && base_domain != "localhost"
            && !base_domain.ends_with(".localhost")
            && base_domain.parse::<std::net::IpAddr>().is_err(),
        "builtin auth requires a lowercase DNS PUSHIT_BASE_DOMAIN (not localhost or an IP)"
    );
    let origin = origin.ok_or_else(|| {
        anyhow::anyhow!("builtin auth requires PUSHIT_EXTERNAL_ORIGIN=https://<base-domain>[:port]")
    })?;
    let url = reqwest::Url::parse(origin)?;
    let port = url.port_or_known_default().unwrap_or(0);
    let canonical = if port == 443 {
        format!("https://{base_domain}")
    } else {
        format!("https://{base_domain}:{port}")
    };
    anyhow::ensure!(url.scheme() == "https" && url.host_str() == Some(base_domain) && port != 0 && origin == canonical,
        "PUSHIT_EXTERNAL_ORIGIN must be the exact HTTPS base origin, without credentials, path, trailing slash, query or fragment; omit :443");
    Ok(())
}

pub fn valid_dns_name(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_configuration_has_no_url_normalization_or_public_backend_escape() {
        for origin in [
            None,
            Some("http://platform.test"),
            Some("https://other.test"),
            Some("https://platform.test/"),
            Some("https://platform.test:443"),
            Some("https://platform.test:0"),
            Some("https://platform.test/path"),
            Some("https://user@platform.test"),
            Some("https://PLATFORM.test"),
            Some("https://platform.test?x"),
            Some("https://platform.test#x"),
            Some("https://platform.test:08443"),
            Some(" https://platform.test"),
            Some("https://platform.test/../"),
            Some("https://platform.test\\evil"),
        ] {
            assert!(
                validate_builtin("platform.test", "127.0.0.1:9100", origin).is_err(),
                "accepted {origin:?}"
            );
        }
        for bind in [
            "0.0.0.0:9100",
            "[::]:9100",
            "localhost:9100",
            "127.0.0.2:9100",
            "127.0.0.1:0",
        ] {
            assert!(
                validate_builtin("platform.test", bind, Some("https://platform.test")).is_err()
            );
        }
        for base in [
            "localhost",
            "foo.localhost",
            "127.0.0.1",
            "UPPER.test",
            "bad..test",
            "bad-.test",
            "-bad.test",
            "a.test/path",
            "a.test.",
            "127.1",
            "0x7f.1",
            "2130706433",
        ] {
            assert!(
                validate_builtin(base, "127.0.0.1:9100", Some(&format!("https://{base}"))).is_err()
            );
        }
        let overlong = [
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(59),
        ]
        .join(".");
        assert_eq!(overlong.len(), 251);
        assert!(validate_builtin(
            &overlong,
            "127.0.0.1:9100",
            Some(&format!("https://{overlong}"))
        )
        .is_err());
        assert!(validate_builtin(
            "platform.test",
            "127.0.0.1:9100",
            Some("https://platform.test")
        )
        .is_ok());
        assert!(validate_builtin(
            "platform.test",
            "[::1]:9100",
            Some("https://platform.test:8443")
        )
        .is_ok());
    }

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
