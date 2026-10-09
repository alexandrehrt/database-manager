//! Saved data sources (TOML in the app config dir) and their passwords (OS keychain).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;
use dbm_core::config::DataSourceConfig;
use serde::{Deserialize, Serialize};

const KEYCHAIN_SERVICE: &str = "database-manager";

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    sources: Vec<DataSourceConfig>,
}

fn config_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::config_dir().context("no config directory on this system")?;
    Ok(dir.join("database-manager").join("connections.toml"))
}

pub fn load_sources() -> anyhow::Result<Vec<DataSourceConfig>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let store: Store = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(store.sources)
}

pub fn save_sources(sources: &[DataSourceConfig]) -> anyhow::Result<()> {
    let path = config_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(&Store { sources: sources.to_vec() })?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

fn entry(source_id: &str) -> anyhow::Result<keyring::Entry> {
    Ok(keyring::Entry::new(KEYCHAIN_SERVICE, source_id)?)
}

/// Blocking: may show a keychain prompt, so call it off the UI thread.
pub fn load_password(source_id: &str) -> Option<String> {
    entry(source_id).ok()?.get_password().ok()
}

pub fn save_password(source_id: &str, password: &str) -> anyhow::Result<()> {
    Ok(entry(source_id)?.set_password(password)?)
}

pub fn delete_password(source_id: &str) {
    // A missing entry is the desired end state, so the error is irrelevant.
    if let Ok(e) = entry(source_id) {
        let _ = e.delete_credential();
    }
}

fn history_path() -> anyhow::Result<PathBuf> {
    Ok(config_path()?.with_file_name("history.json"))
}

/// Executed statements per data source id, most recent first.
pub fn load_history() -> HashMap<String, Vec<String>> {
    history_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save_history(history: &HashMap<String, Vec<String>>) -> anyhow::Result<()> {
    let path = history_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string(history)?).with_context(|| format!("writing {}", path.display()))
}
