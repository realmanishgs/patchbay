/**
 * Library view: every plugin on disk (from any source), a guarded remove flow that also
 * offers the data a plugin left behind, and an importer that gathers loose plugin files
 * into the plugin folders.
 */
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { api, type ImportReport, type Inspection, type LibraryItem, type RemoveReport } from "./api";
import { esc, fmtBytes } from "./util";

interface Context {
  toast: (text: string, error?: boolean) => void;
  reloadGallery: () => Promise<void>;
  vst2Folder: () => string;
  isActive: () => boolean;
}

type Dialog =
  | { step: "inspect"; path: string; inspection: Inspection | null; picked: Set<string>; error: string | null }
  | { step: "confirm"; inspection: Inspection; picked: Set<string>; understood: boolean; busy: boolean }
  | { step: "result"; name: string; report: RemoveReport };

let ctx: Context;
let items: LibraryItem[] | null = null;
let loading = false;
let query = "";
let dragging = false;
let lastImport: ImportReport | null = null;
let dialog: Dialog | null = null;

const host = () => document.getElementById("library-view");
const dialogHost = () => document.getElementById("confirm");

/* ---------- data ---------- */

export async function refresh(): Promise<void> {
  if (loading) return;
  loading = true;
  render();
  try {
    items = await api.listLibrary();
  } catch (e) {
    ctx.toast(`Could not scan your plugin folders: ${String(e)}`, true);
  } finally {
    loading = false;
    render();
  }
}

export function setQuery(value: string): void {
  query = value.trim().toLowerCase();
  render();
}

export function ensureLoaded(): void {
  if (items === null) void refresh();
  else render();
}

/** Closes the remove dialog if one is open. Returns true when it handled the keypress. */
export function closeDialog(): boolean {
  if (!dialog) return false;
  if (dialog.step === "confirm" && dialog.busy) return true;
  dialog = null;
  renderDialog();
  return true;
}

/* ---------- list ---------- */

function rowHtml(item: LibraryItem): string {
  const source = item.managed_id
    ? `<span class="tag text-violet-300">Patchbay</span>`
    : "";
  return `<li class="row">
    <div class="min-w-0 flex-1">
      <div class="flex items-center gap-2">
        <span class="truncate text-sm font-medium text-zinc-100">${esc(item.name)}</span>
        <span class="tag uppercase">${esc(item.format)}</span>${source}
      </div>
      <p class="mt-0.5 truncate font-mono text-[11px] text-zinc-600" title="${esc(item.path)}">${esc(item.path)}</p>
    </div>
    <span class="shrink-0 text-xs tabular-nums text-zinc-500">${fmtBytes(item.size)}</span>
    <button class="ghost-btn shrink-0 text-red-300 hover:text-red-200" data-lib="remove" data-path="${esc(item.path)}">Remove…</button>
  </li>`;
}

function importSummary(): string {
  if (!lastImport) return "";
  const lines: string[] = [];
  if (lastImport.imported.length) {
    lines.push(`<p class="text-emerald-300">Added ${lastImport.imported.length} plugin${lastImport.imported.length === 1 ? "" : "s"}.</p>`);
  }
  for (const s of lastImport.skipped) {
    lines.push(`<p class="truncate text-zinc-500" title="${esc(s.path)}">Skipped ${esc(s.path)} — ${esc(s.reason)}</p>`);
  }
  return `<div class="mt-3 space-y-1 text-xs">${lines.join("")}</div>`;
}

function render(): void {
  const el = host();
  if (!el) return;
  const all = items ?? [];
  const shown = all.filter(
    (i) => !query || i.name.toLowerCase().includes(query) || i.format.includes(query) || i.path.toLowerCase().includes(query),
  );
  const total = all.reduce((sum, i) => sum + i.size, 0);
  const folder = ctx.vst2Folder();
  el.innerHTML = `
    <section class="drop ${dragging ? "drop-active" : ""}" id="lib-drop">
      <p class="text-sm text-zinc-300">Drop plugins or folders here</p>
      <p class="mt-1 text-xs text-zinc-600">.dll (VST2) · .vst3 · .clap · .component — copied, never moved</p>
      <div class="mt-4 flex w-full max-w-md gap-2">
        <input id="lib-path" class="search min-w-0 flex-1" spellcheck="false" autocomplete="off"
          placeholder="…or paste a full path" />
        <button class="ghost-btn" data-lib="add-path">Add</button>
      </div>
      ${importSummary()}
      ${folder ? `<p class="mt-4 max-w-md text-center text-[11px] leading-relaxed text-zinc-600">VST2 .dll files are gathered in
        <span class="font-mono text-zinc-500">${esc(folder)}</span>.
        Add that folder to your DAW's plugin scan paths.
        <button class="underline hover:text-zinc-300" data-lib="copy" data-text="${esc(folder)}">Copy path</button></p>` : ""}
    </section>

    <div class="mt-8 flex items-center justify-between">
      <p class="text-xs text-zinc-500">${loading ? "Scanning…" : `${shown.length} of ${all.length} plugins · ${fmtBytes(total)} on disk`}</p>
      <button class="ghost-btn" data-lib="rescan" ${loading ? "disabled" : ""}>Rescan</button>
    </div>
    <ul class="mt-3 space-y-2">
      ${
        items === null
          ? ""
          : shown.length
            ? shown.map(rowHtml).join("")
            : `<li class="py-12 text-center text-sm text-zinc-600">No plugins found.</li>`
      }
    </ul>`;
}

/* ---------- remove dialog ---------- */

function leftoverRow(l: { path: string; kind: string; size: number }, picked: Set<string>): string {
  const label = l.kind === "content" ? "Presets / content" : "Settings & data";
  return `<label class="flex cursor-pointer items-start gap-3 rounded-lg border border-white/5 p-3">
    <input type="checkbox" class="mt-1 accent-violet-500" data-lib="pick" data-path="${esc(l.path)}" ${picked.has(l.path) ? "checked" : ""} />
    <span class="min-w-0 flex-1">
      <span class="flex justify-between gap-2 text-sm text-zinc-200"><span>${label}</span><span class="text-xs text-zinc-500">${fmtBytes(l.size)}</span></span>
      <span class="mt-0.5 block break-all font-mono text-[11px] text-zinc-600">${esc(l.path)}</span>
    </span>
  </label>`;
}

function shell(inner: string): string {
  return `<div class="fixed inset-0 z-[60] flex items-center justify-center bg-black/75 p-4">
    <div role="dialog" aria-modal="true" class="max-h-[90vh] w-full max-w-lg overflow-y-auto rounded-2xl border border-white/10 bg-zinc-950 p-6 shadow-2xl">${inner}</div>
  </div>`;
}

function inspectHtml(d: Extract<Dialog, { step: "inspect" }>): string {
  if (d.error) {
    return shell(`<h2 class="text-base font-semibold">Remove plugin</h2>
      <p class="mt-3 text-sm text-red-300">${esc(d.error)}</p>
      <div class="mt-5 flex justify-end"><button class="btn btn-ghost" data-lib="cancel">Close</button></div>`);
  }
  if (!d.inspection) {
    return shell(`<h2 class="text-base font-semibold">Remove plugin</h2>
      <p class="mt-3 animate-pulse text-sm text-zinc-500">Looking for related files…</p>`);
  }
  const { item, leftovers, vendor_uninstallers } = d.inspection;
  const vendor = vendor_uninstallers.length
    ? `<div class="mt-5 rounded-xl border border-violet-400/20 bg-violet-500/5 p-4">
        <p class="text-sm text-zinc-200">This plugin came with its own uninstaller.</p>
        <p class="mt-1 text-xs text-zinc-500">Using it is the cleanest option: it also removes registry entries Patchbay cannot touch.</p>
        ${vendor_uninstallers
          .map(
            (v) => `<button class="btn btn-ghost mt-3 w-full justify-between" data-lib="vendor" data-name="${esc(v.display_name)}" data-path="${esc(item.path)}">
              <span class="truncate">${esc(v.display_name)}</span><span class="text-xs text-zinc-500">Run uninstaller</span></button>`,
          )
          .join("")}
      </div>`
    : "";
  const related = leftovers.length
    ? `<h3 class="mt-6 text-xs font-semibold uppercase tracking-wide text-zinc-500">Related data found</h3>
       <p class="mt-1 text-xs text-zinc-600">Matched by folder name. Presets and content are left unticked because they may hold your own work.</p>
       <div class="mt-3 space-y-2">${leftovers.map((l) => leftoverRow(l, d.picked)).join("")}</div>`
    : `<p class="mt-6 text-xs text-zinc-600">No separate settings or content folders were found for this plugin.</p>`;
  return shell(`
    <h2 class="text-base font-semibold">Remove ${esc(item.name)}</h2>
    <div class="mt-4 rounded-lg border border-white/5 p-3">
      <div class="flex justify-between gap-2 text-sm text-zinc-200"><span>Plugin file <span class="tag uppercase">${esc(item.format)}</span></span><span class="text-xs text-zinc-500">${fmtBytes(item.size)}</span></div>
      <p class="mt-0.5 break-all font-mono text-[11px] text-zinc-600">${esc(item.path)}</p>
    </div>
    ${vendor}
    ${related}
    <div class="mt-6 flex justify-end gap-2">
      <button class="btn btn-ghost" data-lib="cancel">Cancel</button>
      <button class="btn btn-primary" data-lib="to-confirm">Continue</button>
    </div>`);
}

function confirmHtml(d: Extract<Dialog, { step: "confirm" }>): string {
  const { item, leftovers } = d.inspection;
  const chosen = leftovers.filter((l) => d.picked.has(l.path));
  const total = item.size + chosen.reduce((s, l) => s + l.size, 0);
  const list = [item.path, ...chosen.map((l) => l.path)]
    .map((p) => `<li class="break-all font-mono text-[11px] text-zinc-400">${esc(p)}</li>`)
    .join("");
  return shell(`
    <div class="rounded-xl border border-red-500/30 bg-red-500/10 p-4">
      <h2 class="text-base font-semibold text-red-200">Permanently delete ${1 + chosen.length} item${chosen.length ? "s" : ""}?</h2>
      <p class="mt-2 text-sm text-red-100/80">This frees ${fmtBytes(total)}. Files are deleted immediately — they do <strong>not</strong> go to the Recycle Bin and cannot be recovered.</p>
    </div>
    <ul class="mt-4 space-y-1.5">${list}</ul>
    <ul class="mt-4 list-disc space-y-1 pl-5 text-xs text-zinc-500">
      <li>Close your DAW first. A plugin that is loaded cannot be deleted.</li>
      <li>Projects that use this plugin will report it as missing.</li>
      <li>Windows may ask for administrator permission for protected folders.</li>
    </ul>
    <label class="mt-5 flex cursor-pointer items-center gap-2 text-sm text-zinc-300">
      <input type="checkbox" class="accent-red-500" data-lib="understood" ${d.understood ? "checked" : ""} ${d.busy ? "disabled" : ""} />
      I understand this cannot be undone
    </label>
    <div class="mt-5 flex justify-end gap-2">
      <button class="btn btn-ghost" data-lib="back" ${d.busy ? "disabled" : ""}>Back</button>
      <button class="btn danger-btn" data-lib="delete" ${d.understood && !d.busy ? "" : "disabled"}>${d.busy ? "Deleting…" : "Delete permanently"}</button>
    </div>`);
}

function resultHtml(d: Extract<Dialog, { step: "result" }>): string {
  const { report } = d;
  const failed = report.failed
    .map(
      (f) => `<li class="text-xs text-red-300"><span class="break-all font-mono">${esc(f.path)}</span><br />${esc(f.error)}</li>`,
    )
    .join("");
  return shell(`
    <h2 class="text-base font-semibold">${esc(d.name)}</h2>
    <p class="mt-3 text-sm text-zinc-300">${report.removed.length} item${report.removed.length === 1 ? "" : "s"} removed · ${fmtBytes(report.freed)} freed.</p>
    ${failed ? `<p class="mt-4 text-xs font-semibold uppercase tracking-wide text-red-300">Could not remove</p><ul class="mt-2 space-y-2">${failed}</ul>` : ""}
    <div class="mt-5 flex justify-end"><button class="btn btn-primary" data-lib="cancel">Done</button></div>`);
}

function renderDialog(): void {
  const el = dialogHost();
  if (!el) return;
  if (!dialog) {
    el.innerHTML = "";
    return;
  }
  el.innerHTML =
    dialog.step === "inspect" ? inspectHtml(dialog) : dialog.step === "confirm" ? confirmHtml(dialog) : resultHtml(dialog);
}

async function startRemove(path: string): Promise<void> {
  dialog = { step: "inspect", path, inspection: null, picked: new Set(), error: null };
  renderDialog();
  try {
    const inspection = await api.inspectPlugin(path);
    if (dialog?.step !== "inspect" || dialog.path !== path) return;
    dialog.inspection = inspection;
    // Settings and caches are pre-ticked; user content (presets, samples) is opt-in.
    dialog.picked = new Set(inspection.leftovers.filter((l) => l.kind === "data").map((l) => l.path));
  } catch (e) {
    if (dialog?.step === "inspect") dialog.error = String(e);
  }
  renderDialog();
}

async function performDelete(): Promise<void> {
  if (dialog?.step !== "confirm" || !dialog.understood || dialog.busy) return;
  const { inspection, picked } = dialog;
  dialog.busy = true;
  renderDialog();
  let report: RemoveReport;
  try {
    report = await api.removePlugin(inspection.item.path, [...picked]);
  } catch (e) {
    dialog = null;
    renderDialog();
    ctx.toast(`Remove failed: ${String(e)}`, true);
    return;
  }
  dialog = { step: "result", name: inspection.item.name, report };
  renderDialog();
  await refresh();
  await ctx.reloadGallery();
}

/* ---------- import ---------- */

async function importPaths(paths: string[]): Promise<void> {
  if (!paths.length) return;
  try {
    lastImport = await api.importPlugins(paths);
    ctx.toast(
      lastImport.imported.length
        ? `Added ${lastImport.imported.length} plugin${lastImport.imported.length === 1 ? "" : "s"}`
        : "Nothing was added",
      !lastImport.imported.length,
    );
  } catch (e) {
    ctx.toast(`Import failed: ${String(e)}`, true);
  }
  await refresh();
  await ctx.reloadGallery();
}

/* ---------- wiring ---------- */

export function initLibrary(context: Context): void {
  ctx = context;

  document.addEventListener("click", (ev) => {
    const el = (ev.target as HTMLElement).closest<HTMLElement>("[data-lib]");
    if (!el) return;
    switch (el.dataset.lib) {
      case "rescan":
        void refresh();
        break;
      case "remove":
        void startRemove(el.dataset.path ?? "");
        break;
      case "cancel":
        closeDialog();
        break;
      case "to-confirm":
        if (dialog?.step === "inspect" && dialog.inspection) {
          dialog = { step: "confirm", inspection: dialog.inspection, picked: dialog.picked, understood: false, busy: false };
          renderDialog();
        }
        break;
      case "back":
        if (dialog?.step === "confirm") {
          dialog = { step: "inspect", path: dialog.inspection.item.path, inspection: dialog.inspection, picked: dialog.picked, error: null };
          renderDialog();
        }
        break;
      case "delete":
        void performDelete();
        break;
      case "vendor":
        void api
          .runVendorUninstaller(el.dataset.path ?? "", el.dataset.name ?? "")
          .then(() => {
            dialog = null;
            renderDialog();
            ctx.toast("Uninstaller started. Press Rescan when it finishes.");
          })
          .catch((e) => ctx.toast(String(e), true));
        break;
      case "add-path": {
        const input = document.getElementById("lib-path") as HTMLInputElement | null;
        const value = input?.value ?? "";
        if (input) input.value = "";
        void importPaths(value.split(/\r?\n/).filter((s) => s.trim()));
        break;
      }
      case "copy":
        void navigator.clipboard
          ?.writeText(el.dataset.text ?? "")
          .then(() => ctx.toast("Path copied"))
          .catch(() => ctx.toast("Could not copy the path", true));
        break;
    }
  });

  document.addEventListener("change", (ev) => {
    const el = ev.target as HTMLInputElement;
    if (el.dataset?.lib === "pick" && dialog?.step === "inspect") {
      const path = el.dataset.path ?? "";
      if (el.checked) dialog.picked.add(path);
      else dialog.picked.delete(path);
    } else if (el.dataset?.lib === "understood" && dialog?.step === "confirm") {
      dialog.understood = el.checked;
      renderDialog();
    }
  });

  document.addEventListener("keydown", (ev) => {
    const el = ev.target as HTMLElement;
    if (ev.key === "Enter" && el.id === "lib-path") {
      const input = el as HTMLInputElement;
      const value = input.value;
      input.value = "";
      void importPaths([value]);
    }
  });

  // Native file drops arrive as paths from the webview, not as DOM events.
  void getCurrentWebview().onDragDropEvent((event) => {
    if (!ctx.isActive()) return;
    const payload = event.payload;
    if (payload.type === "over" || payload.type === "enter") {
      if (!dragging) {
        dragging = true;
        document.getElementById("lib-drop")?.classList.add("drop-active");
      }
    } else if (payload.type === "leave") {
      dragging = false;
      document.getElementById("lib-drop")?.classList.remove("drop-active");
    } else if (payload.type === "drop") {
      dragging = false;
      document.getElementById("lib-drop")?.classList.remove("drop-active");
      void importPaths(payload.paths);
    }
  });
}
