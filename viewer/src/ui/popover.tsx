// Popover on the native Popover API (light-dismiss, Esc, top layer), positioned under its trigger.
import { useId, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import * as Icon from "./icons";
import { filterLabel } from "../lib/fmt";
import type { FilterOption } from "../lib/types";

function place(trigger: HTMLElement, pop: HTMLElement, align: "start" | "end") {
  const r = trigger.getBoundingClientRect();
  const w = pop.offsetWidth;
  const h = pop.offsetHeight;
  let left = align === "end" ? r.right - w : r.left;
  left = Math.max(8, Math.min(left, window.innerWidth - w - 8));
  let top = r.bottom + 4;
  if (top + h > window.innerHeight - 8 && r.top - h - 4 > 8) top = r.top - h - 4; // flip up
  pop.style.left = `${left}px`;
  pop.style.top = `${top}px`;
}

export function Popover({
  trigger,
  children,
  align = "start",
  className = "",
  width,
}: {
  /** Render prop: spread `props` onto the trigger button. */
  trigger: (props: { popoverTarget: string; "aria-expanded": boolean; ref: React.Ref<HTMLButtonElement> }) => ReactNode;
  children: ReactNode | ((close: () => void) => ReactNode);
  align?: "start" | "end";
  className?: string;
  width?: number;
}) {
  const id = useId();
  const btn = useRef<HTMLButtonElement>(null);
  const pop = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false);

  useLayoutEffect(() => {
    const el = pop.current;
    if (!el) return;
    const onToggle = (e: Event) => {
      const isOpen = (e as ToggleEvent).newState === "open";
      setOpen(isOpen);
      if (isOpen && btn.current) place(btn.current, el, align);
    };
    el.addEventListener("toggle", onToggle);
    return () => el.removeEventListener("toggle", onToggle);
  }, [align]);

  useLayoutEffect(() => {
    if (!open) return;
    const reposition = () => btn.current && pop.current && place(btn.current, pop.current, align);
    window.addEventListener("resize", reposition);
    window.addEventListener("scroll", reposition, true);
    return () => {
      window.removeEventListener("resize", reposition);
      window.removeEventListener("scroll", reposition, true);
    };
  }, [open, align]);

  const close = () => pop.current?.hidePopover();
  return (
    <>
      {trigger({ popoverTarget: id, "aria-expanded": open, ref: btn })}
      <div ref={pop} id={id} popover="auto" className={`pop ${className}`} style={width ? { width } : undefined}>
        {open && (typeof children === "function" ? children(close) : children)}
      </div>
    </>
  );
}

/** Multi-select filter with counts and type-to-filter (agents, models, tasks…). */
export function FilterMenu({
  label,
  options,
  selected,
  onChange,
  format = filterLabel,
}: {
  label: string;
  options: FilterOption[] | undefined;
  selected: string[];
  onChange: (values: string[]) => void;
  format?: (v: string) => string;
}) {
  const [q, setQ] = useState("");
  const shown = (options ?? []).filter((o) => format(o.value).toLowerCase().includes(q.toLowerCase()));
  const toggle = (v: string) => onChange(selected.includes(v) ? selected.filter((x) => x !== v) : [...selected, v]);
  return (
    <Popover
      trigger={(p) => (
        <button {...p} className="btn btn--sm filter-btn" data-count={selected.length} disabled={!options?.length} title={options?.length ? undefined : `No ${label.toLowerCase()} to filter by`}>
          {label}
          <Icon.ChevronDown size={12} />
        </button>
      )}
    >
      {(options?.length ?? 0) > 8 && (
        <input className="input" style={{ width: "100%", marginBottom: 4 }} placeholder={`Filter ${label.toLowerCase()}`} value={q} onChange={(e) => setQ(e.target.value)} autoFocus />
      )}
      {shown.map((o) => (
        <label key={o.value} className="menu-item">
          <input type="checkbox" checked={selected.includes(o.value)} onChange={() => toggle(o.value)} />
          <span className="trunc">{format(o.value)}</span>
          <span className="menu-item__count">{o.count}</span>
        </label>
      ))}
      {shown.length === 0 && <div className="menu-label muted">No matches</div>}
      {selected.length > 0 && (
        <>
          <div className="menu-sep" />
          <button className="menu-item" onClick={() => onChange([])}>
            Clear {label.toLowerCase()}
          </button>
        </>
      )}
    </Popover>
  );
}

/** Single-choice dropdown menu (group-by, color-by). */
export function SelectMenu<T extends string>({ label, value, options, onChange }: { label: string; value: T; options: { id: T; label: string }[]; onChange: (v: T) => void }) {
  const current = options.find((o) => o.id === value);
  return (
    <Popover
      trigger={(p) => (
        <button {...p} className="btn btn--sm">
          <span className="muted">{label}</span> {current?.label ?? value}
          <Icon.ChevronDown size={12} />
        </button>
      )}
    >
      {(close) =>
        options.map((o) => (
          <button
            key={o.id}
            className="menu-item"
            data-active={o.id === value}
            onClick={() => {
              onChange(o.id);
              close();
            }}
          >
            <span style={{ width: 14 }}>{o.id === value && <Icon.Check size={13} />}</span>
            {o.label}
          </button>
        ))
      }
    </Popover>
  );
}
