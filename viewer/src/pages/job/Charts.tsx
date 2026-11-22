// Charts tab: reward-vs-cost frontier and an outcome waffle. Hand-built SVG/CSS.
import { useMemo } from "react";
import { Link } from "react-router";
import * as fmt from "../../lib/fmt";
import { lower } from "../../lib/fmt";
import { useStored } from "../../lib/state";
import type { TrialSummary } from "../../lib/types";
import { Empty, ErrorBox, Seg, SkeletonRows } from "../../ui/kit";
import { tokenSum, trialHref } from "./util";

type XKey = "cost" | "duration" | "tokens";
const X: Record<XKey, { label: string; get: (t: TrialSummary) => number | null; fmt: (n: number) => string; tick?: (n: number) => string }> = {
  cost: { label: "Average cost per attempt", get: (t) => t.cost_usd, fmt: (n) => fmt.usd(n), tick: (n) => (n ? `$${+n.toPrecision(3)}` : "$0") },
  duration: { label: "Average time per attempt", get: (t) => fmt.ms(t.started_at, t.finished_at), fmt: (n) => fmt.duration(n) },
  tokens: { label: "Average tokens per attempt", get: (t) => tokenSum(t.input_tokens, t.cached_input_tokens, t.output_tokens), fmt: (n) => fmt.compact(n) },
};

const X_NAME: Record<XKey, string> = { cost: "cost", duration: "time", tokens: "tokens" };

const groupOf = (t: TrialSummary) => [t.agent_name, fmt.model(t.model_provider, t.model_name), t.reasoning_effort].filter((x) => x && x !== "—").join(" · ");

export function Charts({ job, trials, error }: { job: string; trials: TrialSummary[] | undefined; error: unknown }) {
  if (error && !trials) return <ErrorBox error={error} />;
  if (!trials) return <SkeletonRows />;
  if (!trials.length) return <Empty title="No attempts to chart yet" />;
  return (
    <div className="charts">
      <Frontier trials={trials} />
      <Waffle job={job} trials={trials} />
    </div>
  );
}

// ---------------------------------------------------------------- frontier scatter

interface Pt {
  label: string;
  x: number;
  y: number;
  n: number;
  frontier?: boolean;
}

function mean(xs: number[]) {
  return xs.reduce((a, b) => a + b, 0) / xs.length;
}

/** Points with no other point at ≤ x and ≥ y (lower cost, higher reward). */
export function markFrontier(pts: Pt[]): Pt[] {
  const sorted = [...pts].sort((a, b) => a.x - b.x || b.y - a.y);
  let best = -Infinity;
  for (const p of sorted) {
    if (p.y > best) {
      p.frontier = true;
      best = p.y;
    }
  }
  return sorted;
}

/** ~5 round ticks spanning [lo, hi]. */
function ticks(lo: number, hi: number): number[] {
  if (hi <= lo) hi = lo + 1;
  const raw = (hi - lo) / 4;
  const mag = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((s) => s >= raw)!;
  const out = [];
  // Extend until a tick reaches `hi`, so the largest point is always inside the axis.
  for (let v = Math.floor(lo / step) * step; ; v += step) {
    out.push(+v.toPrecision(12));
    if (v >= hi - step * 1e-9) break;
  }
  return out;
}

function Frontier({ trials }: { trials: TrialSummary[] }) {
  const [xKey, setXKey] = useStored<XKey>("job-frontier-x", "cost");
  const x = X[xKey];
  const pts = useMemo(() => {
    const groups = new Map<string, TrialSummary[]>();
    for (const t of trials) groups.set(groupOf(t), [...(groups.get(groupOf(t)) ?? []), t]);
    const out: Pt[] = [];
    for (const [label, ts] of groups) {
      const ys = ts.map((t) => t.reward).filter((v): v is number => typeof v === "number");
      const xs = ts.map(x.get).filter((v): v is number => typeof v === "number");
      if (ys.length && xs.length) out.push({ label: label || "(unnamed)", x: mean(xs), y: mean(ys), n: ts.length });
    }
    return markFrontier(out);
  }, [trials, x]);

  const W = 720,
    H = 340,
    L = 56,
    R = 24,
    T = 16,
    B = 44;
  const xt = ticks(0, Math.max(...pts.map((p) => p.x), 0));
  const yMax = Math.max(1, ...pts.map((p) => p.y));
  const yt = ticks(Math.min(0, ...pts.map((p) => p.y)), yMax);
  const sx = (v: number) => L + ((v - xt[0]) / (xt[xt.length - 1] - xt[0] || 1)) * (W - L - R);
  const sy = (v: number) => H - B - ((v - yt[0]) / (yt[yt.length - 1] - yt[0] || 1)) * (H - T - B);
  const front = pts.filter((p) => p.frontier);

  return (
    <section className="panel">
      <div className="panel__head">
        <h3>Score against {X_NAME[xKey]}</h3>
        <Seg
          label="Compare the score with"
          value={xKey}
          onChange={setXKey}
          options={[
            { id: "cost", label: "Cost" },
            { id: "duration", label: "Time" },
            { id: "tokens", label: "Tokens" },
          ]}
        />
      </div>
      <div className="panel__body">
        {!pts.length ? (
          <Empty title="Nothing to plot">At least one attempt needs a score and a known {X_NAME[xKey] === "tokens" ? "number of tokens" : X_NAME[xKey]}.</Empty>
        ) : (
          <>
            <div className="chart-scroll">
              <svg viewBox={`0 0 ${W} ${H}`} className="chart" role="img" aria-label={`Average score against ${lower(x.label)} for each agent and model`}>
                {yt.map((v) => (
                  <g key={`y${v}`}>
                    <line x1={L} x2={W - R} y1={sy(v)} y2={sy(v)} className="chart__grid" />
                    <text x={L - 8} y={sy(v)} className="chart__tick" textAnchor="end" dominantBaseline="middle">
                      {fmt.score(v, 2)}
                    </text>
                  </g>
                ))}
                {xt.map((v) => (
                  <text key={`x${v}`} x={sx(v)} y={H - B + 16} className="chart__tick" textAnchor="middle">
                    {(x.tick ?? x.fmt)(v)}
                  </text>
                ))}
                <text x={(L + W - R) / 2} y={H - 6} className="chart__axis" textAnchor="middle">
                  {x.label}
                </text>
                <text x={14} y={(T + H - B) / 2} className="chart__axis" textAnchor="middle" transform={`rotate(-90 14 ${(T + H - B) / 2})`}>
                  Average score
                </text>
                {front.length > 1 && <polyline points={front.map((p) => `${sx(p.x)},${sy(p.y)}`).join(" ")} className="chart__frontier" />}
                {pts.map((p) => (
                  <g key={p.label}>
                    <circle cx={sx(p.x)} cy={sy(p.y)} r={p.frontier ? 5 : 4} className={p.frontier ? "chart__pt chart__pt--front" : "chart__pt"}>
                      <title>{`${p.label}\naverage score ${fmt.score(p.y)} · ${x.fmt(p.x)} · ${p.n} attempt${p.n === 1 ? "" : "s"}`}</title>
                    </circle>
                    <text x={sx(p.x) + 8} y={sy(p.y) - 8} className="chart__label" textAnchor={sx(p.x) > W - 200 ? "end" : "start"} dx={sx(p.x) > W - 200 ? -16 : 0}>
                      {p.label}
                    </text>
                  </g>
                ))}
              </svg>
            </div>
            <p className="chart-note">Each point is an agent and model. The line joins the ones that no other beats on both score and {X_NAME[xKey]}.</p>
          </>
        )}
      </div>
    </section>
  );
}

// ---------------------------------------------------------------- outcome waffle

type GroupBy = "task" | "model";

function outcome(t: TrialSummary): { cls: string; style?: React.CSSProperties; text: string } {
  if (t.error_type) return { cls: "w-sq w-sq--err", text: `error: ${t.error_type}` };
  if (!t.finished_at) return { cls: "w-sq w-sq--pending", text: t.started_at ? "running" : "waiting" };
  if (typeof t.reward !== "number") return { cls: "w-sq w-sq--none", text: "no score" };
  const v = Math.max(0, Math.min(1, t.reward));
  return { cls: "w-sq", style: { background: `color-mix(in oklch, var(--color-heat-1) ${Math.round(v * 100)}%, var(--color-heat-0))` }, text: ("reward" in t.evals ? fmt.verdict(t.reward) : null) ?? `score ${fmt.score(t.reward)}` };
}

function Waffle({ job, trials }: { job: string; trials: TrialSummary[] }) {
  const [by, setBy] = useStored<GroupBy>("job-waffle-by", "task");
  const [transposed, setTransposed] = useStored("job-waffle-t", false);
  const groups = useMemo(() => {
    const m = new Map<string, TrialSummary[]>();
    for (const t of trials) {
      const k = by === "task" ? t.task_name : groupOf(t) || "(unnamed)";
      m.set(k, [...(m.get(k) ?? []), t]);
    }
    return [...m.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  }, [trials, by]);

  return (
    <section className="panel">
      <div className="panel__head">
        <h3>Every attempt</h3>
        <div className="toolbar" style={{ margin: 0 }}>
          <Seg
            label="Group by"
            value={by}
            onChange={setBy}
            options={[
              { id: "task", label: "By task" },
              { id: "model", label: "By agent and model" },
            ]}
          />
          <button className={`btn btn--sm${transposed ? " is-on" : ""}`} aria-pressed={transposed} onClick={() => setTransposed(!transposed)}>
            Stack in columns
          </button>
        </div>
      </div>
      <div className="panel__body">
        <div className="chart-scroll">
          <div className={`waffle${transposed ? " waffle--cols" : ""}`}>
            {groups.map(([k, ts]) => {
              const done = ts.filter((t) => t.finished_at);
              const binary = done.length > 0 && done.every((t) => (t.reward == null ? t.error_type : "reward" in t.evals && fmt.verdict(t.reward)));
              const rs = ts.map((t) => t.reward).filter((v): v is number => typeof v === "number");
              return (
                <div key={k} className="waffle__group">
                  <span className="waffle__label trunc" title={k}>
                    {k}
                  </span>
                  <span className="waffle__cells">
                    {ts.map((t) => {
                      const o = outcome(t);
                      return <Link key={t.name} to={trialHref(job, t.name)} className={o.cls} style={o.style} title={`${t.name}\n${o.text}`} aria-label={`${t.name}: ${o.text}`} />;
                    })}
                  </span>
                  <span className="waffle__mean num" title={rs.length ? `average score ${fmt.score(mean(rs))}` : undefined}>
                    {binary ? fmt.passed(done.filter((t) => t.reward === 1).length, done.length) : rs.length ? fmt.score(mean(rs), 2) : "—"}
                  </span>
                </div>
              );
            })}
          </div>
        </div>
        <div className="legend">
          <span>
            <i className="w-sq" style={{ background: "var(--color-heat-0)" }} /> failed (0)
          </span>
          <span>
            <i className="w-sq" style={{ background: "var(--color-heat-1)" }} /> passed (1)
          </span>
          <span>
            <i className="w-sq w-sq--err" /> error
          </span>
          <span>
            <i className="w-sq w-sq--pending" /> running or waiting
          </span>
          <span>
            <i className="w-sq w-sq--none" /> no score
          </span>
        </div>
      </div>
    </section>
  );
}
