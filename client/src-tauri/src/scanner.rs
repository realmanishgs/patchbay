//! First-run adoption: plugins already on disk are recorded as installed, so they are not
//! downloaded again. They are adopted at the catalog's current version, which means they
//! only show an update when a newer release appears after adoption.

use crate::catalog::{Catalog, PluginEntry};
use crate::db::{Db, InstalledPlugin};
use crate::installer::{bundle_format, default_dir, target_dir};
use std::fs;
use std::path::PathBuf;

pub const SCAN_FLAG: &str = "first_scan_done";

pub(crate) fn scan_dirs(db: &Db) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ["vst3", "clap", "au"]
        .iter()
        .filter_map(|format| target_dir(db, format).ok())
        .collect();
    for format in ["vst3", "clap"] {
        dirs.extend(default_dir(format));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".vst3"));
        dirs.push(home.join(".clap"));
        dirs.push(home.join("Library/Audio/Plug-Ins/VST3"));
        dirs.push(home.join("Library/Audio/Plug-Ins/CLAP"));
        dirs.push(home.join("Library/Audio/Plug-Ins/Components"));
    }
    if cfg!(target_os = "linux") {
        dirs.push(PathBuf::from("/usr/lib/vst3"));
        dirs.push(PathBuf::from("/usr/lib/clap"));
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Adopts every bundle that matches a catalog entry and is not already tracked.
/// Matching is by normalised file stem (alphanumerics, lower-cased) against id or name.
pub fn first_scan(db: &Db, catalog: &Catalog) -> Result<usize, String> {
    let now = crate::poller::now_unix();
    let mut adopted = 0;
    for dir in scan_dirs(db) {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let Some(format) = bundle_format(&file_name) else {
                continue;
            };
            let stem = file_name
                .rsplit_once('.')
                .map(|(stem, _)| stem)
                .unwrap_or(file_name.as_str());
            let Some(plugin) = match_plugin(catalog, &normalize(stem)) else {
                continue;
            };
            if db.get_installed(&plugin.id)?.is_some() {
                continue;
            }
            db.upsert_installed(&InstalledPlugin {
                id: plugin.id.clone(),
                installed_version: plugin.latest.version.clone(),
                install_path: entry.path().to_string_lossy().into_owned(),
                format: format.to_string(),
                updated_at: now,
                source: "scan".to_string(),
            })?;
            adopted += 1;
        }
    }
    db.set_setting(SCAN_FLAG, "1")?;
    Ok(adopted)
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn match_plugin<'a>(catalog: &'a Catalog, norm: &str) -> Option<&'a PluginEntry> {
    catalog
        .plugins
        .iter()
        .find(|p| norm == normalize(&p.id) || norm == normalize(&p.name))
}
