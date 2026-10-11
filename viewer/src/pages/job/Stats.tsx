import { Link } from "react-router";
import * as fmt from "../../lib/fmt";
import type { JobResult } from "../../lib/types";
import { Stat, StackBar } from "../../ui/kit";
import { trialHref } from "./util";

export function StatsStrip({ job, result }: { job: string; result: JobResult }) {
  const s = result.stats;
  const evals = Object.entries(s.evals);
  const many = evals.length > 1;
  const uncached = s.n_input_tokens != null ? s.n_input_tokens - (s.n_cache_tokens ?? 0) : null;
  const totalTokens = (uncached ?? 0) + (s.n_cache_tokens ?? 0) + (s.n_output_tokens ?? 0);
  const extra = [
    ["running", s.n_running_trials],
    ["pending", s.n_pending_trials],
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
          ...e.metrics.flatMap((m) =>
            Object.entries(m).map(([name, v]) => <Stat key={`${key}:${name}`} label={name} value={fmt.score(v)} sub={many ? <span className="trunc" title={key}>{key}</span> : `${e.n_trials} trials`} />),
          ),
          ...Object.entries(e.pass_at_k ?? {}).map(([k, v]) => <Stat key={`${key}:pass@${k}`} label={`pass@${k}`} value={fmt.score(v)} sub={many ? <span className="trunc" title={key}>{key}</span> : undefined} />),
        ])}
        <Stat label="completed" value={<span className="num">{s.n_completed_trials}/{result.n_total_trials}</span>} sub={extra.length ? extra.map(([l, n]) => `${n} ${l}`).join(" · ") : undefined} />
        <Stat label="errored" value={<span style={s.n_errored_trials ? { color: "var(--color-bad)" } : undefined}>{s.n_errored_trials}</span>} />
        <Stat
          label="tokens"
          value={fmt.compact(totalTokens || null)}
          sub={
            totalTokens > 0 && (
              <span className="job-tokens">
                <StackBar
                  height={5}
                  parts={[
                    { label: "uncached input", value: uncached ?? 0, color: "var(--color-phase-agent)" },
                    { label: "cached input", value: s.n_cache_tokens ?? 0, color: "var(--color-phase-cache)" },
                    { label: "output", value: s.n_output_tokens ?? 0, color: "var(--color-phase-verify)" },
                  ]}
                />
                <span className="num">
                  {fmt.compact(uncached)} in · {fmt.compact(s.n_cache_tokens)} cached · {fmt.compact(s.n_output_tokens)} out
                </span>
              </span>
            )
          }
        />
        <Stat label="cost" value={fmt.usd(s.cost_usd)} sub={s.cost_usd != null && s.n_completed_trials ? `${fmt.usd(s.cost_usd / s.n_completed_trials)} per trial` : undefined} />
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
