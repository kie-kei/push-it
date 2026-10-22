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
    #[serde(default)]
    pub build: Build,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Capabilities {
    /// MVP: presence of the key grants unrestricted outgoing HTTP.
    #[serde(default)]
    pub http_out: Option<Vec<String>>,
    #[serde(default)]
    pub env: Vec<String>,
    /// Logical connection names this component may open via wasi:sql.
    /// `local` is reserved for the app-scoped sqlite database
    /// (`$PUSHIT_DATA/appdata/<app>/local.db`); `$SECRET/<name>` grants
    /// resolve the DSN from app secret `<name>`.
    #[serde(default)]
    pub sql: Vec<String>,
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
            match n.strip_prefix("$SECRET/") {
                Some(key) if !key.trim().is_empty() => {}
                _ => bail!(
                    "capabilities.sql: '{n}' must be 'local' or '$SECRET/<name>' \
                     where <name> is an app secret holding the DSN"
                ),
            }
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

    pub fn http_out(&self) -> bool {
        self.capabilities.http_out.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal() {
        let m = Manifest::parse("name: my-app\n").unwrap();
        assert_eq!(m.name, "my-app");
        assert!(!m.http_out());
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
        assert!(m.http_out());

        assert_eq!(m.build.ui_dist.as_deref(), Some("ui/dist"));
    }

    #[test]
    fn parses_sql_capability() {
        let m = Manifest::parse("name: a\ncapabilities:\n  sql: [local, \"$SECRET/pg1\"]\n").unwrap();
        assert_eq!(m.capabilities.sql, vec!["local", "$SECRET/pg1"]);
        let m = Manifest::parse("name: a\n").unwrap();
        assert!(m.capabilities.sql.is_empty());
        // plain names no longer resolve anywhere — invalid
        assert!(Manifest::parse("name: a\ncapabilities:\n  sql: [sl]\n").is_err());
        assert!(Manifest::parse("name: a\ncapabilities:\n  sql: [\"$SECRET/ \"]\n").is_err());
        assert!(Manifest::parse("name: a\ncapabilities:\n  sql: [\"\"]\n").is_err());
        assert!(Manifest::parse("name: a\ncapabilities:\n  sql: [\" \"]\n").is_err());
    }
}
