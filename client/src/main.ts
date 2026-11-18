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

function mountShell(): void {
  $("app").innerHTML = `
    <div class="mx-auto max-w-6xl px-4 py-5">
      <header class="flex flex-wrap items-center gap-3">
        <div class="flex items-center gap-2">
          <span class="h-2.5 w-2.5 rounded-full bg-violet-500"></span>
          <h1 class="text-lg font-semibold tracking-tight">VST Gallery</h1>
        </div>
        <input id="search" type="search" autocomplete="off" spellcheck="false"
          placeholder="Search plugins…" class="field min-w-0 flex-1 sm:max-w-xs" />
        <select id="f-category" class="field" aria-label="Category">
          <option value="all">All categories</option>
          <option value="synth">Synths</option>
          <option value="effect">Effects</option>
          <option value="sampler">Samplers</option>
          <option value="daw">DAWs &amp; hosts</option>
          <option value="utility">Utilities</option>
        </select>
        <select id="f-format" class="field" aria-label="Format">
          <option value="all">All formats</option>
          <option value="vst3">VST3</option>
          <option value="clap">CLAP</option>
          <option value="au">AU</option>
        </select>
        <select id="f-license" class="field" aria-label="License">
          <option value="all">Open Source &amp; Commercial</option>
          <option value="open">Open Source</option>
          <option value="commercial">Commercial</option>
        </select>
        <div class="ml-auto flex gap-2">
          <button class="btn btn-ghost" data-action="check" id="btn-check">Check now</button>
          <button class="btn btn-ghost" data-action="settings">Settings</button>
        </div>
      </header>
      <div id="banner" class="mt-4"></div>
      <main id="grid" class="mt-4 grid gap-3 sm:grid-cols-2 xl:grid-cols-3"></main>
      <footer id="status" class="mt-6 text-xs text-zinc-500"></footer>
    </div>
    <div id="modal"></div>`;

  $<HTMLInputElement>("search").addEventListener("input", (e) => {
    state.query = (e.target as HTMLInputElement).value;
    renderGrid();
  });
  const filters = [
    ["f-category", "category"],
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
    return `<span class="text-xs text-zinc-500">No build for this OS</span>`;
  }
  if (!inst) {
    return `<button class="btn btn-primary" data-action="install" data-id="${esc(p.id)}">Install</button>`;
  }
  if (view.updates.includes(p.id)) {
    return `<button class="btn btn-primary" data-action="install" data-id="${esc(p.id)}">Update to ${esc(p.latest.version)}</button>`;
  }
  return `<span class="text-xs text-emerald-400">Installed · up to date</span>`;
}

function cardHtml(p: PluginEntry, view: GalleryView, installed: Map<string, InstalledPlugin>): string {
  const inst = installed.get(p.id);
  const licenseBadge = p.open_source
    ? `<span class="rounded bg-emerald-950 px-1.5 py-0.5 text-[11px] text-emerald-300">Open Source</span>`
    : `<span class="rounded bg-amber-950 px-1.5 py-0.5 text-[11px] text-amber-300">Commercial</span>`;
  const formats = p.formats
    .map((f) => `<span class="rounded border border-zinc-700 px-1.5 py-0.5 text-[11px] uppercase text-zinc-400">${esc(f)}</span>`)
    .join(" ");
  return `<article class="flex flex-col justify-between gap-4 rounded-xl border border-zinc-800 bg-zinc-900/60 p-4 transition hover:border-zinc-700">
    <div class="space-y-2">
      <div class="flex items-start justify-between gap-2">
        <div class="min-w-0">
          <h2 class="truncate font-semibold text-zinc-100">${esc(p.name)}</h2>
          <p class="truncate text-xs text-zinc-500">${esc(p.vendor)}</p>
        </div>
        <span class="shrink-0 rounded bg-zinc-800 px-1.5 py-0.5 font-mono text-[11px] text-zinc-300">v${esc(inst?.installed_version ?? p.latest.version)}</span>
      </div>
      <p class="line-clamp-2 text-sm text-zinc-400">${esc(p.description ?? "")}</p>
      <div class="flex flex-wrap items-center gap-1.5">
        <span class="rounded bg-violet-950 px-1.5 py-0.5 text-[11px] capitalize text-violet-300">${esc(p.category)}</span>
        ${licenseBadge}
        ${formats}
      </div>
    </div>
    <div class="min-h-[2.25rem]">${actionHtml(p, view, inst)}</div>
  </article>`;
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
  status.textContent = `Showing ${plugins.length} of ${view.catalog.plugins.length} plugins · last checked ${fmtTime(view.last_checked)}`;
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

function renderAll(): void {
  renderBanner();
  renderGrid();
  renderModal();
}

/* ---------- actions ---------- */

async function load(): Promise<void> {
  try {
    state.view = await api.getGallery();
  } catch (e) {
    toast(`Could not load the catalog: ${String(e)}`, true);
  }
  renderAll();
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
  await load();
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
  await load();
}

async function clearKey(keyName: string): Promise<void> {
  try {
    await api.clearVendorKey(keyName);
    toast("License key removed");
  } catch (e) {
    toast(String(e), true);
  }
  await load();
}

/* ---------- wiring ---------- */

let gridFrame = 0;
function scheduleGrid(): void {
  if (gridFrame) return;
  gridFrame = window.requestAnimationFrame(() => {
    gridFrame = 0;
    renderGrid();
  });
}

function bindEvents(): void {
  document.addEventListener("click", (ev) => {
    const el = (ev.target as HTMLElement).closest<HTMLElement>("[data-action]");
    if (!el) return;
    switch (el.dataset.action) {
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
    if (ev.key === "Escape" && state.settingsOpen) {
      state.settingsOpen = false;
      renderModal();
    }
  });
}

async function init(): Promise<void> {
  mountShell();
  bindEvents();
  await listen<Progress>("install-progress", (event) => {
    state.progress.set(event.payload.id, event.payload);
    scheduleGrid();
  });
  await listen<number>("catalog-updated", () => {
    void load();
  });
  await load();
}

void init();
