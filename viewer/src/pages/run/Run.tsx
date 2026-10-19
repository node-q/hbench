// /run — build a JobConfig, launch `hbench run`, follow the launch until the job page has data.
import { useCallback, useEffect, useMemo, useState } from "react";
import { Link, useNavigate } from "react-router";
import "./run.css";
import * as Icon from "../../ui/icons";
import { CodeView } from "../../ui/content";
import { Empty, ErrorBox, Loading, PageHead, Seg, Spinner } from "../../ui/kit";
import { useToast } from "../../ui/overlay";
import { ApiError, enc, send, useApi } from "../../lib/api";
import { ago } from "../../lib/fmt";
import { useDebounced, useStored } from "../../lib/state";
import type { KwargSpec, Obj, RunOptions, RunStatus } from "../../lib/types";
import { useCommands, useTitle, type Command } from "../../shell";
import { Area, Check, KvRows, KwargField, Num, Section, Select, ShowAllErrors, Text } from "./fields";
import { draftErrors, emptyDraft, fromConfig, jsonError, newAgent, numError, sourceError, sourceLabel, toConfig, type AgentDraft, type Draft, type Source } from "./form";

type History = { job_name: string; config: Obj }[];

export default function Run() {
  useTitle("New run");
  const options = useApi<RunOptions>("/api/run/options");
  const history = useApi<History>("/api/run/history?limit=20");
  const models = useApi<{ models: string[] }>("/api/run/models");
  const [draft, setDraft] = useStored<Draft>("run:draft", emptyDraft());
  const [launched, setLaunched] = useState<string | null>(null);
  const [submitError, setSubmitError] = useState<ApiError | Error | null>(null);
  const [submitted, setSubmitted] = useState(false);
  const [busy, setBusy] = useState(false);
  const toast = useToast();

  const specs = options.data?.agent_kwargs ?? {};
  const up = useCallback(
    (fn: (d: Draft) => void) => {
      const next = structuredClone(draft);
      fn(next);
      setDraft(next);
    },
    [draft, setDraft],
  );
  const load = useCallback(
    (cfg: Obj, name: string) => {
      setDraft(fromConfig(cfg, specs));
      setSubmitted(false);
      setSubmitError(null);
      toast({ text: `Loaded the settings of ${name}.` });
    },
    [setDraft, specs, toast],
  );

  const commands: Command[] = useMemo(() => {
    const last = history.data?.[0];
    return last ? [{ id: "run:last", label: `Load last run (${last.job_name})`, group: "This page", run: () => load(last.config, last.job_name) }] : [];
  }, [history.data, load]);
  useCommands(commands);

  const config = useMemo(() => toConfig(draft, specs), [draft, specs]);
  const errors = draftErrors(draft, specs);
  if (draft.env.type !== "docker") errors.push("hbench runs docker environments only.");

  const start = async () => {
    setSubmitted(true);
    setSubmitError(null);
    if (errors.length) return;
    setBusy(true);
    try {
      const r = await send<{ job_name: string }>("POST", "/api/run", config);
      setLaunched(r.job_name);
      history.reload();
    } catch (e) {
      setSubmitError(e as Error);
    } finally {
      setBusy(false);
    }
  };

  // Server errors that belong to one field are shown there too.
  const serverMsg = submitError?.message ?? "";
  const jobNameServerError = /already exists/.test(serverMsg) ? serverMsg : undefined;
  const sourceServerError = /dataset, a task, or a local path/.test(serverMsg) ? serverMsg : undefined;

  if (options.error) return <div className="page"><ErrorBox error={options.error} title="Couldn't load the run options" onRetry={options.reload} /></div>;
  if (!options.data) return <div className="page"><Loading label="Loading agents and environments…" /></div>;
  const opts = options.data;

  return (
    <div className="page">
      <PageHead
        crumbs={[{ to: "/", label: "Jobs" }]}
        title="New run"
        meta={
          <span>
            Writes to <span className="mono">{opts.jobs_dir}</span>
          </span>
        }
      />
      {launched && <LaunchPanel job={launched} onDismiss={() => setLaunched(null)} />}
      {submitError && (
        <div style={{ marginBottom: "var(--space-md)" }}>
          <ErrorBox error={submitError} title="The run didn't start" />
        </div>
      )}
      {submitted && errors.length > 0 && (
        <div className="error-box" role="alert" style={{ marginBottom: "var(--space-md)" }}>
          <strong>Fix {errors.length === 1 ? "this" : `these ${errors.length}`} before starting</strong>
          <ul className="run-errlist">
            {[...new Set(errors)].map((e) => (
              <li key={e}>{e}</li>
            ))}
          </ul>
        </div>
      )}
      <ShowAllErrors.Provider value={submitted}>
        <div className="run-grid">
          <form
            className="run-form"
            onSubmit={(e) => {
              e.preventDefault();
              start();
            }}
            noValidate
          >
            <SourceSection draft={draft} up={up} serverError={sourceServerError} />
            <section className="run-block" aria-labelledby="run-agents">
              <div className="run-block__head">
                <h2 id="run-agents">Agents</h2>
                <span className="muted run-block__note">Each agent runs every task.</span>
              </div>
              <datalist id="run-models">
                {(models.data?.models ?? []).map((m) => (
                  <option key={m} value={m} />
                ))}
              </datalist>
              {draft.agents.map((a, i) => (
                <AgentCard
                  key={i}
                  index={i}
                  agent={a}
                  agents={opts.agents}
                  specs={specs[a.name] ?? []}
                  removable={draft.agents.length > 1}
                  onChange={(next) => up((d) => void (d.agents[i] = next))}
                  onRemove={() => up((d) => void d.agents.splice(i, 1))}
                />
              ))}
              <div>
                <button type="button" className="btn btn--sm" onClick={() => up((d) => void d.agents.push(newAgent(d.agents[d.agents.length - 1]?.name ?? "oracle")))}>
                  <Icon.Plus size={12} /> Add agent
                </button>
              </div>
            </section>
            <EnvSection draft={draft} up={up} opts={opts} />
            <JobSection draft={draft} up={up} jobNameServerError={jobNameServerError} />
            <div className="run-submit">
              <button
                type="button"
                className="btn btn--ghost"
                onClick={() => {
                  setDraft(emptyDraft());
                  setSubmitted(false);
                  setSubmitError(null);
                }}
              >
                Reset form
              </button>
              <span className="toolbar__spacer" />
              {submitted && errors.length > 0 && <span className="run-submit__msg">{errors.length} to fix</span>}
              <button type="submit" className="btn btn--primary" disabled={busy} data-state={busy ? "loading" : undefined}>
                {busy ? <Spinner label="Starting" /> : <Icon.Play size={13} />}
                {busy ? "Starting…" : "Start run"}
              </button>
            </div>
          </form>
          <aside className="run-aside">
            <Preview config={config} />
            <HistoryList history={history.data} error={history.error} onLoad={load} />
          </aside>
        </div>
      </ShowAllErrors.Provider>
    </div>
  );
}

type Up = (fn: (d: Draft) => void) => void;

// ---------------------------------------------------------------- task source

function SourceSection({ draft, up, serverError }: { draft: Draft; up: Up; serverError?: string }) {
  const ds = draft.dataset;
  const [pickMsg, setPickMsg] = useState<string>();
  const [picking, setPicking] = useState(false);
  const browse = async () => {
    setPicking(true);
    setPickMsg(undefined);
    try {
      const r = await send<{ path: string | null }>("POST", "/api/run/pick-directory");
      if (r.path) up((d) => void (d.path = r.path!));
    } catch (e) {
      setPickMsg(`${(e as Error).message} Type the path instead.`);
    } finally {
      setPicking(false);
    }
  };
  const err = serverError ?? sourceError(draft);
  return (
    <section className="run-block" aria-labelledby="run-source">
      <div className="run-block__head">
        <h2 id="run-source">Tasks</h2>
        <Seg<Source>
          label="Task source"
          value={draft.source}
          onChange={(v) => up((d) => void (d.source = v))}
          options={[
            { id: "dataset", label: "Registry dataset" },
            { id: "path", label: "Local folder" },
            { id: "git", label: "Git task" },
          ]}
        />
      </div>
      {draft.source === "dataset" && (
        <>
          <div className="run-row run-row--name">
            <Text label="Dataset" required value={ds.name} onChange={(v) => up((d) => void (d.dataset.name = v))} placeholder="terminal-bench" mono error={err} help="Name in the registry." />
            <Text label="Version" value={ds.version} onChange={(v) => up((d) => void (d.dataset.version = v))} placeholder="2.0" mono help="Blank = latest." />
          </div>
          <div className="run-row">
            <Text label="Include tasks" value={ds.task_names} onChange={(v) => up((d) => void (d.dataset.task_names = v))} placeholder="*api*, test_*" mono help="Comma-separated names or globs." />
            <Text label="Exclude tasks" value={ds.exclude_task_names} onChange={(v) => up((d) => void (d.dataset.exclude_task_names = v))} placeholder="slow_*" mono help="Applied after include." />
            <Num label="Max tasks" value={ds.n_tasks} onChange={(v) => up((d) => void (d.dataset.n_tasks = v))} placeholder="all" error={numError(ds.n_tasks, { int: true, min: 1 })} help="Blank = every task." />
          </div>
          <Section title="Registry & download" summary={[ds.registry_url, ds.registry_path, ds.download_dir].filter(Boolean).join(" · ") || "node-q's registry (node-q/hbench-datasets)"}>
            <div className="run-row">
              <Text label="Registry URL" value={ds.registry_url} onChange={(v) => up((d) => void (d.dataset.registry_url = v))} placeholder="https://…/registry.json" mono help="Blank = node-q's registry (node-q/hbench-datasets)." />
              <Text label="Registry path" value={ds.registry_path} onChange={(v) => up((d) => void (d.dataset.registry_path = v))} placeholder="./registry.json" mono help="A local registry.json instead of a URL." />
            </div>
            <Text label="Download dir" value={ds.download_dir} onChange={(v) => up((d) => void (d.dataset.download_dir = v))} placeholder="~/.cache/…" mono help="Blank = the default cache." />
            <Check label="Re-download the dataset even if it's cached" checked={ds.overwrite} onChange={(v) => up((d) => void (d.dataset.overwrite = v))} />
          </Section>
        </>
      )}
      {draft.source === "path" && (
        <div className="run-row run-row--browse">
          <Text
            label="Folder"
            required
            value={draft.path}
            onChange={(v) => up((d) => void (d.path = v))}
            placeholder="/path/to/tasks or /path/to/one-task"
            mono
            error={err}
            help={pickMsg ?? "A single task folder (with task.toml) or a folder of tasks."}
          />
          <button type="button" className="btn run-browse" onClick={browse} disabled={picking} data-state={picking ? "loading" : undefined}>
            {picking ? <Spinner label="Waiting for the folder picker" /> : <Icon.Folder size={14} />} Browse…
          </button>
        </div>
      )}
      {draft.source === "git" && (
        <>
          <Text label="Repository URL" required value={draft.git.git_url} onChange={(v) => up((d) => void (d.git.git_url = v))} placeholder="https://github.com/org/tasks.git" mono error={err} />
          <div className="run-row">
            <Text label="Task path in the repo" required value={draft.git.path} onChange={(v) => up((d) => void (d.git.path = v))} placeholder="tasks/hello-world" mono />
            <Text label="Commit" value={draft.git.git_commit_id} onChange={(v) => up((d) => void (d.git.git_commit_id = v))} placeholder="a1b2c3d" mono help="Blank = default branch head." />
          </div>
        </>
      )}
    </section>
  );
}

// ---------------------------------------------------------------- agents

function AgentCard({ index, agent, agents, specs, removable, onChange, onRemove }: { index: number; agent: AgentDraft; agents: string[]; specs: KwargSpec[]; removable: boolean; onChange: (a: AgentDraft) => void; onRemove: () => void }) {
  const set = (patch: Partial<AgentDraft>) => onChange({ ...agent, ...patch });
  const nSet = Object.values(agent.kwargs).filter((v) => v.trim()).length;
  const advanced = [agent.n_concurrent && `${agent.n_concurrent} concurrent`, agent.custom.length && `${agent.custom.length} custom kwargs`, agent.env.length && `${agent.env.length} env vars`].filter(Boolean).join(" · ");
  return (
    <div className="run-agent">
      <div className="run-row run-row--agent">
        <Select label={`Agent ${index + 1}`} value={agent.name} onChange={(name) => onChange({ ...agent, name, kwargs: {} })} options={agents} />
        <Text label="Model" value={agent.model} onChange={(model) => set({ model })} list="run-models" placeholder="provider/model" mono autoComplete="off" help="Any LiteLLM name; suggestions as you type." />
        {removable && (
          <button type="button" className="btn btn--ghost btn--icon run-agent__remove" aria-label={`Remove agent ${index + 1}`} title="Remove agent" onClick={onRemove}>
            <Icon.Trash size={14} />
          </button>
        )}
      </div>
      <Section title={`${agent.name} options`} summary={specs.length ? (nSet ? `${nSet} set` : `${specs.length} available`) : "none declared"} open={specs.some((s) => s.required)}>
        {specs.length ? (
          <div className="run-kwargs">
            {specs.map((s) => (
              <KwargField key={s.key} spec={s} value={agent.kwargs[s.key] ?? ""} onChange={(v) => set({ kwargs: { ...agent.kwargs, [s.key]: v } })} />
            ))}
          </div>
        ) : (
          <p className="muted run-note">No configurable options are declared for this agent.</p>
        )}
      </Section>
      <Section title="Advanced agent options" summary={advanced || undefined}>
        <div className="run-row">
          <Num label="Max concurrent (this agent)" value={agent.n_concurrent} onChange={(v) => set({ n_concurrent: v })} placeholder="job limit" error={numError(agent.n_concurrent, { int: true, min: 1 })} />
          <Num label="Override timeout (s)" value={agent.override_timeout_sec} onChange={(v) => set({ override_timeout_sec: v })} placeholder="task value" error={numError(agent.override_timeout_sec, { min: 0 })} />
        </div>
        <div className="run-row">
          <Num label="Override setup timeout (s)" value={agent.override_setup_timeout_sec} onChange={(v) => set({ override_setup_timeout_sec: v })} placeholder="default" error={numError(agent.override_setup_timeout_sec, { min: 0 })} />
          <Num label="Max timeout (s)" value={agent.max_timeout_sec} onChange={(v) => set({ max_timeout_sec: v })} placeholder="none" error={numError(agent.max_timeout_sec, { min: 0 })} help="Caps the task's timeout." />
        </div>
        <Text label="Skills" value={agent.skills} onChange={(v) => set({ skills: v })} placeholder="./skills/foo, ./skills/bar" mono help="Comma-separated skill directories." />
        <Text label="Extra allowed hosts" value={agent.extra_allowed_hosts} onChange={(v) => set({ extra_allowed_hosts: v })} placeholder="api.example.com, *.pypi.org" mono />
        <KvRows label="Custom kwargs" rows={agent.custom} onChange={(custom) => set({ custom })} keyPlaceholder="key" valuePlaceholder='value or JSON, e.g. 3 or {"a":1}' />
        <KvRows label="Environment variables" rows={agent.env} onChange={(env) => set({ env })} valuePlaceholder="value or ${HOST_VAR}" help="Secrets are written to config.json as ${KEY} templates or redacted." />
      </Section>
    </div>
  );
}

// ---------------------------------------------------------------- environment

function EnvSection({ draft, up, opts }: { draft: Draft; up: Up; opts: RunOptions }) {
  const e = draft.env;
  const overrides = [e.override_cpus && `${e.override_cpus} CPU`, e.override_memory_mb && `${e.override_memory_mb} MB`, e.override_gpus && `${e.override_gpus} GPU`].filter(Boolean);
  const summary = [e.type, e.force_build && "rebuild", !e.delete && "keep env", ...overrides].filter(Boolean).join(" · ");
  const policies = opts.resource_modes.length ? opts.resource_modes : ["auto"];
  return (
    <Section title="Environment & resources" summary={summary}>
      <Select label="Environment" value={e.type} onChange={(v) => up((d) => void (d.env.type = v))} options={e.type === "docker" ? ["docker"] : ["docker", e.type]} help={e.type === "docker" ? "Sandbox each trial in a Docker container." : <span className="run-warn">hbench runs docker only; this type fails validation.</span>} />
      <div className="run-checks">
        <Check label="Force rebuild the image" checked={e.force_build} onChange={(v) => up((d) => void (d.env.force_build = v))} />
        <Check label="Delete the environment after each trial" checked={e.delete} onChange={(v) => up((d) => void (d.env.delete = v))} />
      </div>
      <div className="run-row run-row--4">
        <Num label="CPUs" value={e.override_cpus} onChange={(v) => up((d) => void (d.env.override_cpus = v))} placeholder="task" error={numError(e.override_cpus, { int: true, min: 1 })} />
        <Num label="Memory (MB)" value={e.override_memory_mb} onChange={(v) => up((d) => void (d.env.override_memory_mb = v))} placeholder="task" error={numError(e.override_memory_mb, { int: true, min: 1 })} />
        <Num label="Storage (MB)" value={e.override_storage_mb} onChange={(v) => up((d) => void (d.env.override_storage_mb = v))} placeholder="task" error={numError(e.override_storage_mb, { int: true, min: 1 })} />
        <Num label="GPUs" value={e.override_gpus} onChange={(v) => up((d) => void (d.env.override_gpus = v))} placeholder="task" error={numError(e.override_gpus, { int: true, min: 0 })} />
      </div>
      <div className="run-row">
        <Select label="CPU policy" value={e.cpu_policy} onChange={(v) => up((d) => void (d.env.cpu_policy = v))} options={policies} />
        <Select label="Memory policy" value={e.memory_policy} onChange={(v) => up((d) => void (d.env.memory_policy = v))} options={policies} />
      </div>
      <Text label="Extra allowed hosts" value={e.extra_allowed_hosts} onChange={(v) => up((d) => void (d.env.extra_allowed_hosts = v))} placeholder="registry.npmjs.org, *.github.com" mono help="Comma-separated; applies to every trial." />
      <Area label="Environment kwargs (JSON)" value={e.kwargs} onChange={(v) => up((d) => void (d.env.kwargs = v))} placeholder='{"keep_containers": true}' error={jsonError(e.kwargs, true)} />
      <KvRows label="Environment variables" rows={e.env} onChange={(env) => up((d) => void (d.env.env = env))} />
    </Section>
  );
}

// ---------------------------------------------------------------- job, retries, verifier

function JobSection({ draft, up, jobNameServerError }: { draft: Draft; up: Up; jobNameServerError?: string }) {
  const j = draft.job;
  const r = draft.retry;
  const v = draft.verifier;
  const jobSummary = [j.job_name || "auto name", `${j.n_attempts || 1}× attempts`, `${j.n_concurrent_trials || 4} concurrent`, j.debug && "debug", j.install_only && "install only"].filter(Boolean).join(" · ");
  return (
    <>
      <Section title="Job settings" summary={jobSummary} open={!!jobNameServerError}>
        <Text label="Job name" value={j.job_name} onChange={(x) => up((d) => void (d.job.job_name = x))} placeholder="auto (timestamp)" mono error={jobNameServerError} help="Folder name under the jobs dir." />
        <div className="run-row">
          <Num label="Attempts per trial" value={j.n_attempts} onChange={(x) => up((d) => void (d.job.n_attempts = x))} error={numError(j.n_attempts, { int: true, min: 1 })} />
          <Num label="Concurrent trials" value={j.n_concurrent_trials} onChange={(x) => up((d) => void (d.job.n_concurrent_trials = x))} error={numError(j.n_concurrent_trials, { int: true, min: 1 })} />
          <Num label="Timeout multiplier" value={j.timeout_multiplier} onChange={(x) => up((d) => void (d.job.timeout_multiplier = x))} step="any" error={numError(j.timeout_multiplier, { min: 0 })} help="Scales every timeout." />
        </div>
        <div className="run-row run-row--4">
          <Num label="Agent ×" value={j.agent_timeout_multiplier} onChange={(x) => up((d) => void (d.job.agent_timeout_multiplier = x))} placeholder="1" step="any" error={numError(j.agent_timeout_multiplier, { min: 0 })} />
          <Num label="Verifier ×" value={j.verifier_timeout_multiplier} onChange={(x) => up((d) => void (d.job.verifier_timeout_multiplier = x))} placeholder="1" step="any" error={numError(j.verifier_timeout_multiplier, { min: 0 })} />
          <Num label="Agent setup ×" value={j.agent_setup_timeout_multiplier} onChange={(x) => up((d) => void (d.job.agent_setup_timeout_multiplier = x))} placeholder="1" step="any" error={numError(j.agent_setup_timeout_multiplier, { min: 0 })} />
          <Num label="Env build ×" value={j.environment_build_timeout_multiplier} onChange={(x) => up((d) => void (d.job.environment_build_timeout_multiplier = x))} placeholder="1" step="any" error={numError(j.environment_build_timeout_multiplier, { min: 0 })} />
        </div>
        <div className="run-checks">
          <Check label="Debug logging" checked={j.debug} onChange={(x) => up((d) => void (d.job.debug = x))} />
          <Check label="Install only (skip the agent run and verifier)" checked={j.install_only} onChange={(x) => up((d) => void (d.job.install_only = x))} />
        </div>
        <Area label="Extra instructions" value={j.extra_instructions} onChange={(x) => up((d) => void (d.job.extra_instructions = x))} placeholder="One per line; appended to every task instruction." />
        <Area label="Extra instruction files" value={j.extra_instruction_paths} onChange={(x) => up((d) => void (d.job.extra_instruction_paths = x))} placeholder="./notes/style.md" rows={2} />
      </Section>
      <Section title="Retries" summary={Number(r.max_retries) > 0 ? `up to ${r.max_retries}` : "off"}>
        <div className="run-row">
          <Num label="Max retries" value={r.max_retries} onChange={(x) => up((d) => void (d.retry.max_retries = x))} error={numError(r.max_retries, { int: true, min: 0 })} help="Per trial, on errors not excluded below." />
          <Num label="Wait multiplier" value={r.wait_multiplier} onChange={(x) => up((d) => void (d.retry.wait_multiplier = x))} step="any" error={numError(r.wait_multiplier, { min: 0 })} />
        </div>
        <div className="run-row">
          <Num label="Min wait (s)" value={r.min_wait_sec} onChange={(x) => up((d) => void (d.retry.min_wait_sec = x))} step="any" error={numError(r.min_wait_sec, { min: 0 })} />
          <Num label="Max wait (s)" value={r.max_wait_sec} onChange={(x) => up((d) => void (d.retry.max_wait_sec = x))} step="any" error={numError(r.max_wait_sec, { min: 0 })} />
        </div>
        <Text label="Retry only these errors" value={r.include_exceptions} onChange={(x) => up((d) => void (d.retry.include_exceptions = x))} placeholder="EnvironmentStartTimeoutError" mono help="Comma-separated; blank = any error not excluded." />
        <Area label="Never retry these errors" value={r.exclude_exceptions} onChange={(x) => up((d) => void (d.retry.exclude_exceptions = x))} rows={3} />
      </Section>
      <Section title="Verifier" summary={v.disable ? "disabled" : [v.override_timeout_sec && `${v.override_timeout_sec}s`, "on"].filter(Boolean).join(" · ")}>
        <Check label="Disable verification (skip running tests)" checked={v.disable} onChange={(x) => up((d) => void (d.verifier.disable = x))} />
        <div className="run-row">
          <Num label="Override timeout (s)" value={v.override_timeout_sec} onChange={(x) => up((d) => void (d.verifier.override_timeout_sec = x))} placeholder="task value" error={numError(v.override_timeout_sec, { min: 0 })} />
          <Num label="Max timeout (s)" value={v.max_timeout_sec} onChange={(x) => up((d) => void (d.verifier.max_timeout_sec = x))} placeholder="none" error={numError(v.max_timeout_sec, { min: 0 })} />
        </div>
        <Area label="Verifier kwargs (JSON)" value={v.kwargs} onChange={(x) => up((d) => void (d.verifier.kwargs = x))} placeholder='{"key": "value"}' error={jsonError(v.kwargs, true)} />
        <KvRows label="Environment variables" rows={v.env} onChange={(env) => up((d) => void (d.verifier.env = env))} />
      </Section>
    </>
  );
}

// ---------------------------------------------------------------- preview, history, launch status

function Preview({ config }: { config: Obj }) {
  const body = useDebounced(config, 350);
  const [yaml, setYaml] = useState<string>();
  const [err, setErr] = useState<string>();
  useEffect(() => {
    let live = true;
    send<string>("POST", "/api/run/config.yaml", body)
      .then((t) => live && (setYaml(t), setErr(undefined)))
      .catch((e) => live && setErr(e.message));
    return () => {
      live = false;
    };
  }, [body]);
  const download = () => {
    if (!yaml) return;
    const a = document.createElement("a");
    a.href = URL.createObjectURL(new Blob([yaml], { type: "application/x-yaml" }));
    a.download = "config.yaml";
    a.click();
    URL.revokeObjectURL(a.href);
  };
  return (
    <div className="panel run-preview">
      <div className="panel__head">
        <span className="label">config.yaml</span>
        <button type="button" className="btn btn--sm" onClick={download} disabled={!yaml}>
          <Icon.Download size={13} /> Download
        </button>
      </div>
      <div className="panel__body run-preview__body">
        <p className="run-cli mono">
          <span className="faint">$</span> hbench run -c config.yaml
        </p>
        {err ? <p className="field__help is-error">{err}</p> : yaml ? <CodeView code={yaml} lang="yaml" lines={false} maxHeight="52vh" /> : <span className="skeleton" />}
      </div>
    </div>
  );
}

function HistoryList({ history, error, onLoad }: { history?: History; error?: Error; onLoad: (c: Obj, name: string) => void }) {
  return (
    <div className="panel">
      <div className="panel__head">
        <span className="label">Recent runs</span>
        {history?.[0] && (
          <button type="button" className="btn btn--sm" onClick={() => onLoad(history[0].config, history[0].job_name)}>
            Load last run
          </button>
        )}
      </div>
      {error ? (
        <div className="panel__body">
          <ErrorBox error={error} />
        </div>
      ) : !history ? (
        <div className="panel__body">
          <span className="skeleton" />
        </div>
      ) : history.length === 0 ? (
        <div className="panel__body">
          <Empty title="No runs yet">Started runs show up here so you can reuse their settings.</Empty>
        </div>
      ) : (
        <ul className="run-history">
          {history.map((h) => {
            const agents = (h.config.agents ?? []).map((a: Obj) => [a.name, a.model_name].filter(Boolean).join(" · ")).join(", ");
            return (
              <li key={h.job_name}>
                <div className="run-history__text">
                  <Link to={`/jobs/${enc(h.job_name)}`} className="link mono trunc">
                    {h.job_name}
                  </Link>
                  <span className="muted trunc">
                    {sourceLabel(h.config)} · {agents || "—"}
                  </span>
                </div>
                <button type="button" className="btn btn--sm" onClick={() => onLoad(h.config, h.job_name)} aria-label={`Load settings of ${h.job_name}`}>
                  Load
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

function LaunchPanel({ job, onDismiss }: { job: string; onDismiss: () => void }) {
  const navigate = useNavigate();
  const status = useApi<RunStatus>(`/api/run/${enc(job)}/status`, { poll: 1500 });
  const [stopping, setStopping] = useState(false);
  const [stopErr, setStopErr] = useState<string>();
  const [startedAt] = useState(() => new Date().toISOString());
  const s = status.data;
  useEffect(() => {
    if (s?.job_ready) navigate(`/jobs/${enc(job)}`);
  }, [s?.job_ready, job, navigate]);
  const failed = s && !s.running && s.returncode != null && s.returncode !== 0;
  const stop = async () => {
    setStopping(true);
    try {
      await send("DELETE", `/api/run/${enc(job)}`);
      status.reload();
    } catch (e) {
      setStopErr((e as Error).message);
    } finally {
      setStopping(false);
    }
  };
  return (
    <section className={`panel run-launch${failed ? " run-launch--bad" : ""}`} aria-live="polite">
      <div className="panel__head">
        <span className="run-launch__state">
          {failed ? <Icon.Alert size={14} /> : s?.running !== false ? <span className="dot dot--live" /> : <Icon.Check size={14} />}
          <strong className="ink">{failed ? `Run exited with code ${s!.returncode}` : s?.running === false ? "Run finished" : "Preparing run"}</strong>
          <span className="mono muted trunc">{job}</span>
          <span className="faint">started {ago(startedAt)}</span>
        </span>
        <span className="toolbar" style={{ margin: 0 }}>
          <Link to={`/jobs/${enc(job)}`} className="btn btn--sm">
            Open job <Icon.ArrowRight size={12} />
          </Link>
          {s?.running && (
            <button type="button" className="btn btn--sm btn--danger" onClick={stop} disabled={stopping}>
              <Icon.Stop size={12} /> {stopping ? "Stopping…" : "Stop"}
            </button>
          )}
          {!s?.running && (
            <button type="button" className="btn btn--ghost btn--icon btn--sm" aria-label="Dismiss" onClick={onDismiss}>
              <Icon.Close size={13} />
            </button>
          )}
        </span>
      </div>
      <div className="panel__body">
        {stopErr && <p className="field__help is-error">{stopErr}</p>}
        {status.error && !s ? (
          <ErrorBox error={status.error} />
        ) : (
          <>
            {!failed && <p className="muted run-note">The job page opens as soon as the run writes its first results.</p>}
            {s?.log_tail ? <CodeView code={s.log_tail} lang="bash" lines={false} wrap maxHeight="18rem" /> : <p className="faint mono run-note">waiting for output…</p>}
          </>
        )}
      </div>
    </section>
  );
}
