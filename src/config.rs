//! Global config: the `~/.wookie/config.toml` registry mapping project roots
//! to wikis, plus defaults. `WOOKIE_HOME` overrides the home for testing.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub fn wookie_home() -> PathBuf {
    if let Some(home) = std::env::var_os("WOOKIE_HOME") {
        return PathBuf::from(home);
    }
    let home = std::env::var_os("HOME").expect("HOME is not set");
    PathBuf::from(home).join(".wookie")
}

pub fn user_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is not set"))
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct GlobalConfig {
    #[serde(default)]
    pub wikis: BTreeMap<String, WikiEntry>,
    #[serde(default)]
    pub defaults: Defaults,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct WikiEntry {
    #[serde(default)]
    pub project_roots: Vec<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Defaults {
    #[serde(default = "default_true")]
    pub auto_commit: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults { auto_commit: true }
    }
}

impl GlobalConfig {
    pub fn load(home: &Path) -> Result<GlobalConfig> {
        let path = home.join("config.toml");
        if !path.exists() {
            return Ok(GlobalConfig::default());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        fs::create_dir_all(home)?;
        let path = home.join("config.toml");
        let raw = toml::to_string_pretty(self)?;
        fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))
    }
}
