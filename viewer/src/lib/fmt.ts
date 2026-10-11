// Display formatting. Every function accepts null/undefined and returns "—" for it.

const DASH = "—";
const isNum = (n: unknown): n is number => typeof n === "number" && Number.isFinite(n);

export function int(n: number | null | undefined): string {
  return isNum(n) ? Math.round(n).toLocaleString("en-US") : DASH;
}

/** 1234 → 1.2k, 3_400_000 → 3.4M. */
export function compact(n: number | null | undefined): string {
  if (!isNum(n)) return DASH;
  const a = Math.abs(n);
  if (a < 1000) return String(Math.round(n));
  for (const [div, unit] of [
    [1e9, "B"],
    [1e6, "M"],
    [1e3, "k"],
  ] as const) {
    if (a >= div) {
      const v = n / div;
      return `${v >= 100 ? Math.round(v) : v.toFixed(1).replace(/\.0$/, "")}${unit}`;
    }
  }
  return String(n);
}

export function usd(n: number | null | undefined): string {
  if (!isNum(n)) return DASH;
  if (n === 0) return "$0";
  if (Math.abs(n) < 0.01) return `$${n.toFixed(4)}`;
  if (Math.abs(n) < 100) return `$${n.toFixed(2)}`;
  return `$${Math.round(n).toLocaleString("en-US")}`;
}

/** Rewards/metrics: up to 3 decimals, trailing zeros kept for column alignment. */
export function score(n: number | null | undefined, digits = 3): string {
  return isNum(n) ? n.toFixed(digits) : DASH;
}

export function pct(n: number | null | undefined, digits = 0): string {
  return isNum(n) ? `${(n * 100).toFixed(digits)}%` : DASH;
}

/** 950 → "950ms", 65_000 → "1m 05s", 3_725_000 → "1h 02m". */
export function duration(ms: number | null | undefined): string {
  if (!isNum(ms)) return DASH;
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s < 10 ? s.toFixed(1) : Math.round(s)}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(Math.round(s % 60)).padStart(2, "0")}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${String(m % 60).padStart(2, "0")}m`;
}

export function ms(start: string | null | undefined, end: string | null | undefined): number | null {
  if (!start || !end) return null;
  const d = Date.parse(end) - Date.parse(start);
  return Number.isFinite(d) ? d : null;
}

export function bytes(n: number | null | undefined): string {
  if (!isNum(n)) return DASH;
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
  return `${(n / 1024 ** 3).toFixed(1)} GB`;
}

/** "Oct 10, 15:07" (this year) or "Oct 10 2025, 15:07". */
export function date(iso: string | null | undefined): string {
  if (!iso) return DASH;
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const sameYear = d.getFullYear() === new Date().getFullYear();
  return d.toLocaleString("en-US", {
    month: "short",
    day: "numeric",
    ...(sameYear ? {} : { year: "numeric" }),
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

export function dateTime(iso: string | null | undefined): string {
  if (!iso) return DASH;
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString("en-US", { hour12: false });
}

/** "3m ago", "2d ago". */
export function ago(iso: string | null | undefined): string {
  if (!iso) return DASH;
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return DASH;
  const s = Math.max(0, (Date.now() - t) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  if (s < 86400 * 30) return `${Math.floor(s / 86400)}d ago`;
  return date(iso);
}

/** `provider/model` or just model. */
export function model(provider: string | null | undefined, name: string | null | undefined): string {
  if (!name) return DASH;
  return provider ? `${provider}/${name}` : name;
}

/** First metric value of the first eval (`{"reward": {"metrics": [{"mean": 0.5}]}}` → 0.5). */
export function firstMetric(evals: Record<string, { metrics: Record<string, number | null>[] }> | null | undefined): number | null {
  const e = evals && Object.values(evals)[0];
  const m = e?.metrics?.[0];
  const v = m && Object.values(m)[0];
  return isNum(v) ? v : null;
}

/** Display for a `__missing__` filter value. */
export function filterLabel(v: string): string {
  if (v === "__missing__") return "(none)";
  return v.startsWith("\\") ? v.slice(1) : v;
}
