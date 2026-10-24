//! Named CLI profiles: per-instance server + token, one active at a time.
//! Stored in `<config>/pushit/profiles.toml` (0600) — plaintext tokens,
//! same trust level as the legacy single-token file.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default)]
pub struct Store {
    pub active: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Entry>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    pub server: String,
    #[serde(default)]
    pub token: String,
}

pub fn config_dir() -> Option<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|p| p.join("pushit"))
}

pub fn path() -> Option<PathBuf> {
    config_dir().map(|p| p.join("profiles.toml"))
}

pub fn load() -> Store {
    let Some(p) = path() else {
        return Store::default();
    };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(store: &Store) -> Result<()> {
    let p = path().ok_or_else(|| anyhow::anyhow!("cannot determine config dir (set HOME)"))?;
    write_private(&p, toml::to_string_pretty(store)?.as_bytes())
}

impl Store {
    pub fn get(&self, name: &str) -> Result<&Entry> {
        self.profiles
            .get(name)
            .with_context(|| format!("profile '{name}' not found (pushit profile list)"))
    }

    pub fn set_token(&mut self, name: &str, token: &str) -> Result<()> {
        let e = self
            .profiles
            .get_mut(name)
            .with_context(|| format!("profile '{name}' not found"))?;
        e.token = token.to_string();
        Ok(())
    }
}

/// Publish credentials atomically, with private permissions from creation.
pub fn write_private(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("missing credential directory")?;
    let mut dirs = std::fs::DirBuilder::new();
    dirs.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dirs.mode(0o700);
    }
    dirs.create(parent)?;
    let temp = parent.join(format!(
        ".pushit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
