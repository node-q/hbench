import { useEffect, useMemo, useState } from "react";
import { Link } from "react-router";
import { useApi, qs, enc } from "../../lib/api";
import { useDebounced, useUrlState } from "../../lib/state";
import { duration, int } from "../../lib/fmt";
import type { Page, TaskDefinitionFilters, TaskDefinitionSummary } from "../../lib/types";
import { DataTable, sortRows, useColumns, type Column, type SortState } from "../../ui/table";
import { FilterMenu } from "../../ui/popover";
import { Empty, ErrorBox, PageHead, Pagination, SearchField, SkeletonRows } from "../../ui/kit";
import { useTitle } from "../../shell";
import "./tasks.css";

const mb = (n: number | null) => (n == null ? "—" : n >= 1024 ? `${+(n / 1024).toFixed(1)} GB` : `${n} MB`);
const secs = (n: number | null) => duration(n == null ? null : n * 1000);

const FLAGS: [keyof TaskDefinitionSummary, string, string][] = [
  ["has_instruction", "I", "instruction"],
  ["has_environment", "E", "environment"],
  ["has_tests", "T", "tests"],
  ["has_solution", "S", "solution"],
  ["has_docker_compose", "C", "docker compose"],
];

export function Flags({ t }: { t: Pick<TaskDefinitionSummary, "has_instruction" | "has_environment" | "has_tests" | "has_solution" | "has_docker_compose"> }) {
  const label = FLAGS.map(([k, , name]) => `${name}: ${t[k as keyof typeof t] ? "yes" : "no"}`).join(", ");
  return (
    <span className="flags" aria-label={label} title={label}>
      {FLAGS.map(([k, ch]) => (
        <span key={k} className={t[k as keyof typeof t] ? "flags__on" : "flags__off"} aria-hidden="true">
          {ch}
        </span>
      ))}
    </span>
  );
}

export function Tags({ tags, max = 2 }: { tags: string[]; max?: number }) {
  if (!tags.length) return <span className="faint">—</span>;
  return (
    <span className="tag-row" title={tags.join(", ")}>
      {tags.slice(0, max).map((t) => (
        <span key={t} className="chip">
          {t}
        </span>
      ))}
      {tags.length > max && <span className="faint mono">+{tags.length - max}</span>}
    </span>
  );
}

export default function TaskDefinitions() {
  useTitle("Tasks");
  const url = useUrlState();
  const [q, setQ] = useState(url.get("q"));
  const dq = useDebounced(q);
  useEffect(() => {
    if (dq !== url.get("q")) url.set({ q: dq });
  }, [dq]); // eslint-disable-line react-hooks/exhaustive-deps

  const difficulty = url.all("difficulty");
  const category = url.all("category");
  const tag = url.all("tag");
  const page = Number(url.get("page")) || 1;
  const sort: SortState | null = url.get("sort") ? { by: url.get("sort"), order: url.get("order") === "asc" ? "asc" : "desc" } : null;

  const filters = useApi<TaskDefinitionFilters>("/api/task-definitions/filters");
  const list = useApi<Page<TaskDefinitionSummary>>(`/api/task-definitions${qs({ q: url.get("q"), difficulty, category, tag, page, page_size: 100 })}`);

  const columns: Column<TaskDefinitionSummary>[] = [
    { id: "name", header: "Name", sort: "name", fixed: true, cell: (t) => <Link to={`/task-definitions/${enc(t.name)}`} className="ink" style={{ fontWeight: 500 }}>{t.name}</Link> },
    { id: "difficulty", header: "Difficulty", sort: "difficulty", cell: (t) => (t.metadata?.difficulty ? <span className={`chip diff--${t.metadata.difficulty}`}>{t.metadata.difficulty}</span> : <span className="faint">—</span>) },
    { id: "category", header: "Category", sort: "category", cell: (t) => t.metadata?.category ?? <span className="faint">—</span> },
    { id: "tags", header: "Tags", cell: (t) => <Tags tags={(t.metadata?.tags as string[]) ?? []} /> },
    { id: "source", header: "Source", sort: "source", hidden: true, cell: (t) => t.source ?? <span className="faint">—</span> },
    { id: "agent_timeout", header: "Agent timeout", sort: "agent_timeout_sec", align: "right", cell: (t) => <span className="num">{secs(t.agent_timeout_sec)}</span> },
    { id: "verifier_timeout", header: "Verifier timeout", sort: "verifier_timeout_sec", align: "right", cell: (t) => <span className="num">{secs(t.verifier_timeout_sec)}</span> },
    { id: "os", header: "OS", sort: "os", hidden: true, cell: (t) => <span className="mono">{t.os ?? "—"}</span> },
    { id: "cpus", header: "CPUs", sort: "cpus", align: "right", cell: (t) => <span className="num">{int(t.cpus)}</span> },
    { id: "memory", header: "Memory", sort: "memory_mb", align: "right", cell: (t) => <span className="num">{mb(t.memory_mb)}</span> },
    { id: "storage", header: "Storage", sort: "storage_mb", align: "right", hidden: true, cell: (t) => <span className="num">{mb(t.storage_mb)}</span> },
    { id: "gpus", header: "GPUs", sort: "gpus", align: "right", hidden: true, cell: (t) => <span className="num">{int(t.gpus)}</span> },
    { id: "flags", header: "Files", cell: (t) => <Flags t={t} /> },
  ];
  const { visible, picker } = useColumns("task-definitions", columns);

  const rows = useMemo(
    () => sortRows(list.data?.items ?? [], sort, (t, k) => (k in t ? (t as any)[k] : t.metadata?.[k])),
    [list.data, sort?.by, sort?.order], // eslint-disable-line react-hooks/exhaustive-deps
  );
  const anyFilter = !!(url.get("q") || difficulty.length || category.length || tag.length);
  const clear = () => {
    setQ("");
    url.set({ q: null, difficulty: [], category: [], tag: [] });
  };

  return (
    <div className="page">
      <PageHead
        title="Tasks"
        meta={
          list.data && (
            <span className="num">
              {list.data.total} task{list.data.total === 1 ? "" : "s"}
            </span>
          )
        }
      />
      <div className="toolbar">
        <SearchField value={q} onChange={setQ} placeholder="Search name, source, metadata" />
        <FilterMenu label="Difficulty" options={filters.data?.difficulties} selected={difficulty} onChange={(v) => url.set({ difficulty: v })} />
        <FilterMenu label="Category" options={filters.data?.categories} selected={category} onChange={(v) => url.set({ category: v })} />
        <FilterMenu label="Tag" options={filters.data?.tags} selected={tag} onChange={(v) => url.set({ tag: v })} />
        {anyFilter && (
          <button className="btn btn--ghost btn--sm" onClick={clear}>
            Clear filters
          </button>
        )}
        <span className="toolbar__spacer" />
        {picker}
      </div>
      {list.error ? (
        <ErrorBox error={list.error} onRetry={list.reload} />
      ) : !list.data ? (
        <SkeletonRows />
      ) : (
        <>
          <DataTable
            label="Task definitions"
            rows={rows}
            columns={visible}
            rowKey={(t) => t.name}
            href={(t) => `/task-definitions/${enc(t.name)}`}
            sort={sort}
            onSort={(s) => url.set({ sort: s?.by, order: s?.order })}
            empty={
              <Empty
                title={anyFilter ? "No tasks match those filters" : "No tasks in this folder"}
                action={
                  anyFilter ? (
                    <button className="btn btn--sm" onClick={clear}>
                      Clear filters
                    </button>
                  ) : undefined
                }
              >
                {!anyFilter && "Each task is a subdirectory with a task.toml."}
              </Empty>
            }
          />
          <Pagination page={list.data.page} totalPages={list.data.total_pages} total={list.data.total} onPage={(n) => url.set({ page: n })} />
        </>
      )}
    </div>
  );
}
