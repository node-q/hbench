// Each catalog dataset's details (task count, description, licence, creator), loaded when the launcher shows the
// dataset: /api/run/catalog lists every dataset from the registry's index alone, with `details` null for those the
// index and this machine don't have yet. Loaded details stay for the app session.
import { useSyncExternalStore } from "react";
import { enc, getJson } from "../../lib/api";
import type { CatalogDataset, CatalogDetails } from "../../lib/types";

export type DetailState =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ready"; details: CatalogDetails }
  | { status: "error"; message: string };

/** Datasets per request, and requests at a time: the server opens each request's datasets in parallel. */
export const BATCH = 6;
export const IN_FLIGHT = 2;

export interface DetailsAnswer {
  details: Record<string, CatalogDetails>;
  errors: Record<string, string>;
}
type Fetch = (names: string[]) => Promise<DetailsAnswer>;

const key = (d: CatalogDataset) => `${d.name}@${d.version}`;

export function detailsStore(fetch: Fetch) {
  const states = new Map<string, DetailState>();
  const listeners = new Set<() => void>();
  let queue: CatalogDataset[] = [];
  let running = 0;
  let version = 0;
  const emit = () => {
    version++;
    listeners.forEach((l) => l());
  };

  const pump = () => {
    while (running < IN_FLIGHT && queue.length) {
      const batch = queue.splice(0, BATCH);
      running++;
      fetch(batch.map((d) => d.name))
        .then((r) => {
          for (const d of batch) {
            const details = r.details?.[d.name];
            states.set(
              key(d),
              details ? { status: "ready", details } : { status: "error", message: r.errors?.[d.name] || "The registry sent no details for this dataset." },
            );
          }
        })
        .catch((e: unknown) => {
          const message = (e instanceof Error && e.message) || "The details didn't load.";
          for (const d of batch) states.set(key(d), { status: "error", message });
        })
        .finally(() => {
          running--;
          emit();
          pump();
        });
    }
  };

  /** What the launcher can show of `d` now. */
  const state = (d: CatalogDataset): DetailState => (d.details ? { status: "ready", details: d.details } : (states.get(key(d)) ?? { status: "idle" }));

  /** Load the details of those of `ds` that are neither loaded, loading nor failed. */
  const load = (ds: CatalogDataset[]) => {
    const wanted = ds.filter((d) => state(d).status === "idle");
    if (!wanted.length) return;
    for (const d of wanted) states.set(key(d), { status: "loading" });
    queue.push(...wanted);
    emit();
    pump();
  };

  /** Load `d`'s details again after they failed. */
  const retry = (d: CatalogDataset) => {
    if (state(d).status !== "error") return;
    states.delete(key(d));
    load([d]);
  };

  const subscribe = (l: () => void) => (listeners.add(l), () => void listeners.delete(l));
  return { state, load, retry, subscribe, version: () => version };
}

const store = detailsStore((names) => getJson<DetailsAnswer>(`/api/run/catalog/details?${names.map((n) => `name=${enc(n)}`).join("&")}`));

/** The session's details store; components re-render when any dataset's details change. */
export function useDetails() {
  useSyncExternalStore(store.subscribe, store.version);
  return store;
}
