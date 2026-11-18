/**
 * Makes the gallery behave like a native desktop window: no browser context menu, no
 * reload / print / save / view-source shortcuts, no page zoom, no back/forward navigation.
 * Text fields keep their normal editing behaviour.
 */

const isMac = /Macintosh|Mac OS/i.test(navigator.userAgent);

function isTextField(target: EventTarget | null): boolean {
  return (
    target instanceof HTMLElement &&
    (target.matches("input, textarea") || target.isContentEditable)
  );
}

export function installNativeGuards(): void {
  document.addEventListener("contextmenu", (e) => {
    if (!isTextField(e.target)) e.preventDefault();
  });

  document.addEventListener(
    "keydown",
    (e) => {
      const mod = e.ctrlKey || e.metaKey;
      const key = e.key.toLowerCase();
      const blocked =
        e.key === "F5" ||
        (mod && ["r", "p", "s", "u", "+", "=", "-", "0"].includes(key)) ||
        (e.altKey && (e.key === "ArrowLeft" || e.key === "ArrowRight"));
      if (blocked) e.preventDefault();
    },
    { capture: true },
  );

  // Ctrl/Cmd + wheel zooms the page in browsers; block it.
  document.addEventListener(
    "wheel",
    (e) => {
      if (e.ctrlKey || e.metaKey) e.preventDefault();
    },
    { passive: false },
  );

  // Trackpad pinch on WebKit emits gesture events instead of wheel events.
  for (const type of ["gesturestart", "gesturechange", "gestureend"]) {
    document.addEventListener(type, (e) => e.preventDefault());
  }
}

/**
 * Platform chrome. macOS uses an overlay title bar (traffic lights float over the content),
 * so a 28px drag strip at the top keeps the window movable. Windows and Linux keep their
 * native title bars.
 */
export function applyPlatformChrome(): void {
  document.documentElement.dataset.platform = isMac
    ? "mac"
    : /Windows/i.test(navigator.userAgent)
      ? "windows"
      : "linux";

  if (isMac) {
    const strip = document.createElement("div");
    strip.setAttribute("data-tauri-drag-region", "");
    strip.className = "fixed inset-x-0 top-0 z-40 h-7";
    document.body.prepend(strip);
  }
}
