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
  const [analysisTick, setAnalysisTick] = useState(0);
  useTitle(job);

  const setTab = (id: string) => url.set({ tab: id === "tasks" ? null : id, sort: null, order: null });
  const commands = useMemo<Command[]>(
    () => [
      ...TABS.map((t) => ({ id: `job-tab:${t.id}`, label: `Show ${t.label.toLowerCase()}`, group: "This job", run: () => setTab(t.id) })),
      { id: "job:analyze", label: "Analyze trials…", group: "This job", run: () => setAnalyze(true) },
      ...(chart.data ?? []).map((t) => ({ id: `job-trial:${t.name}`, label: t.name, group: "Trials in this job", hint: fmt.score(t.reward), run: () => navigate(trialHref(job, t.name)) })),
    ],
    [chart.data, job],
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
                  <span className="dot dot--live" /> running
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
    </div>
  );
}

function DeleteDialog({ job, open, onClose }: { job: string; open: boolean; onClose: () => void }) {
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Error>();
  const navigate = useNavigate();
  const toast = useToast();
  const close = () => {
    setTyped("");
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
          <button className="btn" onClick={close}>
            Keep job
          </button>
          <button className="btn btn--primary" disabled={typed !== job || busy} data-state={busy ? "loading" : undefined} onClick={run}>
            {busy && <span className="spinner" />} Delete job
          </button>
        </>
      }
    >
      <p>
        This removes the job folder and every trial in it from disk. It can't be undone.
      </p>
      <div className="field">
        <label htmlFor="del-confirm">
          Type <code className="ink">{job}</code> to confirm
        </label>
        <input id="del-confirm" className="input input--mono" value={typed} onChange={(e) => setTyped(e.target.value)} autoComplete="off" onKeyDown={(e) => e.key === "Enter" && typed === job && run()} />
      </div>
      {error && <ErrorBox error={error} title="Delete failed" />}
    </Dialog>
  );
}
