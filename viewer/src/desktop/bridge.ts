// The Actuaria desktop app shows this viewer in its own window. It marks the page with
// `window.__ACTUARIA_DESKTOP__` before any script runs, drives it through `window.__actuaria.command(..)` (native
// menus), and handles navigations to `/__actuaria/<action>` (open folder, reveal, external links). There is no
// IPC. `?desktop` previews the desktop layout in a browser.

declare global {
  interface Window {
    __ACTUARIA_DESKTOP__?: { version: string; platform: string };
    __actuaria?: { command: (cmd: string) => void; ack: () => void };
  }
}

const PREVIEW_KEY = "hb:desktop-preview";

function detect(): boolean {
  if (window.__ACTUARIA_DESKTOP__) return true;
  if (new URLSearchParams(location.search).has("desktop")) sessionStorage.setItem(PREVIEW_KEY, "1");
  return sessionStorage.getItem(PREVIEW_KEY) === "1";
}

/** The desktop app's name (its tauri.conf.json productName; APP_NAME in desktop/src-tauri/src/main.rs). */
export const APP_NAME = "Actuaria Studio Essentials";
export const isDesktop = detect();
export const desktopInfo = window.__ACTUARIA_DESKTOP__;
export const isMac = (desktopInfo?.platform ?? navigator.platform.toLowerCase()).startsWith("mac");

export type NativeAction = "open-folder" | "reveal" | "server-log" | "open-recent" | "theme" | "notify" | "update" | "install" | "run-link";

// An action is a navigation the app cancels, and a second navigation started before the first is decided
// replaces it. So actions go out one at a time: the next waits for the app's ack (or a short timeout).
const outbox: string[] = [];
let waiting: ReturnType<typeof setTimeout> | undefined;

function pump() {
  if (waiting !== undefined) return;
  const next = outbox.shift();
  if (!next) return;
  waiting = setTimeout(acked, 500);
  location.href = next;
}

function acked() {
  clearTimeout(waiting);
  waiting = undefined;
  pump();
}

/** Ask the app to do something native. Outside the app (preview) this only logs. */
export function native(action: NativeAction, params: Record<string, string> = {}) {
  if (!window.__ACTUARIA_DESKTOP__) {
    console.info("[desktop preview] native action:", action, params);
    return;
  }
  const q = new URLSearchParams(params).toString();
  const prefix = `/__actuaria/${action}`;
  // Only the latest theme matters.
  if (action === "theme") for (let i = outbox.length - 1; i >= 0; i--) if (outbox[i].startsWith(prefix)) outbox.splice(i, 1);
  outbox.push(`${prefix}${q ? `?${q}` : ""}`);
  pump();
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
    ack: acked,
  };
  document.documentElement.dataset.shell = "desktop";
  // The studio is dark unless the user picked light.
  if (!localStorage.getItem("hb:mode")) document.documentElement.dataset.mode = "dark";
}
