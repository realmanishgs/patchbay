//! Background update checker. Fetches the catalog with `If-None-Match`, so an unchanged
//! index costs one 304 response, then recomputes which installed plugins are outdated.

use crate::catalog::{is_newer, Catalog};
use crate::db::Db;
use crate::{refresh_tray, scanner, AppState, INDEX_URL};
use reqwest::{header, StatusCode};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

pub const POLL_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Long-running task: check now, then every `POLL_INTERVAL`.
pub async fn run(app: AppHandle) {
    loop {
        if let Err(e) = check_once(&app).await {
            eprintln!("[poller] check failed: {e}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// One full cycle: refresh the catalog, adopt pre-existing plugins on first run,
/// then update the tray badge. Returns the number of plugins with updates.
pub async fn check_once(app: &AppHandle) -> Result<usize, String> {
    let state = app.state::<AppState>();
    let catalog = fetch_catalog(&state).await?;
    if state.db.get_setting(scanner::SCAN_FLAG)?.is_none() {
        let adopted = scanner::first_scan(&state.db, &catalog)?;
        eprintln!("[scanner] adopted {adopted} existing plugin(s)");
    }
    *state
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned".to_string())? = Some(catalog);
    let pending = refresh_badge(app)?;
    let _ = app.emit("catalog-updated", pending);
    Ok(pending)
}

/// Recompute updates from the cached catalog (no network) and refresh the tray.
pub fn refresh_badge(app: &AppHandle) -> Result<usize, String> {
    let state = app.state::<AppState>();
    let cached = state
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned".to_string())?
        .clone();
    let Some(catalog) = cached else {
        return Ok(0);
    };
    let pending = pending_updates(&state.db, &catalog)?;
    refresh_tray(app, pending.len());
    Ok(pending.len())
}

/// Ids of installed plugins whose catalog version is newer than the installed one.
pub fn pending_updates(db: &Db, catalog: &Catalog) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for installed in db.list_installed()? {
        let Some(entry) = catalog.plugins.iter().find(|p| p.id == installed.id) else {
            continue;
        };
        let has_build = entry
            .assets
            .contains_key(crate::catalog::platform_key());
        if has_build && is_newer(&entry.latest.version, &installed.installed_version) {
            out.push(installed.id);
        }
    }
    Ok(out)
}

/// Catalog from memory, or fetched when this session has none yet.
pub async fn current_catalog(state: &AppState) -> Result<Catalog, String> {
    let cached = state
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned".to_string())?
        .clone();
    match cached {
        Some(catalog) => Ok(catalog),
        None => fetch_catalog(state).await,
    }
}

/// ETag-conditional fetch. A 304 reuses the cached body; network failures fall back to it too,
/// so the gallery still works offline once it has synced once.
pub async fn fetch_catalog(state: &AppState) -> Result<Catalog, String> {
    let (etag, _, cached) = state.db.sync_state()?;
    let now = now_unix();

    let mut request = state
        .http
        .get(INDEX_URL)
        .header(header::ACCEPT, "application/json");
    if let (Some(tag), Some(_)) = (&etag, &cached) {
        request = request.header(header::IF_NONE_MATCH, tag.as_str());
    }

    match request.send().await {
        Ok(resp) if resp.status() == StatusCode::NOT_MODIFIED => {
            let body = cached.ok_or("304 received without a cached catalog")?;
            state.db.touch_checked(now)?;
            parse_catalog(&body)
        }
        Ok(resp) if resp.status().is_success() => {
            let new_etag = resp
                .headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let body = resp
                .text()
                .await
                .map_err(|e| format!("reading the catalog failed: {e}"))?;
            let catalog = parse_catalog(&body)?;
            state.db.save_fetch(new_etag.as_deref(), now, &body)?;
            Ok(catalog)
        }
        Ok(resp) => fallback(cached, format!("catalog returned HTTP {}", resp.status())),
        Err(e) => fallback(cached, format!("catalog request failed: {e}")),
    }
}

fn fallback(cached: Option<String>, reason: String) -> Result<Catalog, String> {
    match cached {
        Some(body) => {
            eprintln!("[poller] {reason}; using the cached catalog");
            parse_catalog(&body)
        }
        None => Err(reason),
    }
}

fn parse_catalog(body: &str) -> Result<Catalog, String> {
    serde_json::from_str(body).map_err(|e| format!("invalid catalog JSON: {e}"))
}
