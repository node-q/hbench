// App shell: top bar (wordmark, section links, ⌘K search pill, theme toggle) + command palette.
// Pages add their own palette entries with `useCommands([...])`.
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { NavLink, Outlet, useNavigate } from "react-router";
import * as Icon from "./ui/icons";
import { useApi } from "./lib/api";
import { useKey } from "./lib/state";
import type { AppConfig, JobSummary, Page, TaskDefinitionSummary } from "./lib/types";
import { ToastProvider } from "./ui/overlay";

export interface Command {
  id: string;
  label: string;
  group: string;
  hint?: string;
  run: () => void;
}

const ConfigCtx = createContext<AppConfig | undefined>(undefined);
export const useAppConfig = () => useContext(ConfigCtx);

const CommandsCtx = createContext<(id: string, cmds: Command[] | null) => void>(() => {});

/** Register palette commands while the calling component is mounted. */
export function useCommands(cmds: Command[]) {
  const register = useContext(CommandsCtx);
  const key = useRef(Math.random().toString(36)).current;
  useEffect(() => {
    register(key, cmds);
  }, [register, key, cmds]);
  useEffect(() => () => register(key, null), [register, key]);
}

function useMode(): [string, () => void] {
  const [mode, setMode] = useState(() => document.documentElement.dataset.mode ?? "light");
  const toggle = useCallback(() => {
    const next = mode === "dark" ? "light" : "dark";
    document.documentElement.dataset.mode = next;
    localStorage.setItem("hb:mode", next);
    setMode(next);
  }, [mode]);
  return [mode, toggle];
}

export function Shell() {
  const config = useApi<AppConfig>("/api/config");
  const [mode, toggleMode] = useMode();
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [pageCmds, setPageCmds] = useState<Record<string, Command[]>>({});
  const register = useCallback((id: string, cmds: Command[] | null) => {
    setPageCmds((m) => {
      const next = { ...m };
      if (cmds) next[id] = cmds;
      else delete next[id];
      return next;
    });
  }, []);
  useKey(
    "mod+k",
    useCallback((e: KeyboardEvent) => {
      e.preventDefault();
      setPaletteOpen((o) => !o);
    }, []),
  );
  useKey(
    "/",
    useCallback((e: KeyboardEvent) => {
      e.preventDefault();
      setPaletteOpen(true);
    }, []),
  );

  const tasksMode = config.data?.mode === "tasks";
  const links = tasksMode
    ? [{ to: "/task-definitions", label: "Tasks" }]
    : [
        { to: "/", label: "Jobs", end: true },
        { to: "/compare", label: "Compare" },
        { to: "/run", label: "New run" },
      ];

  return (
    <ConfigCtx.Provider value={config.data}>
      <CommandsCtx.Provider value={register}>
        <ToastProvider>
          <a href="#main" className="sr-only">
            Skip to content
          </a>
          <header className="bar">
            <NavLink to={tasksMode ? "/task-definitions" : "/"} className="bar__brand" aria-label="hbench home">
              Hbench
            </NavLink>
            <nav className="bar__links" aria-label="Sections">
              {links.map((l) => (
                <NavLink key={l.to} to={l.to} end={l.end} className="bar__link">
                  {l.label}
                </NavLink>
              ))}
            </nav>
            <button className="bar__search" onClick={() => setPaletteOpen(true)} aria-label="Search and commands (⌘K)">
              <Icon.Search size={14} />
              <span className="bar__search-text">{tasksMode ? "Jump to a task…" : "Jump to a job, trial or page…"}</span>
              <span className="bar__search-kbd">
                <kbd>⌘</kbd>
                <kbd>K</kbd>
              </span>
            </button>
            <div className="bar__right">
              {config.data && (
                <span className="bar__folder mono" title={config.data.folder}>
                  {config.data.folder.split("/").slice(-2).join("/")}
                </span>
              )}
              <button className="btn btn--ghost btn--icon" onClick={toggleMode} aria-label={mode === "dark" ? "Switch to light mode" : "Switch to dark mode"} title="Toggle theme">
                {mode === "dark" ? <Icon.Sun /> : <Icon.Moon />}
              </button>
            </div>
          </header>
          <main id="main">
            <Outlet />
          </main>
          <Palette open={paletteOpen} onClose={() => setPaletteOpen(false)} tasksMode={tasksMode} pageCmds={pageCmds} toggleMode={toggleMode} />
        </ToastProvider>
      </CommandsCtx.Provider>
    </ConfigCtx.Provider>
  );
}

function Palette({ open, onClose, tasksMode, pageCmds, toggleMode }: { open: boolean; onClose: () => void; tasksMode: boolean; pageCmds: Record<string, Command[]>; toggleMode: () => void }) {
  const navigate = useNavigate();
  const ref = useRef<HTMLDialogElement>(null);
  const [q, setQ] = useState("");
  const [active, setActive] = useState(0);
  // Jump targets load lazily the first time the palette opens.
  const [wanted, setWanted] = useState(false);
  useEffect(() => {
    if (open) setWanted(true);
  }, [open]);
  const jobs = useApi<Page<JobSummary>>(wanted && !tasksMode ? "/api/jobs?page_size=100" : null);
  const tasks = useApi<Page<TaskDefinitionSummary>>(wanted && tasksMode ? "/api/task-definitions?page_size=100" : null);

  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (open && !d.open) {
      setQ("");
      setActive(0);
      d.showModal();
    }
    if (!open && d.open) d.close();
  }, [open]);

  const all: Command[] = useMemo(() => {
    const go = (to: string) => () => navigate(to);
    const base: Command[] = tasksMode
      ? [{ id: "p:tasks", label: "Tasks", group: "Pages", run: go("/task-definitions") }]
      : [
          { id: "p:jobs", label: "Jobs", group: "Pages", run: go("/") },
          { id: "p:compare", label: "Compare jobs", group: "Pages", run: go("/compare") },
          { id: "p:run", label: "Start a new run", group: "Pages", run: go("/run") },
        ];
    base.push({ id: "a:theme", label: "Toggle light / dark", group: "Actions", run: toggleMode });
    const page = Object.values(pageCmds).flat();
    const jumps: Command[] = tasksMode
      ? (tasks.data?.items ?? []).map((t) => ({ id: `t:${t.name}`, label: t.name, group: "Tasks", hint: (t.metadata?.category as string) ?? undefined, run: go(`/task-definitions/${encodeURIComponent(t.name)}`) }))
      : (jobs.data?.items ?? []).map((j) => ({ id: `j:${j.name}`, label: j.name, group: "Jobs", hint: [j.agents.join(", "), j.models.join(", ")].filter(Boolean).join(" · "), run: go(`/jobs/${encodeURIComponent(j.name)}`) }));
    return [...page, ...base, ...jumps];
  }, [tasksMode, pageCmds, jobs.data, tasks.data, navigate, toggleMode]);

  const shown = useMemo(() => {
    const terms = q.toLowerCase().split(/\s+/).filter(Boolean);
    return all.filter((c) => terms.every((t) => `${c.label} ${c.hint ?? ""} ${c.group}`.toLowerCase().includes(t))).slice(0, 60);
  }, [all, q]);
  useEffect(() => setActive(0), [q]);

  const run = (c: Command | undefined) => {
    if (!c) return;
    onClose();
    c.run();
  };

  let lastGroup = "";
  return (
    <dialog
      ref={ref}
      className="palette"
      onClose={onClose}
      onClick={(e) => {
        if (e.target === ref.current) onClose();
      }}
      aria-label="Command palette"
    >
      {open && (
        <>
          <div className="palette__field">
            <Icon.Search size={15} />
            <input
              autoFocus
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder={tasksMode ? "Search tasks and commands" : "Search jobs, trials and commands"}
              aria-controls="palette-list"
              aria-activedescendant={shown[active] ? `pal-${active}` : undefined}
              onKeyDown={(e) => {
                if (e.key === "ArrowDown") {
                  e.preventDefault();
                  setActive((a) => Math.min(shown.length - 1, a + 1));
                } else if (e.key === "ArrowUp") {
                  e.preventDefault();
                  setActive((a) => Math.max(0, a - 1));
                } else if (e.key === "Enter") {
                  e.preventDefault();
                  run(shown[active]);
                }
              }}
            />
            <kbd>esc</kbd>
          </div>
          <div className="palette__list" id="palette-list" role="listbox">
            {shown.map((c, i) => {
              const head = c.group !== lastGroup ? (lastGroup = c.group) : null;
              return (
                <div key={c.id}>
                  {head && <div className="label palette__group">{head}</div>}
                  <button
                    id={`pal-${i}`}
                    role="option"
                    aria-selected={i === active}
                    className="menu-item"
                    data-active={i === active}
                    onMouseMove={() => setActive(i)}
                    onClick={() => run(c)}
                  >
                    <span className="trunc">{c.label}</span>
                    {c.hint && <span className="trunc faint" style={{ marginLeft: "auto", fontSize: "var(--text-xs)", maxWidth: "45%" }}>{c.hint}</span>}
                  </button>
                </div>
              );
            })}
            {!shown.length && <div className="menu-label muted">Nothing matches “{q}”.</div>}
          </div>
          <div className="palette__foot label">
            <span>
              <kbd>↑</kbd>
              <kbd>↓</kbd> move
            </span>
            <span>
              <kbd>↵</kbd> open
            </span>
          </div>
        </>
      )}
    </dialog>
  );
}

/** Document title helper. */
export function useTitle(title: string | undefined) {
  useEffect(() => {
    document.title = title ? `${title} · hbench` : "hbench";
  }, [title]);
}

export function NotFound({ what = "Page" }: { what?: string }): ReactNode {
  return (
    <div className="page">
      <div className="empty">
        <span className="empty__title">{what} not found</span>
        <NavLink to="/" className="link">
          Back to jobs
        </NavLink>
      </div>
    </div>
  );
}
