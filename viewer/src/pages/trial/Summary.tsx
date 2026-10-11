// Outcome strip: rewards, phase timing, token mix, exception.
import * as fmt from "../../lib/fmt";
import type { StepResult, TimingInfo, TrialResult } from "../../lib/types";
import { Reward, StackBar } from "../../ui/kit";

const PHASES = [
  { key: "environment_setup", label: "Environment setup", color: "var(--color-phase-env)" },
  { key: "agent_setup", label: "Agent setup", color: "var(--color-phase-setup)" },
  { key: "agent_execution", label: "Agent execution", color: "var(--color-phase-agent)" },
  { key: "verifier", label: "Verifier", color: "var(--color-phase-verify)" },
] as const;

function span(t: TimingInfo | null | undefined, now: string | null): number | null {
  return t?.started_at ? fmt.ms(t.started_at, t.finished_at ?? now) : null;
}

export function Summary({ r, step, running }: { r: TrialResult; step?: StepResult; running: boolean }) {
  const src = step ?? r;
  const now = running ? new Date().toISOString() : null;
  const rewards = Object.entries(src.verifier_result?.rewards ?? {});
  const phases = PHASES.map((ph) => ({ ...ph, value: span((src as any)[ph.key] as TimingInfo | null, now) }));
  const timed = phases.filter((ph) => ph.value != null);

  // Tokens: the step's own context, or the trial's (summed over steps when the trial has no top-level one).
  const ctxs = step ? [step.agent_result] : r.agent_result ? [r.agent_result] : (r.step_results ?? []).map((s) => s.agent_result);
  const sum = (k: "n_input_tokens" | "n_cache_tokens" | "n_output_tokens" | "cost_usd") => {
    const vs = ctxs.map((c) => c?.[k]).filter((v): v is number => typeof v === "number");
    return vs.length ? vs.reduce((a, b) => a + b, 0) : null;
  };
  const input = sum("n_input_tokens");
  const cached = sum("n_cache_tokens");
  const output = sum("n_output_tokens");
  const cost = sum("cost_usd");
  const uncached = input != null ? input - (cached ?? 0) : null;
  const exc = src.exception_info;

  return (
    <section className="trial-summary" aria-label="Outcome">
      <div className="trial-summary__cell">
        <span className="label">Rewards</span>
        {rewards.length ? (
          <dl className="kv trial-summary__kv">
            {rewards.map(([k, v]) => (
              <div key={k} style={{ display: "contents" }}>
                <dt className="mono">{k}</dt>
                <dd>{typeof v === "number" ? <Reward value={v} /> : <span className="mono">{String(v)}</span>}</dd>
              </div>
            ))}
          </dl>
        ) : (
          <span className="muted">{running ? "Pending verification" : "No verifier result"}</span>
        )}
      </div>

      <div className="trial-summary__cell">
        <span className="label">Timing</span>
        {timed.length ? (
          <>
            <StackBar parts={timed.map((ph) => ({ label: ph.label, value: ph.value!, color: ph.color }))} />
            <ul className="tlegend">
              {phases.map((ph) => (
                <li key={ph.key}>
                  <i style={{ background: ph.color }} />
                  <span>{ph.label}</span>
                  <span className="num ink">{fmt.duration(ph.value)}</span>
                </li>
              ))}
            </ul>
          </>
        ) : (
          <span className="muted">No timing data</span>
        )}
      </div>

      <div className="trial-summary__cell">
        <span className="label">Tokens</span>
        {input != null || output != null ? (
          <>
            <StackBar
              parts={[
                { label: "Cached input", value: cached ?? 0, color: "var(--color-phase-cache)" },
                { label: "Uncached input", value: uncached ?? 0, color: "var(--color-phase-agent)" },
                { label: "Output", value: output ?? 0, color: "var(--color-phase-verify)" },
              ]}
            />
            <ul className="tlegend">
              <li>
                <i style={{ background: "var(--color-phase-cache)" }} />
                <span>Cached input</span>
                <span className="num ink">{fmt.compact(cached)}</span>
              </li>
              <li>
                <i style={{ background: "var(--color-phase-agent)" }} />
                <span>Uncached input</span>
                <span className="num ink">{fmt.compact(uncached)}</span>
              </li>
              <li>
                <i style={{ background: "var(--color-phase-verify)" }} />
                <span>Output</span>
                <span className="num ink">{fmt.compact(output)}</span>
              </li>
              <li>
                <i />
                <span>Cost</span>
                <span className="num ink">{fmt.usd(cost)}</span>
              </li>
            </ul>
          </>
        ) : (
          <span className="muted">No token data</span>
        )}
      </div>

      {exc && (
        <div className="trial-summary__exc" role="note">
          <span className="label" style={{ color: "var(--color-bad)" }}>
            {exc.exception_type}
          </span>
          <p className="mono trial-summary__msg">{exc.exception_message.split("\n")[0]}</p>
          {exc.occurred_at && <span className="faint" style={{ fontSize: "var(--text-xs)" }}>at {fmt.dateTime(exc.occurred_at)}</span>}
        </div>
      )}
    </section>
  );
}
