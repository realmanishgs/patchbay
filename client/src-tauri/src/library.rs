//! Library tools: list every plugin on disk (whatever installed it), remove one together with
//! the data it left behind, and import loose plugin files into the plugin folders.
//!
//! Removal is deliberately conservative. The UI never sends a path to delete on its own
//! authority: every path is re-derived here from a fresh scan and must match, so a compromised
//! or buggy front end cannot make this module delete arbitrary files.

use crate::db::Db;
use crate::installer::{bundle_format, place_bundle, remove_path, run_program, target_dir};
use crate::scanner;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// A VST2 plugin is a bare shared library; this export name identifies one.
const VST2_MARKER: &[u8] = b"VSTPluginMain";
const MAX_SNIFF_BYTES: u64 = 512 * 1024 * 1024;
const MAX_WALK_DEPTH: u32 = 2;
/// Names shorter than this (after normalising) are too generic to match data folders safely.
const MIN_MATCH_LEN: usize = 4;

#[derive(Debug, Clone, Serialize)]
pub struct LibraryItem {
    pub path: String,
    pub name: String,
    pub format: String,
    pub size: u64,
    /// Catalog id when Patchbay installed (or adopted) this plugin.
    pub managed_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Leftover {
    pub path: String,
    /// "data": settings and caches. "content": presets, samples and other user-facing files.
    pub kind: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VendorUninstaller {
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct Inspection {
    pub item: LibraryItem,
    pub leftovers: Vec<Leftover>,
    pub vendor_uninstallers: Vec<VendorUninstaller>,
}

#[derive(Debug, Serialize)]
pub struct Failure {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct RemoveReport {
    pub removed: Vec<String>,
    pub failed: Vec<Failure>,
    pub freed: u64,
}

#[derive(Debug, Serialize)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub imported: Vec<String>,
    pub skipped: Vec<Skipped>,
}

/* ---------- helpers ---------- */

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    let s = s.trim_end_matches('\\').to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        p.to_string_lossy().trim_end_matches('/').to_string()
    }
}

fn stem(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => name,
    }
}

fn size_of(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        let Ok(read) = fs::read_dir(path) else {
            return 0;
        };
        read.flatten().map(|e| size_of(&e.path())).sum()
    } else {
        meta.len()
    }
}

/// True if the file contains `needle`. Streams the file so large plugins stay cheap.
fn file_contains(path: &Path, needle: &[u8]) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() > MAX_SNIFF_BYTES {
        return false;
    }
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut buf = vec![0u8; 1 << 20];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        carry.extend_from_slice(&buf[..n]);
        if carry.windows(needle.len()).any(|w| w == needle) {
            return true;
        }
        let keep = needle.len().saturating_sub(1).min(carry.len());
        carry.drain(..carry.len() - keep);
    }
}

fn is_vst2_binary(path: &Path) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    (ext == "dll" || ext == "so") && file_contains(path, VST2_MARKER)
}

fn is_vst2_bundle(name: &str) -> bool {
    cfg!(target_os = "macos") && name.to_ascii_lowercase().ends_with(".vst")
}

/// Folders worth scanning: every format folder plus the usual VST2 locations.
fn plugin_dirs(db: &Db) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let modern = scanner::scan_dirs(db);
    let mut vst2: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = target_dir(db, "vst2") {
        vst2.push(dir);
    }
    if cfg!(windows) {
        for base in [r"C:\Program Files", r"C:\Program Files (x86)"] {
            vst2.push(PathBuf::from(base).join("VSTPlugins"));
            vst2.push(PathBuf::from(base).join("Steinberg").join("VSTPlugins"));
            vst2.push(PathBuf::from(base).join("Common Files").join("VST2"));
            vst2.push(PathBuf::from(base).join("Common Files").join("Steinberg").join("VST2"));
        }
    } else if cfg!(target_os = "macos") {
        vst2.push(PathBuf::from("/Library/Audio/Plug-Ins/VST"));
        if let Some(home) = dirs::home_dir() {
            vst2.push(home.join("Library/Audio/Plug-Ins/VST"));
        }
    } else {
        vst2.push(PathBuf::from("/usr/lib/vst"));
        if let Some(home) = dirs::home_dir() {
            vst2.push(home.join(".vst"));
        }
    }
    vst2.sort();
    vst2.dedup();
    (modern, vst2)
}

fn walk(dir: &Path, depth: u32, vst2: bool, out: &mut Vec<(PathBuf, &'static str)>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // includes Patchbay's own staging copies
        }
        let path = entry.path();
        // Bundles are plugins; never descend into them.
        if let Some(format) = bundle_format(&name) {
            out.push((path, format));
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if vst2 && is_vst2_bundle(&name) {
                out.push((path, "vst2"));
            } else if depth < MAX_WALK_DEPTH {
                walk(&path, depth + 1, vst2, out);
            }
        } else if kind.is_file() && vst2 && is_vst2_binary(&path) {
            out.push((path, "vst2"));
        }
    }
}

/// Every plugin found on disk, with the ones Patchbay tracks marked.
pub fn list_items(db: &Db) -> Result<Vec<LibraryItem>, String> {
    let (modern, vst2) = plugin_dirs(db);
    let mut found: Vec<(PathBuf, &'static str)> = Vec::new();
    for dir in &modern {
        walk(dir, 0, false, &mut found);
    }
    for dir in &vst2 {
        walk(dir, 0, true, &mut found);
    }

    let managed: HashMap<String, (String, String)> = db
        .list_installed()?
        .into_iter()
        .map(|r| (path_key(Path::new(&r.install_path)), (r.id, r.format)))
        .collect();

    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for (path, format) in found {
        let key = path_key(&path);
        if !seen.insert(key.clone()) {
            continue;
        }
        items.push(LibraryItem {
            name: stem(&path),
            format: format.to_string(),
            size: size_of(&path),
            managed_id: managed.get(&key).map(|(id, _)| id.clone()),
            path: path.to_string_lossy().into_owned(),
        });
    }
    // Apps Patchbay installed elsewhere (portable hosts and DAWs) are not in a plugin folder.
    for (key, (id, format)) in &managed {
        let path = db
            .get_installed(id)?
            .map(|r| PathBuf::from(r.install_path))
            .filter(|p| p.exists());
        if let Some(path) = path {
            if seen.insert(key.clone()) {
                items.push(LibraryItem {
                    name: stem(&path),
                    format: format.clone(),
                    size: size_of(&path),
                    managed_id: Some(id.clone()),
                    path: path.to_string_lossy().into_owned(),
                });
            }
        }
    }
    items.sort_by_key(|i| i.name.to_lowercase());
    Ok(items)
}

fn find_item(db: &Db, path: &str) -> Result<LibraryItem, String> {
    let wanted = path_key(Path::new(path));
    list_items(db)?
        .into_iter()
        .find(|i| path_key(Path::new(&i.path)) == wanted)
        .ok_or_else(|| "that plugin is no longer in the library; rescan and try again".to_string())
}

/* ---------- leftovers ---------- */

fn data_roots() -> Vec<(PathBuf, &'static str)> {
    let mut roots: Vec<(PathBuf, &'static str)> = Vec::new();
    for dir in [dirs::data_dir(), dirs::data_local_dir(), dirs::config_dir()]
        .into_iter()
        .flatten()
    {
        roots.push((dir, "data"));
    }
    if let Some(base) = std::env::var_os("PROGRAMDATA") {
        roots.push((PathBuf::from(base), "data"));
    }
    if let Some(docs) = dirs::document_dir() {
        roots.push((docs.join("VST3 Presets"), "content"));
        roots.push((docs, "content"));
    }
    let mut seen = HashSet::new();
    roots.retain(|(p, _)| seen.insert(path_key(p)));
    roots
}

fn find_leftovers(name: &str, vendor: Option<&str>) -> Vec<Leftover> {
    let wanted = normalize(name);
    if wanted.chars().count() < MIN_MATCH_LEN {
        return Vec::new();
    }
    let vendor_norm = vendor.map(normalize).filter(|v| !v.is_empty());
    let mut out = Vec::new();
    for (root, kind) in data_roots() {
        let presets_root = normalize(&root.file_name().unwrap_or_default().to_string_lossy())
            .contains("preset");
        let Ok(read) = fs::read_dir(&root) else {
            continue;
        };
        for entry in read.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let child = entry.path();
            let child_norm = normalize(&entry.file_name().to_string_lossy());
            if child_norm == wanted {
                out.push(Leftover { size: size_of(&child), path: child.to_string_lossy().into_owned(), kind: kind.to_string() });
                continue;
            }
            // <root>/<Vendor>/<Plugin>: only when the vendor is known or this is a presets folder.
            let vendor_match = vendor_norm.as_deref() == Some(child_norm.as_str());
            if vendor_match || presets_root {
                let Ok(inner) = fs::read_dir(&child) else {
                    continue;
                };
                for sub in inner.flatten() {
                    if sub.file_type().map(|t| t.is_dir()).unwrap_or(false)
                        && normalize(&sub.file_name().to_string_lossy()) == wanted
                    {
                        let path = sub.path();
                        out.push(Leftover { size: size_of(&path), path: path.to_string_lossy().into_owned(), kind: kind.to_string() });
                    }
                }
            }
        }
    }
    out
}

/// Refuses anything that is not clearly a plugin or plugin-data location.
fn is_deletable(path: &Path, db: &Db) -> bool {
    if !path.is_absolute() || path.components().count() < 4 {
        return false;
    }
    let key = path_key(path);
    if let Some(home) = dirs::home_dir() {
        let home = path_key(&home);
        if key == home || home.starts_with(&format!("{key}\\")) || home.starts_with(&format!("{key}/")) {
            return false;
        }
    }
    let (modern, vst2) = plugin_dirs(db);
    let roots: Vec<String> = data_roots().iter().map(|(p, _)| path_key(p)).collect();
    let protected = modern
        .iter()
        .chain(vst2.iter())
        .map(|p| path_key(p))
        .chain(roots);
    for p in protected {
        if key == p || p.starts_with(&format!("{key}\\")) || p.starts_with(&format!("{key}/")) {
            return false;
        }
    }
    true
}

/// Catalog id of the plugin at `path`, if Patchbay tracks it (used to look up its vendor).
pub fn managed_id(db: &Db, path: &str) -> Result<Option<String>, String> {
    Ok(find_item(db, path)?.managed_id)
}

pub fn inspect(db: &Db, path: &str, vendor: Option<&str>) -> Result<Inspection, String> {
    let item = find_item(db, path)?;
    let leftovers = find_leftovers(&item.name, vendor);
    let vendor_uninstallers = find_vendor_uninstallers(&item.name)
        .into_iter()
        .map(|(display_name, _)| VendorUninstaller { display_name })
        .collect();
    Ok(Inspection { item, leftovers, vendor_uninstallers })
}

/* ---------- removal ---------- */

pub fn remove(
    db: &Db,
    path: &str,
    leftover_paths: &[String],
    vendor: Option<&str>,
    confirmed: bool,
) -> Result<RemoveReport, String> {
    if !confirmed {
        return Err("removal was not confirmed".to_string());
    }
    let item = find_item(db, path)?;
    let allowed: HashSet<String> = find_leftovers(&item.name, vendor)
        .iter()
        .map(|l| path_key(Path::new(&l.path)))
        .collect();

    let mut targets: Vec<PathBuf> = vec![PathBuf::from(&item.path)];
    for extra in leftover_paths {
        let p = PathBuf::from(extra);
        if !allowed.contains(&path_key(&p)) {
            return Err(format!("{extra} is not data of this plugin; nothing was removed"));
        }
        targets.push(p);
    }
    for t in &targets {
        if !is_deletable(t, db) {
            return Err(format!("refusing to remove {}; nothing was removed", t.display()));
        }
    }

    let mut report = RemoveReport { removed: Vec::new(), failed: Vec::new(), freed: 0 };
    let mut needs_admin: Vec<(PathBuf, u64)> = Vec::new();
    for target in targets {
        let size = size_of(&target);
        match remove_path(&target) {
            Ok(()) => {
                report.freed += size;
                report.removed.push(target.to_string_lossy().into_owned());
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && !is_locked(&e) => {
                needs_admin.push((target, size));
            }
            Err(e) => report.failed.push(Failure {
                path: target.to_string_lossy().into_owned(),
                error: describe(&e),
            }),
        }
    }
    if !needs_admin.is_empty() {
        let paths: Vec<PathBuf> = needs_admin.iter().map(|(p, _)| p.clone()).collect();
        let outcome = elevated_remove(&paths);
        for (path, size) in needs_admin {
            if path.exists() {
                report.failed.push(Failure {
                    path: path.to_string_lossy().into_owned(),
                    error: match &outcome {
                        Err(e) => e.clone(),
                        Ok(()) => "still present after the administrator step; is it in use?".to_string(),
                    },
                });
            } else {
                report.freed += size;
                report.removed.push(path.to_string_lossy().into_owned());
            }
        }
    }

    let plugin_gone = !Path::new(&item.path).exists();
    if plugin_gone {
        if let Some(id) = &item.managed_id {
            db.delete_installed(id)?;
        }
    }
    Ok(report)
}

/// Windows reports a file held open by another program as access denied (os error 5) or a
/// sharing violation (32). Only the first can be fixed by elevation, and it is ambiguous, so
/// a sharing violation is the one case we surface as "in use" directly.
fn is_locked(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(32)
}

fn describe(e: &std::io::Error) -> String {
    if is_locked(e) {
        "in use by another program; close your DAW and try again".to_string()
    } else {
        e.to_string()
    }
}

#[cfg(windows)]
fn elevated_remove(paths: &[PathBuf]) -> Result<(), String> {
    let lit = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "''"));
    let mut body = String::from("$ErrorActionPreference = 'Stop'\r\n");
    for p in paths {
        body.push_str(&format!(
            "if (Test-Path -LiteralPath {0}) {{ Remove-Item -LiteralPath {0} -Recurse -Force }}\r\n",
            lit(p)
        ));
    }
    let script_path = std::env::temp_dir().join(format!(
        "patchbay-remove-{}-{}.ps1",
        std::process::id(),
        crate::poller::now_unix()
    ));
    fs::write(&script_path, body).map_err(|e| e.to_string())?;
    let script_arg = script_path.to_string_lossy().replace('\'', "''");
    let launcher = format!(
        "$p = Start-Process -FilePath powershell.exe -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File','\"{script_arg}\"'); exit $p.ExitCode"
    );
    let result = run_program(
        "powershell",
        &[
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-Command".to_string(),
            launcher,
        ],
    );
    let _ = fs::remove_file(&script_path);
    result.map_err(|e| format!("administrator permission was not granted or removal failed ({e})"))
}

#[cfg(target_os = "macos")]
fn elevated_remove(paths: &[PathBuf]) -> Result<(), String> {
    let quoted: Vec<String> = paths
        .iter()
        .map(|p| format!("'{}'", p.to_string_lossy().replace('\'', "'\\''")))
        .collect();
    let shell = format!("rm -rf {}", quoted.join(" "));
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        shell.replace('\\', "\\\\").replace('"', "\\\"")
    );
    run_program("osascript", &["-e".to_string(), script])
}

#[cfg(not(any(windows, target_os = "macos")))]
fn elevated_remove(_paths: &[PathBuf]) -> Result<(), String> {
    Err("administrator permission is needed to remove this; remove it with sudo".to_string())
}

/* ---------- vendor uninstallers (Windows registry) ---------- */

#[cfg(windows)]
fn registry_uninstallers() -> Vec<(String, String)> {
    use std::os::windows::process::CommandExt;
    let script = "$k='HKLM:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*',\
        'HKLM:\\SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*',\
        'HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*'; \
        $r = @(Get-ItemProperty -Path $k -ErrorAction SilentlyContinue | \
        Where-Object { $_.DisplayName -and $_.UninstallString } | \
        ForEach-Object { [pscustomobject]@{ n = [string]$_.DisplayName; u = [string]$_.UninstallString } }); \
        ConvertTo-Json -InputObject $r -Compress";
    let Ok(output) = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x0800_0000)
        .output()
    else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };
    let rows = match value {
        serde_json::Value::Array(rows) => rows,
        obj @ serde_json::Value::Object(_) => vec![obj],
        _ => Vec::new(),
    };
    rows.iter()
        .filter_map(|r| {
            Some((r.get("n")?.as_str()?.to_string(), r.get("u")?.as_str()?.to_string()))
        })
        .collect()
}

#[cfg(not(windows))]
fn registry_uninstallers() -> Vec<(String, String)> {
    Vec::new()
}

fn find_vendor_uninstallers(name: &str) -> Vec<(String, String)> {
    let wanted = normalize(name);
    if wanted.chars().count() < MIN_MATCH_LEN {
        return Vec::new();
    }
    registry_uninstallers()
        .into_iter()
        .filter(|(display, _)| {
            let d = normalize(display);
            d.contains(&wanted) && !d.contains("patchbay")
        })
        .collect()
}

/// Starts the vendor's own uninstaller (interactive, so the vendor's dialog and UAC prompt
/// appear). It never waits: the user finishes it, then rescans.
#[cfg(windows)]
pub fn run_vendor_uninstaller(db: &Db, path: &str, display_name: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let item = find_item(db, path)?;
    let (_, command) = find_vendor_uninstallers(&item.name)
        .into_iter()
        .find(|(d, _)| d == display_name)
        .ok_or("that uninstaller is no longer registered")?;
    std::process::Command::new("cmd")
        .arg("/C")
        .raw_arg(format!("\"{command}\""))
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not start the uninstaller: {e}"))
}

#[cfg(not(windows))]
pub fn run_vendor_uninstaller(_db: &Db, _path: &str, _display_name: &str) -> Result<(), String> {
    Err("vendor uninstallers are only looked up on Windows".to_string())
}

/* ---------- import ---------- */

fn classify(path: &Path) -> Result<&'static str, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(format) = bundle_format(&name) {
        return Ok(format);
    }
    if path.is_dir() && is_vst2_bundle(&name) {
        return Ok("vst2");
    }
    if path.is_file() && is_vst2_binary(path) {
        return Ok("vst2");
    }
    if path.is_file()
        && matches!(
            path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).as_deref(),
            Some("dll") | Some("so")
        )
    {
        return Err("this library has no VST entry point, so it is not a VST2 plugin".to_string());
    }
    Err("not a .dll, .vst3, .clap or .component plugin".to_string())
}

pub fn import(db: &Db, inputs: &[String]) -> Result<ImportReport, String> {
    let mut candidates: Vec<(PathBuf, &'static str)> = Vec::new();
    let mut report = ImportReport { imported: Vec::new(), skipped: Vec::new() };
    for raw in inputs {
        let cleaned = raw.trim().trim_matches('"').trim();
        if cleaned.is_empty() {
            continue;
        }
        let path = PathBuf::from(cleaned);
        if !path.is_absolute() {
            report.skipped.push(Skipped { path: cleaned.to_string(), reason: "use a full path".to_string() });
            continue;
        }
        if !path.exists() {
            report.skipped.push(Skipped { path: cleaned.to_string(), reason: "does not exist".to_string() });
            continue;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let is_plain_folder =
            path.is_dir() && bundle_format(&name).is_none() && !is_vst2_bundle(&name);
        if is_plain_folder {
            let mut found = Vec::new();
            walk(&path, 0, true, &mut found);
            if found.is_empty() {
                report.skipped.push(Skipped {
                    path: cleaned.to_string(),
                    reason: "no plugins found in this folder".to_string(),
                });
            }
            candidates.extend(found);
        } else {
            match classify(&path) {
                Ok(format) => candidates.push((path, format)),
                Err(reason) => report.skipped.push(Skipped { path: cleaned.to_string(), reason }),
            }
        }
    }

    for (src, format) in candidates {
        let shown = src.to_string_lossy().into_owned();
        let dir = match target_dir(db, format) {
            Ok(dir) => dir,
            Err(reason) => {
                report.skipped.push(Skipped { path: shown, reason });
                continue;
            }
        };
        let Some(name) = src.file_name() else {
            continue;
        };
        let dest = dir.join(name);
        if path_key(&dest) == path_key(&src) || dest.exists() {
            report.skipped.push(Skipped {
                path: shown,
                reason: format!("already in {}", dir.display()),
            });
            continue;
        }
        match place_bundle(&src, &dir) {
            Ok(placed) => report.imported.push(placed.to_string_lossy().into_owned()),
            Err(reason) => report.skipped.push(Skipped { path: shown, reason }),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_ignores_case_and_punctuation() {
        assert_eq!(normalize("Odin 2"), normalize("odin2"));
        assert_eq!(normalize("Surge-XT"), "surgext");
    }

    #[test]
    fn short_names_never_match_data_folders() {
        assert!(find_leftovers("EQ", None).is_empty());
        assert!(find_vendor_uninstallers("EQ").is_empty());
    }

    #[test]
    fn marker_is_found_across_read_boundaries() {
        let dir = std::env::temp_dir().join(format!("patchbay-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("p.dll");
        let mut data = vec![0u8; (1 << 20) - 5];
        data.extend_from_slice(VST2_MARKER);
        fs::write(&file, &data).unwrap();
        assert!(is_vst2_binary(&file));
        fs::write(&file, vec![0u8; 4096]).unwrap();
        assert!(!is_vst2_binary(&file));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn roots_and_short_paths_are_not_deletable() {
        let dir = std::env::temp_dir().join(format!("patchbay-db-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("t.sqlite3")).unwrap();
        assert!(!is_deletable(Path::new("C:\\"), &db));
        assert!(!is_deletable(Path::new("relative"), &db));
        if let Some(home) = dirs::home_dir() {
            assert!(!is_deletable(&home, &db));
            assert!(!is_deletable(home.parent().unwrap(), &db));
        }
        if let Some(docs) = dirs::document_dir() {
            assert!(!is_deletable(&docs, &db));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    fn temp_db(tag: &str) -> (PathBuf, Db) {
        let dir = std::env::temp_dir().join(format!("patchbay-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("t.sqlite3")).unwrap();
        (dir, db)
    }

    #[test]
    fn remove_flow_is_guarded_and_deletes_plugin_and_data() {
        let (dir, db) = temp_db("rm");
        let vst3 = dir.join("vst3");
        let bundle = vst3.join("PatchbayZzProbe.vst3");
        fs::create_dir_all(bundle.join("Contents")).unwrap();
        fs::write(bundle.join("Contents").join("a.bin"), vec![1u8; 2048]).unwrap();
        db.set_setting("path.vst3", vst3.to_str().unwrap()).unwrap();

        let appdata = dirs::data_dir().unwrap().join("PatchbayZzProbe");
        fs::create_dir_all(&appdata).unwrap();
        fs::write(appdata.join("settings.ini"), b"x").unwrap();
        let bystander = dir.join("bystander.txt");
        fs::write(&bystander, b"keep").unwrap();

        let path = bundle.to_string_lossy().into_owned();
        let inspection = inspect(&db, &path, None).unwrap();
        assert_eq!(inspection.item.format, "vst3");
        assert!(inspection.item.size >= 2048);
        let data = appdata.to_string_lossy().into_owned();
        assert!(inspection.leftovers.iter().any(|l| l.path == data && l.kind == "data"));

        // Not confirmed, or an unrelated path smuggled in: nothing may be deleted.
        assert!(remove(&db, &path, &[], None, false).is_err());
        let evil = bystander.to_string_lossy().into_owned();
        assert!(remove(&db, &path, &[evil], None, true).is_err());
        assert!(bundle.exists() && bystander.exists());
        // A path that was never scanned is refused as a plugin.
        assert!(remove(&db, &bystander.to_string_lossy(), &[], None, true).is_err());

        let report = remove(&db, &path, &[data], None, true).unwrap();
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.removed.len(), 2);
        assert!(!bundle.exists() && !appdata.exists());
        assert!(bystander.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_accepts_vst2_and_rejects_other_dlls() {
        let (dir, db) = temp_db("imp");
        let vst2 = dir.join("vst2");
        db.set_setting("path.vst2", vst2.to_str().unwrap()).unwrap();
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        let good = src.join("Good.dll");
        let mut bytes = vec![0u8; 100];
        bytes.extend_from_slice(VST2_MARKER);
        fs::write(&good, bytes).unwrap();
        let bad = src.join("helper.dll");
        fs::write(&bad, vec![0u8; 100]).unwrap();

        let report = import(
            &db,
            &[good.to_string_lossy().into_owned(), bad.to_string_lossy().into_owned(), "relative.dll".into()],
        )
        .unwrap();
        assert_eq!(report.imported.len(), 1);
        assert_eq!(report.skipped.len(), 2);
        assert!(vst2.join("Good.dll").exists() && good.exists());

        let again = import(&db, &[good.to_string_lossy().into_owned()]).unwrap();
        assert!(again.imported.is_empty() && again.skipped[0].reason.starts_with("already in"));
        let _ = fs::remove_dir_all(&dir);
    }
}

