//! SQLite persistence for installed plugins, sync state, settings and vendor-credential
//! metadata. Secret values never touch SQLite: they live in the OS keyring.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

const KEYRING_SERVICE: &str = "patchbay";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS installed_plugins (
    id                TEXT PRIMARY KEY,
    installed_version TEXT    NOT NULL,
    install_path     TEXT    NOT NULL,
    format           TEXT    NOT NULL,
    updated_at       INTEGER NOT NULL,
    source           TEXT    NOT NULL DEFAULT 'install'
);
CREATE TABLE IF NOT EXISTS sync_state (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    etag         TEXT,
    last_checked INTEGER,
    catalog_body TEXT
);
INSERT OR IGNORE INTO sync_state (id) VALUES (1);
CREATE TABLE IF NOT EXISTS vendor_credentials (
    key_name   TEXT PRIMARY KEY,
    auth_type  TEXT    NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

#[derive(Debug, Clone, Serialize)]
pub struct InstalledPlugin {
    pub id: String,
    pub installed_version: String,
    pub install_path: String,
    pub format: String,
    pub updated_at: i64,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VendorCredential {
    pub key_name: String,
    pub auth_type: String,
    pub updated_at: i64,
}

pub struct Db {
    conn: Mutex<Connection>,
}

fn to_s<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn map_installed(r: &rusqlite::Row<'_>) -> rusqlite::Result<InstalledPlugin> {
    Ok(InstalledPlugin {
        id: r.get(0)?,
        installed_version: r.get(1)?,
        install_path: r.get(2)?,
        format: r.get(3)?,
        updated_at: r.get(4)?,
        source: r.get(5)?,
    })
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(to_s)?;
        conn.execute_batch(SCHEMA).map_err(to_s)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "database lock poisoned".to_string())?;
        f(&conn).map_err(to_s)
    }

    pub fn sync_state(&self) -> Result<(Option<String>, Option<i64>, Option<String>), String> {
        self.with(|c| {
            c.query_row(
                "SELECT etag, last_checked, catalog_body FROM sync_state WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
        })
    }

    pub fn touch_checked(&self, now: i64) -> Result<(), String> {
        self.with(|c| {
            c.execute(
                "UPDATE sync_state SET last_checked = ?1 WHERE id = 1",
                params![now],
            )
            .map(|_| ())
        })
    }

    pub fn save_fetch(&self, etag: Option<&str>, now: i64, body: &str) -> Result<(), String> {
        self.with(|c| {
            c.execute(
                "UPDATE sync_state SET etag = ?1, last_checked = ?2, catalog_body = ?3 WHERE id = 1",
                params![etag, now, body],
            )
            .map(|_| ())
        })
    }

    pub fn list_installed(&self) -> Result<Vec<InstalledPlugin>, String> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, installed_version, install_path, format, updated_at, source
                 FROM installed_plugins ORDER BY id",
            )?;
            let rows = stmt.query_map([], map_installed)?;
            rows.collect()
        })
    }

    pub fn get_installed(&self, id: &str) -> Result<Option<InstalledPlugin>, String> {
        self.with(|c| {
            c.query_row(
                "SELECT id, installed_version, install_path, format, updated_at, source
                 FROM installed_plugins WHERE id = ?1",
                [id],
                map_installed,
            )
            .optional()
        })
    }

    pub fn upsert_installed(&self, p: &InstalledPlugin) -> Result<(), String> {
        self.with(|c| {
            c.execute(
                "INSERT INTO installed_plugins
                    (id, installed_version, install_path, format, updated_at, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                    installed_version = excluded.installed_version,
                    install_path      = excluded.install_path,
                    format            = excluded.format,
                    updated_at        = excluded.updated_at,
                    source            = excluded.source",
                params![
                    p.id,
                    p.installed_version,
                    p.install_path,
                    p.format,
                    p.updated_at,
                    p.source
                ],
            )
            .map(|_| ())
        })
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, String> {
        self.with(|c| {
            c.query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [key],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        self.with(|c| {
            c.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map(|_| ())
        })
    }

    pub fn list_vendors(&self) -> Result<Vec<VendorCredential>, String> {
        self.with(|c| {
            let mut stmt =
                c.prepare("SELECT key_name, auth_type, updated_at FROM vendor_credentials")?;
            let rows = stmt.query_map([], |r| {
                Ok(VendorCredential {
                    key_name: r.get(0)?,
                    auth_type: r.get(1)?,
                    updated_at: r.get(2)?,
                })
            })?;
            rows.collect()
        })
    }

    pub fn upsert_vendor(&self, key_name: &str, auth_type: &str, now: i64) -> Result<(), String> {
        self.with(|c| {
            c.execute(
                "INSERT INTO vendor_credentials (key_name, auth_type, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key_name) DO UPDATE SET auth_type = excluded.auth_type,
                    updated_at = excluded.updated_at",
                params![key_name, auth_type, now],
            )
            .map(|_| ())
        })
    }

    pub fn remove_vendor(&self, key_name: &str) -> Result<(), String> {
        self.with(|c| {
            c.execute(
                "DELETE FROM vendor_credentials WHERE key_name = ?1",
                [key_name],
            )
            .map(|_| ())
        })
    }
}

fn entry(key_name: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, key_name).map_err(to_s)
}

pub fn store_secret(key_name: &str, secret: &str) -> Result<(), String> {
    entry(key_name)?.set_password(secret).map_err(to_s)
}

pub fn read_secret(key_name: &str) -> Result<String, String> {
    entry(key_name)?
        .get_password()
        .map_err(|e| format!("no license or token stored for '{key_name}': {e}"))
}

pub fn delete_secret(key_name: &str) -> Result<(), String> {
    match entry(key_name)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(to_s(e)),
    }
}
