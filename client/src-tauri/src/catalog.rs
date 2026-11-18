//! Catalog types shared by the poller and installer. They mirror the JSON written by
//! `registry/scripts/compile_registry.py`; unknown fields are ignored so the index can grow.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub release_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub key_name: Option<String>,
    #[serde(default)]
    pub exchange_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub category: String,
    pub license: String,
    pub open_source: bool,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub about: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub screenshot: Option<String>,
    pub install_strategy: String,
    #[serde(default)]
    pub silent_flags: Option<String>,
    pub auth_type: String,
    #[serde(default)]
    pub auth: Option<AuthBlock>,
    #[serde(default)]
    pub formats: Vec<String>,
    pub latest: Release,
    #[serde(default)]
    pub assets: HashMap<String, Asset>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub generated_at: Option<String>,
    pub plugins: Vec<PluginEntry>,
}

/// Asset key for the running OS, matching the keys used in `source.asset_patterns`.
pub fn platform_key() -> &'static str {
    if cfg!(target_os = "windows") {
        "win_x64"
    } else if cfg!(target_os = "macos") {
        "mac_universal"
    } else {
        "linux_x64"
    }
}

/// True when `remote` should be offered over `local`. Semver is used when both parse;
/// otherwise (nightly tags, date tags) any difference counts as an update.
pub fn is_newer(remote: &str, local: &str) -> bool {
    match (
        semver::Version::parse(remote.trim_start_matches('v')),
        semver::Version::parse(local.trim_start_matches('v')),
    ) {
        (Ok(r), Ok(l)) => r > l,
        _ => remote != local,
    }
}
