import { Link } from "react-router";
import * as fmt from "../../lib/fmt";
import type { EvalStats, JobResult } from "../../lib/types";
import { Stat, StackBar } from "../../ui/kit";
import { trialHref } from "./util";

/** Passed and graded trials of one eval; null unless every score is 1 (passed) or 0 (failed). An error before grading fails. */
function passes(e: EvalStats): { n: number; of: number } | null {
  const keys = Object.keys(e.reward_stats ?? {});
  const byScore = Object.entries(e.reward_stats?.reward ?? {});
  if ((keys.length && !byScore.length) || byScore.some(([v]) => !fmt.verdict(Number(v)))) return null;
  const graded = new Set(byScore.flatMap(([, names]) => names));
  if (graded.size !== e.n_trials) return null;
  const all = new Set([...graded, ...Object.values(e.exception_stats ?? {}).flat()]);
  if (!all.size) return null;
  return { n: byScore.filter(([v]) => Number(v) === 1).reduce((a, [, names]) => a + names.length, 0), of: all.size };
}

/** "14 of 16 passed" with the exact score under it, or the metrics as they are when answers have partial scores. */
function scores(key: string, e: EvalStats, many: boolean) {
  const name = (m: string) => (m === "mean" ? "average score" : m);
  const where = many && (
    <span className="trunc" title={key}>
      {key}
    </span>
  );
  const metrics = e.metrics.flatMap((m) => Object.entries(m));
  const got = passes(e);
  if (got)
    return [
      <Stat
        key={`${key}:passed`}
        label="passed"
        value={<span className="num">{got.n} of {got.of}</span>}
        sub={
          <>
            {metrics.map(([m, v]) => `${name(m)} ${fmt.score(v)}`).join(" · ")}
            {where && <> {where}</>}
          </>
        }
      />,
    ];
  return metrics.map(([m, v]) => <Stat key={`${key}:${m}`} label={name(m)} value={fmt.score(v)} sub={where || `${e.n_trials} attempt${e.n_trials === 1 ? "" : "s"}`} />);
}

export function StatsStrip({ job, result }: { job: string; result: JobResult }) {
  const s = result.stats;
  const evals = Object.entries(s.evals);
  const many = evals.length > 1;
  const uncached = s.n_input_tokens != null ? s.n_input_tokens - (s.n_cache_tokens ?? 0) : null;
  const totalTokens = (uncached ?? 0) + (s.n_cache_tokens ?? 0) + (s.n_output_tokens ?? 0);
  const extra = [
    ["running", s.n_running_trials],
    ["waiting", s.n_pending_trials],
    ["cancelled", s.n_cancelled_trials],
    ["retries", s.n_retries],
  ].filter(([, n]) => (n as number) > 0);

  // exception type → trial names, merged across evals
  const exceptions: Record<string, string[]> = {};
  for (const [, e] of evals) for (const [type, names] of Object.entries(e.exception_stats ?? {})) exceptions[type] = [...(exceptions[type] ?? []), ...names];

  return (
    <>
      <div className="stats">
        {evals.flatMap(([key, e]) => [
          ...scores(key, e, many),
          ...Object.entries(e.pass_at_k ?? {}).map(([k, v]) => <Stat key={`${key}:pass@${k}`} label={`passed in ${k} tries`} value={fmt.pct(v)} sub={<span className="trunc" title={key}>{many ? key : `pass@${k} ${fmt.score(v)}`}</span>} />),
        ])}
        <Stat label="finished" value={<span className="num">{s.n_completed_trials} of {result.n_total_trials}</span>} sub={extra.length ? extra.map(([l, n]) => `${n} ${l}`).join(" · ") : undefined} />
        <Stat label="errors" value={<span style={s.n_errored_trials ? { color: "var(--color-bad)" } : undefined}>{s.n_errored_trials}</span>} />
        <Stat
          label="tokens"
          value={fmt.compact(totalTokens || null)}
          sub={
            totalTokens > 0 && (
              <span className="job-tokens">
                <StackBar
                  height={5}
                  parts={[
                    { label: "new input", value: uncached ?? 0, color: "var(--color-phase-agent)" },
                    { label: "cached input", value: s.n_cache_tokens ?? 0, color: "var(--color-phase-cache)" },
                    { label: "output", value: s.n_output_tokens ?? 0, color: "var(--color-phase-verify)" },
                  ]}
                />
                <span className="num">
                  {fmt.compact(uncached)} new in · {fmt.compact(s.n_cache_tokens)} cached · {fmt.compact(s.n_output_tokens)} out
                </span>
              </span>
            )
          }
        />
        <Stat label="cost" value={fmt.usd(s.cost_usd)} sub={s.cost_usd != null && s.n_completed_trials ? `${fmt.usd(s.cost_usd / s.n_completed_trials)} per attempt` : undefined} />
      </div>
      {Object.keys(exceptions).length > 0 && (
        <div className="job-exceptions">
          {Object.entries(exceptions).map(([type, names]) => (
            <div key={type} className="job-exceptions__row">
              <span className="chip chip--bad">{type}</span>
              <span className="num faint">{names.length}×</span>
              <span className="job-exceptions__trials">
                {names.map((n) => (
                  <Link key={n} to={trialHref(job, n)} className="link mono">
                    {n}
                  </Link>
                ))}
              </span>
            </div>
          ))}
        </div>
      )}
    </>
  );
}
