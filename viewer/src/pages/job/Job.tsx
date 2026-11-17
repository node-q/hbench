import { useEffect, useMemo, useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { ApiError, p, send, useApi } from "../../lib/api";
import * as fmt from "../../lib/fmt";
import { useUrlState } from "../../lib/state";
import type { JobResult, TrialSummary } from "../../lib/types";
import { NotFound, useCommands, useTitle, type Command } from "../../shell";
import * as Icon from "../../ui/icons";
import { CopyButton, ErrorBox, PageHead, SkeletonRows, Tabs } from "../../ui/kit";
import { Dialog, useToast } from "../../ui/overlay";
import { AnalysisTab, AnalyzeDialog } from "./Analysis";
import { Charts } from "./Charts";
import { ConfigTab } from "./ConfigTab";
import { StatsStrip } from "./Stats";
import { TasksTab, TrialsTab } from "./Lists";
import { trialHref } from "./util";
import "./job.css";

const TABS = [
  { id: "tasks", label: "Tasks" },
  { id: "trials", label: "Trials" },
  { id: "charts", label: "Charts" },
  { id: "analysis", label: "Analysis" },
  { id: "config", label: "Config" },
];

export default function Job() {
  const job = useParams().jobName!;
  const navigate = useNavigate();
  const url = useUrlState();
  const tab = url.get("tab") || "tasks";
  const [poll, setPoll] = useState(true);
  const res = useApi<JobResult>(p("jobs", job), { poll: poll ? 4000 : undefined });
  const live = !!res.data && !res.data.finished_at;
  useEffect(() => setPoll(!res.data || live), [res.data, live]);
  const listPoll = live ? 4000 : undefined;
  const chart = useApi<TrialSummary[]>(p("jobs", job, "chart-trials"), { poll: listPoll });
  const [analyze, setAnalyze] = useState(false);
  const [del, setDel] = useState(false);
  const [stopAsk, setStopAsk] = useState(false);
  // When this page asked the run to stop; a run that is still going a while later may be force-stopped.
  const [stoppedAt, setStoppedAt] = useState<number>();
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!stoppedAt || !live) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [stoppedAt, live]);
  const stopping = live && stoppedAt != null;
  const canForce = stopping && now - stoppedAt! > FORCE_AFTER_MS;
  const [analysisTick, setAnalysisTick] = useState(0);
  useTitle(job);

  const setTab = (id: string) => url.set({ tab: id === "tasks" ? null : id, sort: null, order: null });
  const commands = useMemo<Command[]>(
    () => [
      ...TABS.map((t) => ({ id: `job-tab:${t.id}`, label: `Show ${t.label.toLowerCase()}`, group: "This job", run: () => setTab(t.id) })),
      { id: "job:analyze", label: "Analyze trials…", group: "This job", run: () => setAnalyze(true) },
      ...(live ? [{ id: "job:stop", label: "Stop this run…", group: "This job", run: () => setStopAsk(true) }] : []),
      ...(chart.data ?? []).map((t) => ({ id: `job-trial:${t.name}`, label: t.name, group: "Trials in this job", hint: fmt.score(t.reward), run: () => navigate(trialHref(job, t.name)) })),
    ],
    [chart.data, job, live],
  );
  useCommands(commands);

  if (res.error instanceof ApiError && res.error.status === 404) return <NotFound what="Job" />;
  const r = res.data;
  const path = r ? decodeURIComponent(r.job_uri.replace(/^file:\/\//, "")) : "";
  const elapsed = r ? fmt.ms(r.started_at, r.finished_at ?? (live ? new Date().toISOString() : null)) : null;

  return (
    <div className="page">
      <PageHead
        crumbs={[{ to: "/", label: "Jobs" }]}
        title={
          <span className="job-title">
            <span className="trunc">{job}</span>
            {path && <CopyButton text={path} label="Copy job path" />}
          </span>
        }
        meta={
          r && (
            <>
              {live ? (
                <span className="chip chip--accent">
                  <span className="dot dot--live" /> {stopping ? "stopping" : "running"}
                </span>
              ) : (
                <span className="chip chip--good">finished</span>
              )}
              <span title={fmt.dateTime(r.started_at)}>started {fmt.date(r.started_at)}</span>
              <span className="num">{fmt.duration(elapsed)}</span>
              <span className="num">{r.n_total_trials} trials</span>
            </>
          )
        }
        actions={
          <>
            {live &&
              (stopping ? (
                <button className="btn btn--danger" disabled={!canForce} onClick={() => setStopAsk(true)} title={canForce ? "Remove the run's containers now" : "Trials in progress are being cancelled"}>
                  {canForce ? <Icon.Stop size={14} /> : <span className="spinner" />} {canForce ? "Force stop" : "Stopping…"}
                </button>
              ) : (
                <button className="btn btn--danger" onClick={() => setStopAsk(true)}>
                  <Icon.Stop size={14} /> Stop
                </button>
              ))}
            <button className="btn" onClick={() => setAnalyze(true)}>
              <Icon.Sparkle size={14} /> Analyze
            </button>
            <Link className="btn" to={`/compare?job=${encodeURIComponent(job)}`}>
              <Icon.Columns size={14} /> Compare with…
            </Link>
            <button className="btn btn--danger btn--icon" onClick={() => setDel(true)} aria-label="Delete job" title="Delete job">
              <Icon.Trash size={14} />
            </button>
          </>
        }
      />
      {res.error && !r && <ErrorBox error={res.error} onRetry={res.reload} />}
      {!r && !res.error && <SkeletonRows rows={3} />}
      {r && <StatsStrip job={job} result={r} />}

      <div className="section">
        <Tabs label="Job views" tabs={TABS.map((t) => ({ ...t, count: t.id === "trials" ? (chart.data?.length ?? null) : undefined }))} value={tab} onChange={setTab} />
        {tab === "tasks" && <TasksTab job={job} poll={listPoll} />}
        {tab === "trials" && <TrialsTab job={job} poll={listPoll} />}
        {tab === "charts" && <Charts job={job} trials={chart.data} error={chart.error} />}
        {tab === "analysis" && <AnalysisTab job={job} tick={analysisTick} onAnalyze={() => setAnalyze(true)} />}
        {tab === "config" && <ConfigTab job={job} />}
      </div>

      <AnalyzeDialog
        job={job}
        open={analyze}
        onClose={() => setAnalyze(false)}
        onDone={() => {
          setAnalysisTick((t) => t + 1);
          setTab("analysis");
        }}
      />
      <DeleteDialog job={job} open={del} onClose={() => setDel(false)} />
      <StopDialog
        job={job}
        open={stopAsk && live}
        force={stopping}
        onClose={() => setStopAsk(false)}
        onStopped={() => {
          setStopAsk(false);
          setStoppedAt(Date.now());
          setNow(Date.now());
          res.reload();
        }}
      />
    </div>
  );
}

function DeleteDialog({ job, open, onClose }: { job: string; open: boolean; onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Error>();
  const navigate = useNavigate();
  const toast = useToast();
  const close = () => {
    setError(undefined);
    onClose();
  };
  const run = async () => {
    setBusy(true);
    try {
      await send("DELETE", p("jobs", job));
      toast({ text: `Deleted ${job}` });
      navigate("/");
    } catch (e) {
      setError(e as Error);
      setBusy(false);
    }
  };
  return (
    <Dialog
      open={open}
      onClose={close}
      title="Delete this job?"
      footer={
        <>
          <button className="btn" onClick={close} autoFocus>
            Keep job
          </button>
          <button className="btn btn--danger" disabled={busy} data-state={busy ? "loading" : undefined} onClick={run}>
            {busy && <span className="spinner" />} Delete job
          </button>
        </>
      }
    >
      <p>
        Delete <code className="ink">{job}</code> and every trial in it from disk? This can't be undone.
      </p>
      {error && <ErrorBox error={error} title="Delete failed" />}
    </Dialog>
  );
}

/** How long a stop may take (trials tear down their containers) before the page offers to force it. */
const FORCE_AFTER_MS = 20_000;

function StopDialog({ job, open, force, onClose, onStopped }: { job: string; open: boolean; force: boolean; onClose: () => void; onStopped: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Error>();
  const close = () => {
    setError(undefined);
    onClose();
  };
  const run = async () => {
    setBusy(true);
    try {
      await send("POST", p("jobs", job, "stop"));
      setError(undefined);
      onStopped();
    } catch (e) {
      setError(e as Error);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open={open}
      onClose={close}
      title={force ? "Force stop this run?" : "Stop this run?"}
      footer={
        <>
          <button className="btn" onClick={close} autoFocus>
            Keep running
          </button>
          <button className="btn btn--danger" disabled={busy} data-state={busy ? "loading" : undefined} onClick={run}>
            {busy && <span className="spinner" />} {force ? "Force stop" : "Stop run"}
          </button>
        </>
      }
    >
      <p>
        {force
          ? "The run is still tearing down. Force stop removes its containers now; trials in progress end without results."
          : "Trials in progress are cancelled and their containers removed. Finished trials keep their results."}
      </p>
      {error && <ErrorBox error={error} title="Couldn't stop the run" />}
    </Dialog>
  );
}
