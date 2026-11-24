// The run link, `actuaria://run/actuaria/<dataset>[?version=<x.y[.z]>]`: a Marketplace page's "Open in Actuaria
// Studio". The desktop app parses it and sends it here as `window.__ACTUARIA_RUN__` with an `actuaria:run` event
// (desktop/src-tauri/src/run_link.rs). The page looks the name up in the catalogue New run already loads, picks it
// and shows where it came from. It never starts a run, changes an agent's settings or fetches anything from the
// link: the user still presses Run.
import { useSyncExternalStore } from "react";
import type { Catalog, CatalogDataset } from "../lib/types";
import { emptyDraft, type Draft } from "../pages/run/form";

/** What the app pushes: "none", a link to open, or why the link isn't valid. */
export interface RunLinkView {
  state: string;
  nonce: string;
  dataset?: string | null;
  version?: string | null;
  message?: string | null;
}

declare global {
  interface Window {
    __ACTUARIA_RUN__?: RunLinkView;
  }
}

/** A link the page has taken and New run has yet to apply. */
export type RunRequest = { nonce: string; dataset: string; version: string | null } | { nonce: string; error: string };

const seen = new Set<string>();

/**
 * The request in what the app pushed, unless there is none, it is malformed or this page already took it (the
 * app pushes its state again on every page load).
 */
export function accept(v: unknown): RunRequest | null {
  if (!v || typeof v !== "object") return null;
  const { state, nonce, dataset, version, message } = v as RunLinkView;
  if (typeof nonce !== "string" || !nonce || seen.has(nonce)) return null;
  let r: RunRequest;
  if (state === "open" && typeof dataset === "string" && dataset) r = { nonce, dataset: dataset.slice(0, 64), version: typeof version === "string" && version ? version.slice(0, 20) : null };
  else if (state === "error") r = { nonce, error: typeof message === "string" && message.trim() ? message.slice(0, 200) : "This link is not valid." };
  else return null;
  seen.add(nonce);
  return r;
}

// The request waiting for New run, which may not be on screen yet.
let pending: RunRequest | null = null;
const subscribers = new Set<() => void>();
const notify = () => subscribers.forEach((f) => f());

// Tells the app a link was applied, so the next may come (RunLink.tsx sets it to the native route).
let onTaken: (nonce: string) => void = () => {};
export function whenTaken(f: (nonce: string) => void) {
  onTaken = f;
}

export function offer(r: RunRequest) {
  pending = r;
  notify();
}

/** New run has applied the request (or shown why it couldn't): only now is the app told it was taken. */
export function settle() {
  const done = pending;
  pending = null;
  notify();
  if (done) onTaken(done.nonce);
}

export function useRunRequest(): RunRequest | null {
  return useSyncExternalStore(
    (f) => {
      subscribers.add(f);
      return () => subscribers.delete(f);
    },
    () => pending,
  );
}

export const notInCatalogue = (label: string) => `${label} isn't in the catalogue. Update the app or open the Marketplace.`;

/** The catalogue's entry and the version to run, or why the link can't be followed. */
export function resolve(catalog: Catalog, dataset: string, version: string | null): { entry: CatalogDataset; version: string } | { error: string } {
  const entry = catalog.datasets.find((d) => d.name === dataset);
  if (!entry) return { error: notInCatalogue(dataset) };
  if (version && !entry.versions.includes(version)) return { error: notInCatalogue(`${dataset} ${version}`) };
  return { entry, version: version ?? entry.version };
}

/**
 * The draft with the test set picked, as picking its card does: the set's own filters stay if it was already
 * picked, and nothing else changes (agents, environment, job settings).
 */
export function pickFromLink(draft: Draft, entry: CatalogDataset, version: string, current: CatalogDataset | undefined): Draft {
  const next = structuredClone(draft);
  if (next.source === "dataset" && current?.name === entry.name) next.dataset.version = version;
  else next.dataset = { ...emptyDraft().dataset, name: entry.name, version };
  next.source = "dataset";
  return next;
}

export const openedNotice = (entry: CatalogDataset, version: string) => `Opened from the Marketplace: ${entry.name} ${version}`;
