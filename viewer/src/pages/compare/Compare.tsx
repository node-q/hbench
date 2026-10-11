import { Fragment, useMemo } from "react";
import { Link } from "react-router";
import { useApi, qs } from "../../lib/api";
import * as fmt from "../../lib/fmt";
import { useUrlState } from "../../lib/state";
import type { CompareCell, CompareData, JobSummary, Page } from "../../lib/types";
import { useTitle } from "../../shell";
import { FilterMenu, SelectMenu } from "../../ui/popover";
import { Empty, ErrorBox, PageHead, Seg, SkeletonRows } from "../../ui/kit";
import "./compare.css";

type Metric = "reward" | "duration" | "cost" | "tokens";
const METRICS: { id: Metric; label: string; get: (c: CompareCell) => number | null; show: (n: number | null) => string }[] = [
  { id: "reward", label: "Reward", get: (c) => c.avg_reward, show: (n) => fmt.score(n) },
  { id: "duration", label: "Duration", get: (c) => c.avg_duration_ms, show: fmt.duration },
  { id: "cost", label: "Cost", get: (c) => c.avg_cost_usd, show: fmt.usd },
  { id: "tokens", label: "Tokens", get: (c) => c.avg_tokens, show: fmt.compact },
];

type ColGroup = "none" | "job" | "agent" | "model";
type RowGroup = "none" | "source";

interface AxisItem {
  key: string;
  label: string;
  sub: string;
  group: string;
}

const avg = (xs: number[]) => (xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : null);

export default function Compare() {
  useTitle("Compare");
  const url = useUrlState();
  const jobs = url.all("job");
  const metric = (url.get("color") || "reward") as Metric;
  const transpose = url.get("t") === "1";
  const colGroup = (url.get("cg") || "none") as ColGroup;
  const rowGroup = (url.get("rg") || "none") as RowGroup;
  const m = METRICS.find((x) => x.id === metric) ?? METRICS[0];

  const list = useApi<Page<JobSummary>>("/api/jobs?page_size=100");
  const data = useApi<CompareData>(jobs.length ? `/api/compare${qs({ job: jobs })}` : null);

  const view = useMemo(() => {
    const d = data.data;
    if (!d) return null;
    const amGroup = (a: CompareData["agent_models"][number]) =>
      colGroup === "job" ? a.job_name : colGroup === "agent" ? a.agent_name ?? "—" : colGroup === "model" ? fmt.model(a.model_provider, a.model_name) : "";
    const ams: AxisItem[] = d.agent_models.map((a) => ({
      key: a.key,
      label: `${a.agent_name ?? "—"} · ${fmt.model(a.model_provider, a.model_name)}`,
      sub: a.job_name,
      group: amGroup(a),
    }));
    const tasks: AxisItem[] = d.tasks.map((t) => ({ key: t.key, label: t.task_name, sub: t.source ?? "", group: rowGroup === "source" ? t.source ?? "(no source)" : "" }));
    // Stable grouping: items of a group stay in their server (best-first) order, groups in first-seen order.
    const grouped = (items: AxisItem[]) => {
      const order = [...new Set(items.map((i) => i.group))];
      return order.flatMap((g) => items.filter((i) => i.group === g));
    };
    const rows = grouped(transpose ? ams : tasks);
    const cols = grouped(transpose ? tasks : ams);
    const cell = (r: AxisItem, c: AxisItem) => (transpose ? d.cells[c.key]?.[r.key] : d.cells[r.key]?.[c.key]);
    const values = Object.values(d.cells).flatMap((x) => Object.values(x).map(m.get)).filter((v): v is number => v != null);
    const lo = Math.min(...values);
    const hi = Math.max(...values);
    // Rewards in [0,1] shade on the absolute scale; everything else min–max across the grid.
    const unit = metric === "reward" && lo >= 0 && hi <= 1;
    const norm = (v: number | null) => (v == null ? null : unit ? v : hi > lo ? (v - lo) / (hi - lo) : 0.5);
    const meanOf = (cs: (CompareCell | undefined)[]) => avg(cs.map((c) => (c ? m.get(c) : null)).filter((v): v is number => v != null));
    return { rows, cols, cell, norm, meanOf };
  }, [data.data, transpose, colGroup, rowGroup, metric, m]);

  const jobOptions = (list.data?.items ?? []).map((j) => ({ value: j.name, count: j.n_total_trials }));
  const colLabel = transpose ? "Tasks" : "Agent · model";
  const rowLabel = transpose ? "Agent · model" : "Task";

  return (
    <div className="page">
      <PageHead
        crumbs={[{ to: "/", label: "Jobs" }]}
        title="Compare"
        meta={<span>Average {m.label.toLowerCase()} per task for each agent and model, across the selected jobs.</span>}
      />
      <div className="toolbar">
        <FilterMenu label={jobs.length ? `Jobs` : "Pick jobs"} options={jobOptions} selected={jobs} onChange={(v) => url.set({ job: v })} />
        <SelectMenu label="Colour by" value={metric} options={METRICS.map((x) => ({ id: x.id, label: x.label }))} onChange={(v) => url.set({ color: v === "reward" ? null : v })} />
        <SelectMenu
          label="Columns by"
          value={colGroup}
          options={[
            { id: "none", label: "No grouping" },
            { id: "job", label: "Job" },
            { id: "agent", label: "Agent" },
            { id: "model", label: "Model" },
          ]}
          onChange={(v) => url.set({ cg: v === "none" ? null : v })}
        />
        <SelectMenu
          label="Rows by"
          value={rowGroup}
          options={[
            { id: "none", label: "No grouping" },
            { id: "source", label: "Source" },
          ]}
          onChange={(v) => url.set({ rg: v === "none" ? null : v })}
        />
        <Seg
          label="Orientation"
          value={transpose ? "t" : "n"}
          options={[
            { id: "n", label: "Tasks as rows" },
            { id: "t", label: "Tasks as columns" },
          ]}
          onChange={(v) => url.set({ t: v === "t" ? "1" : null })}
        />
      </div>

      {jobs.length > 0 && (
        <div className="cmp-picked">
          {jobs.map((j) => (
            <span key={j} className="chip">
              <Link to={`/jobs/${encodeURIComponent(j)}`} className="link ink">
                {j}
              </Link>
              <button className="cmp-x" aria-label={`Remove ${j}`} onClick={() => url.set({ job: jobs.filter((x) => x !== j) })}>
                ×
              </button>
            </span>
          ))}
        </div>
      )}

      {!jobs.length ? (
        <Empty title="Select at least one job to compare">Pick jobs above, or select rows on the Jobs page and choose Compare.</Empty>
      ) : data.error && !data.data ? (
        <ErrorBox error={data.error} onRetry={data.reload} />
      ) : !view ? (
        <SkeletonRows />
      ) : !view.rows.length || !view.cols.length ? (
        <Empty title="Nothing to compare">The selected jobs have no trials yet.</Empty>
      ) : (
        <>
          <div className="table-wrap cmp-wrap">
            <table className="cmp">
              <thead>
                {view.cols.some((c) => c.group) && (
                  <tr className="cmp-grouprow">
                    <th className="cmp-corner" />
                    {spans(view.cols).map(([g, n], i) => (
                      <th key={i} colSpan={n} className="cmp-group">
                        {g}
                      </th>
                    ))}
                  </tr>
                )}
                <tr>
                  <th className="cmp-corner label">
                    {rowLabel} <span className="faint">↓</span> / {colLabel} <span className="faint">→</span>
                  </th>
                  {view.cols.map((c) => {
                    const mean = view.meanOf(view.rows.map((r) => view.cell(r, c)));
                    return (
                      <th key={c.key} className="cmp-col" title={`${c.label}\n${c.sub}`}>
                        <span className="cmp-col__label">{c.label}</span>
                        <span className="cmp-col__sub mono">{c.sub}</span>
                        <span className="cmp-col__mean num">{m.show(mean)}</span>
                      </th>
                    );
                  })}
                </tr>
              </thead>
              <tbody>
                {view.rows.map((r, i) => (
                  <Fragment key={r.key}>
                    {r.group && r.group !== view.rows[i - 1]?.group && (
                      <tr>
                        <th className="cmp-rowgroup label" colSpan={view.cols.length + 1}>
                          {r.group}
                        </th>
                      </tr>
                    )}
                    <tr>
                      <th className="cmp-row" scope="row" title={r.sub ? `${r.label}\n${r.sub}` : r.label}>
                        <div className="cmp-row__in">
                          <span className="trunc">{r.label}</span>
                          <span className="num faint">{m.show(view.meanOf(view.cols.map((c) => view.cell(r, c))))}</span>
                        </div>
                      </th>
                      {view.cols.map((c) => {
                        const cl = view.cell(r, c);
                        return <Cell key={c.key} cell={cl} norm={cl ? view.norm(m.get(cl)) : null} show={m.show} get={m.get} task={transpose ? c : r} am={transpose ? r : c} />;
                      })}
                    </tr>
                  </Fragment>
                ))}
              </tbody>
            </table>
          </div>
          <p className="cmp-legend label">
            <span className="cmp-swatch" style={{ background: "var(--color-heat-0)" }} /> {metric === "reward" ? "0" : "low"}
            <span className="cmp-swatch" style={{ background: "var(--color-heat-1)" }} /> {metric === "reward" ? "1" : "high"} · blank = not run · click a cell for its trials
          </p>
        </>
      )}
    </div>
  );
}

/** Contiguous runs of the same group label → [label, count]. */
function spans(items: AxisItem[]): [string, number][] {
  const out: [string, number][] = [];
  for (const it of items) {
    const last = out[out.length - 1];
    if (last && last[0] === it.group) last[1]++;
    else out.push([it.group, 1]);
  }
  return out;
}

function Cell({ cell, norm, show, get, task, am }: { cell: CompareCell | undefined; norm: number | null; show: (n: number | null) => string; get: (c: CompareCell) => number | null; task: AxisItem; am: AxisItem }) {
  if (!cell) return <td className="cmp-cell cmp-cell--none" aria-label="not run" />;
  const pct = norm == null ? 0 : Math.round(Math.max(0, Math.min(1, norm)) * 100);
  // agent_models key = job::agent::provider::model
  const [, agent, provider, model] = am.key.split("::");
  const to = `/jobs/${encodeURIComponent(cell.job_name)}${qs({ tab: "trials", task: task.label, agent: agent || null, provider: provider || null, model: model || null })}`;
  const title = [
    `${task.label} · ${am.label}`,
    `${cell.n_completed}/${cell.n_trials} trials completed`,
    `reward ${fmt.score(cell.avg_reward)}`,
    `duration ${fmt.duration(cell.avg_duration_ms)}`,
    `cost ${fmt.usd(cell.avg_cost_usd)}`,
    `tokens ${fmt.compact(cell.avg_tokens)}`,
  ].join("\n");
  return (
    <td className="cmp-cell" style={{ background: `color-mix(in oklch, var(--color-heat-1) ${pct}%, var(--color-heat-0))` }} data-dark={pct > 55 || undefined}>
      <Link to={to} title={title} className="cmp-cell__link num">
        {show(get(cell))}
        {cell.n_completed < cell.n_trials && <span className="cmp-cell__partial">{cell.n_completed}/{cell.n_trials}</span>}
      </Link>
    </td>
  );
}
