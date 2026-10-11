// Tasks and Trials tabs: server-side filter/sort/paginate tables sharing one URL filter state.
import { useEffect, useMemo, useState } from "react";
import { p, qs, useApi } from "../../lib/api";
import * as fmt from "../../lib/fmt";
import { useDebounced, useUrlState } from "../../lib/state";
import type { Page, TaskFilters, TaskSummary, TrialSummary } from "../../lib/types";
import { FilterMenu } from "../../ui/popover";
import { Empty, ErrorBox, Pagination, Reward, SearchField, SkeletonRows } from "../../ui/kit";
import { DataTable, useColumns, type Column, type SortState } from "../../ui/table";
import { evalValue, extraEvalKeys, fv, tokenSum, trialHref } from "./util";

type Url = ReturnType<typeof useUrlState>;

function useFilters(url: Url, keys: string[]) {
  const params: Record<string, string | string[]> = {};
  for (const k of keys) params[k] = k === "q" ? url.get(k) : url.all(k);
  return params;
}

function Toolbar({ job, url, keys, placeholder, picker }: { job: string; url: Url; keys: [string, string, keyof TaskFilters][]; placeholder: string; picker: React.ReactNode }) {
  const filters = useApi<TaskFilters>(p("jobs", job, "tasks", "filters"));
  const [q, setQ] = useState(url.get("q"));
  const dq = useDebounced(q);
  useEffect(() => {
    if (dq !== url.get("q")) url.set({ q: dq });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dq]);
  const active = keys.some(([k]) => url.all(k).length) || !!url.get("q");
  return (
    <div className="toolbar">
      <SearchField value={q} onChange={setQ} placeholder={placeholder} width={240} />
      {keys.map(([k, label, opt]) => (
        <FilterMenu key={k} label={label} options={filters.data?.[opt]} selected={url.all(k)} onChange={(v) => url.set({ [k]: v })} />
      ))}
      {active && (
        <button
          className="btn btn--sm btn--ghost"
          onClick={() => {
            setQ("");
            url.set(Object.fromEntries([["q", null], ...keys.map(([k]) => [k, null])]));
          }}
        >
          Clear filters
        </button>
      )}
      <span className="toolbar__spacer" />
      {picker}
    </div>
  );
}

function sortOf(url: Url): SortState | null {
  const by = url.get("sort");
  return by ? { by, order: url.get("order") === "asc" ? "asc" : "desc" } : null;
}

const setSort = (url: Url) => (s: SortState | null) => url.set({ sort: s?.by ?? null, order: s?.order ?? null });

const TASK_FILTERS: [string, string, keyof TaskFilters][] = [
  ["task", "Task", "tasks"],
  ["agent", "Agent", "agents"],
  ["provider", "Provider", "providers"],
  ["model", "Model", "models"],
];

export function TasksTab({ job, poll }: { job: string; poll?: number }) {
  const url = useUrlState();
  const sort = sortOf(url);
  const page = Number(url.get("page")) || 1;
  const f = useFilters(url, ["q", "agent", "provider", "model", "task"]);
  const res = useApi<Page<TaskSummary>>(p("jobs", job, "tasks") + qs({ ...f, sort_by: sort?.by, sort_order: sort?.order, page, page_size: 100 }), { poll });
  const rows = res.data?.items ?? [];
  const extras = useMemo(() => extraEvalKeys(rows), [rows]);

  const columns: Column<TaskSummary>[] = [
    { id: "task", header: "Task", sort: "task_name", fixed: true, cell: (r) => <span className="ink">{r.task_name}</span> },
    { id: "source", header: "Source", sort: "source", cell: (r) => <span className="muted">{r.source ?? "—"}</span>, hidden: true },
    { id: "agent", header: "Agent", sort: "agent_name", cell: (r) => r.agent_name ?? "—" },
    { id: "model", header: "Model", sort: "model_name", cell: (r) => <span className="mono">{fmt.model(r.model_provider, r.model_name)}</span> },
    { id: "trials", header: "Trials", sort: "n_trials", align: "right", cell: (r) => <span className="num">{r.n_completed}/{r.n_trials}</span> },
    {
      id: "errors",
      header: "Errors",
      sort: "n_errors",
      cell: (r) =>
        r.n_errors ? (
          <span className="job-errs">
            <span className="num" style={{ color: "var(--color-bad)" }}>
              {r.n_errors}
            </span>
            {r.exception_types.map((t) => (
              <span key={t} className="chip chip--bad">
                {t}
              </span>
            ))}
          </span>
        ) : (
          <span className="faint num">0</span>
        ),
    },
    { id: "reward", header: "Avg reward", sort: "avg_reward", cell: (r) => (r.n_completed < r.n_trials && r.avg_reward == null ? <span className="chip chip--accent">running</span> : <Reward value={r.avg_reward} />) },
    ...extras.map<Column<TaskSummary>>((k) => ({ id: `eval:${k}`, header: k, align: "right", cell: (r) => <span className="num">{fmt.score(evalValue(r.evals, k))}</span> })),
    { id: "duration", header: "Avg time", sort: "avg_duration_ms", align: "right", cell: (r) => <span className="num">{fmt.duration(r.avg_duration_ms)}</span> },
    { id: "tokens", header: "Avg tokens", align: "right", cell: (r) => <span className="num">{fmt.compact(r.avg_tokens)}</span> },
    { id: "in", header: "Avg in", sort: "avg_input_tokens", align: "right", hidden: true, cell: (r) => <span className="num">{fmt.compact(r.avg_input_tokens)}</span> },
    { id: "cached", header: "Avg cached", sort: "avg_cached_input_tokens", align: "right", hidden: true, cell: (r) => <span className="num">{fmt.compact(r.avg_cached_input_tokens)}</span> },
    { id: "out", header: "Avg out", sort: "avg_output_tokens", align: "right", hidden: true, cell: (r) => <span className="num">{fmt.compact(r.avg_output_tokens)}</span> },
    { id: "cost", header: "Avg cost", sort: "avg_cost_usd", align: "right", cell: (r) => <span className="num">{fmt.usd(r.avg_cost_usd)}</span> },
  ];
  const { visible, picker } = useColumns("job-tasks", columns);

  // A task row opens the Trials tab filtered to that task + agent + model.
  const trialsOf = (r: TaskSummary) => {
    const q = new URLSearchParams({ tab: "trials", task: r.task_name, agent: fv(r.agent_name), provider: fv(r.model_provider), model: fv(r.model_name) });
    return `?${q}`;
  };

  return (
    <>
      <Toolbar job={job} url={url} keys={TASK_FILTERS} placeholder="Search tasks, agents, models" picker={picker} />
      {res.error && <ErrorBox error={res.error} onRetry={res.reload} />}
      {!res.data && !res.error ? (
        <SkeletonRows />
      ) : (
        <DataTable
            label="Tasks"
            rows={rows}
            columns={visible}
            rowKey={(r) => [r.task_name, r.source, r.agent_name, r.model_provider, r.model_name].join("|")}
            href={trialsOf}
            sort={sort}
            onSort={setSort(url)}
            empty={<Empty title="No tasks match these filters">Clear a filter or the search to see more.</Empty>}
          />
      )}
      {res.data && <Pagination page={res.data.page} totalPages={res.data.total_pages} total={res.data.total} onPage={(n) => url.set({ page: n })} />}
    </>
  );
}

const TRIAL_FILTERS: [string, string, keyof TaskFilters][] = [
  ["task", "Task", "tasks"],
  ["agent", "Agent", "agents"],
  ["provider", "Provider", "providers"],
  ["model", "Model", "models"],
  ["source", "Source", "sources"],
  ["reasoning_effort", "Effort", "reasoning_efforts"],
];

export function TrialsTab({ job, poll }: { job: string; poll?: number }) {
  const url = useUrlState();
  const sort = sortOf(url);
  const page = Number(url.get("page")) || 1;
  const f = useFilters(url, ["q", "agent", "provider", "model", "task", "source", "reasoning_effort"]);
  const res = useApi<Page<TrialSummary>>(p("jobs", job, "trials") + qs({ ...f, sort_by: sort?.by, sort_order: sort?.order, page, page_size: 100 }), { poll });
  const rows = res.data?.items ?? [];
  const extras = useMemo(() => extraEvalKeys(rows), [rows]);

  const columns: Column<TrialSummary>[] = [
    { id: "name", header: "Trial", sort: "name", fixed: true, cell: (r) => <span className="mono ink">{r.name}</span> },
    { id: "task", header: "Task", sort: "task_name", cell: (r) => r.task_name },
    { id: "source", header: "Source", sort: "source", hidden: true, cell: (r) => <span className="muted">{r.source ?? "—"}</span> },
    { id: "agent", header: "Agent", sort: "agent_name", cell: (r) => <span title={r.reported_agent_name && r.reported_agent_name !== r.agent_name ? `reported as ${r.reported_agent_name}` : undefined}>{r.agent_name}</span> },
    { id: "model", header: "Model", sort: "model_name", cell: (r) => <span className="mono">{fmt.model(r.model_provider, r.model_name)}</span> },
    { id: "effort", header: "Effort", sort: "reasoning_effort", hidden: true, cell: (r) => r.reasoning_effort ?? <span className="faint">—</span> },
    {
      id: "reward",
      header: "Reward",
      sort: "reward",
      cell: (r) => (r.started_at && !r.finished_at ? <span className="chip chip--accent">running</span> : !r.started_at && !r.finished_at && r.reward == null ? <span className="chip">pending</span> : <Reward value={r.reward} />),
    },
    ...extras.map<Column<TrialSummary>>((k) => ({ id: `eval:${k}`, header: k, align: "right", cell: (r) => <span className="num">{fmt.score(evalValue(r.evals, k))}</span> })),
    { id: "error", header: "Error", sort: "error_type", cell: (r) => (r.error_type ? <span className="chip chip--bad">{r.error_type}</span> : <span className="faint">—</span>) },
    { id: "started", header: "Started", sort: "started_at", cell: (r) => <span className="muted" title={fmt.dateTime(r.started_at)}>{fmt.date(r.started_at)}</span> },
    { id: "duration", header: "Time", sort: "duration", align: "right", cell: (r) => <span className="num">{fmt.duration(fmt.ms(r.started_at, r.finished_at))}</span> },
    {
      id: "tokens",
      header: "Tokens",
      align: "right",
      cell: (r) => <span className="num">{fmt.compact(tokenSum(r.input_tokens, r.cached_input_tokens, r.output_tokens))}</span>,
      title: (r) => `${fmt.int(r.input_tokens)} in · ${fmt.int(r.cached_input_tokens)} cached · ${fmt.int(r.output_tokens)} out`,
    },
    { id: "cost", header: "Cost", sort: "cost_usd", align: "right", cell: (r) => <span className="num">{fmt.usd(r.cost_usd)}</span> },
  ];
  const { visible, picker } = useColumns("job-trials", columns);

  return (
    <>
      <Toolbar job={job} url={url} keys={TRIAL_FILTERS} placeholder="Search trials" picker={picker} />
      {res.error && <ErrorBox error={res.error} onRetry={res.reload} />}
      {!res.data && !res.error ? (
        <SkeletonRows />
      ) : (
        <DataTable
          label="Trials"
          rows={rows}
          columns={visible}
          rowKey={(r) => r.name}
          href={(r) => trialHref(job, r.name)}
          sort={sort}
          onSort={setSort(url)}
          empty={<Empty title="No trials match these filters">Clear a filter or the search to see more.</Empty>}
        />
      )}
      {res.data && <Pagination page={res.data.page} totalPages={res.data.total_pages} total={res.data.total} onPage={(n) => url.set({ page: n })} />}
    </>
  );
}
