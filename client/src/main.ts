import "./styles.css";
import { listen } from "@tauri-apps/api/event";
import {
  api,
  type GalleryView,
  type InstalledPlugin,
  type PluginEntry,
  type Progress,
} from "./api";
import { fuzzyScore } from "./fuzzy";
import { applyPlatformChrome, installNativeGuards } from "./native";

interface Toast {
  text: string;
  error: boolean;
}

interface UiState {
  view: GalleryView | null;
  query: string;
  category: string;
  format: string;
  license: string;
  progress: Map<string, Progress>;
  settingsOpen: boolean;
  toast: Toast | null;
  checking: boolean;
  updatingAll: boolean;
  detailId: string | null;
  preview: { id: string; url: string | null; loading: boolean; error: string | null } | null;
}

const state: UiState = {
  view: null,
  query: "",
  category: "all",
  format: "all",
  license: "all",
  progress: new Map(),
  settingsOpen: false,
  toast: null,
  checking: false,
  updatingAll: false,
  detailId: null,
  preview: null,
};

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing element #${id}`);
  return el as T;
};

function esc(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function platformKey(): "win_x64" | "mac_universal" | "linux_x64" {
  const ua = navigator.userAgent;
  if (/Windows/i.test(ua)) return "win_x64";
  if (/Macintosh|Mac OS/i.test(ua)) return "mac_universal";
  return "linux_x64";
}

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

function fmtTime(unix: number | null): string {
  if (!unix) return "never";
  return new Date(unix * 1000).toLocaleString();
}

function toast(text: string, error = false): void {
  state.toast = { text, error };
  renderBanner();
  window.setTimeout(() => {
    if (state.toast?.text === text) {
      state.toast = null;
      renderBanner();
    }
  }, 6000);
}

/* ---------- shell (rendered once; only the regions below are re-rendered) ---------- */

const CATEGORIES: [string, string][] = [
  ["all", "All"],
  ["synth", "Synths"],
  ["effect", "Effects"],
  ["sampler", "Samplers"],
  ["daw", "DAWs & hosts"],
  ["utility", "Utilities"],
];

function mountShell(): void {
  $("app").innerHTML = `
    <div class="mx-auto max-w-5xl px-6 pb-16 pt-8">
      <header class="flex flex-wrap items-center gap-4">
        <div class="flex items-center gap-2.5">
          <span class="logo-dot"></span>
          <h1 class="text-[15px] font-semibold tracking-tight">Patchbay</h1>
        </div>
        <input id="search" type="search" autocomplete="off" spellcheck="false"
          placeholder="Search plugins" class="search min-w-0 flex-1 sm:max-w-xs" />
        <div class="ml-auto flex items-center gap-1">
          <button class="ghost-btn" data-action="check" id="btn-check">Check now</button>
          <button class="ghost-btn" data-action="settings">Settings</button>
        </div>
      </header>
      <nav class="mt-7 flex flex-wrap items-center gap-1.5">
        <div id="chips" class="flex flex-wrap gap-1.5"></div>
        <div class="ml-auto flex items-center gap-1">
          <select id="f-format" class="mini-select" aria-label="Format">
            <option value="all">Any format</option>
            <option value="vst3">VST3</option>
            <option value="clap">CLAP</option>
            <option value="au">AU</option>
          </select>
          <select id="f-license" class="mini-select" aria-label="License">
            <option value="all">Any license</option>
            <option value="open">Open source</option>
            <option value="commercial">Commercial</option>
          </select>
        </div>
      </nav>
      <div id="banner" class="mt-5 empty:hidden"></div>
      <main id="grid" class="mt-5 grid gap-3 sm:grid-cols-2 lg:grid-cols-3"></main>
      <footer id="status" class="mt-8 text-center text-xs text-zinc-600"></footer>
    </div>
    <div id="drawer"></div>
    <div id="modal"></div>`;
  renderChips();

  $<HTMLInputElement>("search").addEventListener("input", (e) => {
    state.query = (e.target as HTMLInputElement).value;
    renderGrid();
  });
  const filters = [
    ["f-format", "format"],
    ["f-license", "license"],
  ] as const;
  for (const [id, key] of filters) {
    $<HTMLSelectElement>(id).addEventListener("change", (e) => {
      state[key] = (e.target as HTMLSelectElement).value;
      renderGrid();
    });
  }
}

function renderChips(): void {
  $("chips").innerHTML = CATEGORIES.map(
    ([value, label]) =>
      `<button class="chip" data-action="category" data-value="${value}" aria-pressed="${state.category === value}">${esc(label)}</button>`,
  ).join("");
}

/* ---------- rendering ---------- */

function installedMap(view: GalleryView): Map<string, InstalledPlugin> {
  return new Map(view.installed.map((i) => [i.id, i]));
}

function visiblePlugins(view: GalleryView): PluginEntry[] {
  const q = state.query.trim();
  const scored = view.catalog.plugins
    .filter((p) => state.category === "all" || p.category === state.category)
    .filter((p) => state.format === "all" || p.formats.includes(state.format))
    .filter((p) => state.license === "all" || (state.license === "open") === p.open_source)
    .map((p) => ({
      p,
      score: q
        ? Math.max(
            fuzzyScore(q, p.name) * 2,
            fuzzyScore(q, p.vendor),
            fuzzyScore(q, p.category),
            fuzzyScore(q, p.description ?? ""),
          )
        : 0,
    }))
    .filter((x) => !q || x.score >= 0);
  scored.sort((a, b) => (q ? b.score - a.score : a.p.name.localeCompare(b.p.name)));
  return scored.map((x) => x.p);
}

function progressHtml(p: Progress): string {
  const label =
    p.phase === "download"
      ? "Downloading"
      : p.phase === "verify"
        ? "Verifying SHA-256"
        : p.phase === "install"
          ? "Installing"
          : "Finishing";
  const hasTotal = p.total !== null && p.total > 0;
  const pct = hasTotal ? Math.min(100, (p.received / (p.total as number)) * 100) : null;
  const bytes = hasTotal
    ? `${fmtBytes(p.received)} / ${fmtBytes(p.total as number)}`
    : fmtBytes(p.received);
  const bar =
    pct === null
      ? `<div class="h-1.5 w-full overflow-hidden rounded-full bg-zinc-800"><div class="h-full w-1/3 animate-pulse rounded-full bg-violet-500"></div></div>`
      : `<div class="h-1.5 w-full overflow-hidden rounded-full bg-zinc-800"><div class="h-full rounded-full bg-violet-500" style="width:${pct.toFixed(1)}%"></div></div>`;
  return `<div class="space-y-1.5">
    <div class="flex justify-between text-xs text-zinc-400"><span>${label}</span><span>${bytes}</span></div>
    ${bar}
  </div>`;
}

function actionHtml(p: PluginEntry, view: GalleryView, inst: InstalledPlugin | undefined): string {
  const progress = state.progress.get(p.id);
  if (progress) return progressHtml(progress);
  if (!p.assets[platformKey()]) {
    return `<span class="text-xs text-zinc-600">No build for this OS</span>`;
  }
  if (!inst) {
    return `<button class="btn btn-primary" data-action="install" data-id="${esc(p.id)}">Install</button>`;
  }
  if (view.updates.includes(p.id)) {
    return `<button class="btn btn-primary" data-action="install" data-id="${esc(p.id)}">Update</button>`;
  }
  return `<span class="text-xs text-zinc-500">Up to date</span>`;
}

function formatsLabel(p: PluginEntry): string {
  return p.formats.map((f) => f.toUpperCase()).join(" · ");
}

function cardHtml(p: PluginEntry, view: GalleryView, installed: Map<string, InstalledPlugin>): string {
  const inst = installed.get(p.id);
  const dot = !inst
    ? ""
    : view.updates.includes(p.id)
      ? `<span class="status-dot bg-red-500" title="Update available"></span>`
      : `<span class="status-dot bg-emerald-500" title="Installed"></span>`;
  return `<article class="card" data-action="open" data-id="${esc(p.id)}" tabindex="0" role="button" aria-label="${esc(p.name)} details">
    <div class="flex items-start justify-between gap-3">
      <div class="min-w-0">
        <h2 class="truncate text-[15px] font-medium text-zinc-100">${esc(p.name)}</h2>
        <p class="mt-0.5 truncate text-xs text-zinc-500">${esc(p.vendor)} · <span class="capitalize">${esc(p.category)}</span></p>
      </div>
      ${dot}
    </div>
    <p class="mt-3 line-clamp-2 text-[13px] leading-relaxed text-zinc-400">${esc(p.description ?? "")}</p>
    <div class="mt-4 flex min-h-8 items-center justify-between gap-3">
      <span class="truncate font-mono text-[11px] text-zinc-600">${esc(formatsLabel(p))}</span>
      <div class="shrink-0">${actionHtml(p, view, inst)}</div>
    </div>
  </article>`;
}

function fact(label: string, value: string): string {
  return `<div><dt class="text-[11px] uppercase tracking-wider text-zinc-600">${esc(label)}</dt><dd class="mt-0.5 text-sm text-zinc-300">${esc(value)}</dd></div>`;
}

const STRATEGY_LABEL: Record<string, string> = {
  bundle_extract: "Copies plugin bundle",
  silent_installer: "Silent installer",
  portable_app: "Portable app",
};

function previewHtml(p: PluginEntry): string {
  if (!p.screenshot) return "";
  const pv = state.preview?.id === p.id ? state.preview : null;
  if (pv?.url) {
    return `<img src="${esc(pv.url)}" alt="${esc(p.name)} screenshot" class="w-full rounded-xl border border-white/5" decoding="async" />`;
  }
  if (pv?.loading) return `<div class="preview-box animate-pulse">Loading preview…</div>`;
  return `<button class="preview-box hover:border-zinc-600" data-action="preview" data-id="${esc(p.id)}">
    <span>Load preview image</span>
    <span class="text-[11px] text-zinc-600">${pv?.error ? esc(pv.error) : "Fetched on demand, never stored"}</span>
  </button>`;
}

function detailHtml(p: PluginEntry, view: GalleryView): string {
  const inst = installedMap(view).get(p.id);
  const about = (p.about ?? p.description ?? "")
    .split(/\n{2,}/)
    .filter(Boolean)
    .map((para) => `<p>${esc(para)}</p>`)
    .join("");
  const tags = p.tags.map((tg) => `<span class="tag">${esc(tg)}</span>`).join("");
  const platforms = (["win_x64", "mac_universal", "linux_x64"] as const)
    .filter((k) => p.assets[k])
    .map((k) => (k === "win_x64" ? "Windows" : k === "mac_universal" ? "macOS" : "Linux"))
    .join(", ");
  const published = p.latest.published_at ? new Date(p.latest.published_at).toLocaleDateString() : "—";
  const home = p.homepage
    ? `<div class="mt-6 flex items-center justify-between gap-3 rounded-xl border border-white/5 px-3 py-2">
        <span class="truncate font-mono text-xs text-zinc-500">${esc(p.homepage.replace(/^https?:\/\//, ""))}</span>
        <button class="ghost-btn shrink-0" data-action="copy" data-text="${esc(p.homepage)}">Copy link</button>
      </div>`
    : "";
  return `
    <div class="fixed inset-0 z-40 bg-black/60 backdrop-blur-sm" data-action="close-detail"></div>
    <aside role="dialog" aria-modal="true" aria-label="${esc(p.name)}" class="drawer">
      <div class="flex items-start justify-between gap-4">
        <div class="min-w-0">
          <h2 class="truncate text-xl font-semibold tracking-tight">${esc(p.name)}</h2>
          <p class="mt-1 text-sm text-zinc-500">${esc(p.vendor)}</p>
        </div>
        <button class="ghost-btn" data-action="close-detail" aria-label="Close">Close</button>
      </div>
      <div class="mt-5 flex min-h-9 items-center gap-3" id="drawer-action">${actionHtml(p, view, inst)}</div>
      ${tags ? `<div class="mt-5 flex flex-wrap gap-1.5">${tags}</div>` : ""}
      <div class="mt-5 space-y-3 text-sm leading-relaxed text-zinc-400">${about}</div>
      ${p.screenshot ? `<div class="mt-6" id="drawer-preview">${previewHtml(p)}</div>` : ""}
      <dl class="mt-7 grid grid-cols-2 gap-x-6 gap-y-4 border-t border-white/5 pt-6">
        ${fact("Latest", `v${p.latest.version}`)}
        ${fact("Installed", inst ? `v${inst.installed_version}` : "Not installed")}
        ${fact("License", `${p.license}${p.open_source ? "" : " (commercial)"}`)}
        ${fact("Formats", formatsLabel(p) || "—")}
        ${fact("Platforms", platforms || "—")}
        ${fact("Released", published)}
        ${fact("Install", STRATEGY_LABEL[p.install_strategy] ?? p.install_strategy)}
        ${p.auth_type !== "none" ? fact("Needs", "Vendor license key") : ""}
      </dl>
      ${home}
    </aside>`;
}

function findPlugin(id: string | null): PluginEntry | undefined {
  return id ? state.view?.catalog.plugins.find((p) => p.id === id) : undefined;
}

function renderDetail(): void {
  const drawer = $("drawer");
  const p = findPlugin(state.detailId);
  if (!p || !state.view) {
    drawer.innerHTML = "";
    return;
  }
  drawer.innerHTML = detailHtml(p, state.view);
}

/** Cheap in-place refresh used for install progress so the drawer keeps its scroll position. */
function renderDetailAction(): void {
  const p = findPlugin(state.detailId);
  const slot = document.getElementById("drawer-action");
  if (!p || !slot || !state.view) return;
  slot.innerHTML = actionHtml(p, state.view, installedMap(state.view).get(p.id));
}

function renderPreview(): void {
  const p = findPlugin(state.detailId);
  const slot = document.getElementById("drawer-preview");
  if (p && slot) slot.innerHTML = previewHtml(p);
}

function openDetail(id: string): void {
  closePreview();
  state.detailId = id;
  renderDetail();
}

function closeDetail(): void {
  closePreview();
  state.detailId = null;
  renderDetail();
}

function closePreview(): void {
  if (state.preview?.url) URL.revokeObjectURL(state.preview.url);
  state.preview = null;
}

const MAX_PREVIEW_BYTES = 6 * 1024 * 1024;

/** Fetches the screenshot once, on request, bypassing the HTTP cache; the blob is dropped on close. */
async function loadPreview(id: string): Promise<void> {
  const p = findPlugin(id);
  if (!p?.screenshot || state.preview?.loading) return;
  closePreview();
  state.preview = { id, url: null, loading: true, error: null };
  renderPreview();
  try {
    const res = await fetch(p.screenshot, { cache: "no-store", referrerPolicy: "no-referrer" });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const blob = await res.blob();
    if (blob.size > MAX_PREVIEW_BYTES) throw new Error("image too large");
    if (state.detailId !== id) return;
    state.preview = { id, url: URL.createObjectURL(blob), loading: false, error: null };
  } catch (e) {
    if (state.detailId !== id) return;
    state.preview = { id, url: null, loading: false, error: `Could not load (${String(e)})` };
  }
  renderPreview();
}

function renderGrid(): void {
  const grid = $("grid");
  const status = $("status");
  const view = state.view;
  if (!view) {
    grid.innerHTML = "";
    return;
  }
  const installed = installedMap(view);
  const plugins = visiblePlugins(view);
  grid.innerHTML = plugins.length
    ? plugins.map((p) => cardHtml(p, view, installed)).join("")
    : `<p class="col-span-full py-16 text-center text-sm text-zinc-500">No plugins match these filters.</p>`;
  status.textContent = `${plugins.length} of ${view.catalog.plugins.length} plugins · checked ${fmtTime(view.last_checked)}`;
}

function renderBanner(): void {
  const banner = $("banner");
  const parts: string[] = [];
  const updates = state.view?.updates.length ?? 0;
  if (updates > 0) {
    parts.push(`
      <div class="flex flex-wrap items-center justify-between gap-3 rounded-xl border border-violet-800 bg-violet-950/40 px-4 py-3">
        <div class="flex items-center gap-2 text-sm text-violet-100">
          <span class="h-2 w-2 rounded-full bg-red-500"></span>
          ${updates} update${updates === 1 ? "" : "s"} available
        </div>
        <button class="btn btn-primary" data-action="update-all" ${state.updatingAll ? "disabled" : ""}>
          Update All (${updates})
        </button>
      </div>`);
  }
  if (state.toast) {
    const color = state.toast.error
      ? "border-red-900 bg-red-950/50 text-red-200"
      : "border-zinc-800 bg-zinc-900 text-zinc-200";
    parts.push(`<div role="status" class="rounded-xl border px-4 py-3 text-sm ${color}">${esc(state.toast.text)}</div>`);
  }
  banner.innerHTML = parts.join("");
  const check = document.getElementById("btn-check") as HTMLButtonElement | null;
  if (check) {
    check.disabled = state.checking;
    check.textContent = state.checking ? "Checking…" : "Check now";
  }
}

function settingsHtml(view: GalleryView): string {
  const vendorRows = view.vendors.length
    ? view.vendors
        .map(
          (v) => `
          <div class="space-y-2 rounded-lg border border-zinc-800 p-3">
            <div class="flex items-center justify-between text-sm">
              <span class="font-medium text-zinc-200">${esc(v.label)}</span>
              <span class="text-xs ${v.configured ? "text-emerald-400" : "text-zinc-500"}">${v.configured ? "Key stored" : "No key"}</span>
            </div>
            <div class="flex gap-2">
              <input type="password" autocomplete="off" data-key-input="${esc(v.key_name)}"
                placeholder="License key" class="field min-w-0 flex-1" />
              <button class="btn btn-ghost" data-action="save-key" data-key="${esc(v.key_name)}" data-auth="${esc(v.auth_type)}">Save</button>
              ${v.configured ? `<button class="btn btn-ghost" data-action="clear-key" data-key="${esc(v.key_name)}">Remove</button>` : ""}
            </div>
          </div>`,
        )
        .join("")
    : `<p class="text-sm text-zinc-500">No plugin in the catalog needs a vendor license.</p>`;

  return `
    <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/70 p-4">
      <div role="dialog" aria-modal="true" aria-labelledby="settings-title"
        class="max-h-[90vh] w-full max-w-lg overflow-y-auto rounded-2xl border border-zinc-800 bg-zinc-900 p-5 shadow-2xl">
        <div class="mb-4 flex items-center justify-between">
          <h2 id="settings-title" class="text-base font-semibold">Settings</h2>
          <button class="btn btn-ghost" data-action="close-settings" aria-label="Close">✕</button>
        </div>

        <section class="space-y-3">
          <h3 class="text-xs font-semibold uppercase tracking-wide text-zinc-500">Install folders</h3>
          <p class="text-xs text-zinc-500">Leave a field empty to use the system default. Changing a folder does not move existing plugins.</p>
          <label class="block space-y-1 text-sm"><span class="text-zinc-400">VST3</span>
            <input id="path-vst3" class="field w-full" value="${esc(view.paths.vst3)}" spellcheck="false" /></label>
          <label class="block space-y-1 text-sm"><span class="text-zinc-400">CLAP</span>
            <input id="path-clap" class="field w-full" value="${esc(view.paths.clap)}" spellcheck="false" /></label>
          <label class="block space-y-1 text-sm"><span class="text-zinc-400">AU (macOS)</span>
            <input id="path-au" class="field w-full" value="${esc(view.paths.au)}" spellcheck="false" /></label>
          <div class="flex justify-end"><button class="btn btn-primary" data-action="save-paths">Save folders</button></div>
        </section>

        <section class="mt-6 space-y-3">
          <h3 class="text-xs font-semibold uppercase tracking-wide text-zinc-500">Vendor licenses</h3>
          <p class="text-xs text-zinc-500">Keys are stored in your OS keychain and exchanged with the vendor for short-lived download links.</p>
          ${vendorRows}
        </section>
      </div>
    </div>`;
}

function renderModal(): void {
  const modal = $("modal");
  if (!state.settingsOpen || !state.view) {
    modal.innerHTML = "";
    return;
  }
  modal.innerHTML = settingsHtml(state.view);
}

function renderAll(refreshModal: boolean): void {
  renderBanner();
  renderGrid();
  if (state.detailId) {
    if (findPlugin(state.detailId)) renderDetailAction();
    else closeDetail();
  }
  // Re-rendering the Settings dialog discards whatever the user has typed, so background
  // refreshes leave it alone; only actions that change its contents ask for a refresh.
  if (refreshModal) renderModal();
}

/* ---------- actions ---------- */

async function load(refreshModal = false): Promise<void> {
  try {
    state.view = await api.getGallery();
  } catch (e) {
    toast(`Could not load the catalog: ${String(e)}`, true);
  }
  renderAll(refreshModal);
}

async function install(id: string): Promise<void> {
  if (state.progress.has(id)) return;
  state.progress.set(id, { id, phase: "download", received: 0, total: null });
  renderGrid();
  try {
    const record = await api.install(id);
    toast(`${record.id} installed (v${record.installed_version})`);
  } catch (e) {
    toast(`Install failed: ${String(e)}`, true);
  } finally {
    state.progress.delete(id);
    await load();
  }
}

async function updateAll(): Promise<void> {
  if (state.updatingAll) return;
  state.updatingAll = true;
  renderBanner();
  try {
    const outcomes = await api.updateAll();
    const failed = outcomes.filter((o) => !o.ok);
    if (failed.length) {
      toast(`${outcomes.length - failed.length} updated, ${failed.length} failed: ${failed[0].error ?? ""}`, true);
    } else {
      toast(outcomes.length ? `${outcomes.length} plugin(s) updated` : "Nothing to update");
    }
  } catch (e) {
    toast(`Update failed: ${String(e)}`, true);
  } finally {
    state.updatingAll = false;
    await load();
  }
}

async function checkNow(): Promise<void> {
  state.checking = true;
  renderBanner();
  try {
    const pending = await api.checkNow();
    toast(pending ? `${pending} update(s) found` : "Everything is up to date");
  } catch (e) {
    toast(`Check failed: ${String(e)}`, true);
  } finally {
    state.checking = false;
    await load();
  }
}

async function savePaths(): Promise<void> {
  const value = (id: string) => $<HTMLInputElement>(id).value.trim();
  try {
    await api.savePaths({ vst3: value("path-vst3"), clap: value("path-clap"), au: value("path-au") });
    toast("Install folders saved");
  } catch (e) {
    toast(String(e), true);
  }
  await load(true);
}

async function saveKey(keyName: string, authType: string): Promise<void> {
  const input = document.querySelector<HTMLInputElement>(`input[data-key-input="${CSS.escape(keyName)}"]`);
  const secret = input?.value.trim() ?? "";
  if (!secret) {
    toast("Enter a license key first", true);
    return;
  }
  try {
    await api.setVendorKey(keyName, authType, secret);
    toast("License key saved to the OS keychain");
  } catch (e) {
    toast(String(e), true);
  }
  await load(true);
}

async function clearKey(keyName: string): Promise<void> {
  try {
    await api.clearVendorKey(keyName);
    toast("License key removed");
  } catch (e) {
    toast(String(e), true);
  }
  await load(true);
}

/* ---------- wiring ---------- */

let gridFrame = 0;
function scheduleGrid(): void {
  if (gridFrame) return;
  gridFrame = window.requestAnimationFrame(() => {
    gridFrame = 0;
    renderGrid();
    renderDetailAction();
  });
}

function bindEvents(): void {
  document.addEventListener("click", (ev) => {
    const el = (ev.target as HTMLElement).closest<HTMLElement>("[data-action]");
    if (!el) return;
    switch (el.dataset.action) {
      case "open":
        openDetail(el.dataset.id ?? "");
        break;
      case "close-detail":
        closeDetail();
        break;
      case "preview":
        void loadPreview(el.dataset.id ?? "");
        break;
      case "copy":
        void navigator.clipboard
          ?.writeText(el.dataset.text ?? "")
          .then(() => toast("Link copied"))
          .catch(() => toast("Could not copy the link", true));
        break;
      case "category":
        state.category = el.dataset.value ?? "all";
        renderChips();
        renderGrid();
        break;
      case "install":
        void install(el.dataset.id ?? "");
        break;
      case "update-all":
        void updateAll();
        break;
      case "check":
        void checkNow();
        break;
      case "settings":
        state.settingsOpen = true;
        renderModal();
        break;
      case "close-settings":
        state.settingsOpen = false;
        renderModal();
        break;
      case "save-paths":
        void savePaths();
        break;
      case "save-key":
        void saveKey(el.dataset.key ?? "", el.dataset.auth ?? "license_key_header");
        break;
      case "clear-key":
        void clearKey(el.dataset.key ?? "");
        break;
    }
  });

  document.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") {
      if (state.settingsOpen) {
        state.settingsOpen = false;
        renderModal();
      } else if (state.detailId) {
        closeDetail();
      }
    }
    const card = (ev.target as HTMLElement).closest<HTMLElement>("article[data-action='open']");
    if (card && (ev.key === "Enter" || ev.key === " ") && ev.target === card) {
      ev.preventDefault();
      openDetail(card.dataset.id ?? "");
    }
  });
}

async function init(): Promise<void> {
  installNativeGuards();
  applyPlatformChrome();
  mountShell();
  bindEvents();
  await listen<Progress>("install-progress", (event) => {
    const { id, phase } = event.payload;
    // `done` and `error` are terminal; clearing here also covers updates started from the tray.
    if (phase === "done" || phase === "error") state.progress.delete(id);
    else state.progress.set(id, event.payload);
    scheduleGrid();
  });
  await listen<number>("catalog-updated", () => {
    void load();
  });
  await load();
}

void init();
