# VST Gallery

An open-source gallery and silent updater for VST3 / CLAP / AU plugins.

- **$0 hosting.** Metadata is a static `index.min.json` built by GitHub Actions and served from GitHub Pages (or jsDelivr). Binaries are never proxied: the app downloads straight from upstream GitHub Releases or vendor CDNs.
- **Tray-first.** A Rust + Tauri v2 daemon sits in the system tray. The WebView is created when you open the gallery and destroyed when you close it.
- **Quiet updates.** A background task polls the index every 4 hours with `ETag` / `If-None-Match`, so an unchanged catalog costs a single `304`. Outdated plugins turn the tray icon into a red-dot badge.
- **Safe installs.** Downloads are verified against SHA-256, staged in the OS temp folder, and bundles are moved into place atomically. Temp files are removed on success, failure and early return.

## Layout

```
registry/                 static catalog (no server)
  schema/plugin.schema.json
  plugins/*.json          one manifest per plugin (16 seeded)
  scripts/compile_registry.py
.github/workflows/build-index.yml   cron every 6h + on push -> GitHub Pages
client/                   Vite + TypeScript + Tailwind gallery UI
client/src-tauri/         Rust core: tray, poller, installer, scanner, SQLite
```

## Build the catalog locally

```powershell
python -m pip install -r registry/requirements.txt
$env:GITHUB_TOKEN = gh auth token   # optional, avoids API rate limits
python registry/scripts/compile_registry.py --plugins registry/plugins --schema registry/schema/plugin.schema.json --out dist
```

Manifests with `"enabled": false` are skipped and listed in the output. Six seeded plugins are disabled because their upstream release source could not be verified; see each manifest's `notes`.

## Run the app

Prerequisites: Node 20+, Rust stable (MSVC on Windows), and the Tauri v2 system dependencies for your OS.

Set the catalog URL at build time, for example:

```powershell
$env:VST_GALLERY_INDEX_URL = "https://<your-user>.github.io/<your-repo>/index.min.json"
```

Without it, the app points at a reserved `.invalid` host and shows a "catalog request failed" message.

```powershell
cd client
npm install
# one-time: generate the platform icon set from a 512x512 PNG
npx tauri icon path\to\app-icon.png
npm run tauri dev
```

Release build: `npm run tauri build`.

## Federated and commercial vendors

- `federated_feed_url` and `vendor_manifest_url` are read by the compiler. Each feed is a JSON object with a `plugins` array in the index shape (`id`, `name`, `vendor`, `category`, `latest`, and `assets` keyed by platform with an `https` URL and a `sha256`). Translate Open Audio Stack feeds into that shape before publishing.
- `auth_type: "license_key_header"` exchanges the locally stored key with the vendor's `exchange_url` for a short-lived signed download URL. `bearer_token` sends the stored token as a bearer header. Keys are stored in the OS keychain, never in SQLite or in the index.

## Known limits

- Installing into system folders (`C:\Program Files\Common Files\VST3`, `/Library/Audio/Plug-Ins`) needs administrator rights. The app reports the permission error and lets you choose a writable folder in Settings. Vendor installers that require elevation trigger a UAC prompt.
- Silent flags in several manifests are inferred from the installer type (Inno Setup / NSIS) and are marked in `notes`. Verify them before relying on silent installs.
- Plugins found on first launch are adopted at the catalog's current version, so they are not redownloaded. A later release is offered as an update as usual.
