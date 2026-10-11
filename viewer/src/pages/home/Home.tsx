import { useEffect, useMemo, useState } from "react";
import { Link, Navigate, useNavigate } from "react-router";
import { useApi, send, p, qs } from "../../lib/api";
import * as fmt from "../../lib/fmt";
import { useUrlState, useDebounced } from "../../lib/state";
import type { JobFilters, JobSummary, Page } from "../../lib/types";
import { useAppConfig, useCommands, useTitle, type Command } from "../../shell";
import { DataTable, sortRows, useColumns, type Column, type SortState } from "../../ui/table";
import { FilterMenu, SelectMenu } from "../../ui/popover";
import { CopyButton, Empty, ErrorBox, PageHead, Pagination, Reward, SearchField, SkeletonRows } from "../../ui/kit";
import { Dialog, useToast } from "../../ui/overlay";
import * as Icon from "../../ui/icons";
import "./home.css";

const DATES = [
  { value: "today", label: "Last 24 hours" },
  { value: "week", label: "Last 7 days" },
  { value: "month", label: "Last 30 days" },
];

const running = (j: JobSummary) => !!j.started_at && !j.finished_at;

function sortValue(j: JobSummary, key: string): unknown {
  switch (key) {
    case "duration":
      return fmt.ms(j.started_at, j.finished_at ?? (j.started_at ? new Date().toISOString() : null));
    case "result":
      return fmt.firstMetric(j.evals);
    case "trials":
      return j.n_completed_trials;
    case "agents":
      return j.agents.join(",");
    case "models":
      return j.models.join(",");
    case "datasets":
      return j.datasets.join(",");
    default:
      return (j as any)[key];
  }
}

export default function Home() {
  const config = useAppConfig();
  useTitle("Jobs");
  const navigate = useNavigate();
  const toast = useToast();
  const url = useUrlState();
  const [search, setSearch] = useState(url.get("q"));
  const q = useDebounced(search);
  useEffect(() => {
    if (q !== url.get("q")) url.set({ q });
  }, [q]); // eslint-disable-line react-hooks/exhaustive-deps

  const page = Number(url.get("page")) || 1;
  const filters = { agent: url.all("agent"), provider: url.all("provider"), model: url.all("model"), date: url.all("date") };
  const anyFilter = !!url.get("q") || Object.values(filters).some((v) => v.length);
  const apiPath = `/api/jobs${qs({ q: url.get("q"), ...filters, page, page_size: 50 })}`;
  const [live, setLive] = useState(false);
  const jobs = useApi<Page<JobSummary>>(config?.mode === "tasks" ? null : apiPath, { poll: live ? 5000 : undefined });
  const options = useApi<JobFilters>(config?.mode === "tasks" ? null : "/api/jobs/filters");
  useEffect(() => setLive(!!jobs.data?.items.some(running)), [jobs.data]);

  const sort: SortState | null = url.get("sort") ? { by: url.get("sort"), order: url.get("order") === "asc" ? "asc" : "desc" } : null;
  const rows = useMemo(() => sortRows(jobs.data?.items ?? [], sort, sortValue), [jobs.data, sort?.by, sort?.order]); // eslint-disable-line react-hooks/exhaustive-deps

  const [selected, setSelected] = useState<string[]>([]);
  const [deleting, setDeleting] = useState<string[] | null>(null);

  const columns: Column<JobSummary>[] = [
    {
      id: "name",
      header: "Job",
      fixed: true,
      sort: "name",
      cell: (j) => (
        <span className="home-name">
          <Link to={`/jobs/${encodeURIComponent(j.name)}`} className="link mono ink">
            {j.name}
          </Link>
          {config && <CopyButton text={`${config.folder}/${j.name}`} label="Copy job path" />}
        </span>
      ),
    },
    { id: "started", header: "Started", sort: "started_at", cell: (j) => <span className="muted">{fmt.ago(j.started_at)}</span>, title: (j) => fmt.dateTime(j.started_at) },
    {
      id: "duration",
      header: "Duration",
      sort: "duration",
      align: "right",
      cell: (j) =>
        running(j) ? (
          <span className="chip chip--accent">
            <span className="dot dot--live" /> running
          </span>
        ) : (
          <span className="num">{fmt.duration(fmt.ms(j.started_at, j.finished_at))}</span>
        ),
    },
    {
      id: "trials",
      header: "Trials",
      sort: "trials",
      align: "right",
      cell: (j) => (
        <span className="num">
          {j.n_completed_trials}
          <span className="faint">/{j.n_total_trials}</span>
          {j.n_errored_trials > 0 && <span className="home-err"> {j.n_errored_trials} err</span>}
        </span>
      ),
    },
    { id: "result", header: "Result", sort: "result", cell: (j) => <Reward value={fmt.firstMetric(j.evals)} /> },
    { id: "agents", header: "Agent", sort: "agents", cell: (j) => j.agents.join(", ") || <span className="faint">—</span> },
    {
      id: "models",
      header: "Model",
      sort: "models",
      cell: (j) => <span className="mono">{j.models.map((m) => (j.providers.length === 1 ? `${j.providers[0]}/${m}` : m)).join(", ") || "—"}</span>,
    },
    { id: "datasets", header: "Source", sort: "datasets", cell: (j) => j.datasets.join(", ") || <span className="faint">—</span> },
    { id: "env", header: "Environment", sort: "environment_type", hidden: true, cell: (j) => <span className="mono">{j.environment_type ?? "—"}</span> },
    { id: "in", header: "Uncached in", sort: "total_input_tokens", align: "right", hidden: true, cell: (j) => <span className="num">{fmt.compact(j.total_input_tokens)}</span> },
    { id: "cached", header: "Cached in", sort: "total_cached_input_tokens", align: "right", hidden: true, cell: (j) => <span className="num">{fmt.compact(j.total_cached_input_tokens)}</span> },
    { id: "out", header: "Output", sort: "total_output_tokens", align: "right", hidden: true, cell: (j) => <span className="num">{fmt.compact(j.total_output_tokens)}</span> },
    { id: "cost", header: "Cost", sort: "total_cost_usd", align: "right", cell: (j) => <span className="num">{fmt.usd(j.total_cost_usd)}</span> },
  ];
  const { visible, picker } = useColumns("jobs", columns);

  const compare = () => navigate(`/compare${qs({ job: selected })}`);
  const cmds: Command[] = useMemo(
    () => (selected.length ? [{ id: "home:compare", label: `Compare ${selected.length} selected job${selected.length > 1 ? "s" : ""}`, group: "This page", run: compare }] : []),
    [selected], // eslint-disable-line react-hooks/exhaustive-deps
  );
  useCommands(cmds);

  if (config?.mode === "tasks") return <Navigate to="/task-definitions" replace />;

  const clearFilters = () => {
    setSearch("");
    url.set({ q: null, agent: null, provider: null, model: null, date: null });
  };
  const data = jobs.data;

  return (
    <div className="page">
      <PageHead
        title="Jobs"
        meta={
          data && (
            <span className="num">
              {data.total} job{data.total === 1 ? "" : "s"}
              {live && " · refreshing every 5s"}
            </span>
          )
        }
        actions={
          <Link to="/run" className="btn btn--primary">
            <Icon.Play size={13} /> New run
          </Link>
        }
      />

      <div className="toolbar">
        <SearchField value={search} onChange={setSearch} placeholder="Search jobs, agents, models" width="min(280px, 100%)" />
        <FilterMenu label="Agent" options={options.data?.agents} selected={filters.agent} onChange={(v) => url.set({ agent: v })} />
        <FilterMenu label="Provider" options={options.data?.providers} selected={filters.provider} onChange={(v) => url.set({ provider: v })} />
        <FilterMenu label="Model" options={options.data?.models} selected={filters.model} onChange={(v) => url.set({ model: v })} />
        <SelectMenu
          label="Started"
          value={filters.date[0] ?? "any"}
          options={[{ id: "any", label: "Any time" }, ...DATES.map((d) => ({ id: d.value, label: d.label }))]}
          onChange={(v) => url.set({ date: v === "any" ? null : v })}
        />
        {anyFilter && (
          <button className="btn btn--ghost btn--sm" onClick={clearFilters}>
            Clear
          </button>
        )}
        <span className="toolbar__spacer" />
        {selected.length > 0 && (
          <>
            <span className="muted num home-selcount">{selected.length} selected</span>
            <button className="btn btn--sm" onClick={compare}>
              Compare {selected.length}
            </button>
            <button className="btn btn--sm btn--danger" onClick={() => setDeleting(selected)}>
              <Icon.Trash size={13} /> Delete
            </button>
          </>
        )}
        {picker}
      </div>

      {jobs.error && !data ? (
        <ErrorBox error={jobs.error} onRetry={jobs.reload} />
      ) : !data ? (
        <SkeletonRows />
      ) : data.total === 0 && !anyFilter ? (
        <Empty title="No jobs in this folder yet">
          <span>Start one from the command line, or use New run.</span>
          <span className="home-cmd">
            <code>hbench run -d &lt;dataset&gt;</code>
            <CopyButton text="hbench run -d <dataset>" label="Copy command" />
          </span>
        </Empty>
      ) : (
        <>
          <DataTable
            label="Jobs"
            rows={rows}
            columns={visible}
            rowKey={(j) => j.name}
            href={(j) => `/jobs/${encodeURIComponent(j.name)}`}
            sort={sort}
            onSort={(s) => url.set({ sort: s?.by ?? null, order: s?.order ?? null, page })}
            selected={selected}
            onSelect={setSelected}
            empty={
              <Empty
                title="No jobs match those filters"
                action={
                  <button className="btn btn--sm" onClick={clearFilters}>
                    Clear filters
                  </button>
                }
              />
            }
          />
          <Pagination page={data.page} totalPages={data.total_pages} total={data.total} onPage={(n) => url.set({ page: n })} />
        </>
      )}

      <DeleteDialog
        names={deleting}
        onClose={() => setDeleting(null)}
        onDone={(gone) => {
          setDeleting(null);
          setSelected((s) => s.filter((x) => !gone.includes(x)));
          jobs.reload();
          options.reload();
          toast({ text: `Deleted ${gone.length === 1 ? gone[0] : `${gone.length} jobs`}` });
        }}
      />
    </div>
  );
}

/** Irreversible: the user types the job name (or the count, for several) to confirm. */
function DeleteDialog({ names, onClose, onDone }: { names: string[] | null; onClose: () => void; onDone: (gone: string[]) => void }) {
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  useEffect(() => {
    setTyped("");
    setError(undefined);
  }, [names]);
  if (!names) return null;
  const expect = names.length === 1 ? names[0] : `delete ${names.length} jobs`;
  const run = async () => {
    setBusy(true);
    const gone: string[] = [];
    try {
      for (const n of names) {
        await send("DELETE", p("jobs", n));
        gone.push(n);
      }
      onDone(gone);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      if (gone.length) onDone(gone);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open
      onClose={onClose}
      title={names.length === 1 ? "Delete job" : `Delete ${names.length} jobs`}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn btn--danger" disabled={typed !== expect || busy} data-state={busy ? "loading" : undefined} onClick={run}>
            {busy ? "Deleting…" : "Delete permanently"}
          </button>
        </>
      }
    >
      <p>
        This removes the job directory{names.length > 1 ? "s" : ""} and every trial in {names.length > 1 ? "them" : "it"} from disk. It can't be undone.
      </p>
      {names.length > 1 && (
        <ul className="home-dellist mono">
          {names.map((n) => (
            <li key={n}>{n}</li>
          ))}
        </ul>
      )}
      <div className="field">
        <label htmlFor="del-confirm">
          Type <code className="ink">{expect}</code> to confirm
        </label>
        <input id="del-confirm" className="input input--mono" value={typed} onChange={(e) => setTyped(e.target.value)} autoFocus autoComplete="off" aria-invalid={!!error} />
        <span className={`field__help${error ? " is-error" : ""}`}>{error}</span>
      </div>
    </Dialog>
  );
}
