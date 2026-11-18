#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Patchbay desktop daemon.
//!
//! Runs as a tray-only process. The gallery WebView is created on demand and destroyed when
//! its window closes, so idle memory stays at the tray + SQLite footprint.

mod catalog;
mod db;
mod installer;
mod poller;
mod scanner;

use catalog::Catalog;
use db::{Db, InstalledPlugin};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{
    image::Image,
    menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, Wry,
};

/// Where the static catalog is published. Defaults to the GitHub Pages site of the registry
/// repo; override at compile time with `patchbay_INDEX_URL` (e.g. a jsDelivr mirror).
pub const INDEX_URL: &str = match option_env!("patchbay_INDEX_URL") {
    Some(url) => url,
    None => "https://realmanishgs.github.io/patchbay/index.min.json",
};

const TRAY_ID: &str = "main";
const GALLERY_LABEL: &str = "gallery";
/// Monochrome tray glyph, and the same glyph with a red update dot.
const TRAY_ICON: &[u8] = include_bytes!("../icons/tray.png");
const TRAY_ALERT_ICON: &[u8] = include_bytes!("../icons/tray-alert.png");

/// Shared application state, managed by Tauri.
pub struct AppState {
    pub db: Db,
    pub http: reqwest::Client,
    pub catalog: Mutex<Option<Catalog>>,
    pub busy: Mutex<HashSet<String>>,
}

#[derive(Serialize)]
struct ScanPaths {
    vst3: String,
    clap: String,
    au: String,
}

#[derive(Serialize)]
struct VendorView {
    key_name: String,
    label: String,
    auth_type: String,
    configured: bool,
}

#[derive(Serialize)]
struct GalleryView {
    catalog: Catalog,
    installed: Vec<InstalledPlugin>,
    updates: Vec<String>,
    last_checked: Option<i64>,
    paths: ScanPaths,
    vendors: Vec<VendorView>,
}

#[derive(Serialize)]
pub struct UpdateOutcome {
    id: String,
    ok: bool,
    error: Option<String>,
}

/// Tray glyph for the current state: plain when up to date, with the red dot when updates wait.
fn tray_icon_for(pending: usize) -> tauri::Result<Image<'static>> {
    Image::from_bytes(if pending > 0 { TRAY_ALERT_ICON } else { TRAY_ICON })
}

fn build_menu(app: &AppHandle, pending: usize) -> tauri::Result<Menu<Wry>> {
    let open = MenuItem::with_id(app, "open", "Open Gallery", true, None::<&str>)?;
    let update_label = if pending > 0 {
        format!("Update All ({pending})")
    } else {
        "Update All".to_string()
    };
    let update = MenuItem::with_id(app, "update_all", update_label, pending > 0, None::<&str>)?;
    let check = MenuItem::with_id(app, "check_now", "Check Now", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep_a = PredefinedMenuItem::separator(app)?;
    let sep_b = PredefinedMenuItem::separator(app)?;
    let items: Vec<&dyn IsMenuItem<Wry>> = vec![&open, &sep_a, &update, &check, &sep_b, &quit];
    Menu::with_items(app, &items)
}

/// Re-applies icon, tooltip and menu to reflect the number of pending updates.
pub fn refresh_tray(app: &AppHandle, pending: usize) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    if let Ok(icon) = tray_icon_for(pending) {
        let _ = tray.set_icon(Some(icon));
    }
    let tooltip = if pending > 0 {
        format!("Patchbay - {pending} update(s) available")
    } else {
        "Patchbay".to_string()
    };
    let _ = tray.set_tooltip(Some(tooltip));
    if let Ok(menu) = build_menu(app, pending) {
        let _ = tray.set_menu(Some(menu));
    }
}

fn open_gallery(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(GALLERY_LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    // No hide-on-close: closing the window destroys its WebView and frees its memory.
    let built = WebviewWindowBuilder::new(app, GALLERY_LABEL, WebviewUrl::App("index.html".into()))
        .title("Patchbay")
        .inner_size(1080.0, 720.0)
        .min_inner_size(760.0, 520.0)
        .center()
        .build();
    if let Err(e) = built {
        eprintln!("[gallery] cannot open window: {e}");
    }
}

fn on_menu(app: &AppHandle, id: &str) {
    match id {
        "open" => open_gallery(app),
        "update_all" => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = update_all_inner(&handle).await {
                    eprintln!("[update] {e}");
                }
            });
        }
        "check_now" => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = poller::check_once(&handle).await {
                    eprintln!("[poller] {e}");
                }
            });
        }
        "quit" => app.exit(0),
        _ => {}
    }
}

async fn update_all_inner(app: &AppHandle) -> Result<Vec<UpdateOutcome>, String> {
    let ids = {
        let state = app.state::<AppState>();
        let catalog = poller::current_catalog(&state).await?;
        poller::pending_updates(&state.db, &catalog)?
    };
    let mut outcomes = Vec::with_capacity(ids.len());
    for id in ids {
        let outcome = match installer::install(app, &id).await {
            Ok(_) => UpdateOutcome { id, ok: true, error: None },
            Err(e) => UpdateOutcome { id, ok: false, error: Some(e) },
        };
        outcomes.push(outcome);
    }
    let _ = poller::refresh_badge(app);
    Ok(outcomes)
}

fn scan_paths(db: &Db) -> Result<ScanPaths, String> {
    let show = |format: &str| {
        installer::target_dir(db, format)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    Ok(ScanPaths {
        vst3: show("vst3"),
        clap: show("clap"),
        au: show("au"),
    })
}

fn validate_key_name(key_name: &str) -> Result<(), String> {
    let ok = !key_name.is_empty()
        && key_name.len() <= 64
        && key_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if ok {
        Ok(())
    } else {
        Err("invalid credential name".to_string())
    }
}

#[tauri::command]
async fn get_gallery(app: AppHandle) -> Result<GalleryView, String> {
    let state = app.state::<AppState>();
    let catalog = poller::current_catalog(&state).await?;
    let installed = state.db.list_installed()?;
    let updates = poller::pending_updates(&state.db, &catalog)?;
    let (_, last_checked, _) = state.db.sync_state()?;
    let stored = state.db.list_vendors()?;

    let mut vendors: Vec<VendorView> = Vec::new();
    for plugin in &catalog.plugins {
        let Some(auth) = plugin.auth.as_ref().filter(|a| a.kind != "none") else {
            continue;
        };
        let Some(key) = &auth.key_name else {
            continue;
        };
        if vendors.iter().any(|v| &v.key_name == key) {
            continue;
        }
        vendors.push(VendorView {
            key_name: key.clone(),
            label: plugin.vendor.clone(),
            auth_type: auth.kind.clone(),
            configured: stored.iter().any(|s| &s.key_name == key),
        });
    }

    Ok(GalleryView {
        catalog,
        installed,
        updates,
        last_checked,
        paths: scan_paths(&state.db)?,
        vendors,
    })
}

#[tauri::command]
async fn install_plugin(app: AppHandle, id: String) -> Result<InstalledPlugin, String> {
    let result = installer::install(&app, &id).await;
    let _ = poller::refresh_badge(&app);
    result
}

#[tauri::command]
async fn update_all(app: AppHandle) -> Result<Vec<UpdateOutcome>, String> {
    update_all_inner(&app).await
}

#[tauri::command]
async fn check_now(app: AppHandle) -> Result<usize, String> {
    poller::check_once(&app).await
}

#[tauri::command]
fn save_paths(app: AppHandle, vst3: String, clap: String, au: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    for (format, value) in [("vst3", vst3), ("clap", clap), ("au", au)] {
        let value = value.trim().to_string();
        if !value.is_empty() && !std::path::Path::new(&value).is_absolute() {
            return Err(format!("the {format} folder must be an absolute path"));
        }
        state.db.set_setting(&format!("path.{format}"), &value)?;
    }
    Ok(())
}

#[tauri::command]
fn set_vendor_key(
    app: AppHandle,
    key_name: String,
    auth_type: String,
    secret: String,
) -> Result<(), String> {
    validate_key_name(&key_name)?;
    let secret = secret.trim();
    if secret.is_empty() {
        return Err("the license key is empty".to_string());
    }
    db::store_secret(&key_name, secret)?;
    app.state::<AppState>()
        .db
        .upsert_vendor(&key_name, &auth_type, poller::now_unix())
}

#[tauri::command]
fn clear_vendor_key(app: AppHandle, key_name: String) -> Result<(), String> {
    validate_key_name(&key_name)?;
    db::delete_secret(&key_name)?;
    app.state::<AppState>().db.remove_vendor(&key_name)
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            let data_dir = handle.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db = Db::open(&data_dir.join("gallery.sqlite3"))?;
            let http = reqwest::Client::builder()
                .user_agent(concat!("patchbay/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(120))
                .build()?;
            app.manage(AppState {
                db,
                http,
                catalog: Mutex::new(None),
                busy: Mutex::new(HashSet::new()),
            });

            let menu = build_menu(&handle, 0)?;
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(tray_icon_for(0)?)
                .tooltip("Patchbay")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event: MenuEvent| on_menu(app, event.id.as_ref()))
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        open_gallery(tray.app_handle());
                    }
                })
                .build(app)?;

            tauri::async_runtime::spawn(poller::run(handle.clone()));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_gallery,
            install_plugin,
            update_all,
            check_now,
            save_paths,
            set_vendor_key,
            clear_vendor_key
        ])
        .build(tauri::generate_context!())
        .expect("error while building Patchbay")
        .run(|_app, event| {
            // The app lives in the tray: keep running when the last window closes.
            if let RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}
