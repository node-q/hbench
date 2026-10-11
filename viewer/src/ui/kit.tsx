// Small shared building blocks. Bigger ones (DataTable, Popover, CodeView, Markdown) have their own files.
import { useEffect, useRef, useState, type ReactNode } from "react";
import { Link } from "react-router";
import * as Icon from "./icons";
import { score } from "../lib/fmt";

export function Spinner({ label = "Loading" }: { label?: string }) {
  return <span className="spinner" role="status" aria-label={label} />;
}

/** Spinner that only shows after 300ms, so fast responses don't flash. */
export function Loading({ label = "Loading…" }: { label?: string }) {
  const [show, setShow] = useState(false);
  useEffect(() => {
    const id = setTimeout(() => setShow(true), 300);
    return () => clearTimeout(id);
  }, []);
  if (!show) return null;
  return (
    <div className="muted" style={{ display: "flex", alignItems: "center", gap: "var(--space-xs)", padding: "var(--space-lg) 0", fontSize: "var(--text-sm)" }}>
      <Spinner /> {label}
    </div>
  );
}

export function SkeletonRows({ rows = 6 }: { rows?: number }) {
  return (
    <div style={{ display: "grid", gap: "var(--space-sm)", padding: "var(--space-md) 0" }} aria-busy="true">
      {Array.from({ length: rows }, (_, i) => (
        <span key={i} className="skeleton" style={{ width: `${92 - ((i * 13) % 30)}%` }} />
      ))}
    </div>
  );
}

export function ErrorBox({ error, title = "Couldn't load this", onRetry }: { error: unknown; title?: string; onRetry?: () => void }) {
  const msg = error instanceof Error ? error.message : String(error);
  return (
    <div className="error-box" role="alert">
      <strong>{title}</strong>
      <span className="mono" style={{ fontSize: "var(--text-xs)" }}>
        {msg}
      </span>
      {onRetry && (
        <div>
          <button className="btn btn--sm" onClick={onRetry}>
            <Icon.Refresh size={13} /> Retry
          </button>
        </div>
      )}
    </div>
  );
}

export function Empty({ title, children, action }: { title: string; children?: ReactNode; action?: ReactNode }) {
  return (
    <div className="empty">
      <span className="empty__title">{title}</span>
      {children && <span>{children}</span>}
      {action}
    </div>
  );
}

/** Reward/score with a proportional tick; values outside [0,1] show the number only. */
export function Reward({ value, digits = 3 }: { value: number | null | undefined; digits?: number }) {
  if (value == null || !Number.isFinite(value)) return <span className="faint num">—</span>;
  const inUnit = value >= 0 && value <= 1;
  return (
    <span className="reward">
      {score(value, digits)}
      {inUnit && (
        <span className="reward__bar" aria-hidden="true">
          <i style={{ width: `${value * 100}%` }} />
        </span>
      )}
    </span>
  );
}

/** Trial/job status chip. */
export function Status({ kind, children }: { kind: "good" | "bad" | "warn" | "accent" | "plain"; children: ReactNode }) {
  return <span className={`chip${kind === "plain" ? "" : ` chip--${kind}`}`}>{children}</span>;
}

export function Stat({ label, value, sub }: { label: string; value: ReactNode; sub?: ReactNode }) {
  return (
    <div className="stat">
      <span className="label">{label}</span>
      <span className="stat__value">{value}</span>
      {sub != null && <span className="stat__sub">{sub}</span>}
    </div>
  );
}

export function CopyButton({ text, label = "Copy", small = true }: { text: string; label?: string; small?: boolean }) {
  const [done, setDone] = useState(false);
  const t = useRef<number>(undefined);
  return (
    <button
      className={`btn btn--ghost btn--icon${small ? " btn--sm" : ""}`}
      title={done ? "Copied" : label}
      aria-label={label}
      onClick={(e) => {
        e.stopPropagation();
        navigator.clipboard.writeText(text).then(() => {
          setDone(true);
          clearTimeout(t.current);
          t.current = window.setTimeout(() => setDone(false), 1200);
        });
      }}
    >
      {done ? <Icon.Check size={13} /> : <Icon.Copy size={13} />}
    </button>
  );
}

export interface TabDef {
  id: string;
  label: ReactNode;
  count?: number | null;
  disabled?: boolean;
}

/** Tab strip (selection is owned by the caller, usually URL state). Arrow keys move between tabs. */
export function Tabs({ tabs, value, onChange, label }: { tabs: TabDef[]; value: string; onChange: (id: string) => void; label: string }) {
  const ref = useRef<HTMLDivElement>(null);
  return (
    <div
      className="tabs"
      role="tablist"
      aria-label={label}
      ref={ref}
      onKeyDown={(e) => {
        if (e.key !== "ArrowRight" && e.key !== "ArrowLeft") return;
        const enabled = tabs.filter((t) => !t.disabled);
        const i = enabled.findIndex((t) => t.id === value);
        const next = enabled[(i + (e.key === "ArrowRight" ? 1 : enabled.length - 1)) % enabled.length];
        onChange(next.id);
        requestAnimationFrame(() => ref.current?.querySelector<HTMLButtonElement>('[aria-selected="true"]')?.focus());
      }}
    >
      {tabs.map((t) => (
        <button key={t.id} role="tab" className="tab" aria-selected={t.id === value} tabIndex={t.id === value ? 0 : -1} disabled={t.disabled} onClick={() => onChange(t.id)}>
          {t.label}
          {t.count != null && <span className="tab__count">{t.count}</span>}
        </button>
      ))}
    </div>
  );
}

/** Segmented control for small mutually exclusive options. */
export function Seg<T extends string>({ options, value, onChange, label }: { options: { id: T; label: ReactNode }[]; value: T; onChange: (v: T) => void; label: string }) {
  return (
    <div className="seg" role="group" aria-label={label}>
      {options.map((o) => (
        <button key={o.id} aria-pressed={o.id === value} onClick={() => onChange(o.id)}>
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function SearchField({ value, onChange, placeholder, width = 260, autoFocus }: { value: string; onChange: (v: string) => void; placeholder: string; width?: number | string; autoFocus?: boolean }) {
  return (
    <label className="search" style={{ width }}>
      <span className="sr-only">{placeholder}</span>
      <Icon.Search size={14} />
      <input
        className="input"
        type="search"
        value={value}
        placeholder={placeholder}
        autoFocus={autoFocus}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Escape" && value) {
            e.stopPropagation();
            onChange("");
          }
        }}
      />
    </label>
  );
}

export function Pagination({ page, totalPages, total, onPage }: { page: number; totalPages: number; total: number; onPage: (p: number) => void }) {
  if (totalPages <= 1) return null;
  return (
    <nav className="toolbar" aria-label="Pagination" style={{ justifyContent: "flex-end", marginTop: "var(--space-sm)" }}>
      <span className="muted num" style={{ fontSize: "var(--text-xs)" }}>
        page {page} of {totalPages} · {total} rows
      </span>
      <button className="btn btn--sm btn--icon" disabled={page <= 1} onClick={() => onPage(page - 1)} aria-label="Previous page">
        <Icon.ArrowLeft size={13} />
      </button>
      <button className="btn btn--sm btn--icon" disabled={page >= totalPages} onClick={() => onPage(page + 1)} aria-label="Next page">
        <Icon.ArrowRight size={13} />
      </button>
    </nav>
  );
}

/** Page header: crumbs above an h1, meta row, actions on the right. */
export function PageHead({ crumbs, title, meta, actions }: { crumbs?: { to: string; label: string }[]; title: ReactNode; meta?: ReactNode; actions?: ReactNode }) {
  return (
    <header className="page-head">
      <div className="page-head__title">
        {crumbs && crumbs.length > 0 && (
          <nav aria-label="Breadcrumb" className="label" style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
            {crumbs.map((c) => (
              <span key={c.to} style={{ display: "inline-flex", gap: 6 }}>
                <Link to={c.to} className="link" style={{ color: "var(--color-muted)" }}>
                  {c.label}
                </Link>
                <span aria-hidden="true">/</span>
              </span>
            ))}
          </nav>
        )}
        <h1>{title}</h1>
        {meta && <div className="page-head__meta">{meta}</div>}
      </div>
      {actions && <div className="toolbar" style={{ margin: 0 }}>{actions}</div>}
    </header>
  );
}

/** Horizontal stacked proportion bar (token mix, phase timing). */
export function StackBar({ parts, height = 8 }: { parts: { label: string; value: number; color: string }[]; height?: number }) {
  const total = parts.reduce((a, p) => a + Math.max(0, p.value), 0);
  if (!total) return null;
  return (
    <div style={{ display: "flex", height, borderRadius: 2, overflow: "hidden", background: "var(--color-paper-3)" }} role="img" aria-label={parts.map((p) => `${p.label} ${Math.round((p.value / total) * 100)}%`).join(", ")}>
      {parts.map((p) => (p.value > 0 ? <span key={p.label} title={p.label} style={{ width: `${(p.value / total) * 100}%`, background: p.color }} /> : null))}
    </div>
  );
}
