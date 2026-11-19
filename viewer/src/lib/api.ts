import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react";

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

/** An error body's `{detail}`: a string, or a list of validation errors. */
function detailOf(body: any, status: number): string {
  const d = body?.detail;
  if (typeof d === "string") return d;
  if (Array.isArray(d)) return d.map((e) => e?.msg ?? JSON.stringify(e)).join("; ");
  return `Request failed (${status})`;
}

type Params = Record<string, string | number | boolean | null | undefined | (string | number)[]>;

/** `?a=1&b=x&b=y` — arrays repeat the key, empty values are dropped. */
export function qs(params: Params): string {
  const u = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v == null || v === "" || v === false) continue;
    if (Array.isArray(v)) v.forEach((x) => u.append(k, String(x)));
    else u.append(k, String(v));
  }
  const s = u.toString();
  return s ? `?${s}` : "";
}

export const enc = encodeURIComponent;

/** Path segments joined and encoded: `p("jobs", job, "trials", trial)` → `/api/jobs/<job>/trials/<trial>`. */
export function p(...segs: string[]): string {
  return "/api/" + segs.map(enc).join("/");
}

// The viewer's launch key. `hbench view` prints its address with `#k=<key>`, and the desktop app opens its window
// there; the page keeps the key for this tab and drops it from the address bar. Writes and the account route
// need it (the viewer checks it with Host and Origin, I-20), so other pages in the browser can't use them.
const KEY_STORE = "hb:viewer-key";
const viewerKey: string | null = (() => {
  const m = /(?:^#|&)k=([A-Za-z0-9_-]{32,})/.exec(location.hash);
  try {
    if (m) {
      sessionStorage.setItem(KEY_STORE, m[1]);
      history.replaceState(history.state, "", location.pathname + location.search);
    }
    return m?.[1] ?? sessionStorage.getItem(KEY_STORE);
  } catch {
    return m?.[1] ?? null;
  }
})();

async function request(path: string, init?: RequestInit): Promise<Response> {
  const headers = new Headers(init?.headers);
  if (viewerKey) headers.set("X-Viewer-Key", viewerKey);
  const r = await fetch(path, { ...init, headers });
  if (!r.ok) {
    let body: any = null;
    try {
      body = await r.json();
    } catch {
      /* non-JSON error body */
    }
    throw new ApiError(r.status, detailOf(body, r.status));
  }
  return r;
}

// GETs in flight, for a loading indicator.
let inFlight = 0;
const busyListeners = new Set<() => void>();
function track<T>(work: Promise<T>): Promise<T> {
  const bump = (by: number) => {
    inFlight += by;
    busyListeners.forEach((l) => l());
  };
  bump(1);
  return work.finally(() => bump(-1));
}
const subscribeBusy = (l: () => void) => (busyListeners.add(l), () => busyListeners.delete(l));

/** Whether any GET is in flight. */
export const useBusy = () => useSyncExternalStore(subscribeBusy, () => inFlight > 0);

export async function getJson<T>(path: string, signal?: AbortSignal): Promise<T> {
  return track(request(path, { signal }).then((r) => r.json()));
}

export async function getText(path: string, signal?: AbortSignal): Promise<string> {
  return track(request(path, { signal }).then((r) => r.text()));
}

export async function send<T>(method: "POST" | "DELETE", path: string, body?: unknown): Promise<T> {
  const r = await request(path, {
    method,
    headers: body === undefined ? undefined : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const type = r.headers.get("content-type") ?? "";
  return (type.includes("json") ? r.json() : r.text()) as Promise<T>;
}

export interface Resource<T> {
  data: T | undefined;
  error: ApiError | Error | undefined;
  loading: boolean;
  reload: () => void;
}

// A tiny in-memory cache so back/forward navigation renders instantly; no eviction (a session views few URLs).
const cache = new Map<string, unknown>();

/**
 * GET `path` (null = skip). Keeps the previous data while refetching, so tables don't flash on filter changes.
 * `poll` re-fetches every N ms while the tab is visible (live jobs).
 */
export function useApi<T>(path: string | null, opts: { poll?: number; text?: boolean } = {}): Resource<T> {
  const [data, setData] = useState<T | undefined>(() => (path ? (cache.get(path) as T | undefined) : undefined));
  const [error, setError] = useState<Error>();
  const [loading, setLoading] = useState(!!path);
  const [tick, setTick] = useState(0);
  const lastPath = useRef(path);

  useEffect(() => {
    if (!path) {
      setLoading(false);
      return;
    }
    if (lastPath.current !== path) {
      lastPath.current = path;
      if (cache.has(path)) setData(cache.get(path) as T);
    }
    const ctl = new AbortController();
    setLoading(true);
    (opts.text ? getText(path, ctl.signal) : getJson<T>(path, ctl.signal))
      .then((d) => {
        cache.set(path, d);
        setData(d as T);
        setError(undefined);
      })
      .catch((e) => {
        if (e.name !== "AbortError") setError(e);
      })
      .finally(() => {
        if (!ctl.signal.aborted) setLoading(false);
      });
    return () => ctl.abort();
  }, [path, tick, opts.text]);

  useEffect(() => {
    if (!path || !opts.poll) return;
    const id = setInterval(() => {
      if (document.visibilityState === "visible") setTick((t) => t + 1);
    }, opts.poll);
    return () => clearInterval(id);
  }, [path, opts.poll]);

  const reload = useCallback(() => setTick((t) => t + 1), []);
  return { data, error, loading, reload };
}
