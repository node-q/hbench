//! Job = attempts x tasks x agents: concurrency, per-agent pools, retries, resume, stats, locks.

use crate::config::{JobConfig, TaskRef, TrialConfig};
use crate::fmt::title_case;
use crate::metrics;
use crate::registry;
use crate::task::Task;
use crate::trial::{Trial, TrialResult, write_atomic};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{Semaphore, watch};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AgentDatasetStats {
    pub n_trials: usize,
    pub n_errors: usize,
    pub metrics: Vec<metrics::MetricOut>,
    pub pass_at_k: BTreeMap<usize, f64>,
    /// reward key -> reward value -> trial names
    pub reward_stats: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    pub exception_stats: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct JobStats {
    pub n_completed_trials: usize,
    pub n_errored_trials: usize,
    pub n_running_trials: usize,
    pub n_pending_trials: usize,
    pub n_cancelled_trials: usize,
    pub n_retries: usize,
    pub evals: BTreeMap<String, AgentDatasetStats>,
    pub n_input_tokens: Option<i64>,
    pub n_cache_tokens: Option<i64>,
    pub n_output_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub id: uuid::Uuid,
    pub started_at: DateTime<Utc>,
    pub updated_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub n_total_trials: usize,
    pub stats: JobStats,
}

pub fn evals_key(r: &TrialResult) -> String {
    let dataset = r.source.as_deref().unwrap_or("adhoc");
    match &r.agent_info.model_info {
        Some(m) => format!("{}__{}__{}", r.agent_info.name, m.name, dataset),
        None => format!("{}__{}", r.agent_info.name, dataset),
    }
}

/// A reward value as a JSON object key: integers plain, floats with a decimal point (`1.0`, `0.5`).
fn reward_key(n: &serde_json::Number) -> String {
    match n.as_i64() {
        Some(i) => i.to_string(),
        None => format!("{:?}", n.as_f64().unwrap_or(f64::NAN)),
    }
}

pub fn compute_stats(
    results: &[TrialResult],
    n_total: usize,
    n_running: usize,
    n_retries: usize,
    metric_cfgs: &[crate::config::MetricConfig],
) -> JobStats {
    let mut s = JobStats { n_retries, n_running_trials: n_running, ..Default::default() };
    let mut groups: BTreeMap<String, Vec<&TrialResult>> = BTreeMap::new();
    for r in results {
        s.n_completed_trials += 1;
        let key = evals_key(r);
        groups.entry(key.clone()).or_default().push(r);
        let e = s.evals.entry(key).or_default();
        if let Some(rw) = r.reward() {
            e.n_trials += 1;
            for (k, v) in rw {
                e.reward_stats.entry(k.clone()).or_default().entry(reward_key(v)).or_default().push(r.trial_name.clone());
            }
        }
        if let Some(ex) = &r.exception_info {
            e.exception_stats.entry(ex.exception_type.clone()).or_default().push(r.trial_name.clone());
            e.n_errors += 1;
            s.n_errored_trials += 1;
            if ex.exception_type == crate::env::ErrorKind::Cancelled.as_str() {
                s.n_cancelled_trials += 1;
            }
        }
        let (i, c, o, cost) = r.usage_totals();
        let add = |acc: &mut Option<i64>, v: Option<i64>| {
            if let Some(v) = v {
                *acc = Some(acc.unwrap_or(0) + v)
            }
        };
        add(&mut s.n_input_tokens, i);
        add(&mut s.n_cache_tokens, c);
        add(&mut s.n_output_tokens, o);
        if let Some(v) = cost {
            s.cost_usd = Some(s.cost_usd.unwrap_or(0.0) + v);
        }
    }
    let default_mean = [crate::config::MetricConfig::default()];
    let cfgs: &[crate::config::MetricConfig] = if metric_cfgs.is_empty() { &default_mean } else { metric_cfgs };
    for (key, rs) in groups {
        let rewards: Vec<Option<&crate::trial::Rewards>> = rs.iter().map(|r| r.reward()).collect();
        let e = s.evals.get_mut(&key).unwrap();
        for m in cfgs {
            match metrics::compute(m, &rewards) {
                Ok(v) => e.metrics.push(v),
                Err(err) => eprintln!("metric {} failed: {err:#}", m.kind.as_str()),
            }
        }
        e.pass_at_k = metrics::pass_at_k(&rs);
    }
    s.n_pending_trials = n_total.saturating_sub(s.n_completed_trials + n_running);
    s
}

pub fn trial_name(t: &TaskRef) -> String {
    let base = t.display_name();
    let short: String = base.chars().take(32).collect();
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{}__{}", short.trim_end_matches(['_', '-']), &id[..7])
}

/// Resolve tasks + datasets into task refs; download git tasks.
pub fn resolve_tasks(cfg: &JobConfig) -> Result<Vec<TaskRef>> {
    let mut v: Vec<TaskRef> = cfg.tasks.clone();
    for d in &cfg.datasets {
        v.extend(registry::dataset_tasks(d, cfg.verifier.disable)?);
    }
    for t in v.iter_mut() {
        if t.git_url.is_none()
            && let Some(p) = &t.path
        {
            t.path = Some(p.canonicalize().with_context(|| format!("task path {}", p.display()))?);
        }
    }
    registry::download(&v)?;
    if v.is_empty() {
        bail!("no tasks selected");
    }
    Ok(v)
}

/// Key of a trial config for resume matching (excludes name/job id/job dir). Local tasks match by
/// dir name, so a job (and its tasks) can be moved and still resume.
fn resume_key(c: &TrialConfig) -> Value {
    let mut c = c.clone();
    if c.task.git_url.is_none() {
        c.task.path = c.task.path.as_deref().and_then(Path::file_name).map(PathBuf::from);
    }
    let mut v = serde_json::to_value(&c).unwrap();
    if let Some(o) = v.as_object_mut() {
        o.remove("trial_name");
        o.remove("job_id");
        o.remove("trials_dir");
    }
    v
}

/// The explicit trial plan of a job not built from its config (regrade, --diff), kept for resume.
const PLAN_FILE: &str = "trials.json";

fn stored_plan(cfg: &JobConfig) -> Result<Option<Vec<TrialConfig>>> {
    let job_dir = cfg.job_dir();
    let Ok(s) = std::fs::read_to_string(job_dir.join(PLAN_FILE)) else {
        let regrade = std::fs::read_dir(&job_dir)?.flatten().any(|e| {
            let config = std::fs::read_to_string(e.path().join("config.json")).ok();
            config.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()).is_some_and(|c| !c["source_trial"].is_null())
        });
        if regrade || !cfg.source_jobs.is_empty() {
            bail!(
                "{} is a regrade/--diff job without a stored trial plan ({PLAN_FILE}); it cannot be resumed, start it again instead",
                job_dir.display()
            );
        }
        return Ok(None);
    };
    let mut plan: Vec<TrialConfig> = serde_json::from_str(&s).with_context(|| format!("parsing {PLAN_FILE}"))?;
    for c in &mut plan {
        c.trials_dir = job_dir.clone();
    }
    Ok(Some(plan))
}

/// A panicking writer must not take every later lock down with it.
fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn build_trial_configs(cfg: &JobConfig, tasks: &[TaskRef], job_id: uuid::Uuid) -> Vec<TrialConfig> {
    let mut out = vec![];
    for _ in 0..cfg.n_attempts {
        for t in tasks {
            for a in &cfg.agents {
                out.push(TrialConfig {
                    task: t.clone(),
                    trial_name: trial_name(t),
                    trials_dir: cfg.job_dir(),
                    install_only: cfg.install_only,
                    timeout_multiplier: cfg.timeout_multiplier,
                    agent_timeout_multiplier: cfg.agent_timeout_multiplier,
                    verifier_timeout_multiplier: cfg.verifier_timeout_multiplier,
                    agent_setup_timeout_multiplier: cfg.agent_setup_timeout_multiplier,
                    environment_build_timeout_multiplier: cfg.environment_build_timeout_multiplier,
                    agent: a.clone(),
                    user_agent: cfg.user_agent.clone(),
                    environment: cfg.environment.clone(),
                    verifier: {
                        let mut v = cfg.verifier.clone();
                        v.disable |= cfg.install_only;
                        v
                    },
                    artifacts: cfg.artifacts.clone(),
                    extra_instruction_paths: cfg.extra_instruction_paths.clone(),
                    extra_instructions: cfg.extra_instructions.clone(),
                    job_id: Some(job_id),
                    source_trial: None,
                });
            }
        }
    }
    out
}

/// The finished trials and counters of a running job, shared by its trial tasks.
struct JobProgress {
    results: Vec<TrialResult>,
    running: usize,
    retries: usize,
}

/// What a job carries over from an earlier run's result.json: its id, and the retries that run
/// counted, so `n_retries` covers every attempt across `jobs resume`. A new job gets a fresh id.
fn carried_over(job_dir: &Path) -> (uuid::Uuid, usize) {
    match std::fs::read_to_string(job_dir.join("result.json")).ok().and_then(|s| serde_json::from_str::<JobResult>(&s).ok()) {
        Some(prev) => (prev.id, prev.stats.n_retries),
        None => (uuid::Uuid::new_v4(), 0),
    }
}

/// Run (or resume) a job. `filter_error_types`: on resume, rerun trials that failed with these.
pub async fn run_job(
    cfg: JobConfig,
    trial_configs: Option<Vec<TrialConfig>>,
    filter_error_types: &[String],
    cancel: watch::Receiver<bool>,
) -> Result<JobResult> {
    let job_dir = cfg.job_dir();
    std::fs::create_dir_all(&job_dir)?;
    let (job_id, prior_retries) = carried_over(&job_dir);
    std::fs::write(job_dir.join("config.json"), serde_json::to_string_pretty(&cfg.config_json())?)?;
    let log = crate::env::TrialLog::create(&job_dir.join("job.log"))?;
    log.line(&format!("job {} started ({} agent config(s))", cfg.job_name, cfg.agents.len()));
    for key in cfg.unread_settings() {
        let why = if key == "environment.override_storage_mb" { "hbench does not limit a trial's disk" } else { "hbench ignores it" };
        let warning = format!("warning: {key} is set, but {why}");
        eprintln!("{warning}");
        log.line(&warning);
    }

    let mut pending = match trial_configs {
        Some(t) => {
            write_atomic(&job_dir.join(PLAN_FILE), serde_json::to_string_pretty(&t)?)?;
            t
        }
        None => match stored_plan(&cfg)? {
            Some(t) => t,
            None => build_trial_configs(&cfg, &resolve_tasks(&cfg)?, job_id),
        },
    };
    // Load each distinct task once (validates before any container starts).
    let mut tasks: HashMap<PathBuf, Arc<Task>> = HashMap::new();
    for c in &pending {
        let p = registry::local_path(&c.task);
        if !tasks.contains_key(&p) {
            tasks.insert(p.clone(), Arc::new(Task::load(&p)?));
        }
    }

    // Resume: keep finished trials whose config is still planned; drop partial/filtered ones.
    let mut previous = vec![];
    for e in std::fs::read_dir(&job_dir)? {
        let p = e?.path();
        if !p.is_dir() || !p.join("config.json").exists() {
            continue;
        }
        let r: Option<TrialResult> = std::fs::read_to_string(p.join("result.json")).ok().and_then(|s| serde_json::from_str(&s).ok());
        let keep = r.as_ref().is_some_and(|r| r.exception_info.as_ref().is_none_or(|e| !filter_error_types.contains(&e.exception_type)));
        match r {
            Some(r) if keep => {
                let id = resume_key(&r.config);
                if let Some(i) = pending.iter().position(|c| resume_key(c) == id) {
                    pending.remove(i);
                    previous.push(r);
                    continue;
                }
                previous.push(r); // finished trial from an older plan: kept for stats
            }
            _ => std::fs::remove_dir_all(&p)?,
        }
    }
    let n_total = previous.len() + pending.len();
    if !previous.is_empty() {
        eprintln!("resuming: {} trial(s) already complete, {} to run", previous.len(), pending.len());
        log.line(&format!(
            "resuming: {} trial(s) already complete, {} to run, {prior_retries} retries so far",
            previous.len(),
            pending.len()
        ));
    }
    if !cfg.quiet {
        eprintln!("job {}: {} trial(s) to run, concurrency {}", job_dir.display(), pending.len(), cfg.n_concurrent_trials);
    }
    let lock = serde_json::json!({
        "schema_version": 3,
        "created_at": Utc::now(),
        "hbench": {"version": env!("CARGO_PKG_VERSION")},
        "n_concurrent_trials": cfg.n_concurrent_trials,
        "retry": cfg.retry,
        "trials": pending.iter().map(crate::trial::trial_lock).collect::<Vec<_>>(),
    });
    std::fs::write(job_dir.join("lock.json"), serde_json::to_string_pretty(&lock)?)?;

    let job_result = Arc::new(Mutex::new(JobResult {
        id: job_id,
        started_at: Utc::now(),
        updated_at: None,
        finished_at: None,
        n_total_trials: n_total,
        stats: JobStats::default(),
    }));
    let progress = Arc::new(Mutex::new(JobProgress { results: previous, running: 0, retries: prior_retries }));
    let metric_cfgs = Arc::new(cfg.metrics.clone());
    let persist_job_result = {
        let (job_result, progress, path, metric_cfgs) =
            (job_result.clone(), progress.clone(), job_dir.join("result.json"), metric_cfgs.clone());
        move || {
            let current = locked(&progress);
            let mut jr = locked(&job_result);
            jr.updated_at = Some(Utc::now());
            jr.stats = compute_stats(&current.results, jr.n_total_trials, current.running, current.retries, &metric_cfgs);
            let _ = write_atomic(&path, serde_json::to_string_pretty(&*jr).unwrap());
        }
    };
    persist_job_result();

    let sem = Arc::new(Semaphore::new(cfg.n_concurrent_trials));
    let mut pools: HashMap<String, Arc<Semaphore>> = HashMap::new();
    for a in &cfg.agents {
        if let Some(n) = a.n_concurrent {
            pools.entry(a.concurrency_key()).or_insert_with(|| Arc::new(Semaphore::new(n)));
        }
    }
    let quiet = cfg.quiet;
    let mut handles = Vec::new();
    for tcfg in pending {
        let task = tasks[&registry::local_path(&tcfg.task)].clone();
        let pool = pools.get(&tcfg.agent.concurrency_key()).cloned();
        let (sem, progress, persist_job_result, cancel, retry, log) =
            (sem.clone(), progress.clone(), persist_job_result.clone(), cancel.clone(), cfg.retry.clone(), log.clone());
        handles.push(tokio::spawn(async move {
            // The agent's own pool first, so a trial holding a global slot never waits on it.
            let _agent_permit = match pool {
                Some(p) => Some(p.acquire_owned().await.unwrap()),
                None => None,
            };
            let _permit = sem.acquire_owned().await.unwrap();
            if *cancel.borrow() {
                return;
            }
            locked(&progress).running += 1;
            persist_job_result();
            log.line(&format!("trial {} started (task {}, agent {})", tcfg.trial_name, tcfg.task.display_name(), tcfg.agent.name()));
            let mut attempt = 0;
            let result = loop {
                let r = match Trial::new(tcfg.clone(), task.clone()) {
                    Ok(mut t) => {
                        t.progress = !quiet;
                        // Its own task, so a panic becomes this trial's error instead of losing the bookkeeping.
                        match tokio::spawn(t.run(cancel.clone())).await {
                            Ok(r) => r,
                            Err(e) => TrialResult::errored(tcfg.clone(), &task, &anyhow::anyhow!("trial panicked: {e}")),
                        }
                    }
                    Err(e) => {
                        eprintln!("{}: could not create trial: {e:#}", tcfg.trial_name);
                        TrialResult::errored(tcfg.clone(), &task, &e)
                    }
                };
                let Some(kind) = r.exception_info.as_ref().map(|e| e.exception_type.clone()) else { break r };
                if !retry.should_retry(&kind) || attempt >= retry.max_retries || *cancel.borrow() {
                    break r;
                }
                locked(&progress).retries += 1;
                let wait = retry.backoff_sec(attempt);
                attempt += 1;
                eprintln!("retrying {} ({attempt}/{}) after {kind} in {wait:.1}s", tcfg.trial_name, retry.max_retries);
                log.line(&format!("trial {} retry {attempt} after {kind}", tcfg.trial_name));
                let _ = std::fs::remove_dir_all(tcfg.trials_dir.join(&tcfg.trial_name));
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            };
            {
                let mut current = locked(&progress);
                current.running -= 1;
                current.results.push(result.clone());
                log.line(&format!(
                    "trial {} finished: rewards={} exception={}",
                    result.trial_name,
                    result.reward().map(|r| serde_json::to_string(r).unwrap()).unwrap_or_else(|| "-".into()),
                    result.exception_info.as_ref().map(|e| e.exception_type.as_str()).unwrap_or("-")
                ));
                if !quiet {
                    let reward = result.reward().map(|r| serde_json::to_string(r).unwrap()).unwrap_or_else(|| "-".into());
                    let err = result
                        .exception_info
                        .as_ref()
                        .map(|e| format!(" [{}: {}]", e.exception_type, crate::env::tail(&e.exception_message, 200)))
                        .unwrap_or_default();
                    eprintln!("[{}/{}] {} reward={reward}{err}", current.results.len(), n_total, result.trial_name);
                }
            }
            persist_job_result();
        }));
    }
    for h in handles {
        if let Err(e) = h.await {
            eprintln!("trial task failed: {e}");
        }
    }
    if !*cancel.borrow() {
        locked(&job_result).finished_at = Some(Utc::now());
        log.line("job finished");
    } else {
        log.line("job cancelled");
    }
    persist_job_result();
    // A secret first seen in a later trial may also be in an earlier one's files, and in job.log.
    crate::redact::scrub_dir(&job_dir);
    let jr = locked(&job_result).clone();
    Ok(jr)
}

pub fn load_config(job_dir: &Path) -> Result<JobConfig> {
    // `jobs resume -p .` (or `..`) has no file name to take the job's name from.
    let resolved;
    let job_dir = if job_dir.file_name().is_some() {
        job_dir
    } else {
        resolved = job_dir.canonicalize().with_context(|| format!("job dir {}", job_dir.display()))?;
        &resolved
    };
    let p = job_dir.join("config.json");
    let s = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    let mut cfg: JobConfig = serde_json::from_str(&s).context("parsing job config.json")?;
    // Trust the directory we were pointed at (the job may have been moved).
    cfg.job_name = job_dir.file_name().context("a job dir can't be /")?.to_string_lossy().into_owned();
    cfg.jobs_dir = job_dir.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok(cfg)
}

/// Job results tables + "Job Info", laid out as a dashboard with bars.
pub fn print_summary(jr: &JobResult, job_dir: &Path) {
    print!("\n{}", render_summary(jr, job_dir, use_color()));
}

fn use_color() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

const BAR: usize = 24;

fn bar(frac: f64) -> (String, String) {
    let n = ((frac.clamp(0.0, 1.0) * BAR as f64).round() as usize).min(BAR);
    ("█".repeat(n), "░".repeat(BAR - n))
}

pub fn render_summary(jr: &JobResult, job_dir: &Path, color: bool) -> String {
    let paint = |code: &str, t: &str| if color { format!("\x1b[{code}m{t}\x1b[0m") } else { t.to_string() };
    // Score colour: green = good, yellow = partial, red = zero.
    let tone = |v: f64| {
        if v >= 1.0 {
            "32"
        } else if v > 0.0 {
            "33"
        } else {
            "31"
        }
    };
    let mut out = String::new();
    for (key, e) in &jr.stats.evals {
        let mut metrics: Vec<(String, &Value)> = Vec::new();
        for (i, m) in e.metrics.iter().enumerate() {
            for (k, v) in m {
                let label = if e.metrics.len() > 1 { format!("Metric {}: {k}", i + 1) } else { k.clone() };
                metrics.push((title_case(&label), v));
            }
        }
        let pass: Vec<(String, f64)> = e.pass_at_k.iter().map(|(k, v)| (format!("Pass@{k}"), *v)).collect();
        let rewards: Vec<(&String, usize)> =
            e.reward_stats.values().flat_map(|vals| vals.iter().rev().map(|(v, n)| (v, n.len()))).collect();
        let w = metrics
            .iter()
            .map(|(l, _)| l.chars().count())
            .chain(pass.iter().map(|(l, _)| l.len()))
            .chain(rewards.iter().map(|(v, _)| v.chars().count()))
            .chain(e.exception_stats.keys().map(|x| x.chars().count()))
            .chain([10])
            .max()
            .unwrap();
        let title = group_title(key);
        out += &format!("{}\n{}\n", paint("1", &title), paint("2", &"─".repeat(title.chars().count().max(w + BAR + 12))));

        // n_trials counts graded trials only; metrics divide by every finished trial (no reward = 0).
        let names: std::collections::HashSet<&String> =
            e.reward_stats.values().flat_map(|v| v.values().flatten()).chain(e.exception_stats.values().flatten()).collect();
        let total = names.len().max(e.n_trials);
        let ungraded = total.saturating_sub(e.n_trials);
        let errs = if e.n_errors == 0 { paint("2", "0 exceptions") } else { paint("33", &format!("{} exceptions", e.n_errors)) };
        out += &format!(
            "  {:<w$}  {}  {}\n\n",
            "Trials",
            paint("1", &format!("{total:>6}")),
            [paint("2", "finished"), format!("{} graded", e.n_trials), errs].join(&paint("2", " · "))
        );

        let score = |label: &str, text: String, frac: Option<f64>| {
            let bars = frac
                .map(|f| {
                    let (a, b) = bar(f);
                    format!("  {}{}", paint(tone(f), &a), paint("2", &b))
                })
                .unwrap_or_default();
            format!("  {label:<w$}  {}{bars}\n", paint("1", &format!("{text:>6}")))
        };
        for (l, v) in &metrics {
            out += &score(l, metric_text(v), v.as_f64().filter(|f| (0.0..=1.0).contains(f)));
        }
        for (l, v) in &pass {
            out += &score(l, format!("{v:.3}"), Some(*v));
        }

        let counts = |head: &str, rows: Vec<(String, usize, &str)>| {
            let total = total.max(1) as f64;
            let mut s = format!("\n  {}\n", paint("2", &format!("{head:<w$}   count  share of trials")));
            for (label, n, code) in rows {
                let (a, b) = bar(n as f64 / total);
                s += &format!(
                    "  {}  {}  {}{}  {}\n",
                    paint(if code == "2" { "2" } else { "0" }, &format!("{label:<w$}")),
                    paint("1", &format!("{n:>6}")),
                    paint(code, &a),
                    paint("2", &b),
                    paint("2", &format!("{:.0}%", 100.0 * n as f64 / total))
                );
            }
            s
        };
        if !rewards.is_empty() {
            let mut rows: Vec<(String, usize, &str)> =
                rewards.iter().map(|(v, n)| ((*v).clone(), *n, v.parse::<f64>().map(tone).unwrap_or("36"))).collect();
            if ungraded > 0 {
                rows.push(("no reward".into(), ungraded, "2"));
            }
            out += &counts("Reward", rows);
        }
        if !e.exception_stats.is_empty() {
            out += &counts("Exception", e.exception_stats.iter().map(|(x, n)| (x.clone(), n.len(), "33")).collect());
        }
        out += "\n";
    }
    let s = &jr.stats;
    let mut info = vec![format!("runtime {}", format_duration(jr.started_at, jr.finished_at))];
    // Tokens/cost are shown here too, not only kept in result.json.
    if let Some(c) = s.cost_usd {
        info.push(format!("cost ${c:.2}"));
        info.push(format!("tokens {} in ({} cached) · {} out", human(s.n_input_tokens), human(s.n_cache_tokens), human(s.n_output_tokens)));
    }
    out += &format!("{}  {}\n", paint("1", "Job"), info.join(&paint("2", " · ")));
    out += &format!("     {} {}\n", paint("2", "results"), job_dir.join("result.json").display());
    out += &format!("     {} hbench view {}\n", paint("2", "inspect"), job_dir.parent().unwrap_or(Path::new(".")).display());
    out
}

fn human(n: Option<i64>) -> String {
    let n = n.unwrap_or(0) as f64;
    if n >= 1e6 {
        format!("{:.2}M", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.1}K", n / 1e3)
    } else {
        format!("{n}")
    }
}

/// `agent__model__dataset` -> `dataset • agent • model` (model names may themselves contain `__`).
fn group_title(key: &str) -> String {
    let parts: Vec<&str> = key.split("__").collect();
    match parts.as_slice() {
        [agent, dataset] => format!("{dataset} • {agent}"),
        [agent, model @ .., dataset] if !model.is_empty() => format!("{dataset} • {agent} • {}", model.join("__")),
        _ => key.to_string(),
    }
}

fn format_duration(start: DateTime<Utc>, end: Option<DateTime<Utc>>) -> String {
    let Some(end) = end else { return "unknown".into() };
    let t = (end - start).num_seconds().max(0);
    let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Metric value for the summary table: floats as `{:.3}`, `-` for none, strings as-is, anything else as JSON.
fn metric_text(v: &Value) -> String {
    match v {
        Value::Number(n) if n.is_f64() => format!("{:.3}", n.as_f64().unwrap()),
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trial::*;

    pub fn result(task: &str, trial: &str, reward: Option<f64>, err: Option<&str>) -> TrialResult {
        let cfg: TrialConfig =
            serde_json::from_value(serde_json::json!({"task": {"path": task}, "trial_name": trial, "trials_dir": "j"})).unwrap();
        TrialResult {
            id: uuid::Uuid::nil(),
            task_name: task.into(),
            trial_name: trial.into(),
            trial_uri: String::new(),
            task_id: serde_json::json!({"path": task}),
            source: None,
            task_checksum: String::new(),
            config: cfg,
            agent_info: AgentInfo { name: "oracle".into(), version: "1.0.0".into(), model_info: None },
            agent_result: None,
            verifier_result: reward
                .map(|r| VerifierResult { rewards: Some([("reward".into(), serde_json::Number::from_f64(r).unwrap())].into()) }),
            verifier_environment_mode: None,
            exception_info: err.map(|e| ExceptionInfo {
                exception_type: e.into(),
                exception_message: String::new(),
                exception_traceback: String::new(),
                occurred_at: Utc::now(),
            }),
            started_at: None,
            finished_at: None,
            environment_setup: None,
            agent_setup: None,
            agent_execution: None,
            verifier: None,
            step_results: None,
            egress: None,
        }
    }

    #[test]
    fn stats_mean_and_pass_at_k() {
        let rs = vec![
            result("a", "a1", Some(1.0), None),
            result("a", "a2", Some(0.0), None),
            result("b", "b1", None, Some("AgentTimeoutError")),
            result("b", "b2", Some(0.0), None),
        ];
        let s = compute_stats(&rs, 5, 0, 0, &[]);
        let e = &s.evals["oracle__adhoc"];
        assert_eq!((e.n_trials, e.n_errors, s.n_errored_trials, s.n_pending_trials), (3, 1, 1, 1));
        assert_eq!(e.metrics[0]["mean"], 0.25);
        assert_eq!(e.pass_at_k[&2], 0.5);
        assert_eq!(e.reward_stats["reward"]["1.0"], vec!["a1"]);
        assert_eq!(e.exception_stats["AgentTimeoutError"], vec!["b1"]);
        let three = [result("c", "1", Some(1.0), None), result("c", "2", Some(0.0), None), result("c", "3", Some(0.0), None)];
        assert!((metrics::pass_at_k(&three.iter().collect::<Vec<_>>())[&2] - 2.0 / 3.0).abs() < 1e-12);
        let nb = [result("c", "1", Some(0.5), None), result("c", "2", Some(1.0), None)];
        assert!(metrics::pass_at_k(&nb.iter().collect::<Vec<_>>()).is_empty());
    }

    #[test]
    fn summary_dashboard() {
        let rs = vec![
            result("a", "a1", Some(1.0), None),
            result("a", "a2", Some(0.0), None),
            result("b", "b1", None, Some("AgentTimeoutError")),
            result("b", "b2", Some(0.0), None),
        ];
        let t0 = Utc::now();
        let jr = JobResult {
            id: uuid::Uuid::nil(),
            started_at: t0,
            updated_at: None,
            finished_at: Some(t0 + chrono::Duration::seconds(3725)),
            n_total_trials: 4,
            stats: compute_stats(&rs, 4, 0, 0, &[]),
        };
        let out = render_summary(&jr, Path::new("jobs/j"), false);
        let expect = "\
adhoc • oracle
─────────────────────────────────────────────────────
  Trials                  4  finished · 3 graded · 1 exceptions

  Mean                0.250  ██████░░░░░░░░░░░░░░░░░░
  Pass@2              0.500  ████████████░░░░░░░░░░░░

  Reward              count  share of trials
  1.0                     1  ██████░░░░░░░░░░░░░░░░░░  25%
  0.0                     2  ████████████░░░░░░░░░░░░  50%
  no reward               1  ██████░░░░░░░░░░░░░░░░░░  25%

  Exception           count  share of trials
  AgentTimeoutError       1  ██████░░░░░░░░░░░░░░░░░░  25%

Job  runtime 1h 2m 5s
     results jobs/j/result.json
     inspect hbench view jobs
";
        assert_eq!(out, expect);
        assert_eq!(
            group_title("claude-code__deepseek/deepseek-v4-flash__terminal-bench@2.0"),
            "terminal-bench@2.0 • claude-code • deepseek/deepseek-v4-flash"
        );
        assert_eq!(title_case("Metric 1: pass_rate"), "Metric 1: Pass_Rate");
    }

    #[test]
    fn trial_plan_order_and_resume_key() {
        let cfg: JobConfig = serde_json::from_value(serde_json::json!({
            "n_attempts": 2, "agents": [{"name": "oracle"}, {"name": "nop"}], "job_name": "j"
        }))
        .unwrap();
        let tasks = vec![TaskRef::local("/t/a".into(), None), TaskRef::local("/t/b".into(), None)];
        let plan = build_trial_configs(&cfg, &tasks, uuid::Uuid::nil());
        let names: Vec<(String, &str)> = plan.iter().map(|c| (c.task.display_name(), c.agent.name())).collect();
        assert_eq!(names.len(), 8);
        assert_eq!(&names[..4], &[("a".into(), "oracle"), ("a".into(), "nop"), ("b".into(), "oracle"), ("b".into(), "nop")]);
        assert_eq!(resume_key(&plan[0]), resume_key(&plan[4]));
        assert_ne!(resume_key(&plan[0]), resume_key(&plan[1]));
        assert!(plan[0].trial_name.starts_with("a__") && plan[0].trial_name.len() == 10);
        // A moved job (tasks moved along) still matches its finished trials.
        let mut moved = plan[0].clone();
        moved.trials_dir = "/elsewhere/j".into();
        moved.task.path = Some("/moved/t/a".into());
        assert_eq!(resume_key(&moved), resume_key(&plan[0]));
    }

    #[test]
    fn regrade_job_without_plan_is_not_resumable() {
        let d = std::env::temp_dir().join(format!("hbench-plan-{}", uuid::Uuid::new_v4()));
        let cfg = JobConfig { jobs_dir: d.clone(), job_name: "j".into(), ..Default::default() };
        std::fs::create_dir_all(cfg.job_dir().join("t1")).unwrap();
        assert!(stored_plan(&cfg).unwrap().is_none());
        std::fs::write(cfg.job_dir().join("t1/config.json"), r#"{"source_trial": {}}"#).unwrap();
        assert!(stored_plan(&cfg).is_err());
        let tc: TrialConfig =
            serde_json::from_value(serde_json::json!({"task": {"path": "/t/a"}, "trial_name": "a__1", "trials_dir": "/old/j"})).unwrap();
        write_atomic(&cfg.job_dir().join(PLAN_FILE), serde_json::to_string(&[tc]).unwrap()).unwrap();
        assert_eq!(stored_plan(&cfg).unwrap().unwrap()[0].trials_dir, cfg.job_dir());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_resumed_job_keeps_its_id_and_its_retries() {
        let d = std::env::temp_dir().join(format!("hbench-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let (fresh, none) = carried_over(&d);
        assert_eq!(none, 0);
        let earlier = JobResult {
            id: fresh,
            started_at: Utc::now(),
            updated_at: None,
            finished_at: None,
            n_total_trials: 2,
            stats: JobStats { n_retries: 30, ..Default::default() },
        };
        std::fs::write(d.join("result.json"), serde_json::to_string(&earlier).unwrap()).unwrap();
        assert_eq!(carried_over(&d), (fresh, 30));
        std::fs::write(d.join("result.json"), "not json").unwrap();
        assert_ne!(carried_over(&d).0, fresh);
        let _ = std::fs::remove_dir_all(&d);
    }
}
