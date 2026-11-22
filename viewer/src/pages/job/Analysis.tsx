import { useState } from "react";
import { Link } from "react-router";
import { p, send, useApi } from "../../lib/api";
import * as fmt from "../../lib/fmt";
import type { Obj, RunOptions } from "../../lib/types";
import { JsonTree, Markdown } from "../../ui/content";
import * as Icon from "../../ui/icons";
import { Empty, ErrorBox, SkeletonRows } from "../../ui/kit";
import { Dialog, useToast } from "../../ui/overlay";
import { trialHref } from "./util";

interface Check {
  outcome: "pass" | "fail" | "not_applicable" | string;
  explanation?: string;
}
interface Result {
  trial_name?: string | null;
  summary?: string | null;
  checks?: Record<string, Check>;
  cost_usd?: number | null;
  error?: string | null;
}

const titleCase = (s: string) => s.replace(/_/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());

export function AnalysisTab({ job, tick, onAnalyze }: { job: string; tick: number; onAnalyze: () => void }) {
  const res = useApi<Obj>(p("jobs", job, "analysis") + (tick ? `?t=${tick}` : ""));
  if (res.error) return <ErrorBox error={res.error} onRetry={res.reload} />;
  if (!res.data) return <SkeletonRows />;
  const a = res.data;
  const results: Result[] | undefined = Array.isArray(a.results) ? a.results : undefined;
  const summary = typeof a.summary === "string" ? a.summary : typeof a.job_summary === "string" ? a.job_summary : null;
  if (!Object.keys(a).length)
    return (
      <Empty
        title="Nothing analyzed yet"
        action={
          <button className="btn btn--primary" onClick={onAnalyze}>
            <Icon.Sparkle size={14} /> Analyze attempts
          </button>
        }
      >
        An agent reads what the AI did in each attempt, and the logs. It writes a summary and a list of pass or fail checks.
      </Empty>
    );
  if (!results && !summary) return <JsonTree value={a} />;

  const checkNames = [...new Set((results ?? []).flatMap((r) => Object.keys(r.checks ?? {})))];
  const cost = (results ?? []).reduce((s, r) => s + (r.cost_usd ?? 0), 0);
  return (
    <div className="analysis">
      {summary && <Markdown text={summary} />}
      {results && (
        <>
          <p className="muted" style={{ fontSize: "var(--text-sm)" }}>
            {results.length} attempt{results.length === 1 ? "" : "s"} analyzed{cost ? ` · ${fmt.usd(cost)}` : ""}.{" "}
            <button className="btn btn--sm btn--ghost" onClick={onAnalyze}>
              <Icon.Refresh size={13} /> Run again
            </button>
          </p>
          {checkNames.length > 0 && (
            <div className="table-wrap">
              <table className="table" aria-label="Analysis checks">
                <thead>
                  <tr>
                    <th>Attempt</th>
                    {checkNames.map((c) => (
                      <th key={c}>{titleCase(c)}</th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {results.map((r, i) => (
                    <tr key={r.trial_name ?? i}>
                      <td>{r.trial_name ? <Link className="link mono" to={trialHref(job, r.trial_name)}>{r.trial_name}</Link> : "—"}</td>
                      {checkNames.map((c) => {
                        const ch = r.checks?.[c];
                        return (
                          <td key={c} title={ch?.explanation}>
                            {ch ? <span className={`chip${ch.outcome === "pass" ? " chip--good" : ch.outcome === "fail" ? " chip--bad" : ""}`}>{ch.outcome === "not_applicable" ? "n/a" : ch.outcome === "pass" ? "passed" : ch.outcome === "fail" ? "failed" : ch.outcome}</span> : <span className="faint">—</span>}
                          </td>
                        );
                      })}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
          <div className="analysis__list">
            {results.map((r, i) => (
              <article key={r.trial_name ?? i} className="analysis__item">
                <h4 className="mono">{r.trial_name ? <Link className="link" to={trialHref(job, r.trial_name)}>{r.trial_name}</Link> : `Result ${i + 1}`}</h4>
                {r.error ? <pre className="code code--wrap">{r.error}</pre> : r.summary ? <Markdown text={r.summary} /> : <span className="faint">No summary.</span>}
              </article>
            ))}
          </div>
        </>
      )}
    </div>
  );
}

export function AnalyzeDialog({ job, open, onClose, onDone }: { job: string; open: boolean; onClose: () => void; onDone: () => void }) {
  const opts = useApi<RunOptions>(open ? "/api/run/options" : null);
  const [agent, setAgent] = useState("codex");
  const [model, setModel] = useState("");
  const [env, setEnv] = useState("docker");
  const [n, setN] = useState(32);
  const [failed, setFailed] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Error>();
  const toast = useToast();

  const run = async () => {
    setBusy(true);
    setError(undefined);
    try {
      const r = await send<{ n_trials_analyzed: number }>("POST", p("jobs", job, "summarize"), { agent, model: model || undefined, environment: env, n_concurrent: n, only_failed: failed });
      toast({ text: `Analyzed ${r.n_trials_analyzed} attempt${r.n_trials_analyzed === 1 ? "" : "s"}` });
      onClose();
      onDone();
    } catch (e) {
      setError(e as Error);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={() => !busy && onClose()}
      title="Analyze attempts"
      footer={
        <>
          <button className="btn" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button className="btn btn--primary" onClick={run} disabled={busy || !opts.data} data-state={busy ? "loading" : undefined}>
            {busy && <span className="spinner" />} {busy ? "Analyzing…" : "Run analysis"}
          </button>
        </>
      }
    >
      <p className="muted" style={{ fontSize: "var(--text-sm)" }}>
        Runs <code>hbench analyze</code> on this run. The agent below reads each attempt. It can take several minutes.
      </p>
      {opts.error && <ErrorBox error={opts.error} title="Couldn't load agents and environments" onRetry={opts.reload} />}
      <div className="analyze-form">
        <div className="field">
          <label htmlFor="an-agent">Agent</label>
          <select id="an-agent" className="select" value={agent} onChange={(e) => setAgent(e.target.value)} disabled={busy}>
            {(opts.data?.agents ?? [agent]).map((a) => (
              <option key={a}>{a}</option>
            ))}
          </select>
        </div>
        <div className="field">
          <label htmlFor="an-env">Environment</label>
          <select id="an-env" className="select" value={env} onChange={(e) => setEnv(e.target.value)} disabled={busy}>
            {(opts.data?.environments ?? [env]).map((a) => (
              <option key={a}>{a}</option>
            ))}
          </select>
        </div>
        <div className="field analyze-form__wide">
          <label htmlFor="an-model">Model</label>
          <input id="an-model" className="input input--mono" value={model} onChange={(e) => setModel(e.target.value)} placeholder="openai/gpt-6-luna" disabled={busy} />
          <span className="field__help">A model name or an alias the agent knows. Leave it empty to use the one shown.</span>
        </div>
        <div className="field">
          <label htmlFor="an-n">Analyses at once</label>
          <input id="an-n" className="input num" type="number" min={1} value={n} onChange={(e) => setN(Math.max(1, Number(e.target.value) || 1))} disabled={busy} />
        </div>
        <label className="check" style={{ alignSelf: "center" }}>
          <input type="checkbox" checked={failed} onChange={(e) => setFailed(e.target.checked)} disabled={busy} /> Only failed attempts
        </label>
      </div>
      {error && <ErrorBox error={error} title="Analysis failed" />}
    </Dialog>
  );
}
