// The Actuaria desktop app shows this viewer in its own window. It marks the page with
// `window.__ACTUARIA_DESKTOP__` before any script runs, drives it through `window.__actuaria.command(..)` (native
// menus), and handles navigations to `/__actuaria/<action>` (open folder, reveal, external links). There is no
// IPC. `?desktop` previews the desktop layout in a browser.

declare global {
  interface Window {
    __ACTUARIA_DESKTOP__?: { version: string; platform: string };
    __actuaria?: { command: (cmd: string) => void };
  }
}

const PREVIEW_KEY = "hb:desktop-preview";

function detect(): boolean {
  if (window.__ACTUARIA_DESKTOP__) return true;
  if (new URLSearchParams(location.search).has("desktop")) sessionStorage.setItem(PREVIEW_KEY, "1");
  return sessionStorage.getItem(PREVIEW_KEY) === "1";
}

export const isDesktop = detect();
export const desktopInfo = window.__ACTUARIA_DESKTOP__;
export const isMac = (desktopInfo?.platform ?? navigator.platform.toLowerCase()).startsWith("mac");

/** Ask the app to do something native. Outside the app (preview) this only logs. */
export function native(action: "open-folder" | "reveal" | "server-log" | "open-recent" | "theme", params: Record<string, string> = {}) {
  if (!window.__ACTUARIA_DESKTOP__) {
    console.info("[desktop preview] native action:", action, params);
    return;
  }
  const q = new URLSearchParams(params).toString();
  location.href = `/__actuaria/${action}${q ? `?${q}` : ""}`;
}

type Handler = (cmd: string) => void;
let handler: Handler | null = null;
const queued: string[] = [];

/** The shell registers the one handler for menu commands; commands sent before it mounts wait for it. */
export function onCommand(h: Handler | null) {
  handler = h;
  if (h) queued.splice(0).forEach(h);
}

if (isDesktop) {
  window.__actuaria = {
    command: (cmd) => (handler ? handler(cmd) : queued.push(cmd)),
  };
  document.documentElement.dataset.shell = "desktop";
  // The studio is dark unless the user picked light.
  if (!localStorage.getItem("hb:mode")) document.documentElement.dataset.mode = "dark";
}
