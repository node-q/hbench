// Desktop (Actuaria) chrome: activity bar, explorer side bar, editor tabs with breadcrumbs, status bar.
// Replaces the web top bar when the viewer runs inside the desktop app; pages render unchanged in the editor area.
import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from "react";
import { NavLink, useLocation, useNavigate } from "react-router";
import * as Icon from "../ui/icons";
import { useApi, useBusy } from "../lib/api";
import { useKey } from "../lib/state";
import type { AppConfig, JobSummary, Page, TaskDefinitionSummary, TrialSummary } from "../lib/types";
import { useToast } from "../ui/overlay";
import { desktopInfo, isMac, native, onCommand } from "./bridge";

const SIDE_KEY = "hb:dk-side";
const TABS_KEY = "hb:dk-tabs";
const MAX_TABS = 14;
const SIDE_DEFAULT = 272;

export const Mark = ({ size = 16 }: { size?: number }) => (
  <svg width={size} height={size} viewBox="0 0 64 64" fill="currentColor" aria-hidden="true" style={{ flexShrink: 0 }}>
    <path d="M13.13724 0l10.15269 10.15263 10.15341-10.15263 12.72841 0-22.88204 22.88029-22.88043-22.88029 12.72796 0z m-13.13724 24.26167l0-12.72793 23.32843 23.32844 34.86233-34.86218 5.80924 0 0 6.91869-16.06904 16.06897 16.06904 16.06903 0 12.72793-22.43301-22.43302-34.64855 34.6484-6.91844 0 0-5.80948 16.96446-16.96439-16.96446-16.96446z m32.11455 39.73833l-12.72792 0 22.0503-22.05029 22.05029 22.05029-12.72792 0-9.32237-9.32237-9.32238 9.32237z" />
  </svg>
);

const running = (j: JobSummary) => !!j.started_at && !j.finished_at;
const kbd = (k: string) => (isMac ? `⌘${k}` : `Ctrl+${k}`);
const jobPath = (job: string) => `/jobs/${encodeURIComponent(job)}`;

interface Where {
  key: string;
  label: string;
  icon: (p: { size?: number }) => ReactNode;
  crumbs: { label: string; to?: string }[];
}

/** What a route is, for its tab and breadcrumbs. */
function describe(pathname: string): Where {
  const seg = pathname.split("/").filter(Boolean).map(decodeURIComponent);
  if (seg[0] === "jobs" && seg[1]) {
    const job = seg[1];
    if (seg[2] === "trials" && seg[3]) {
      return { key: pathname, label: seg[3], icon: Icon.File, crumbs: [{ label: "Jobs", to: "/" }, { label: job, to: jobPath(job) }, { label: seg[3] }] };
    }
    return { key: pathname, label: job, icon: Icon.Folder, crumbs: [{ label: "Jobs", to: "/" }, { label: job }] };
  }
  if (seg[0] === "task-definitions") {
    return seg[1]
      ? { key: pathname, label: seg[1], icon: Icon.File, crumbs: [{ label: "Tasks", to: "/task-definitions" }, { label: seg[1] }] }
      : { key: pathname, label: "Tasks", icon: Icon.List, crumbs: [{ label: "Tasks" }] };
  }
  if (seg[0] === "compare") return { key: pathname, label: "Compare", icon: Icon.Columns, crumbs: [{ label: "Compare" }] };
  if (seg[0] === "run") return { key: pathname, label: "New run", icon: Icon.Play, crumbs: [{ label: "New run" }] };
  if (!seg.length) return { key: "/", label: "Jobs", icon: Icon.List, crumbs: [{ label: "Jobs" }] };
  return { key: pathname, label: seg[seg.length - 1], icon: Icon.File, crumbs: [{ label: pathname }] };
}

interface Tab {
  key: string;
  href: string;
}

function useTabs() {
  const loc = useLocation();
  const navigate = useNavigate();
  const [tabs, setTabs] = useState<Tab[]>(() => {
    try {
      return JSON.parse(sessionStorage.getItem(TABS_KEY) ?? "[]");
    } catch {
      return [];
    }
  });
  const active = describe(loc.pathname).key;
  // No tab open: the last one was closed. The route stays where it was, so the editor shows a welcome
  // screen until the next navigation (even one to the same page) opens a tab again.
  const [empty, setEmpty] = useState(false);
  useEffect(() => {
    const href = loc.pathname + loc.search;
    setEmpty(false);
    setTabs((ts) => {
      const i = ts.findIndex((t) => t.key === active);
      let next = i >= 0 ? ts.map((t, j) => (j === i ? { ...t, href } : t)) : [...ts, { key: active, href }];
      if (next.length > MAX_TABS) next = next.filter((t, j) => t.key === active || j >= next.length - MAX_TABS);
      return next;
    });
  }, [active, loc.pathname, loc.search, loc.key]);
  useEffect(() => sessionStorage.setItem(TABS_KEY, JSON.stringify(tabs)), [tabs]);

  const close = useCallback(
    (key: string) => {
      const i = tabs.findIndex((t) => t.key === key);
      if (i < 0) return;
      const next = tabs.filter((t) => t.key !== key);
      setTabs(next);
      // Closing the active tab shows its neighbour; closing the last one leaves the editor empty.
      if (!next.length) setEmpty(true);
      else if (key === active) navigate(next[Math.min(i, next.length - 1)].href);
    },
    [tabs, active, navigate],
  );
  const closeOthers = useCallback(() => setTabs((ts) => ts.filter((t) => t.key === active)), [active]);
  const step = useCallback(
    (by: number) => {
      if (tabs.length < 2) return;
      const i = tabs.findIndex((t) => t.key === active);
      navigate(tabs[(i + by + tabs.length) % tabs.length].href);
    },
    [tabs, active, navigate],
  );
  return { tabs, active: empty ? "" : active, empty, close, closeOthers, step };
}

function useSideWidth(): [number, (w: number) => void] {
  const [w, setW] = useState(() => Number(localStorage.getItem(`${SIDE_KEY}-w`)) || SIDE_DEFAULT);
  const set = useCallback((n: number) => {
    // Leave the editor at least 480px.
    const c = Math.round(Math.min(560, innerWidth - 48 - 480, Math.max(200, n)));
    localStorage.setItem(`${SIDE_KEY}-w`, String(c));
    setW(c);
  }, []);
  return [w, set];
}

/** Toast (window focused) or native notification (in the background) when a running job finishes. */
function useFinishNotices(jobs: JobSummary[] | undefined) {
  const toast = useToast();
  const navigate = useNavigate();
  const before = useRef<Set<string> | null>(null);
  useEffect(() => {
    if (!jobs) return;
    const now = new Set(jobs.filter(running).map((j) => j.name));
    const prev = before.current;
    before.current = now;
    for (const name of prev ?? []) {
      const j = jobs.find((x) => x.name === name);
      if (!j || now.has(name)) continue;
      const summary = `${j.n_completed_trials}/${j.n_total_trials} trials${j.n_errored_trials ? `, ${j.n_errored_trials} errored` : ""}`;
      if (document.hasFocus()) toast({ text: `Run finished: ${name} · ${summary}`, action: { label: "Open", run: () => navigate(jobPath(name)) } }, 8000);
      else native("notify", { title: "Run finished", body: `${name}\n${summary}` });
    }
  }, [jobs, toast, navigate]);
}

export function DesktopChrome({
  config,
  mode,
  toggleMode,
  openPalette,
  children,
}: {
  config: AppConfig | undefined;
  mode: string;
  toggleMode: () => void;
  openPalette: () => void;
  children: ReactNode;
}) {
  const navigate = useNavigate();
  const loc = useLocation();
  const tasksMode = config?.mode === "tasks";
  const [sideOpen, setSideOpen] = useState(() => localStorage.getItem(SIDE_KEY) !== "closed");
  const [sideW, setSideW] = useSideWidth();
  const [resizing, setResizing] = useState(false);
  const toggleSide = useCallback(() => {
    setSideOpen((o) => {
      localStorage.setItem(SIDE_KEY, o ? "closed" : "open");
      return !o;
    });
  }, []);
  const { tabs, active, empty, close, closeOthers, step } = useTabs();
  const where = describe(loc.pathname);
  const busy = useBusy();
  const [collapseAll, setCollapseAll] = useState(0);

  // Jobs feed the explorer and the status bar; poll faster while something runs.
  const [live, setLive] = useState(false);
  const jobs = useApi<Page<JobSummary>>(config && !tasksMode ? "/api/jobs?page_size=100" : null, { poll: live ? 4000 : 15000 });
  const nRunning = jobs.data?.items.filter(running).length ?? 0;
  useEffect(() => setLive(nRunning > 0), [nRunning]);
  useFinishNotices(jobs.data?.items);

  useEffect(() => {
    onCommand((cmd) => {
      if (cmd === "palette") openPalette();
      else if (cmd === "sidebar") toggleSide();
      else if (cmd === "theme") toggleMode();
      else if (cmd === "back") navigate(-1);
      else if (cmd === "forward") navigate(1);
      else if (cmd === "close-tab") active && close(active);
      else if (cmd === "close-other-tabs") closeOthers();
      else if (cmd === "tab-next") step(1);
      else if (cmd === "tab-prev") step(-1);
      else if (cmd.startsWith("go:")) navigate(tasksMode && cmd === "go:/" ? "/task-definitions" : cmd.slice(3));
    });
    return () => onCommand(null);
  }, [openPalette, toggleSide, toggleMode, navigate, tasksMode, close, closeOthers, step, active]);
  // In the app the native menu owns these shortcuts; the browser preview needs them here.
  const preview = !desktopInfo;
  useKey("mod+b", useCallback((e: KeyboardEvent) => (e.preventDefault(), toggleSide()), [toggleSide]), preview);

  // The native title bar follows the page's theme.
  useEffect(() => native("theme", { mode }), [mode]);

  // Keep the active tab in view; a vertical wheel scrolls the strip sideways.
  const strip = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const still = matchMedia("(prefers-reduced-motion: reduce)").matches;
    const show = () => strip.current?.querySelector('[data-active="true"]')?.scrollIntoView({ block: "nearest", inline: "nearest", behavior: still ? "auto" : "smooth" });
    show();
    // A new tab is still growing to its width; look again once it has.
    const later = setTimeout(show, 220);
    return () => clearTimeout(later);
  }, [active, tabs.length]);

  // A short fade when the editor shows another tab (opacity only: transforms would unpin sticky headers).
  const editor = useRef<HTMLElement>(null);
  const shown = useRef(active);
  // Each tab keeps its scroll position (the editor area is the scroller).
  const scrolls = useRef(new Map<string, number>());
  useEffect(() => {
    const el = editor.current;
    if (!el) return;
    // Space, Page Down and the arrows scroll the editor unless something else has focus.
    if (document.activeElement === document.body) el.focus({ preventScroll: true });
    if (shown.current === active) return;
    shown.current = active;
    const top = scrolls.current.get(active) ?? 0;
    el.scrollTop = top;
    if (!matchMedia("(prefers-reduced-motion: reduce)").matches) {
      el.animate([{ opacity: 0.25 }, { opacity: 1 }], { duration: 160, easing: "cubic-bezier(0.16, 1, 0.3, 1)" });
    }
    // A page that is still loading is not tall enough yet: keep trying for a moment, unless the user scrolls.
    let tries = 0;
    const retry = setInterval(() => {
      if (el.scrollTop === top || ++tries > 15) return stop();
      el.scrollTop = top;
    }, 100);
    const stop = () => {
      clearInterval(retry);
      el.removeEventListener("wheel", stop);
      el.removeEventListener("keydown", stop);
    };
    el.addEventListener("wheel", stop, { passive: true });
    el.addEventListener("keydown", stop);
    return stop;
  }, [active]);

  const folder = config?.folder ?? "";
  const folderName = folder.split(/[\\/]/).filter(Boolean).pop() ?? folder;

  return (
    <div className="dk" data-side={sideOpen ? "open" : "closed"} data-resizing={resizing} data-empty={empty} style={{ "--dk-side-w": `${sideW}px` } as React.CSSProperties}>
      <nav className="dk-activity" aria-label="Views">
        <ActivityButton label={`Explorer (${kbd("B")})`} active={sideOpen} kind="view" onClick={toggleSide}>
          <Icon.Files size={22} />
        </ActivityButton>
        {!tasksMode && (
          <>
            <ActivityButton label="Compare jobs" active={!empty && loc.pathname === "/compare"} kind="page" onClick={() => navigate("/compare")}>
              <Icon.Columns size={22} />
            </ActivityButton>
            <ActivityButton label={`New run (${kbd("N")})`} active={!empty && loc.pathname === "/run"} kind="page" onClick={() => navigate("/run")}>
              <Icon.Play size={22} />
            </ActivityButton>
          </>
        )}
        <ActivityButton label={`Command palette (${kbd("K")})`} kind="page" onClick={openPalette}>
          <Icon.Search size={22} />
        </ActivityButton>
        <div className="dk-activity__spacer" />
        <ActivityButton label={mode === "dark" ? "Light theme" : "Dark theme"} kind="page" onClick={toggleMode}>
          {mode === "dark" ? <Icon.Sun size={20} /> : <Icon.Moon size={20} />}
        </ActivityButton>
      </nav>

      <aside className="dk-side" aria-label="Explorer" inert={!sideOpen}>
        <div className="dk-side__title">
          <span>Explorer</span>
          <span className="dk-side__actions">
            <button className="dk-icon" title="Open folder…" aria-label="Open folder" onClick={() => native("open-folder")}>
              <Icon.Folder size={14} />
            </button>
            {!tasksMode && (
              <>
                <button className="dk-icon" title="Refresh" aria-label="Refresh" onClick={jobs.reload}>
                  <Icon.Refresh size={14} />
                </button>
                <button className="dk-icon" title="Collapse all" aria-label="Collapse all" onClick={() => setCollapseAll((n) => n + 1)}>
                  <Icon.CollapseAll size={14} />
                </button>
              </>
            )}
          </span>
        </div>
        <div className="dk-side__section" title={folder}>
          <Icon.ChevronDown size={12} />
          <span className="trunc">{folderName || "No folder"}</span>
          <span className="dk-side__mode">{tasksMode ? "tasks" : "jobs"}</span>
        </div>
        {tasksMode ? <TaskTree /> : <JobTree jobs={jobs.data?.items} loading={jobs.loading && !jobs.data} error={jobs.error} collapseAll={collapseAll} />}
        <Resizer width={sideW} onWidth={setSideW} onDrag={setResizing} />
      </aside>

      <div className="dk-main">
        <div className="dk-head">
          <div
            className="dk-tabs"
            role="tablist"
            aria-label="Open editors"
            ref={strip}
            onWheel={(e) => {
              if (Math.abs(e.deltaY) > Math.abs(e.deltaX)) e.currentTarget.scrollLeft += e.deltaY;
            }}
          >
            {tabs.map((t) => {
              const d = describe(t.key);
              const isActive = t.key === active;
              return (
                <div
                  key={t.key}
                  role="tab"
                  aria-selected={isActive}
                  className="dk-tab"
                  data-active={isActive}
                  title={d.crumbs.map((c) => c.label).join(" › ")}
                  onClick={() => navigate(t.href)}
                  onAuxClick={(e) => e.button === 1 && close(t.key)}
                >
                  <d.icon size={14} />
                  <span className="trunc">{d.label}</span>
                  <button
                    className="dk-tab__close"
                    aria-label={`Close ${d.label}`}
                    title={isActive ? `Close (${kbd("W")})` : "Close"}
                    onClick={(e) => {
                      e.stopPropagation();
                      close(t.key);
                    }}
                  >
                    <Icon.Close size={12} />
                  </button>
                </div>
              );
            })}
            <div className="dk-tabs__fill" />
          </div>
          <div className="dk-crumbs" aria-label="Breadcrumbs">
            <span className="dk-crumbs__root" title={folder}>
              <Icon.Folder size={13} />
              {folderName}
            </span>
            {(empty ? [] : where.crumbs).map((c, i) => (
              <span key={i} className="dk-crumbs__item">
                <Icon.Chevron size={11} />
                {c.to ? (
                  <NavLink to={c.to} className="dk-crumbs__link">
                    {c.label}
                  </NavLink>
                ) : (
                  <span>{c.label}</span>
                )}
              </span>
            ))}
          </div>
          <div className="dk-progress" data-busy={busy} aria-hidden="true" />
        </div>
        <main id="main" className="dk-editor" ref={editor} tabIndex={-1} onScroll={(e) => scrolls.current.set(active, e.currentTarget.scrollTop)}>
          {empty ? <Welcome tasksMode={tasksMode} openPalette={openPalette} /> : children}
        </main>
      </div>

      <footer className="dk-status" aria-label="Status bar">
        <span className="dk-status__brand">
          <Mark size={12} /> Actuaria
        </span>
        <button className="dk-status__item" title={`${folder}\nReveal in ${isMac ? "Finder" : "file manager"}`} onClick={() => native("reveal")}>
          <Icon.Folder size={12} />
          <span className="trunc">{folderName}</span>
        </button>
        {!tasksMode && (
          <span className="dk-status__item" data-live={nRunning > 0}>
            {nRunning > 0 ? <span className="dk-spin" aria-hidden="true" /> : <Icon.Check size={12} />}
            {nRunning > 0 ? `${nRunning} running` : "Idle"}
          </span>
        )}
        {jobs.error && (
          <span className="dk-status__item dk-status__item--bad">
            <Icon.Alert size={12} /> Viewer unreachable
          </span>
        )}
        <span className="dk-status__fill" />
        <button className="dk-status__item" onClick={openPalette}>
          {kbd("K")} Commands
        </button>
        <button className="dk-status__item" onClick={toggleMode}>
          {mode === "dark" ? "Dark" : "Light"}
        </button>
        <button className="dk-status__item" title="Server log" onClick={() => native("server-log")}>
          {desktopInfo ? `Actuaria v${desktopInfo.version}` : "Desktop preview"}
        </button>
      </footer>
    </div>
  );
}

/** The editor with no tab open: the mark and the ways to get going. */
function Welcome({ tasksMode, openPalette }: { tasksMode: boolean; openPalette: () => void }) {
  const navigate = useNavigate();
  const items: { label: string; keys: string; run: () => void }[] = [
    ...(tasksMode
      ? [{ label: "Show tasks", keys: kbd("1"), run: () => navigate("/task-definitions") }]
      : [
          { label: "Show jobs", keys: kbd("1"), run: () => navigate("/") },
          { label: "Compare jobs", keys: kbd("2"), run: () => navigate("/compare") },
          { label: "Start a new run", keys: kbd("N"), run: () => navigate("/run") },
        ]),
    { label: "Open a folder", keys: kbd("O"), run: () => native("open-folder") },
    { label: "Go to a job, trial or page", keys: kbd("K"), run: openPalette },
  ];
  return (
    <div className="dk-welcome">
      <Mark size={96} />
      <div className="dk-welcome__list">
        {items.map((it) => (
          <button key={it.label} className="dk-welcome__item" onClick={it.run}>
            <span>{it.label}</span>
            <kbd>{it.keys}</kbd>
          </button>
        ))}
      </div>
    </div>
  );
}

function ActivityButton({ label, active, kind, onClick, children }: { label: string; active?: boolean; kind: "view" | "page"; onClick: () => void; children: ReactNode }) {
  return (
    <button className="dk-activity__btn" data-active={!!active} data-kind={kind} aria-label={label} aria-pressed={kind === "view" ? !!active : undefined} title={label} onClick={onClick}>
      {children}
    </button>
  );
}

function Resizer({ width, onWidth, onDrag }: { width: number; onWidth: (w: number) => void; onDrag: (on: boolean) => void }) {
  const start = useRef<{ x: number; w: number } | null>(null);
  const end = () => {
    start.current = null;
    onDrag(false);
  };
  return (
    <div
      className="dk-resizer"
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize side bar"
      aria-valuenow={width}
      tabIndex={0}
      onPointerDown={(e) => {
        start.current = { x: e.clientX, w: width };
        e.currentTarget.setPointerCapture(e.pointerId);
        onDrag(true);
      }}
      onPointerMove={(e) => start.current && onWidth(start.current.w + e.clientX - start.current.x)}
      onPointerUp={end}
      onPointerCancel={end}
      onDoubleClick={() => onWidth(SIDE_DEFAULT)}
      onKeyDown={(e) => {
        if (e.key === "ArrowLeft") onWidth(width - 16);
        else if (e.key === "ArrowRight") onWidth(width + 16);
      }}
    />
  );
}

function Filter({ value, onChange, placeholder }: { value: string; onChange: (v: string) => void; placeholder: string }) {
  return (
    <div className="dk-filter">
      <Icon.Filter size={12} />
      <input value={value} onChange={(e) => onChange(e.target.value)} placeholder={placeholder} aria-label={placeholder} spellCheck={false} />
      {value && (
        <button className="dk-icon" aria-label="Clear filter" onClick={() => onChange("")}>
          <Icon.Close size={11} />
        </button>
      )}
    </div>
  );
}

/** Arrow-key movement between a tree's rows (`.dk-row`); `sides` handles Left/Right for expandable rows. */
function treeKeys(e: ReactKeyboardEvent<HTMLElement>, sides?: (row: HTMLElement, key: "ArrowLeft" | "ArrowRight", focus: (el: HTMLElement | undefined) => void) => void) {
  const rows = [...e.currentTarget.querySelectorAll<HTMLElement>(".dk-row")];
  const row = (e.target as HTMLElement).closest<HTMLElement>(".dk-row");
  if (!row) return;
  const i = rows.indexOf(row);
  const focus = (el: HTMLElement | undefined) => el?.focus();
  if (e.key === "ArrowDown") focus(rows[i + 1]);
  else if (e.key === "ArrowUp") focus(rows[i - 1]);
  else if (e.key === "Home") focus(rows[0]);
  else if (e.key === "End") focus(rows[rows.length - 1]);
  else if ((e.key === "ArrowLeft" || e.key === "ArrowRight") && sides) sides(row, e.key, focus);
  else return;
  e.preventDefault();
}

function JobTree({ jobs, loading, error, collapseAll }: { jobs: JobSummary[] | undefined; loading: boolean; error: Error | undefined; collapseAll: number }) {
  const [q, setQ] = useState("");
  const [open, setOpen] = useState<Record<string, boolean>>({});
  const loc = useLocation();
  const shown = useMemo(() => {
    const n = q.trim().toLowerCase();
    return (jobs ?? []).filter((j) => !n || [j.name, ...j.agents, ...j.models, ...j.datasets].some((s) => s.toLowerCase().includes(n)));
  }, [jobs, q]);
  // Open the job whose page is showing.
  const current = loc.pathname.startsWith("/jobs/") ? decodeURIComponent(loc.pathname.split("/")[2] ?? "") : "";
  useEffect(() => {
    if (current) setOpen((o) => (o[current] === undefined ? { ...o, [current]: true } : o));
  }, [current]);
  useEffect(() => {
    if (collapseAll) setOpen((o) => Object.fromEntries(Object.keys(o).map((k) => [k, false])));
  }, [collapseAll]);
  // One row is a tab stop (the page's own, else the first); arrow keys reach the rest.
  const onTrial = loc.pathname.includes("/trials/") && !!open[current];
  const stop = !onTrial && shown.some((j) => j.name === current) ? current : onTrial ? "" : shown[0]?.name;

  return (
    <>
      <Filter value={q} onChange={setQ} placeholder="Filter jobs, agents, models" />
      <div
        className="dk-tree"
        role="tree"
        aria-label="Jobs"
        onKeyDown={(e) =>
          treeKeys(e, (row, key, focus) => {
            const job = row.dataset.job;
            const tree = row.closest(".dk-tree")!;
            if (key === "ArrowRight" && job && !open[job]) setOpen((o) => ({ ...o, [job]: true }));
            else if (key === "ArrowLeft" && job && open[job]) setOpen((o) => ({ ...o, [job]: false }));
            // Left on a trial goes to its job.
            else if (key === "ArrowLeft" && row.dataset.parent) focus(tree.querySelector<HTMLElement>(`.dk-row[data-job="${CSS.escape(row.dataset.parent)}"]`) ?? undefined);
          })
        }
      >
        {loading && <div className="dk-tree__note">Loading jobs…</div>}
        {error && <div className="dk-tree__note dk-tree__note--bad">{error.message}</div>}
        {jobs && !shown.length && (
          <div className="dk-tree__note">
            {q ? "Nothing matches." : "No jobs in this folder yet."}
            {!q && (
              <NavLink to="/run" className="dk-tree__cta">
                Start a run
              </NavLink>
            )}
          </div>
        )}
        {shown.map((j) => {
          const isOpen = !!open[j.name];
          return (
            <div key={j.name} role="treeitem" aria-expanded={isOpen}>
              <NavLink to={jobPath(j.name)} end className="dk-row" data-job={j.name} tabIndex={j.name === stop ? 0 : -1} title={[j.name, j.agents.join(", "), j.models.join(", ")].filter(Boolean).join("\n")}>
                <button
                  className="dk-twisty"
                  data-open={isOpen}
                  tabIndex={-1}
                  aria-label={isOpen ? "Collapse" : "Expand"}
                  onClick={(e) => {
                    e.preventDefault();
                    setOpen((o) => ({ ...o, [j.name]: !isOpen }));
                  }}
                >
                  <Icon.Chevron size={12} />
                </button>
                <JobState j={j} />
                <span className="trunc dk-row__label">{j.name}</span>
                <span className="dk-row__meta mono">
                  {j.n_completed_trials}/{j.n_total_trials}
                </span>
                <button
                  className="dk-row__act"
                  tabIndex={-1}
                  title={`Reveal in ${isMac ? "Finder" : "file manager"}`}
                  aria-label={`Reveal ${j.name} in ${isMac ? "Finder" : "file manager"}`}
                  onClick={(e) => {
                    e.preventDefault();
                    native("reveal", { path: j.name });
                  }}
                >
                  <Icon.External size={12} />
                </button>
              </NavLink>
              {isOpen && <TrialRows job={j.name} live={running(j)} />}
            </div>
          );
        })}
      </div>
    </>
  );
}

function JobState({ j }: { j: JobSummary }) {
  if (running(j)) return <span className="dk-spin" title="Running" />;
  if (j.n_errored_trials > 0) return <span className="dk-dot dk-dot--warn" title={`${j.n_errored_trials} errored`} />;
  return <span className="dk-dot dk-dot--ok" title="Finished" />;
}

function TrialRows({ job, live }: { job: string; live: boolean }) {
  const loc = useLocation();
  const trials = useApi<Page<TrialSummary>>(`/api/jobs/${encodeURIComponent(job)}/trials?page_size=100`, { poll: live ? 4000 : undefined });
  if (!trials.data) return <div className="dk-tree__note dk-tree__note--nested">{trials.error ? trials.error.message : "Loading…"}</div>;
  if (!trials.data.items.length) return <div className="dk-tree__note dk-tree__note--nested">No trials yet.</div>;
  return (
    <div role="group" className="dk-group">
      {trials.data.items.map((t) => {
        const state = !t.finished_at ? "run" : t.error_type ? "bad" : t.reward == null ? "none" : t.reward > 0 ? "ok" : "fail";
        return (
          <NavLink
            key={t.name}
            to={`${jobPath(job)}/trials/${encodeURIComponent(t.name)}`}
            className="dk-row dk-row--nested"
            role="treeitem"
            data-parent={job}
            tabIndex={decodeURIComponent(loc.pathname) === `/jobs/${job}/trials/${t.name}` ? 0 : -1}
            title={[t.task_name, t.name, t.model_name, t.error_type].filter(Boolean).join("\n")}
          >
            {state === "run" ? <span className="dk-spin" /> : <span className={`dk-dot dk-dot--${state}`} />}
            <span className="trunc dk-row__label">{t.task_name.split("/").pop()}</span>
            <span className="dk-row__meta mono">{t.error_type ? "error" : t.reward == null ? "–" : t.reward.toFixed(2)}</span>
          </NavLink>
        );
      })}
    </div>
  );
}

function TaskTree() {
  const tasks = useApi<Page<TaskDefinitionSummary>>("/api/task-definitions?page_size=100");
  const [q, setQ] = useState("");
  const shown = (tasks.data?.items ?? []).filter((t) => t.name.toLowerCase().includes(q.trim().toLowerCase()));
  return (
    <>
      <Filter value={q} onChange={setQ} placeholder="Filter tasks" />
      <div className="dk-tree" role="tree" aria-label="Tasks" onKeyDown={(e) => treeKeys(e)}>
        {!tasks.data && <div className="dk-tree__note">{tasks.error ? tasks.error.message : "Loading tasks…"}</div>}
        {tasks.data && !shown.length && <div className="dk-tree__note">{q ? "Nothing matches." : "No tasks in this folder."}</div>}
        {shown.map((t, i) => (
          <NavLink key={t.name} to={`/task-definitions/${encodeURIComponent(t.name)}`} className="dk-row" role="treeitem" tabIndex={i === 0 ? 0 : -1}>
            <Icon.File size={13} />
            <span className="trunc dk-row__label">{t.name}</span>
          </NavLink>
        ))}
      </div>
    </>
  );
}
