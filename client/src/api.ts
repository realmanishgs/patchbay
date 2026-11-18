import { invoke } from "@tauri-apps/api/core";

export interface AssetInfo {
  name: string;
  url: string;
  size: number | null;
  sha256: string | null;
  kind: string;
}

export interface AuthBlock {
  type: string;
  key_name: string | null;
  exchange_url: string | null;
}

export interface PluginEntry {
  id: string;
  name: string;
  vendor: string;
  category: "synth" | "effect" | "sampler" | "daw" | "utility";
  license: string;
  open_source: boolean;
  homepage: string | null;
  description: string | null;
  about: string | null;
  tags: string[];
  screenshot: string | null;
  install_strategy: string;
  silent_flags: string | null;
  auth_type: string;
  auth: AuthBlock | null;
  formats: string[];
  latest: { version: string; published_at: string | null; release_url: string | null };
  assets: Record<string, AssetInfo>;
  source: string | null;
}

export interface InstalledPlugin {
  id: string;
  installed_version: string;
  install_path: string;
  format: string;
  updated_at: number;
  source: string;
}

export interface VendorView {
  key_name: string;
  label: string;
  auth_type: string;
  configured: boolean;
}

export interface GalleryView {
  catalog: { schema_version: number; generated_at: string | null; plugins: PluginEntry[] };
  installed: InstalledPlugin[];
  updates: string[];
  last_checked: number | null;
  paths: { vst3: string; clap: string; au: string; vst2: string };
  vendors: VendorView[];
}

export interface Progress {
  id: string;
  phase: "download" | "verify" | "install" | "done" | string;
  received: number;
  total: number | null;
}

export interface UpdateOutcome {
  id: string;
  ok: boolean;
  error: string | null;
}

export interface LibraryItem {
  path: string;
  name: string;
  format: string;
  size: number;
  managed_id: string | null;
}

export interface Leftover {
  path: string;
  kind: "data" | "content" | string;
  size: number;
}

export interface Inspection {
  item: LibraryItem;
  leftovers: Leftover[];
  vendor_uninstallers: { display_name: string }[];
}

export interface RemoveReport {
  removed: string[];
  failed: { path: string; error: string }[];
  freed: number;
}

export interface ImportReport {
  imported: string[];
  skipped: { path: string; reason: string }[];
}

export const api = {
  getGallery: () => invoke<GalleryView>("get_gallery"),
  install: (id: string) => invoke<InstalledPlugin>("install_plugin", { id }),
  updateAll: () => invoke<UpdateOutcome[]>("update_all"),
  checkNow: () => invoke<number>("check_now"),
  savePaths: (paths: { vst3: string; clap: string; au: string; vst2: string }) =>
    invoke<void>("save_paths", paths),
  setVendorKey: (keyName: string, authType: string, secret: string) =>
    invoke<void>("set_vendor_key", { keyName, authType, secret }),
  clearVendorKey: (keyName: string) => invoke<void>("clear_vendor_key", { keyName }),
  listLibrary: () => invoke<LibraryItem[]>("list_library"),
  inspectPlugin: (path: string) => invoke<Inspection>("inspect_plugin", { path }),
  removePlugin: (path: string, leftovers: string[]) =>
    invoke<RemoveReport>("remove_plugin", { path, leftovers, confirmed: true }),
  runVendorUninstaller: (path: string, displayName: string) =>
    invoke<void>("run_vendor_uninstaller", { path, displayName }),
  importPlugins: (paths: string[]) => invoke<ImportReport>("import_plugins", { paths }),
};
