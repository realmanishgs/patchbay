//! Download → verify → extract/install engine.
//!
//! Every install is staged in a `WorkDir` under the OS temp folder. The guard removes it on
//! success, on error and on early return, so nothing is left behind. Bundles are copied into a
//! hidden staging name inside the destination folder and renamed into place, so a DAW never
//! sees a half-written plugin.

use crate::catalog::{platform_key, Asset, PluginEntry};
use crate::db::{self, Db, InstalledPlugin};
use crate::poller::{current_catalog, now_unix};
use crate::AppState;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::AsyncWriteExt;

const PROGRESS_EVENT: &str = "install-progress";
const MAX_SCAN_DEPTH: u32 = 6;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// (path that was written, format label). Installers report an empty path.
type Placed = Vec<(PathBuf, String)>;

#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub id: String,
    pub phase: String,
    pub received: u64,
    pub total: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Zip,
    TarGz,
    Dmg,
    Exe,
    Msi,
    Pkg,
    Other,
}

fn kind_of(name: &str) -> Kind {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".zip") {
        Kind::Zip
    } else if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        Kind::TarGz
    } else if n.ends_with(".dmg") {
        Kind::Dmg
    } else if n.ends_with(".exe") {
        Kind::Exe
    } else if n.ends_with(".msi") {
        Kind::Msi
    } else if n.ends_with(".pkg") {
        Kind::Pkg
    } else {
        Kind::Other
    }
}

/// Plugin bundle format from a file name, or `None` for anything else.
pub fn bundle_format(file_name: &str) -> Option<&'static str> {
    let n = file_name.to_ascii_lowercase();
    if n.ends_with(".vst3") {
        Some("vst3")
    } else if n.ends_with(".clap") {
        Some("clap")
    } else if n.ends_with(".component") {
        Some("au")
    } else {
        None
    }
}

/// Standard OS folder for a plugin format.
pub fn default_dir(format: &str) -> Option<PathBuf> {
    match format {
        "vst3" => {
            if cfg!(target_os = "windows") {
                Some(PathBuf::from(r"C:\Program Files\Common Files\VST3"))
            } else if cfg!(target_os = "macos") {
                Some(PathBuf::from("/Library/Audio/Plug-Ins/VST3"))
            } else {
                dirs::home_dir().map(|h| h.join(".vst3"))
            }
        }
        "clap" => {
            if cfg!(target_os = "windows") {
                Some(PathBuf::from(r"C:\Program Files\Common Files\CLAP"))
            } else if cfg!(target_os = "macos") {
                Some(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"))
            } else {
                dirs::home_dir().map(|h| h.join(".clap"))
            }
        }
        "au" => {
            if cfg!(target_os = "macos") {
                Some(PathBuf::from("/Library/Audio/Plug-Ins/Components"))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Folder a format installs into: the user's Settings override, else the OS default.
pub fn target_dir(db: &Db, format: &str) -> Result<PathBuf, String> {
    if let Some(custom) = db.get_setting(&format!("path.{format}"))? {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    default_dir(format).ok_or_else(|| format!("no default {format} folder on this OS; set one in Settings"))
}

fn portable_root() -> Result<PathBuf, String> {
    let base = if cfg!(target_os = "windows") {
        dirs::data_local_dir().map(|d| d.join("Patchbay").join("Apps"))
    } else if cfg!(target_os = "macos") {
        dirs::home_dir().map(|h| h.join("Applications"))
    } else {
        dirs::home_dir().map(|h| h.join(".local").join("share").join("patchbay").join("apps"))
    };
    base.ok_or_else(|| "no user data folder available".to_string())
}

/// Temp folder for one install; removed when dropped.
struct WorkDir(PathBuf);

impl WorkDir {
    fn create(id: &str) -> Result<Self, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let safe: String = id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        let path = std::env::temp_dir().join(format!(
            "patchbay-{safe}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).map_err(|e| format!("cannot create temp folder: {e}"))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Prevents two installs of the same plugin from running at once.
struct BusyGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    id: String,
}

impl<'a> BusyGuard<'a> {
    fn acquire(set: &'a Mutex<HashSet<String>>, id: &str) -> Result<Self, String> {
        let mut guard = set
            .lock()
            .map_err(|_| "install lock poisoned".to_string())?;
        if !guard.insert(id.to_string()) {
            return Err(format!("{id} is already being installed"));
        }
        Ok(Self {
            set,
            id: id.to_string(),
        })
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut set) = self.set.lock() {
            set.remove(&self.id);
        }
    }
}

/// Detaches a mounted DMG when dropped.
struct MountGuard(PathBuf);

impl Drop for MountGuard {
    fn drop(&mut self) {
        detach_dmg(&self.0);
    }
}

#[cfg(target_os = "macos")]
fn detach_dmg(mountpoint: &Path) {
    let _ = Command::new("hdiutil")
        .args(["detach", "-force", "-quiet"])
        .arg(mountpoint)
        .status();
}

#[cfg(not(target_os = "macos"))]
fn detach_dmg(_mountpoint: &Path) {}

#[cfg(target_os = "macos")]
fn mount_dmg(dmg: &Path, mountpoint: &Path) -> Result<MountGuard, String> {
    let status = Command::new("hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-noautoopen",
            "-quiet",
            "-mountpoint",
        ])
        .arg(mountpoint)
        .arg(dmg)
        .status()
        .map_err(io_msg)?;
    if !status.success() {
        return Err("could not mount the DMG image".to_string());
    }
    Ok(MountGuard(mountpoint.to_path_buf()))
}

#[cfg(not(target_os = "macos"))]
fn mount_dmg(_dmg: &Path, _mountpoint: &Path) -> Result<MountGuard, String> {
    Err("DMG packages can only be installed on macOS".to_string())
}

fn io_msg(e: io::Error) -> String {
    e.to_string()
}

fn access_error(e: io::Error, dir: &Path) -> String {
    if e.kind() == io::ErrorKind::PermissionDenied {
        format!(
            "permission denied at {} - run Patchbay with administrator rights or choose a writable folder in Settings",
            dir.display()
        )
    } else {
        format!("{}: {e}", dir.display())
    }
}

fn require_https(url: &str) -> Result<(), String> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(format!("refusing non-HTTPS URL: {url}"))
    }
}

fn key_name(entry: &PluginEntry) -> Result<String, String> {
    entry
        .auth
        .as_ref()
        .and_then(|a| a.key_name.clone())
        .ok_or_else(|| format!("{} has no key_name in its auth block", entry.name))
}

/// Install one plugin by catalog id. Emits `install-progress` events while it runs, ending with
/// a `done` or `error` phase so every UI (window or tray-triggered) can clear its progress state.
pub async fn install(app: &AppHandle, id: &str) -> Result<InstalledPlugin, String> {
    let result = install_inner(app, id).await;
    if result.is_err() {
        let _ = app.emit(
            PROGRESS_EVENT,
            Progress {
                id: id.to_string(),
                phase: "error".to_string(),
                received: 0,
                total: None,
            },
        );
    }
    result
}

async fn install_inner(app: &AppHandle, id: &str) -> Result<InstalledPlugin, String> {
    let state = app.state::<AppState>();
    let _busy = BusyGuard::acquire(&state.busy, id)?;
    let entry = find_entry(&state, id).await?;
    let asset = entry
        .assets
        .get(platform_key())
        .cloned()
        .ok_or_else(|| format!("{} has no build for this platform", entry.name))?;
    let expected = asset
        .sha256
        .clone()
        .ok_or("index entry has no SHA-256 digest; refusing to install")?;
    let previous = state.db.get_installed(id)?;

    let emit = |phase: &str, received: u64, total: Option<u64>| {
        let _ = app.emit(
            PROGRESS_EVENT,
            Progress {
                id: id.to_string(),
                phase: phase.to_string(),
                received,
                total,
            },
        );
    };

    let work = WorkDir::create(id)?;
    let file_name = Path::new(&asset.name)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("payload.bin")
        .to_string();
    let payload = work.path().join(&file_name);

    let (url, bearer) = resolve_download(&state, &entry, &asset).await?;
    emit("download", 0, asset.size);
    let digest = download(&state, &url, bearer.as_deref(), &payload, |received, total| {
        emit("download", received, total)
    })
    .await?;
    if !digest.eq_ignore_ascii_case(&expected) {
        return Err(format!(
            "SHA-256 mismatch for {}: expected {expected}, got {digest}",
            asset.name
        ));
    }

    emit("install", 0, None);
    let kind = kind_of(&asset.name);
    let placed = if entry.install_strategy == "portable_app" {
        install_portable(&entry, &work, &payload, kind)?
    } else {
        install_payload(&state.db, &entry, &work, &payload, kind)?
    };

    let mut paths: Vec<String> = Vec::new();
    let mut formats: Vec<String> = Vec::new();
    for (path, format) in &placed {
        let text = path.to_string_lossy().into_owned();
        if !text.is_empty() {
            paths.push(text);
        }
        if !formats.contains(format) {
            formats.push(format.clone());
        }
    }

    // Replace the previous version only after the new one is in place. Vendor installers report
    // no paths and write to their own locations, so nothing may be deleted in that case: the old
    // recorded path is usually the very file the installer just replaced.
    if let (Some(prev), false) = (&previous, paths.is_empty()) {
        for old in prev.install_path.split(';').filter(|p| !p.is_empty()) {
            if !paths.iter().any(|p| p == old) {
                let _ = remove_path(Path::new(old));
            }
        }
    }

    let record = InstalledPlugin {
        id: id.to_string(),
        installed_version: entry.latest.version.clone(),
        install_path: paths.join(";"),
        format: formats.join(","),
        updated_at: now_unix(),
        source: "install".to_string(),
    };
    state.db.upsert_installed(&record)?;
    emit("done", 0, None);
    Ok(record)
}

async fn find_entry(state: &AppState, id: &str) -> Result<PluginEntry, String> {
    let catalog = current_catalog(state).await?;
    catalog
        .plugins
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| format!("'{id}' is not in the catalog"))
}

/// Returns the URL to fetch and an optional bearer token. For `license_key_header`, the locally
/// stored key is exchanged with the vendor for a short-lived signed URL.
async fn resolve_download(
    state: &AppState,
    entry: &PluginEntry,
    asset: &Asset,
) -> Result<(String, Option<String>), String> {
    match entry.auth_type.as_str() {
        "none" => Ok((asset.url.clone(), None)),
        "bearer_token" => {
            let token = db::read_secret(&key_name(entry)?)?;
            Ok((asset.url.clone(), Some(token)))
        }
        "license_key_header" => {
            let auth = entry.auth.as_ref().ok_or("auth block missing")?;
            let exchange = auth
                .exchange_url
                .clone()
                .ok_or("license exchange_url missing")?;
            require_https(&exchange)?;
            let license = db::read_secret(&key_name(entry)?)?;
            let resp = state
                .http
                .post(&exchange)
                .header("X-License-Key", license)
                .json(&serde_json::json!({
                    "plugin_id": entry.id,
                    "platform": platform_key(),
                    "version": entry.latest.version,
                }))
                .send()
                .await
                .map_err(|e| format!("license exchange failed: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("license exchange rejected: HTTP {}", resp.status()));
            }
            #[derive(Deserialize)]
            struct Exchange {
                download_url: String,
            }
            let body: Exchange = resp
                .json()
                .await
                .map_err(|e| format!("bad license exchange response: {e}"))?;
            require_https(&body.download_url)?;
            Ok((body.download_url, None))
        }
        other => Err(format!("unsupported auth type '{other}'")),
    }
}

/// Streams the asset to `dest`, reporting progress, and returns its SHA-256 hex digest.
async fn download(
    state: &AppState,
    url: &str,
    bearer: Option<&str>,
    dest: &Path,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<String, String> {
    require_https(url)?;
    let mut request = state.http.get(url);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    let resp = request
        .send()
        .await
        .map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    let total = resp.content_length();
    let mut file = tokio::fs::File::create(dest).await.map_err(io_msg)?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut last_report: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("download interrupted: {e}"))?;
        hasher.update(&chunk[..]);
        file.write_all(&chunk[..]).await.map_err(io_msg)?;
        received += chunk.len() as u64;
        if received - last_report >= 256 * 1024 || Some(received) == total {
            on_progress(received, total);
            last_report = received;
        }
    }
    file.flush().await.map_err(io_msg)?;
    Ok(hex::encode(hasher.finalize()))
}

fn install_payload(
    db: &Db,
    entry: &PluginEntry,
    work: &WorkDir,
    payload: &Path,
    kind: Kind,
) -> Result<Placed, String> {
    match kind {
        Kind::Exe | Kind::Msi | Kind::Pkg => {
            run_installer(payload, kind, entry.silent_flags.as_deref())?;
            Ok(vec![(PathBuf::new(), "installer".to_string())])
        }
        Kind::Zip | Kind::TarGz => {
            let extract = work.path().join("extract");
            extract_archive(kind, payload, &extract)?;
            place_bundles(db, entry, &extract)
        }
        Kind::Dmg => {
            let mountpoint = work.path().join("mount");
            fs::create_dir_all(&mountpoint).map_err(io_msg)?;
            let _mount = mount_dmg(payload, &mountpoint)?;
            if entry.install_strategy == "silent_installer" {
                if let Some(pkg) = top_level_with_suffix(&mountpoint, ".pkg").into_iter().next() {
                    run_installer(&pkg, Kind::Pkg, entry.silent_flags.as_deref())?;
                    return Ok(vec![(PathBuf::new(), "installer".to_string())]);
                }
            }
            match place_bundles(db, entry, &mountpoint) {
                Ok(placed) => Ok(placed),
                Err(bundle_error) => {
                    let apps = place_apps(&mountpoint)?;
                    if apps.is_empty() {
                        Err(bundle_error)
                    } else {
                        Ok(apps)
                    }
                }
            }
        }
        Kind::Other => Err(format!("unsupported package type for {}", entry.name)),
    }
}

fn install_portable(
    entry: &PluginEntry,
    work: &WorkDir,
    payload: &Path,
    kind: Kind,
) -> Result<Placed, String> {
    match kind {
        Kind::Zip | Kind::TarGz => {
            let extract = work.path().join("extract");
            extract_archive(kind, payload, &extract)?;
            let dest = portable_root()?.join(&entry.id);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| access_error(e, parent))?;
            }
            remove_path(&dest).map_err(|e| access_error(e, &dest))?;
            copy_path(&extract, &dest).map_err(|e| access_error(e, &dest))?;
            Ok(vec![(dest, "portable".to_string())])
        }
        Kind::Dmg => {
            let mountpoint = work.path().join("mount");
            fs::create_dir_all(&mountpoint).map_err(io_msg)?;
            let _mount = mount_dmg(payload, &mountpoint)?;
            let apps = place_apps(&mountpoint)?;
            if apps.is_empty() {
                Err(format!("no .app found in the {} disk image", entry.name))
            } else {
                Ok(apps)
            }
        }
        _ => Err(format!("portable apps must be a zip, tar.gz or dmg ({})", entry.name)),
    }
}

fn extract_archive(kind: Kind, src: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(io_msg)?;
    match kind {
        Kind::Zip => extract_zip(src, dest),
        Kind::TarGz => extract_tar_gz(src, dest),
        _ => Err("not an archive".to_string()),
    }
}

fn extract_zip(src: &Path, dest: &Path) -> Result<(), String> {
    let file = fs::File::open(src).map_err(io_msg)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("invalid zip: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("zip entry error: {e}"))?;
        let Some(relative) = entry.enclosed_name() else {
            return Err("zip contains an unsafe path".to_string());
        };
        let out = dest.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(io_msg)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(io_msg)?;
        }
        if entry.is_symlink() {
            let mut target = String::new();
            entry.read_to_string(&mut target).map_err(io_msg)?;
            let link_target = Path::new(target.trim());
            if !symlink_stays_inside(dest, &out, link_target) {
                return Err("zip contains a symlink that points outside the archive".to_string());
            }
            make_symlink(link_target, &out)?;
            continue;
        }
        let mut file = fs::File::create(&out).map_err(io_msg)?;
        io::copy(&mut entry, &mut file).map_err(io_msg)?;
        apply_mode(&out, entry.unix_mode())?;
    }
    Ok(())
}

/// Lexical check that a relative symlink target never climbs above the extraction root, which
/// would let a later archive entry be written outside it through the link.
fn symlink_stays_inside(root: &Path, link: &Path, target: &Path) -> bool {
    use std::path::Component;
    if target.is_absolute() {
        return false;
    }
    let Some(parent) = link.parent().and_then(|p| p.strip_prefix(root).ok()) else {
        return false;
    };
    let mut depth = parent.components().count() as i64;
    for part in target.components() {
        match part {
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::Normal(_) => depth += 1,
            _ => {}
        }
    }
    true
}

fn extract_tar_gz(src: &Path, dest: &Path) -> Result<(), String> {
    let file = fs::File::open(src).map_err(io_msg)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    archive.set_preserve_permissions(true);
    archive
        .unpack(dest)
        .map_err(|e| format!("tar extraction failed: {e}"))
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, link).map_err(io_msg)
}

#[cfg(not(unix))]
fn make_symlink(_target: &Path, _link: &Path) -> Result<(), String> {
    // Windows bundles do not rely on symlinks; skipping keeps extraction portable.
    Ok(())
}

#[cfg(unix)]
fn apply_mode(path: &Path, mode: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(bits) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(bits & 0o777)).map_err(io_msg)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: Option<u32>) -> Result<(), String> {
    Ok(())
}

fn collect_bundles(dir: &Path, depth: u32, out: &mut Vec<(PathBuf, &'static str)>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(format) = bundle_format(&name) {
            out.push((path, format));
            continue;
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            collect_bundles(&path, depth + 1, out);
        }
    }
}

fn top_level_with_suffix(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(suffix)
        })
        .map(|e| e.path())
        .collect()
}

fn place_bundles(db: &Db, entry: &PluginEntry, root: &Path) -> Result<Placed, String> {
    let mut found = Vec::new();
    collect_bundles(root, 0, &mut found);
    if found.is_empty() {
        return Err(format!(
            "no VST3, CLAP or AU bundles found in the {} package",
            entry.name
        ));
    }
    let mut placed = Vec::new();
    for (bundle, format) in found {
        let dir = target_dir(db, format)?;
        placed.push((place_bundle(&bundle, &dir)?, format.to_string()));
    }
    Ok(placed)
}

fn place_apps(root: &Path) -> Result<Placed, String> {
    let apps = top_level_with_suffix(root, ".app");
    if apps.is_empty() {
        return Ok(Vec::new());
    }
    let dest = dirs::home_dir()
        .ok_or("no home folder")?
        .join("Applications");
    let mut placed = Vec::new();
    for app in apps {
        placed.push((place_bundle(&app, &dest)?, "portable".to_string()));
    }
    Ok(placed)
}

/// Copies `src` into `dir` under a hidden staging name, then renames it into place.
/// If the folder is protected (`PermissionDenied`), the same steps run once under a single
/// native admin prompt instead of failing.
fn place_bundle(src: &Path, dir: &Path) -> Result<PathBuf, String> {
    let name: &OsStr = src.file_name().ok_or("bundle without a file name")?;
    match place_bundle_direct(src, dir, name) {
        Ok(path) => Ok(path),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => elevated_place(src, dir, name),
        Err(e) => Err(access_error(e, dir)),
    }
}

fn staging_name(name: &OsStr) -> OsString {
    let mut staging = OsString::from(".");
    staging.push(name);
    staging.push(".patchbay-new");
    staging
}

fn place_bundle_direct(src: &Path, dir: &Path, name: &OsStr) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let final_path = dir.join(name);
    let staging = dir.join(staging_name(name));
    remove_path(&staging)?;
    if let Err(e) = copy_path(src, &staging) {
        let _ = remove_path(&staging);
        return Err(e);
    }
    remove_path(&final_path)?;
    fs::rename(&staging, &final_path)?;
    Ok(final_path)
}

/// Windows: runs the staged copy in an elevated PowerShell started with the `runas` verb
/// (the same ShellExecute mechanism as a UAC prompt). One prompt covers the whole copy.
#[cfg(windows)]
fn elevated_place(src: &Path, dir: &Path, name: &OsStr) -> Result<PathBuf, String> {
    let final_path = dir.join(name);
    let staging = dir.join(staging_name(name));
    let lit = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "''"));
    let body = format!(
        "$ErrorActionPreference = 'Stop'\r\n\
         New-Item -ItemType Directory -Force -Path {dir} | Out-Null\r\n\
         if (Test-Path -LiteralPath {st}) {{ Remove-Item -LiteralPath {st} -Recurse -Force }}\r\n\
         Copy-Item -LiteralPath {src} -Destination {st} -Recurse -Force\r\n\
         if (Test-Path -LiteralPath {fi}) {{ Remove-Item -LiteralPath {fi} -Recurse -Force }}\r\n\
         Rename-Item -LiteralPath {st} -NewName {nm}\r\n",
        dir = lit(dir),
        st = lit(&staging),
        src = lit(src),
        fi = lit(&final_path),
        nm = lit(Path::new(name)),
    );
    let script_path = std::env::temp_dir().join(format!(
        "patchbay-place-{}-{}.ps1",
        std::process::id(),
        now_unix()
    ));
    fs::write(&script_path, body).map_err(io_msg)?;
    let script_arg = script_path.to_string_lossy().replace('\'', "''");
    // The inner quotes keep a user name with spaces (e.g. "MANISH G S") as one argument.
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
    result.map_err(|e| {
        format!("administrator permission was not granted or the copy failed ({e}) at {}", dir.display())
    })?;
    Ok(final_path)
}

/// macOS: the same copy runs through `osascript ... with administrator privileges`, which
/// shows the standard authentication prompt once.
#[cfg(target_os = "macos")]
fn elevated_place(src: &Path, dir: &Path, name: &OsStr) -> Result<PathBuf, String> {
    let final_path = dir.join(name);
    let staging = dir.join(staging_name(name));
    let q = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"));
    let sh = format!(
        "mkdir -p {d} && rm -rf {st} && cp -Rp {src} {st} && rm -rf {fi} && mv {st} {fi}",
        d = q(dir),
        st = q(&staging),
        src = q(src),
        fi = q(&final_path),
    );
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        sh.replace('\\', "\\\\").replace('"', "\\\"")
    );
    run_program("osascript", &["-e".to_string(), script]).map_err(|e| {
        format!("administrator permission was not granted or the copy failed ({e}) at {}", dir.display())
    })?;
    Ok(final_path)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn elevated_place(_src: &Path, dir: &Path, _name: &OsStr) -> Result<PathBuf, String> {
    Err(format!("administrator rights are required to write to {}", dir.display()))
}

/// Recursive copy that keeps symlinks as symlinks (macOS bundles depend on them).
fn copy_path(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        return copy_symlink(src, dst);
    }
    if meta.is_dir() {
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_path(&entry.path(), &dst.join(entry.file_name()))?;
        }
        return Ok(());
    }
    fs::copy(src, dst).map(|_| ())
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    let target = fs::read_link(src)?;
    std::os::unix::fs::symlink(target, dst)
}

#[cfg(not(unix))]
fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    fs::copy(src, dst).map(|_| ())
}

fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Runs a vendor installer silently with the manifest's flags.
fn run_installer(path: &Path, kind: Kind, flags: Option<&str>) -> Result<(), String> {
    let extra: Vec<String> = flags
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let file = path.to_string_lossy().into_owned();
    match kind {
        Kind::Msi => {
            let mut args = vec!["/i".to_string(), file];
            if extra.is_empty() {
                args.extend(["/qn".to_string(), "/norestart".to_string()]);
            } else {
                args.extend(extra);
            }
            run_program("msiexec", &args)
        }
        Kind::Exe => run_program(&file, &extra),
        Kind::Pkg => run_pkg(path),
        _ => Err("not an installer package".to_string()),
    }
}

fn run_program(program: &str, args: &[String]) -> Result<(), String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("{program} exited with {status}")),
        Err(e) if cfg!(windows) && e.raw_os_error() == Some(740) => run_elevated(program, args),
        Err(e) => Err(format!("could not start {program}: {e}")),
    }
}

/// Installers that request administrator rights fail with ERROR_ELEVATION_REQUIRED (740).
/// Retry through PowerShell's RunAs verb; Windows shows its own UAC consent prompt.
#[cfg(windows)]
fn run_elevated(program: &str, args: &[String]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    // Start-Process joins array items with spaces and does not quote them, so items that contain
    // whitespace (the temp path of the downloaded MSI, for one) need their own double quotes.
    let arg_list = args
        .iter()
        .map(|a| {
            if a.contains(char::is_whitespace) {
                quote(&format!("\"{a}\""))
            } else {
                quote(a)
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        "$p = Start-Process -FilePath {} -ArgumentList @({}) -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        quote(program),
        arg_list
    );
    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        script.as_str(),
    ]);
    cmd.creation_flags(CREATE_NO_WINDOW);
    let status = cmd.status().map_err(io_msg)?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("elevated {program} exited with {status}"))
    }
}

#[cfg(not(windows))]
fn run_elevated(program: &str, _args: &[String]) -> Result<(), String> {
    Err(format!("{program} needs elevation, which is not supported here"))
}

#[cfg(target_os = "macos")]
fn run_pkg(path: &Path) -> Result<(), String> {
    let shell = format!(
        "installer -pkg '{}' -target /",
        path.to_string_lossy().replace('\'', "'\\''")
    );
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        shell.replace('\\', "\\\\").replace('"', "\\\"")
    );
    run_program("osascript", &["-e".to_string(), script])
}

#[cfg(not(target_os = "macos"))]
fn run_pkg(_path: &Path) -> Result<(), String> {
    Err("pkg installers are only supported on macOS".to_string())
}
