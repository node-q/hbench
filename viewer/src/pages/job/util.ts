import { MISSING } from "../../lib/types";

/** `metadata_filter_value` on the client: the filter value the server expects for a (possibly null) field. */
export function fv(v: string | null | undefined): string {
  if (v == null) return MISSING;
  return v === MISSING || v.startsWith("\\") ? `\\${v}` : v;
}

export const jobHref = (job: string) => `/jobs/${encodeURIComponent(job)}`;
export const trialHref = (job: string, trial: string) => `${jobHref(job)}/trials/${encodeURIComponent(trial)}`;

/** Total tokens of a row with uncached/cached/output parts; null when nothing is known. */
export function tokenSum(...parts: (number | null | undefined)[]): number | null {
  const known = parts.filter((x): x is number => typeof x === "number");
  return known.length ? known.reduce((a, b) => a + b, 0) : null;
}

/** Eval keys other than the primary `reward`, in first-seen order (dynamic metric columns). */
export function extraEvalKeys(rows: { evals: Record<string, unknown> }[]): string[] {
  const keys: string[] = [];
  for (const r of rows) for (const k of Object.keys(r.evals ?? {})) if (k !== "reward" && !keys.includes(k)) keys.push(k);
  return keys;
}

export function evalValue(evals: Record<string, { metrics: Record<string, number | null>[] }>, key: string): number | null {
  const m = evals?.[key]?.metrics?.[0];
  const v = m ? Object.values(m)[0] : null;
  return typeof v === "number" ? v : null;
}

/** Shared URL keys for the tasks/trials filters. */
export const FILTER_KEYS = ["q", "agent", "provider", "model", "task", "source", "reasoning_effort"] as const;
