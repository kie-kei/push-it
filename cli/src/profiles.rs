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
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, toml::to_string_pretty(store)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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
