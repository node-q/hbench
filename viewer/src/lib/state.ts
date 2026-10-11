import { useCallback, useEffect, useState } from "react";
import { useSearchParams } from "react-router";

/**
 * URL search-param state, so filters/sort/tabs survive reload and can be shared.
 * `get(k)` / `all(k)` read; `set({k: v})` writes (null/""/[] removes; `page` resets unless set explicitly).
 */
export function useUrlState() {
  const [params, setParams] = useSearchParams();
  const get = useCallback((k: string) => params.get(k) ?? "", [params]);
  const all = useCallback((k: string) => params.getAll(k), [params]);
  const set = useCallback(
    (patch: Record<string, string | string[] | number | null | undefined>, replace = true) => {
      setParams(
        (prev) => {
          const next = new URLSearchParams(prev);
          for (const [k, v] of Object.entries(patch)) {
            next.delete(k);
            if (Array.isArray(v)) v.forEach((x) => next.append(k, x));
            else if (v != null && v !== "") next.set(k, String(v));
          }
          if (!("page" in patch)) next.delete("page");
          return next;
        },
        { replace },
      );
    },
    [setParams],
  );
  return { params, get, all, set };
}

/** localStorage-backed state (column visibility, view toggles). */
export function useStored<T>(key: string, initial: T): [T, (v: T) => void] {
  const [v, setV] = useState<T>(() => {
    try {
      const s = localStorage.getItem(`hb:${key}`);
      return s == null ? initial : (JSON.parse(s) as T);
    } catch {
      return initial;
    }
  });
  const set = useCallback(
    (x: T) => {
      setV(x);
      localStorage.setItem(`hb:${key}`, JSON.stringify(x));
    },
    [key],
  );
  return [v, set];
}

/** Debounced copy of a value (search inputs → URL). */
export function useDebounced<T>(value: T, delay = 250): T {
  const [v, setV] = useState(value);
  useEffect(() => {
    const id = setTimeout(() => setV(value), delay);
    return () => clearTimeout(id);
  }, [value, delay]);
  return v;
}

/** Keyboard shortcut, ignored while typing in a field. `combo` like "k", "mod+k", "shift+?". */
export function useKey(combo: string, handler: (e: KeyboardEvent) => void, enabled = true) {
  useEffect(() => {
    if (!enabled) return;
    const parts = combo.toLowerCase().split("+");
    const key = parts.pop()!;
    const mod = parts.includes("mod");
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement;
      const typing = t.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(t.tagName);
      if (typing && !mod && key !== "escape") return;
      if (mod !== (e.metaKey || e.ctrlKey)) return;
      if (e.key.toLowerCase() !== key) return;
      handler(e);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [combo, handler, enabled]);
}
