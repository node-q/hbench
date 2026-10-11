import { useCallback, useEffect, useMemo, useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import "./trial.css";
import { useApi, p } from "../../lib/api";
import { useKey, useUrlState } from "../../lib/state";
import * as fmt from "../../lib/fmt";
import type { FileInfo, TrialNavigation, TrialResult } from "../../lib/types";
import { useCommands, useTitle, type Command } from "../../shell";
import { ErrorBox, PageHead, SkeletonRows, Status, Tabs, type TabDef } from "../../ui/kit";
import * as Icon from "../../ui/icons";
import { Summary } from "./Summary";
import { TrajectoryTab } from "./TrajectoryTab";
import { FilesTab } from "./FilesTab";
import { AgentLogsTab, AnalysisTab, ConfigTab, InteractionTab, RecordingTab, TextFileTab, VerifierTab } from "./tabs";
import { api, type TrialRef } from "./util";

export default function Trial() {
  const { jobName = "", trialName = "" } = useParams();
  const navigate = useNavigate();
  const url = useUrlState();
  const stepParam = url.get("step");

  const navR = useApi<TrialNavigation>(api({ job: jobName, trial: trialName }, "navigation"));
  const pending = navR.data ? !navR.data.finished : true;
  const [polling, setPolling] = useState(false);
  const res = useApi<TrialResult>(api({ job: jobName, trial: trialName }), { poll: polling ? 4000 : undefined });
  const r = res.data;
  const running = !!r && !r.finished_at && pending;
  useEffect(() => setPolling(running), [running]);

  const steps = r?.step_results ?? [];
  const step = steps.length ? (steps.some((s) => s.step_name === stepParam) ? stepParam : undefined) : undefined;
  const ref: TrialRef = useMemo(() => ({ job: jobName, trial: trialName, step }), [jobName, trialName, step]);

  // Root file listing (no step) drives which tabs have data.
  const rootFiles = useApi<FileInfo[]>(api({ job: jobName, trial: trialName }, "files"), { poll: running ? 8000 : undefined });
  const has = useMemo(() => {
    const set = new Set((rootFiles.data ?? []).map((f) => f.path));
    const pre = step ? `steps/${step}/` : "";
    const any = (...paths: string[]) => paths.some((x) => set.has(pre + x));
    const under = (dir: string) => [...set].some((x) => x.startsWith(pre + dir + "/"));
    return {
      known: !!rootFiles.data,
      interaction: any("user-agent/trajectory.json", "agent/bridge-trajectory.json"),
      verifier: under("verifier"),
      logs: any("agent/oracle.txt", "agent/setup/stdout.txt") || [...set].some((x) => x.startsWith(pre + "agent/command-")),
      trialLog: set.has("trial.log"),
      exception: set.has("exception.txt") || !!r?.exception_info,
      artifacts: under("artifacts"),
      recording: set.has("agent/recording.mp4"),
    };
  }, [rootFiles.data, step, r?.exception_info]);

  const tabs: TabDef[] = [
    { id: "trajectory", label: "Trajectory" },
    { id: "interaction", label: "Interaction", disabled: has.known && !has.interaction },
    { id: "verifier", label: "Verifier", disabled: has.known && !has.verifier },
    { id: "logs", label: "Agent logs", disabled: has.known && !has.logs },
    { id: "trial-log", label: "Trial log", disabled: has.known && !has.trialLog },
    { id: "exception", label: "Exception", disabled: has.known && !has.exception },
    { id: "files", label: "Files" },
    { id: "artifacts", label: "Artifacts", disabled: has.known && !has.artifacts },
    { id: "recording", label: "Recording", disabled: has.known && !has.recording },
    { id: "analysis", label: "Analysis" },
    { id: "config", label: "Config" },
  ];
  const tabParam = url.get("tab");
  const tab = tabs.find((t) => t.id === tabParam && !t.disabled)?.id ?? "trajectory";
  const setTab = useCallback((id: string) => url.set({ tab: id === "trajectory" ? null : id, path: null }), [url]);

  const jobUrl = `/jobs/${encodeURIComponent(jobName)}`;
  const trialUrl = (name: string) => `${jobUrl}/trials/${encodeURIComponent(name)}`;
  const nav = navR.data;
  const go = useCallback((name: string | null | undefined) => name && navigate(trialUrl(name) + (tab !== "trajectory" ? `?tab=${tab}` : "")), [navigate, tab, jobName]);
  useKey("[", useCallback(() => go(nav?.previous), [go, nav]));
  useKey("]", useCallback(() => go(nav?.next), [go, nav]));

  const cmds = useMemo<Command[]>(
    () => [
      ...tabs.filter((t) => !t.disabled).map((t) => ({ id: `trial:tab:${t.id}`, label: `Show ${String(t.label).toLowerCase()}`, group: "This trial", run: () => setTab(t.id) })),
      ...(nav?.previous ? [{ id: "trial:prev", label: "Previous trial", hint: nav.previous, group: "This trial", run: () => go(nav.previous) }] : []),
      ...(nav?.next ? [{ id: "trial:next", label: "Next trial", hint: nav.next, group: "This trial", run: () => go(nav.next) }] : []),
      { id: "trial:job", label: "Back to job", hint: jobName, group: "This trial", run: () => navigate(jobUrl) },
    ],
    [tabs.map((t) => `${t.id}${t.disabled}`).join(), nav, jobName],
  );
  useCommands(cmds);
  useTitle(trialName);

  if (res.error && !r)
    return (
      <div className="page">
        <ErrorBox error={res.error} title={`Couldn't load trial ${trialName}`} onRetry={res.reload} />
      </div>
    );
  if (!r)
    return (
      <div className="page">
        <SkeletonRows rows={8} />
      </div>
    );

  const exc = r.exception_info;
  const reward = r.verifier_result?.rewards?.reward;
  const outcome = running ? (
    <Status kind="accent">
      <span className="dot dot--live" /> running
    </Status>
  ) : exc ? (
    <Status kind="bad">{exc.exception_type}</Status>
  ) : typeof reward === "number" ? (
    <Status kind={reward >= 1 ? "good" : reward > 0 ? "warn" : "bad"}>reward {fmt.score(reward)}</Status>
  ) : (
    <Status kind="plain">no reward</Status>
  );
  const mi = r.agent_info?.model_info;
  const effort = r.config?.agent?.kwargs?.reasoning_effort;

  return (
    <div className="page trial">
      <PageHead
        crumbs={[
          { to: "/", label: "Jobs" },
          { to: jobUrl, label: jobName },
        ]}
        title={
          <span className="trial__title">
            <span className="trunc">{trialName}</span> {outcome}
          </span>
        }
        meta={
          <>
            <span>
              task <span className="ink">{r.task_name}</span>
            </span>
            <span>
              agent <span className="ink">{r.agent_info?.name}</span>
              {r.agent_info?.version && <span className="faint mono"> {r.agent_info.version}</span>}
            </span>
            <span>
              model <span className="ink">{fmt.model(mi?.provider, mi?.name)}</span>
            </span>
            {effort && (
              <span>
                effort <span className="ink">{String(effort)}</span>
              </span>
            )}
            {r.source && (
              <span>
                source <span className="ink">{r.source}</span>
              </span>
            )}
            <span title={fmt.dateTime(r.started_at)}>started {fmt.date(r.started_at)}</span>
            <span className="num">{fmt.duration(fmt.ms(r.started_at, r.finished_at ?? (running ? new Date().toISOString() : null)))}</span>
          </>
        }
        actions={
          nav && (
            <div className="trial__nav">
              <button className="btn btn--sm btn--icon" disabled={!nav.previous} onClick={() => go(nav.previous)} title="Previous trial ( [ )" aria-label="Previous trial">
                <Icon.ArrowLeft size={13} />
              </button>
              <span className="num muted">
                {nav.index + 1} / {nav.total}
              </span>
              <button className="btn btn--sm btn--icon" disabled={!nav.next} onClick={() => go(nav.next)} title="Next trial ( ] )" aria-label="Next trial">
                <Icon.ArrowRight size={13} />
              </button>
            </div>
          )
        }
      />

      {steps.length > 0 && (
        <div className="trial__steps" role="group" aria-label="Step">
          <span className="label">Step</span>
          <button className={`btn btn--sm${!step ? " is-on" : ""}`} onClick={() => url.set({ step: null, path: null })}>
            Whole trial
          </button>
          {steps.map((s, i) => {
            const rw = s.verifier_result?.rewards?.reward;
            return (
              <button key={s.step_name} className={`btn btn--sm${step === s.step_name ? " is-on" : ""}`} onClick={() => url.set({ step: s.step_name, path: null })} title={s.exception_info?.exception_type ?? undefined}>
                <span className="faint num">{i + 1}</span> {s.step_name}
                {s.exception_info ? <span className="dot" style={{ color: "var(--color-bad)" }} /> : typeof rw === "number" ? <span className="num muted">{fmt.score(rw, 2)}</span> : null}
              </button>
            );
          })}
        </div>
      )}

      <Summary r={r} step={steps.find((s) => s.step_name === step)} running={running} />

      <div className="section">
        <Tabs tabs={tabs} value={tab} onChange={setTab} label="Trial views" />
        <div role="tabpanel">
          {tab === "trajectory" && <TrajectoryTab t={ref} />}
          {tab === "interaction" && <InteractionTab t={ref} />}
          {tab === "verifier" && <VerifierTab t={ref} />}
          {tab === "logs" && <AgentLogsTab t={ref} />}
          {tab === "trial-log" && <TextFileTab t={{ job: jobName, trial: trialName }} path="trial.log" empty="No trial.log in this trial." />}
          {tab === "exception" && <TextFileTab t={{ job: jobName, trial: trialName }} path="exception.txt" empty="No exception.txt in this trial." fallback={exc?.exception_traceback || exc?.exception_message} />}
          {tab === "files" && <FilesTab t={ref} source="files" />}
          {tab === "artifacts" && <FilesTab t={ref} source="artifacts" />}
          {tab === "recording" && <RecordingTab t={{ job: jobName, trial: trialName }} />}
          {tab === "analysis" && <AnalysisTab t={ref} />}
          {tab === "config" && <ConfigTab config={r.config} />}
        </div>
      </div>
      <p className="trial__hint label">
        <kbd>[</kbd> <kbd>]</kbd> previous / next trial · <Link to={jobUrl} className="link">all trials in {jobName}</Link> · <a className="link" href={p("jobs", jobName, "trials", trialName)} target="_blank" rel="noreferrer">raw result.json</a>
      </p>
    </div>
  );
}
