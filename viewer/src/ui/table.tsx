// DataTable: sortable headers, hideable columns (persisted), clickable rows, j/k + Enter keyboard cursor,
// optional checkbox selection. Sorting is controlled by the caller (server- or client-side).
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { useNavigate } from "react-router";
import * as Icon from "./icons";
import { Popover } from "./popover";
import { useKey, useStored } from "../lib/state";

export interface Column<T> {
  id: string;
  header: string;
  cell: (row: T) => ReactNode;
  /** Sort key sent to the caller; omit for unsortable columns. */
  sort?: string;
  align?: "left" | "right";
  /** Hidden unless the user turns it on. */
  hidden?: boolean;
  /** Can't be hidden. */
  fixed?: boolean;
  width?: number | string;
  title?: (row: T) => string | undefined;
}

export interface SortState {
  by: string;
  order: "asc" | "desc";
}

export function useColumns<T>(storeKey: string, columns: Column<T>[]) {
  const [hidden, setHidden] = useStored<string[] | null>(`cols:${storeKey}`, null);
  const isHidden = (c: Column<T>) => !c.fixed && (hidden ? hidden.includes(c.id) : !!c.hidden);
  const visible = columns.filter((c) => !isHidden(c));
  const toggle = (id: string) => {
    const cur = hidden ?? columns.filter((c) => c.hidden).map((c) => c.id);
    setHidden(cur.includes(id) ? cur.filter((x) => x !== id) : [...cur, id]);
  };
  const picker = (
    <Popover
      align="end"
      trigger={(p) => (
        <button {...p} className="btn btn--sm" title="Choose columns">
          <Icon.Columns size={14} /> Columns
        </button>
      )}
    >
      <div className="menu-label label">Columns</div>
      {columns
        .filter((c) => !c.fixed)
        .map((c) => (
          <label key={c.id} className="menu-item">
            <input type="checkbox" checked={!isHidden(c)} onChange={() => toggle(c.id)} />
            {c.header}
          </label>
        ))}
      {hidden && (
        <>
          <div className="menu-sep" />
          <button className="menu-item" onClick={() => setHidden(null)}>
            Reset to default
          </button>
        </>
      )}
    </Popover>
  );
  return { visible, picker };
}

export function DataTable<T>({
  rows,
  columns,
  rowKey,
  href,
  sort,
  onSort,
  selected,
  onSelect,
  empty,
  keyboard = true,
  label,
}: {
  rows: T[];
  columns: Column<T>[];
  rowKey: (row: T) => string;
  href?: (row: T) => string;
  sort?: SortState | null;
  onSort?: (s: SortState | null) => void;
  selected?: string[];
  onSelect?: (keys: string[]) => void;
  empty?: ReactNode;
  keyboard?: boolean;
  label: string;
}) {
  const navigate = useNavigate();
  const [cursor, setCursor] = useState(-1);
  useEffect(() => setCursor((c) => Math.min(c, rows.length - 1)), [rows.length]);

  const move = useCallback(
    (d: number) => (e: KeyboardEvent) => {
      if (!rows.length) return;
      e.preventDefault();
      setCursor((c) => {
        const n = Math.max(0, Math.min(rows.length - 1, c + d));
        requestAnimationFrame(() => document.querySelector(`[data-row-index="${n}"]`)?.scrollIntoView({ block: "nearest" }));
        return n;
      });
    },
    [rows.length],
  );
  useKey("j", move(1), keyboard);
  useKey("k", move(-1), keyboard);
  useKey(
    "enter",
    useCallback(() => {
      if (href && cursor >= 0 && rows[cursor]) navigate(href(rows[cursor]));
    }, [href, cursor, rows, navigate]),
    keyboard && cursor >= 0,
  );

  const clickSort = (key: string) => {
    if (!onSort) return;
    if (sort?.by !== key) onSort({ by: key, order: "desc" });
    else if (sort.order === "desc") onSort({ by: key, order: "asc" });
    else onSort(null);
  };
  const allKeys = rows.map(rowKey);
  const allOn = !!selected && allKeys.length > 0 && allKeys.every((k) => selected.includes(k));

  if (!rows.length && empty) return <>{empty}</>;
  return (
    <div className="table-wrap">
      <table className="table" aria-label={label}>
        <thead>
          <tr>
            {onSelect && (
              <th className="td-check">
                <input type="checkbox" aria-label="Select all rows" checked={allOn} onChange={() => onSelect(allOn ? [] : allKeys)} />
              </th>
            )}
            {columns.map((c) => {
              const active = c.sort && sort?.by === c.sort;
              return (
                <th key={c.id} data-align={c.align} style={c.width ? { width: c.width } : undefined} aria-sort={active ? (sort!.order === "asc" ? "ascending" : "descending") : undefined}>
                  {c.sort && onSort ? (
                    <button onClick={() => clickSort(c.sort!)}>
                      {c.header}
                      {active ? sort!.order === "asc" ? <Icon.ArrowUp size={11} /> : <Icon.ArrowDown size={11} /> : null}
                    </button>
                  ) : (
                    c.header
                  )}
                </th>
              );
            })}
          </tr>
        </thead>
        <tbody>
          {rows.map((r, i) => {
            const key = rowKey(r);
            const to = href?.(r);
            return (
              <tr
                key={key}
                data-row-index={i}
                data-href={to}
                className={i === cursor ? "is-cursor" : undefined}
                aria-selected={selected?.includes(key) || undefined}
                onClick={(e) => {
                  if (!to || (e.target as HTMLElement).closest("a,button,input,label")) return;
                  if (e.metaKey || e.ctrlKey) window.open(to, "_blank");
                  else navigate(to);
                }}
              >
                {onSelect && (
                  <td className="td-check">
                    <input type="checkbox" aria-label={`Select ${key}`} checked={!!selected?.includes(key)} onChange={() => onSelect(selected!.includes(key) ? selected!.filter((x) => x !== key) : [...selected!, key])} />
                  </td>
                )}
                {columns.map((c) => (
                  <td key={c.id} data-align={c.align} title={c.title?.(r)}>
                    {c.cell(r)}
                  </td>
                ))}
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

/** Client-side sort helper for tables whose data is already fully loaded. Nulls always sort last. */
export function sortRows<T>(rows: T[], sort: SortState | null | undefined, get: (row: T, key: string) => unknown): T[] {
  if (!sort) return rows;
  const dir = sort.order === "asc" ? 1 : -1;
  return [...rows].sort((a, b) => {
    const x = get(a, sort.by);
    const y = get(b, sort.by);
    if (x == null && y == null) return 0;
    if (x == null) return 1;
    if (y == null) return -1;
    if (typeof x === "number" && typeof y === "number") return (x - y) * dir;
    return String(x).localeCompare(String(y), undefined, { numeric: true }) * dir;
  });
}
