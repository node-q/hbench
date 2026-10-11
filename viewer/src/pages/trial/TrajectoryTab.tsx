// ATIF trajectory: filterable, searchable step list. Step bodies render only when expanded (500+ step runs stay fast).
import { memo, useCallback, useMemo, useState, type ReactNode } from "react";
import { useApi } from "../../lib/api";
import * as fmt from "../../lib/fmt";
import { useKey, useUrlState } from "../../lib/state";
import type { Trajectory, TrajectoryStep } from "../../lib/types";
import { CodeView, Markdown } from "../../ui/content";
import { Empty, ErrorBox, SearchField, Seg, SkeletonRows } from "../../ui/kit";
import * as Icon from "../../ui/icons";
import { api, fileUrl, textOf, type TrialRef } from "./util";

type SourceFilter = "all" | "agent" | "user" | "system";
const SHELL_TOOLS = /^(bash|shell|sh|exec|exec_command|run|run_command|run_shell_command|execute_command|terminal|local_shell|command)$/i;

export function TrajectoryTab({ t }: { t: TrialRef }) {
  const res = useApi<Trajectory | null>(api(t, "trajectory"));
  const url = useUrlState();
  const sub = Number(url.get("sub") || 0);
  if (res.error) return <ErrorBox error={res.error} title="Couldn't load the trajectory" onRetry={res.reload} />;
  if (res.data === undefined) return <SkeletonRows rows={10} />;
  if (!res.data?.steps?.length) return <Empty title="No trajectory">This agent didn't write agent/trajectory.json for this trial.</Empty>;
  const subs: Trajectory[] = res.data.subagent_trajectories ?? [];
  const traj = sub > 0 && subs[sub - 1] ? subs[sub - 1] : res.data;
  return (
    <>
      {subs.length > 0 && (
        <div className="toolbar">
          <span className="label">Trajectory</span>
          <select className="select" value={sub} onChange={(e) => url.set({ sub: e.target.value === "0" ? null : e.target.value })} aria-label="Trajectory">
            <option value={0}>Main · {res.data.steps.length} steps</option>
            {subs.map((s, i) => (
              <option key={i} value={i + 1}>
                Subagent {s.agent?.name ?? i + 1}
                {s.session_id ? ` · ${s.session_id.slice(0, 12)}` : ""} · {s.steps.length} steps
              </option>
            ))}
          </select>
        </div>
      )}
      <Steps key={sub} traj={traj} t={t} />
    </>
  );
}

function haystack(s: TrajectoryStep): string {
  return [textOf(s.message), s.reasoning_content ?? "", ...(s.tool_calls ?? []).map((c) => `${c.function_name} ${typeof c.arguments === "string" ? c.arguments : JSON.stringify(c.arguments)}`), ...(s.observation?.results ?? []).map((o) => textOf(o.content))]
    .join("\n")
    .toLowerCase();
}

function Steps({ traj, t }: { traj: Trajectory; t: TrialRef }) {
  const steps = traj.steps;
  const [q, setQ] = useState("");
  const [source, setSource] = useState<SourceFilter>("all");
  const [open, setOpen] = useState<Set<number>>(() => new Set(steps.length <= 40 ? steps.map((_, i) => i) : []));
  const hay = useMemo(() => steps.map(haystack), [steps]);
  const counts = useMemo(() => {
    const c: Record<string, number> = { all: steps.length };
    for (const s of steps) c[s.source] = (c[s.source] ?? 0) + 1;
    return c;
  }, [steps]);
  const shown = useMemo(() => {
    const n = q.trim().toLowerCase();
    return steps.map((_, i) => i).filter((i) => (source === "all" || steps[i].source === source) && (!n || hay[i].includes(n)));
  }, [steps, hay, q, source]);

  const toggle = useCallback((i: number) => setOpen((o) => {
    const n = new Set(o);
    if (n.has(i)) n.delete(i);
    else n.add(i);
    return n;
  }), []);

  // n / p: jump to the next / previous agent step below / above the viewport top.
  const jump = useCallback(
    (dir: 1 | -1) => () => {
      const els = [...document.querySelectorAll<HTMLElement>(".tstep[data-source='agent']")];
      const y = (el: HTMLElement) => el.getBoundingClientRect().top - 70;
      const target = dir === 1 ? els.find((el) => y(el) > 4) : [...els].reverse().find((el) => y(el) < -4);
      target?.scrollIntoView({ block: "start", behavior: "smooth" });
    },
    [],
  );
  useKey("n", jump(1));
  useKey("p", jump(-1));

  const fm = traj.final_metrics;
  const t0 = steps[0]?.timestamp ? Date.parse(steps[0].timestamp) : NaN;

  return (
    <div className="traj">
      {fm && (
        <div className="traj__final num">
          <span className="label">Final</span>
          <span>{fmt.int(fm.total_steps ?? steps.length)} steps</span>
          <span>{fmt.compact(fm.total_prompt_tokens)} prompt</span>
          <span>{fmt.compact(fm.total_cached_tokens)} cached</span>
          <span>{fmt.compact(fm.total_completion_tokens)} completion</span>
          <span>{fmt.usd(fm.total_cost_usd)}</span>
          {traj.agent?.model_name && <span className="muted">{traj.agent.model_name}</span>}
        </div>
      )}
      <div className="toolbar traj__bar">
        <SearchField value={q} onChange={setQ} placeholder="Search steps, commands, output" width={300} />
        <Seg<SourceFilter>
          label="Source"
          value={source}
          onChange={setSource}
          options={(["all", "agent", "user", "system"] as const).filter((s) => s === "all" || counts[s]).map((s) => ({ id: s, label: `${s} ${counts[s] ?? 0}` }))}
        />
        <span className="toolbar__spacer" />
        <span className="num muted" style={{ fontSize: "var(--text-xs)" }}>
          {shown.length} of {steps.length}
        </span>
        <button className="btn btn--sm" onClick={() => setOpen(new Set(shown))}>
          Expand all
        </button>
        <button className="btn btn--sm" onClick={() => setOpen(new Set())}>
          Collapse all
        </button>
        <span className="label traj__keys" title="Jump between agent steps">
          <kbd>n</kbd> <kbd>p</kbd> agent steps
        </span>
      </div>
      {traj.notes && <p className="muted traj__notes">{traj.notes}</p>}
      {shown.length === 0 ? (
        <Empty title="No steps match">Clear the search or the source filter.</Empty>
      ) : (
        <ol className="traj__list">
          {shown.map((i) => (
            <Step key={i} i={i} s={steps[i]} prev={steps[i - 1]} t0={t0} open={open.has(i)} onToggle={toggle} t={t} q={q} />
          ))}
        </ol>
      )}
    </div>
  );
}

const Step = memo(function Step({ i, s, prev, t0, open, onToggle, t, q }: { i: number; s: TrajectoryStep; prev?: TrajectoryStep; t0: number; open: boolean; onToggle: (i: number) => void; t: TrialRef; q: string }) {
  const ts = s.timestamp ? Date.parse(s.timestamp) : NaN;
  const delta = prev?.timestamp && Number.isFinite(ts) ? ts - Date.parse(prev.timestamp) : null;
  const m = s.metrics;
  const preview = textOf(s.message).trim().split("\n")[0] || (s.reasoning_content ?? "").split("\n")[0];
  const calls = s.tool_calls ?? [];
  return (
    <li className={`tstep${open ? " is-open" : ""}`} data-source={s.source}>
      <button className="tstep__head" onClick={() => onToggle(i)} aria-expanded={open}>
        <span className="tstep__chev">
          <Icon.Chevron size={11} />
        </span>
        <span className="num faint tstep__id">#{s.step_id}</span>
        <span className={`tstep__src tstep__src--${s.source}`}>{s.source}</span>
        <span className="tstep__preview trunc">
          {calls.length > 0 && (
            <span className="tstep__calls">
              {calls.map((c, k) => (
                <code key={k}>{c.function_name}</code>
              ))}
            </span>
          )}
          <span className="muted">{preview}</span>
        </span>
        <span className="tstep__meta num">
          {m?.prompt_tokens != null && <span title="Prompt / completion tokens">{fmt.compact(m.prompt_tokens)}→{fmt.compact(m.completion_tokens)}</span>}
          {m?.cost_usd != null && <span>{fmt.usd(m.cost_usd)}</span>}
          {delta != null && <span title={s.timestamp}>+{fmt.duration(delta)}</span>}
          {delta == null && Number.isFinite(ts) && Number.isFinite(t0) && <span title={s.timestamp}>{fmt.duration(ts - t0)}</span>}
        </span>
      </button>
      {open && <StepBody s={s} t={t} q={q} />}
    </li>
  );
});

function StepBody({ s, t, q }: { s: TrajectoryStep; t: TrialRef; q: string }) {
  const results = s.observation?.results ?? [];
  const byCall = new Map(results.filter((r) => r.source_call_id).map((r) => [r.source_call_id!, r]));
  const loose = results.filter((r) => !r.source_call_id || !(s.tool_calls ?? []).some((c) => c.tool_call_id === r.source_call_id));
  const m = s.metrics;
  return (
    <div className="tstep__body">
      {s.reasoning_content && (
        <details className="tstep__reason" open={!!q && s.reasoning_content.toLowerCase().includes(q.toLowerCase())}>
          <summary className="label">Reasoning</summary>
          <p className="tstep__reason-text">{s.reasoning_content}</p>
        </details>
      )}
      <Message v={s.message} source={s.source} t={t} />
      {(s.tool_calls ?? []).map((c) => (
        <div key={c.tool_call_id} className="tcall">
          <div className="tcall__head">
            <Icon.Terminal size={13} />
            <code className="ink">{c.function_name}</code>
            <span className="faint mono tcall__id">{c.tool_call_id}</span>
          </div>
          <Args fn={c.function_name} args={c.arguments} />
          {byCall.get(c.tool_call_id) && <Observation r={byCall.get(c.tool_call_id)!} t={t} />}
        </div>
      ))}
      {loose.map((r, k) => (
        <Observation key={k} r={r} t={t} />
      ))}
      {m && Object.keys(m).length > 0 && (
        <div className="tstep__metrics num faint">
          {m.prompt_tokens != null && <span>prompt {fmt.int(m.prompt_tokens)}</span>}
          {m.cached_tokens != null && <span>cached {fmt.int(m.cached_tokens)}</span>}
          {m.completion_tokens != null && <span>completion {fmt.int(m.completion_tokens)}</span>}
          {m.cost_usd != null && <span>{fmt.usd(m.cost_usd)}</span>}
          {s.model_name && <span>{s.model_name}</span>}
          {s.timestamp && <span>{fmt.dateTime(s.timestamp)}</span>}
        </div>
      )}
    </div>
  );
}

function Message({ v, source, t }: { v: TrajectoryStep["message"]; source: string; t: TrialRef }) {
  if (v == null || v === "") return null;
  const parts = typeof v === "string" ? [{ type: "text", text: v }] : v;
  return (
    <div className="tmsg">
      {parts.map((part: any, k: number) =>
        part?.type === "image" ? (
          <ImagePart key={k} src={part.source} t={t} />
        ) : part?.type === "text" || typeof part?.text === "string" ? (
          source === "system" ? (
            <Long key={k} text={part.text} />
          ) : (
            <MdOrLong key={k} text={part.text} />
          )
        ) : (
          <CodeView key={k} code={JSON.stringify(part, null, 2)} lang="json" lines={false} />
        ),
      )}
    </div>
  );
}

/** Markdown for normal-size text; very long text falls back to a truncated pre block. */
function MdOrLong({ text }: { text: string }) {
  return text.length > 12_000 ? <Long text={text} /> : <Markdown text={text} />;
}

function ImagePart({ src, t }: { src: any; t: TrialRef }) {
  const url = src?.data ? `data:${src.media_type ?? "image/png"};base64,${src.data}` : src?.path ? (/^(https?:|data:)/.test(src.path) ? src.path : fileUrl(t, src.path.startsWith("agent/") ? src.path : `agent/${src.path}`)) : src?.url;
  if (!url) return <span className="muted">Image unavailable</span>;
  return (
    <a href={url} target="_blank" rel="noreferrer" className="timg">
      <img src={url} alt={src?.path ?? "Image from the trajectory"} loading="lazy" />
    </a>
  );
}

function Args({ fn, args }: { fn: string; args: unknown }) {
  if (typeof args === "string") {
    try {
      args = JSON.parse(args);
    } catch {
      return <CodeView code={args as string} lang={SHELL_TOOLS.test(fn) ? "bash" : undefined} lines={false} wrap />;
    }
  }
  const o = (args ?? {}) as Record<string, unknown>;
  const cmdKey = ["command", "cmd", "script"].find((k) => typeof o[k] === "string");
  if (cmdKey && (SHELL_TOOLS.test(fn) || Object.keys(o).length <= 3)) {
    const rest = Object.entries(o).filter(([k]) => k !== cmdKey);
    return (
      <>
        <CodeView code={String(o[cmdKey])} lang="bash" lines={false} wrap />
        {rest.length > 0 && (
          <dl className="kv tcall__rest">
            {rest.map(([k, v]) => (
              <div key={k} style={{ display: "contents" }}>
                <dt className="mono">{k}</dt>
                <dd className="mono">{typeof v === "string" ? v : JSON.stringify(v)}</dd>
              </div>
            ))}
          </dl>
        )}
      </>
    );
  }
  return <CodeView code={JSON.stringify(args, null, 2)} lang="json" lines={false} wrap />;
}

function Observation({ r, t }: { r: NonNullable<TrajectoryStep["observation"]>["results"][number]; t: TrialRef }) {
  const parts = Array.isArray(r.content) ? r.content : null;
  return (
    <div className="tobs">
      <span className="label">Output</span>
      {parts ? parts.map((p: any, k: number) => (p?.type === "image" ? <ImagePart key={k} src={p.source} t={t} /> : <Long key={k} text={textOf([p]) || JSON.stringify(p)} />)) : <Long text={textOf(r.content)} />}
      {r.subagent_trajectory_ref && r.subagent_trajectory_ref.length > 0 && (
        <div className="tobs__refs">
          <span className="label">Subagent</span>
          {r.subagent_trajectory_ref.map((ref, k) => (
            <code key={k} className="chip">
              {String(ref.session_id ?? ref.trajectory_path ?? JSON.stringify(ref))}
            </code>
          ))}
        </div>
      )}
    </div>
  );
}

const LONG_LINES = 30;

/** Preformatted output, truncated to 30 lines with "show all". */
export function Long({ text, lines = LONG_LINES }: { text: string; lines?: number }): ReactNode {
  const [all, setAll] = useState(false);
  if (!text) return <span className="faint mono" style={{ fontSize: "var(--text-xs)" }}>(empty)</span>;
  const split = text.split("\n");
  const cut = !all && split.length > lines;
  return (
    <div className="long">
      <CodeView code={cut ? split.slice(0, lines).join("\n") : text} lines={false} wrap />
      {split.length > lines && (
        <button className="btn btn--sm btn--ghost long__more" onClick={() => setAll(!all)}>
          {all ? "Show less" : `Show all ${fmt.int(split.length)} lines`}
        </button>
      )}
    </div>
  );
}
