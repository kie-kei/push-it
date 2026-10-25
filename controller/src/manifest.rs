use anyhow::{bail, Context};
use serde::Deserialize;
use std::collections::BTreeMap;

const RESERVED: &[&str] = &["hub", "api", "auth"];

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Deploy-time bindings for `capabilities.sql` grants: logical name →
    /// source. Source is `secret:<app-secret-key>` (DSN in that app secret)
    /// or `platform:<global-secret-name>` (DSN in a platform-wide secret).
    /// Authored by the deployer (root manifest injects it), not the app repo.
    #[serde(default)]
    pub sql_bindings: BTreeMap<String, String>,
    #[serde(default)]
    pub build: Build,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Capabilities {
    /// Outgoing-HTTP allowlist. Absent → denied; `["*"]` → all hosts;
    /// otherwise exact hostnames (port ignored) plus `*.suffix` entries.
    #[serde(default)]
    pub http_out: Option<Vec<String>>,
    #[serde(default)]
    pub env: Vec<String>,
    /// Logical connection names this component may open via wasi:sql,
    /// bound to a DSN source at deploy time (`sql_bindings`).
    #[serde(default)]
    pub sql: Vec<String>,
    /// Logical bucket names this component may open via wasi:keyvalue.
    /// `local` is reserved for the app-scoped embedded store
    /// (`$PUSHIT_DATA/appdata/<app>/local.kv`); no other names exist yet.
    #[serde(default)]
    pub kv: Vec<String>,
    /// Logical blobstore names this component may open via wasi:blobstore.
    /// Names must exist in `$PUSHIT_DATA/blobstores.yaml`.
    #[serde(default)]
    pub blobstore: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Build {
    #[serde(default)]
    pub ui_dist: Option<String>,
}

/// Outgoing-HTTP policy resolved from `capabilities.http_out`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpOut {
    Denied,
    Allowed(HostFilter),
}

/// Hostname allowlist matched case-insensitively against the request
/// authority's host (any port is accepted). `*` allows everything,
/// `*.example.internal` allows any depth of subdomain.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostFilter {
    all: bool,
    hosts: Vec<String>,
    suffixes: Vec<String>,
}

impl HostFilter {
    /// Allow every host; the explicit `--http-out` dev override.
    pub fn all() -> Self {
        HostFilter {
            all: true,
            hosts: Vec::new(),
            suffixes: Vec::new(),
        }
    }

    pub fn parse(entries: &[String]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !entries.is_empty(),
            "capabilities.http_out: grant at least one host (or '*' for all)"
        );
        let mut filter = HostFilter::default();
        for entry in entries {
            let entry = entry.trim();
            anyhow::ensure!(!entry.is_empty(), "capabilities.http_out: empty host entry");
            if entry == "*" {
                filter.all = true;
                continue;
            }
            if let Some(suffix) = entry.strip_prefix("*.") {
                anyhow::ensure!(
                    valid_host(suffix),
                    "capabilities.http_out: '{entry}' is not a valid '*.host' entry"
                );
                filter.suffixes.push(suffix.to_ascii_lowercase());
                continue;
            }
            anyhow::ensure!(
                valid_host(entry),
                "capabilities.http_out: '{entry}' is not a valid host (letters, digits, '-', '.'; no port or scheme)"
            );
            filter.hosts.push(entry.to_ascii_lowercase());
        }
        Ok(filter)
    }

    pub fn allows(&self, host: &str) -> bool {
        if self.all {
            return true;
        }
        let host = host.to_ascii_lowercase();
        self.hosts.iter().any(|h| *h == host)
            || self.suffixes.iter().any(|s| {
                host.strip_suffix('.')
                    .unwrap_or(host.as_str())
                    .ends_with(&format!(".{s}"))
            })
    }
}

/// A DNS name (lowercase letters/digits/'-'/'.' labels) or an IP literal.
fn valid_host(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
        || crate::config::valid_dns_name(&host.to_ascii_lowercase())
}

impl Manifest {
    pub fn parse(yaml: &str) -> anyhow::Result<Self> {
        let m: Manifest = serde_yaml::from_str(yaml).context("invalid pushit.yaml")?;
        m.validate_name(&m.name)?;
        if let Some(d) = &m.domain {
            for label in d.split('.') {
                m.validate_name(label)
                    .with_context(|| format!("bad domain label '{label}'"))?;
            }
        }
        for n in &m.capabilities.sql {
            if n == "local" {
                continue;
            }
            if n.starts_with('$') {
                bail!(
                    "capabilities.sql: '{n}' — grant prefixes are gone; declare the plain \
                     logical name and bind its source in the deploy manifest \
                     (sql: {{name: secret:<app-secret-key> | platform:<global-secret-name>}})"
                );
            }
            if n.trim().is_empty() {
                bail!("capabilities.sql: connection names must be non-empty");
            }
            if !m.sql_bindings.contains_key(n) {
                bail!(
                    "capabilities.sql: '{n}' is not bound — set sql: {{{n}: \
                     secret:<app-secret-key> | platform:<global-secret-name>}} in the \
                     deploy (root) manifest or app manifest"
                );
            }
        }
        if let Some(entries) = &m.capabilities.http_out {
            // Fail at deploy time, not at the first denied request.
            HostFilter::parse(entries)?;
        }
        Ok(m)
    }
    fn validate_name(&self, name: &str) -> anyhow::Result<()> {
        if name.is_empty() || name.len() > 63 {
            bail!("name '{name}': must be 1-63 chars");
        }
        if RESERVED.contains(&name) {
            bail!("name '{name}' is reserved");
        }
        let ok = name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && name.as_bytes()[0] != b'-'
            && name.as_bytes()[name.len() - 1] != b'-';
        if !ok {
            bail!("name '{name}': must be a DNS label (a-z, 0-9, '-')");
        }
        Ok(())
    }

    /// Subdomain routed to this app: `domain` if set (single-label → under
    /// base domain), else `name`.
    pub fn route_name(&self) -> &str {
        self.domain.as_deref().unwrap_or(&self.name)
    }

    pub fn http_out_policy(&self) -> HttpOut {
        self.capabilities
            .http_out
            .as_deref()
            .and_then(|entries| HostFilter::parse(entries).ok())
            .map(HttpOut::Allowed)
            .unwrap_or(HttpOut::Denied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal() {
        let m = Manifest::parse("name: my-app\n").unwrap();
        assert_eq!(m.name, "my-app");
        assert_eq!(m.http_out_policy(), HttpOut::Denied);
    }

    #[test]
    fn rejects_reserved_and_bad_names() {
        for n in ["hub", "api", "auth", "Bad", "-x", "x-", "a..b", ""] {
            let y = format!("name: \"{n}\"");
            assert!(Manifest::parse(&y).is_err(), "accepted {n}");
        }
    }

    #[test]
    fn parses_full() {
        let y = r#"
name: reporting-bot
domain: reporting-bot
env:
  CLICKHOUSE_URL: http://ch:8123
secrets: [TOKEN, S3_KEY]
capabilities:
  http_out: [clickhouse.internal]
  env: [CLICKHOUSE_URL]
build:
  ui_dist: ui/dist
"#;
        let m = Manifest::parse(y).unwrap();
        assert_eq!(m.env["CLICKHOUSE_URL"], "http://ch:8123");
        assert_eq!(m.secrets, vec!["TOKEN", "S3_KEY"]);
        assert_eq!(
            m.http_out_policy(),
            HttpOut::Allowed(HostFilter {
                all: false,
                hosts: vec!["clickhouse.internal".into()],
                suffixes: vec![],
            })
        );

        assert_eq!(m.build.ui_dist.as_deref(), Some("ui/dist"));
    }

    #[test]
    fn http_out_allowlist_semantics() {
        let y = |caps: &str| format!("name: a\ncapabilities:\n  http_out: {caps}\n");
        // '*' allows everything
        let m = Manifest::parse(&y("['*']")).unwrap();
        let HttpOut::Allowed(f) = m.http_out_policy() else {
            panic!("expected allowed");
        };
        assert!(f.allows("example.com") && f.allows("10.0.0.1"));
        // exact host, case-insensitive, port is decided by the request not the grant
        let m = Manifest::parse(&y("[CH.Internal]")).unwrap();
        let HttpOut::Allowed(f) = m.http_out_policy() else {
            panic!("expected allowed");
        };
        assert!(f.allows("ch.internal"));
        assert!(!f.allows("other.internal"));
        assert!(!f.allows("ch.internal.evil.example"));
        // '*.suffix' matches any depth of subdomain, not the suffix itself
        let m = Manifest::parse(&y("['*.corp.io', '127.0.0.1']")).unwrap();
        let HttpOut::Allowed(f) = m.http_out_policy() else {
            panic!("expected allowed");
        };
        assert!(f.allows("a.corp.io") && f.allows("a.b.corp.io") && f.allows("127.0.0.1"));
        assert!(!f.allows("corp.io") && !f.allows("evilcorp.io") && !f.allows("acorp.io."));
        // fail closed on malformed grants
        for bad in [
            "[]",
            "['']",
            "['*.']",
            "['*.bad..host']",
            "['bad..host']",
            "['-bad']",
            "['ch.internal:8123']",
            "['https://ch.internal']",
            "['*.ch.internal:8123']",
        ] {
            assert!(Manifest::parse(&y(bad)).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn parses_sql_capability() {
        let m = Manifest::parse("name: a\ncapabilities:\n  sql: [local, db]\nsql_bindings:\n  db: platform:arcadia-prod\n").unwrap();
        assert_eq!(m.capabilities.sql, vec!["local", "db"]);
        assert_eq!(m.sql_bindings["db"], "platform:arcadia-prod");
        let m = Manifest::parse("name: a\n").unwrap();
        assert!(m.capabilities.sql.is_empty());
        // plain grants must be bound
        assert!(Manifest::parse("name: a\ncapabilities:\n  sql: [db]\n").is_err());
        // prefixes are gone
        assert!(Manifest::parse("name: a\ncapabilities:\n  sql: [\"$SECRET/pg1\"]\n").is_err());
        assert!(Manifest::parse("name: a\ncapabilities:\n  sql: [\"$PLATFORM/db\"]\n").is_err());
        assert!(Manifest::parse("name: a\ncapabilities:\n  sql: [\"\"]\n").is_err());
        assert!(Manifest::parse("name: a\ncapabilities:\n  sql: [\" \"]\n").is_err());
    }
}
