// The smaller trial tabs: interaction, verifier, agent logs, text files, recording, analysis, config.
import { useMemo, useState } from "react";
import { ApiError, send, useApi } from "../../lib/api";
import * as fmt from "../../lib/fmt";
import { useStored } from "../../lib/state";
import type { AgentLogs, Interaction, Obj, Recording, RunOptions, VerifierOutput } from "../../lib/types";
import { CodeView, JsonTree, Markdown } from "../../ui/content";
import { Empty, ErrorBox, Reward, SkeletonRows, Status } from "../../ui/kit";
import { useToast } from "../../ui/overlay";
import * as Icon from "../../ui/icons";
import { Long } from "./TrajectoryTab";
import { api, fileUrl, textOf, type TrialRef } from "./util";

function Block({ title, children, aside }: { title: string; children: React.ReactNode; aside?: React.ReactNode }) {
  return (
    <section className="tblock">
      <div className="tblock__head">
        <h3>{title}</h3>
        {aside}
      </div>
      {children}
    </section>
  );
}

// ---------------------------------------------------------------- interaction

export function InteractionTab({ t }: { t: TrialRef }) {
  const res = useApi<Interaction>(api(t, "interaction"));
  if (res.error) return <ErrorBox error={res.error} onRetry={res.reload} />;
  if (!res.data) return <SkeletonRows />;
  const d = res.data;
  if (!d.available) return <Empty title="No simulated-user interaction">This trial ran without a user agent.</Empty>;
  const userSteps = d.user_trajectory?.steps ?? [];
  const errors = [...d.user_parse_errors.map((e) => ({ ...e, log: "user runtime" })), ...d.target_parse_errors.map((e) => ({ ...e, log: "target runtime" }))];
  return (
    <div className="interaction">
      <Block title="Simulated user" aside={<span className="num muted">{userSteps.length} steps · {d.user_events.length} events</span>}>
        {userSteps.length ? (
          <ol className="ilist">
            {userSteps.map((s) => (
              <li key={s.step_id} className="ilist__item">
                <span className={`tstep__src tstep__src--${s.source}`}>{s.source}</span>
                <Long text={textOf(s.message) || (s.tool_calls ?? []).map((c) => `${c.function_name}(${JSON.stringify(c.arguments)})`).join("\n")} lines={12} />
              </li>
            ))}
          </ol>
        ) : (
          <EventList events={d.user_events} />
        )}
      </Block>
      <Block title="Target agent" aside={<span className="num muted">{d.target_events.length} events</span>}>
        {d.target_events.length ? <EventList events={d.target_events} /> : d.bridge_trajectory ? <JsonTree value={d.bridge_trajectory} open={1} /> : <span className="muted">No target content</span>}
      </Block>
      {errors.length > 0 && (
        <Block title="Unparsed log lines">
          <ul className="ilist">
            {errors.map((e, k) => (
              <li key={k} className="ilist__item">
                <span className="chip chip--bad">
                  {e.log} L{e.line_number}
                </span>
                <span className="mono" style={{ fontSize: "var(--text-xs)" }}>
                  {e.error}
                </span>
                <code className="faint trunc">{e.raw}</code>
              </li>
            ))}
          </ul>
        </Block>
      )}
    </div>
  );
}

function EventList({ events }: { events: Obj[] }) {
  if (!events.length) return <span className="muted">No events</span>;
  return (
    <ol className="ilist">
      {events.map((e, k) => {
        const kind = String(e.type ?? e.role ?? e.event ?? e.message?.role ?? "event");
        const text = textOf(e.message?.content ?? e.content ?? e.text) || JSON.stringify(e, null, 2);
        return (
          <li key={k} className="ilist__item">
            <span className="chip">{kind}</span>
            <Long text={text} lines={10} />
          </li>
        );
      })}
    </ol>
  );
}

// ---------------------------------------------------------------- verifier

interface CtrfTest {
  name: string;
  status: string;
  duration?: number;
  message?: string;
  trace?: string;
  suite?: string;
}

export function VerifierTab({ t }: { t: TrialRef }) {
  const res = useApi<VerifierOutput>(api(t, "verifier-output"));
  const ctrf = useMemo(() => {
    try {
      const j = res.data?.ctrf ? JSON.parse(res.data.ctrf) : null;
      return j?.results ? { summary: j.results.summary as Obj | undefined, tests: (j.results.tests ?? []) as CtrfTest[] } : null;
    } catch {
      return null;
    }
  }, [res.data?.ctrf]);
  const [failOnly, setFailOnly] = useState(false);
  if (res.error) return <ErrorBox error={res.error} onRetry={res.reload} />;
  if (!res.data) return <SkeletonRows />;
  const d = res.data;
  if (!d.stdout && !d.stderr && !d.ctrf && !d.reward && !d.reward_details) return <Empty title="No verifier output">The verifier didn't run or wrote nothing.</Empty>;
  const tests = ctrf?.tests.filter((x) => !failOnly || x.status !== "passed") ?? [];
  const passed = ctrf?.tests.filter((x) => x.status === "passed").length ?? 0;
  return (
    <div className="verifier">
      {d.reward && (
        <Block title="reward.json">
          <dl className="kv">
            {Object.entries(d.reward).map(([k, v]) => (
              <div key={k} style={{ display: "contents" }}>
                <dt className="mono">{k}</dt>
                <dd>{typeof v === "number" ? <Reward value={v} /> : <span className="mono">{JSON.stringify(v)}</span>}</dd>
              </div>
            ))}
          </dl>
        </Block>
      )}
      {d.reward_details && (
        <Block title="Reward details">
          <JsonTree value={d.reward_details} open={2} />
        </Block>
      )}
      {ctrf && (
        <Block
          title="Tests"
          aside={
            <span className="toolbar" style={{ margin: 0 }}>
              <span className="num muted">
                {passed} / {ctrf.tests.length} passed
              </span>
              <label className="check">
                <input type="checkbox" checked={failOnly} onChange={(e) => setFailOnly(e.target.checked)} /> Failures only
              </label>
            </span>
          }
        >
          {tests.length ? (
            <div className="table-wrap">
              <table className="table" aria-label="Test results">
                <thead>
                  <tr>
                    <th>Status</th>
                    <th>Test</th>
                    <th data-align="right">Duration</th>
                  </tr>
                </thead>
                <tbody>
                  {tests.map((x, k) => (
                    <TestRow key={k} x={x} />
                  ))}
                </tbody>
              </table>
            </div>
          ) : (
            <span className="muted">No failing tests.</span>
          )}
        </Block>
      )}
      {d.stdout != null && (
        <Block title="test-stdout.txt">
          <CodeView code={d.stdout} wrap maxHeight="60vh" />
        </Block>
      )}
      {d.stderr && (
        <Block title="test-stderr.txt">
          <CodeView code={d.stderr} wrap maxHeight="40vh" />
        </Block>
      )}
    </div>
  );
}

function TestRow({ x }: { x: CtrfTest }) {
  const [open, setOpen] = useState(false);
  const detail = [x.message, x.trace].filter(Boolean).join("\n\n");
  const kind = x.status === "passed" ? "good" : x.status === "failed" ? "bad" : "warn";
  return (
    <>
      <tr onClick={() => detail && setOpen(!open)} style={detail ? { cursor: "pointer" } : undefined}>
        <td>
          <Status kind={kind}>{x.status}</Status>
        </td>
        <td className="mono" style={{ whiteSpace: "normal", fontSize: "var(--text-xs)" }}>
          {x.suite ? <span className="faint">{x.suite} › </span> : null}
          {x.name}
          {detail && <span className="faint"> {open ? "▾" : "▸"}</span>}
        </td>
        <td data-align="right" className="num">
          {x.duration != null ? fmt.duration(x.duration) : "—"}
        </td>
      </tr>
      {open && (
        <tr>
          <td colSpan={3} style={{ whiteSpace: "normal", maxWidth: "none", padding: "var(--space-xs)" }}>
            <CodeView code={detail} wrap lines={false} />
          </td>
        </tr>
      )}
    </>
  );
}

// ---------------------------------------------------------------- agent logs

export function AgentLogsTab({ t }: { t: TrialRef }) {
  const res = useApi<AgentLogs>(api(t, "agent-logs"));
  if (res.error) return <ErrorBox error={res.error} onRetry={res.reload} />;
  if (!res.data) return <SkeletonRows />;
  const d = res.data;
  if (!d.setup && !d.oracle && !d.commands.length) return <Empty title="No agent logs">No setup output, command output or oracle log in this trial.</Empty>;
  return (
    <div>
      {d.setup != null && (
        <Block title="Setup · agent/setup/stdout.txt">
          <CodeView code={d.setup} wrap maxHeight="50vh" path="stdout.txt" />
        </Block>
      )}
      {d.commands.map((c) => (
        <Block key={c.index} title={`Command ${c.index} · agent/command-${c.index}/stdout.txt`}>
          <CodeView code={c.content ?? ""} wrap maxHeight="50vh" />
        </Block>
      ))}
      {d.oracle != null && (
        <Block title="Oracle · agent/oracle.txt">
          <CodeView code={d.oracle} wrap maxHeight="50vh" />
        </Block>
      )}
    </div>
  );
}

// ---------------------------------------------------------------- trial.log / exception.txt

export function TextFileTab({ t, path, empty, fallback }: { t: TrialRef; path: string; empty: string; fallback?: string | null }) {
  const src = fileUrl(t, path);
  const res = useApi<string>(src, { text: true });
  if (res.error) {
    if (fallback) return <CodeView code={fallback} wrap maxHeight="72vh" />;
    if (res.error instanceof ApiError && res.error.status === 404) return <Empty title={empty} />;
    return <ErrorBox error={res.error} onRetry={res.reload} />;
  }
  if (res.data == null) return <SkeletonRows rows={10} />;
  return (
    <CodeView
      code={res.data}
      wrap
      maxHeight="72vh"
      actions={
        <a className="btn btn--sm btn--ghost" href={src} target="_blank" rel="noreferrer" title="Open raw">
          <Icon.External size={13} />
        </a>
      }
    />
  );
}

// ---------------------------------------------------------------- recording

export function RecordingTab({ t }: { t: TrialRef }) {
  const res = useApi<Recording>(api(t, "recording"));
  if (res.error) return <ErrorBox error={res.error} onRetry={res.reload} />;
  if (!res.data) return <SkeletonRows />;
  if (!res.data.available) return <Empty title="No recording">agent/recording.mp4 doesn't exist for this trial.</Empty>;
  const src = api({ job: t.job, trial: t.trial }, "recording/file");
  return (
    <figure className="recording">
      <video controls preload="metadata" src={src} />
      <figcaption className="muted num">
        {res.data.file_path} · {fmt.bytes(res.data.size)} ·{" "}
        <a className="link" href={src} download="recording.mp4">
          download
        </a>
      </figcaption>
    </figure>
  );
}

// ---------------------------------------------------------------- analysis

export function AnalysisTab({ t }: { t: TrialRef }) {
  const res = useApi<AgentLogs>(api(t, "agent-logs"));
  const [summary, setSummary] = useState<string | null>(null);
  const [formOpen, setFormOpen] = useState(false);
  if (res.error) return <ErrorBox error={res.error} onRetry={res.reload} />;
  if (!res.data) return <SkeletonRows />;
  const text = summary ?? res.data.summary;
  const a = res.data.analysis;
  return (
    <div className="analysis">
      <div className="toolbar">
        <span className="toolbar__spacer" />
        <button className="btn btn--sm" onClick={() => setFormOpen(!formOpen)} aria-expanded={formOpen}>
          <Icon.Sparkle size={13} /> {text || a ? "Analyze again" : "Analyze this trial"}
        </button>
      </div>
      {formOpen && (
        <AnalyzeForm
          t={t}
          onDone={(s) => {
            setSummary(s);
            setFormOpen(false);
          }}
        />
      )}
      {text ? (
        <Block title="Summary">
          <Markdown text={text} />
        </Block>
      ) : !a && !formOpen ? (
        <Empty title="No analysis">Run an agent over this trial's logs to explain what happened.</Empty>
      ) : null}
      {a && (
        <Block title="analysis.json">
          {typeof a.summary === "string" && !text && <Markdown text={a.summary} />}
          <JsonTree value={a} open={2} />
        </Block>
      )}
    </div>
  );
}

function AnalyzeForm({ t, onDone }: { t: TrialRef; onDone: (summary: string) => void }) {
  const opts = useApi<RunOptions>("/api/run/options");
  const models = useApi<{ models: string[] }>("/api/run/models");
  const [agent, setAgent] = useStored("analyze:agent", "codex");
  const [model, setModel] = useStored("analyze:model", "");
  const [env, setEnv] = useStored("analyze:env", "docker");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string>();
  const toast = useToast();
  const run = async () => {
    setBusy(true);
    setErr(undefined);
    try {
      const r = await send<{ summary: string }>("POST", api({ job: t.job, trial: t.trial }, "summarize"), { agent, model: model || undefined, environment: env });
      onDone(r.summary ?? "");
      toast({ text: "Analysis finished" });
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  if (opts.error) return <ErrorBox error={opts.error} title="Couldn't load run options" onRetry={opts.reload} />;
  return (
    <form
      className="panel tanalyze-form"
      onSubmit={(e) => {
        e.preventDefault();
        run();
      }}
    >
      <div className="field">
        <label htmlFor="an-agent">Agent</label>
        <select id="an-agent" className="select" value={agent} onChange={(e) => setAgent(e.target.value)} disabled={busy}>
          {(opts.data?.agents ?? [agent]).map((a) => (
            <option key={a}>{a}</option>
          ))}
        </select>
      </div>
      <div className="field">
        <label htmlFor="an-model">Model</label>
        <input id="an-model" className="input input--mono" list="an-models" value={model} onChange={(e) => setModel(e.target.value)} placeholder="openai/gpt-6-luna" disabled={busy} />
        <datalist id="an-models">
          {models.data?.models.map((m) => (
            <option key={m} value={m} />
          ))}
        </datalist>
      </div>
      <div className="field">
        <label htmlFor="an-env">Environment</label>
        <select id="an-env" className="select" value={env} onChange={(e) => setEnv(e.target.value)} disabled={busy}>
          {(opts.data?.environments ?? [env]).map((x) => (
            <option key={x}>{x}</option>
          ))}
        </select>
      </div>
      <div className="tanalyze-form__go">
        <button className="btn btn--primary" type="submit" disabled={busy} data-state={busy ? "loading" : undefined}>
          {busy ? (
            <>
              <span className="spinner" /> Analyzing…
            </>
          ) : (
            "Run analysis"
          )}
        </button>
      </div>
      {err && (
        <p className="field__help is-error tanalyze-form__err" role="alert">
          {err}
        </p>
      )}
    </form>
  );
}

// ---------------------------------------------------------------- config

export function ConfigTab({ config }: { config: Obj }) {
  return (
    <div className="panel">
      <div className="panel__body">
        <JsonTree value={config} open={2} />
      </div>
    </div>
  );
}
