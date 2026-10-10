//! Job = attempts x tasks x agents: concurrency, per-agent pools, retries, resume, stats, locks.

use crate::config::{JobConfig, TaskRef, TrialConfig};
use crate::metrics;
use crate::registry;
use crate::task::Task;
use crate::trial::{Trial, TrialResult};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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

/// Python-repr-like formatting of a reward value used as a JSON object key.
fn reward_key(n: &serde_json::Number) -> String {
    match n.as_i64() {
        Some(i) => i.to_string(),
        None => format!("{:?}", n.as_f64().unwrap_or(f64::NAN)),
    }
}

pub fn compute_stats(results: &[TrialResult], n_total: usize, n_running: usize, n_retries: usize, metric_cfgs: &[crate::config::MetricConfig]) -> JobStats {
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
            if ex.exception_type == "CancelledError" {
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
    let default_mean = [crate::config::MetricConfig { kind: "mean".into(), kwargs: Default::default() }];
    let cfgs: &[crate::config::MetricConfig] = if metric_cfgs.is_empty() { &default_mean } else { metric_cfgs };
    for (key, rs) in groups {
        let rewards: Vec<Option<&crate::trial::Rewards>> = rs.iter().map(|r| r.reward()).collect();
        let e = s.evals.get_mut(&key).unwrap();
        for m in cfgs {
            match metrics::compute(m, &rewards) {
                Ok(v) => e.metrics.push(v),
                Err(err) => eprintln!("metric {} failed: {err:#}", m.kind),
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

/// Identity of a trial config for resume matching (excludes name/job id).
fn identity(c: &TrialConfig) -> Value {
    let mut v = serde_json::to_value(c).unwrap();
    if let Some(o) = v.as_object_mut() {
        o.remove("trial_name");
        o.remove("job_id");
    }
    v
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

struct Shared {
    results: Vec<TrialResult>,
    running: usize,
    retries: usize,
}

/// Run (or resume) a job. `filter_error_types`: on resume, rerun trials that failed with these.
pub async fn run_job(cfg: JobConfig, trial_configs: Option<Vec<TrialConfig>>, filter_error_types: &[String], cancel: watch::Receiver<bool>) -> Result<JobResult> {
    let job_dir = cfg.job_dir();
    std::fs::create_dir_all(&job_dir)?;
    let job_id = match std::fs::read_to_string(job_dir.join("result.json")).ok().and_then(|s| serde_json::from_str::<JobResult>(&s).ok()) {
        Some(prev) => prev.id,
        None => uuid::Uuid::new_v4(),
    };
    std::fs::write(job_dir.join("config.json"), serde_json::to_string_pretty(&cfg)?)?;
    let log = crate::env::TrialLog::create(&job_dir.join("job.log"))?;
    log.line(&format!("job {} started ({} agent config(s))", cfg.job_name, cfg.agents.len()));

    let mut pending = match trial_configs {
        Some(t) => t,
        None => build_trial_configs(&cfg, &resolve_tasks(&cfg)?, job_id),
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
                let id = identity(&r.config);
                if let Some(i) = pending.iter().position(|c| identity(c) == id) {
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

    let job_result = Arc::new(Mutex::new(JobResult { id: job_id, started_at: Utc::now(), updated_at: None, finished_at: None, n_total_trials: n_total, stats: JobStats::default() }));
    let shared = Arc::new(Mutex::new(Shared { results: previous, running: 0, retries: 0 }));
    let metric_cfgs = Arc::new(cfg.metrics.clone());
    let write = {
        let (job_result, shared, path, metric_cfgs) = (job_result.clone(), shared.clone(), job_dir.join("result.json"), metric_cfgs.clone());
        move || {
            let sh = shared.lock().unwrap();
            let mut jr = job_result.lock().unwrap();
            jr.updated_at = Some(Utc::now());
            jr.stats = compute_stats(&sh.results, jr.n_total_trials, sh.running, sh.retries, &metric_cfgs);
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&*jr).unwrap());
        }
    };
    write();

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
        let (sem, shared, write, cancel, retry, log) = (sem.clone(), shared.clone(), write.clone(), cancel.clone(), cfg.retry.clone(), log.clone());
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            if *cancel.borrow() {
                return;
            }
            shared.lock().unwrap().running += 1;
            write();
            log.line(&format!("trial {} started (task {}, agent {})", tcfg.trial_name, tcfg.task.display_name(), tcfg.agent.name()));
            let mut attempt = 0;
            let result = loop {
                let r = match Trial::new(tcfg.clone(), task.clone(), pool.clone()) {
                    Ok(t) => t.run(cancel.clone()).await,
                    Err(e) => {
                        eprintln!("{}: could not create trial: {e:#}", tcfg.trial_name);
                        shared.lock().unwrap().running -= 1;
                        return;
                    }
                };
                let Some(kind) = r.exception_info.as_ref().map(|e| e.exception_type.clone()) else { break r };
                if !retry.should_retry(&kind) || attempt >= retry.max_retries || *cancel.borrow() {
                    break r;
                }
                shared.lock().unwrap().retries += 1;
                let wait = retry.backoff_sec(attempt);
                attempt += 1;
                eprintln!("retrying {} ({attempt}/{}) after {kind} in {wait:.1}s", tcfg.trial_name, retry.max_retries);
                log.line(&format!("trial {} retry {attempt} after {kind}", tcfg.trial_name));
                let _ = std::fs::remove_dir_all(tcfg.trials_dir.join(&tcfg.trial_name));
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            };
            {
                let mut sh = shared.lock().unwrap();
                sh.running -= 1;
                sh.results.push(result.clone());
                log.line(&format!(
                    "trial {} finished: rewards={} exception={}",
                    result.trial_name,
                    result.reward().map(|r| serde_json::to_string(r).unwrap()).unwrap_or_else(|| "-".into()),
                    result.exception_info.as_ref().map(|e| e.exception_type.as_str()).unwrap_or("-")
                ));
                if !quiet {
                    let reward = result.reward().map(|r| serde_json::to_string(r).unwrap()).unwrap_or_else(|| "-".into());
                    let err = result.exception_info.as_ref().map(|e| format!(" [{}: {}]", e.exception_type, crate::env::tail(&e.exception_message, 200))).unwrap_or_default();
                    eprintln!("[{}/{}] {} reward={reward}{err}", sh.results.len(), n_total, result.trial_name);
                }
            }
            write();
        }));
    }
    for h in handles {
        h.await.context("trial task panicked")?;
    }
    if !*cancel.borrow() {
        job_result.lock().unwrap().finished_at = Some(Utc::now());
        log.line("job finished");
    } else {
        log.line("job cancelled");
    }
    write();
    let jr = job_result.lock().unwrap().clone();
    Ok(jr)
}

pub fn load_config(job_dir: &Path) -> Result<JobConfig> {
    let p = job_dir.join("config.json");
    let s = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    let mut cfg: JobConfig = serde_json::from_str(&s).context("parsing job config.json")?;
    // Trust the directory we were pointed at (the job may have been moved).
    cfg.job_name = job_dir.file_name().unwrap().to_string_lossy().into_owned();
    cfg.jobs_dir = job_dir.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok(cfg)
}

pub fn print_summary(jr: &JobResult, job_dir: &Path) {
    let s = &jr.stats;
    println!("\nJob: {}", job_dir.display());
    println!("Trials: {} completed, {} errored, {} total", s.n_completed_trials, s.n_errored_trials, jr.n_total_trials);
    for (k, e) in &s.evals {
        let metrics: Vec<String> = e.metrics.iter().flat_map(|m| m.iter().map(|(k, v)| format!("{k}={}", v.as_f64().map(|f| format!("{f:.3}")).unwrap_or(v.to_string())))).collect();
        let pak: Vec<String> = e.pass_at_k.iter().map(|(k, v)| format!("pass@{k}={v:.3}")).collect();
        println!("  {k}: trials={} errors={} {} {}", e.n_trials, e.n_errors, metrics.join(" "), pak.join(" "));
        for (ex, names) in &e.exception_stats {
            println!("    {ex}: {}", names.len());
        }
    }
    if let Some(c) = s.cost_usd {
        println!("Cost: ${c:.4}  tokens in={} cache={} out={}", s.n_input_tokens.unwrap_or(0), s.n_cache_tokens.unwrap_or(0), s.n_output_tokens.unwrap_or(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trial::*;

    pub fn result(task: &str, trial: &str, reward: Option<f64>, err: Option<&str>) -> TrialResult {
        let cfg: TrialConfig = serde_json::from_value(serde_json::json!({"task": {"path": task}, "trial_name": trial, "trials_dir": "j"})).unwrap();
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
            verifier_result: reward.map(|r| VerifierResult { rewards: Some([("reward".into(), serde_json::Number::from_f64(r).unwrap())].into()) }),
            verifier_environment_mode: None,
            exception_info: err.map(|e| ExceptionInfo { exception_type: e.into(), exception_message: String::new(), exception_traceback: String::new(), occurred_at: Utc::now() }),
            started_at: None,
            finished_at: None,
            environment_setup: None,
            agent_setup: None,
            agent_execution: None,
            verifier: None,
            step_results: None,
        }
    }

    #[test]
    fn stats_mean_and_pass_at_k() {
        let rs = vec![result("a", "a1", Some(1.0), None), result("a", "a2", Some(0.0), None), result("b", "b1", None, Some("AgentTimeoutError")), result("b", "b2", Some(0.0), None)];
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
    fn trial_plan_order_and_identity() {
        let cfg: JobConfig = serde_json::from_value(serde_json::json!({
            "n_attempts": 2, "agents": [{"name": "oracle"}, {"name": "nop"}], "job_name": "j"
        }))
        .unwrap();
        let tasks = vec![TaskRef::local("/t/a".into(), None), TaskRef::local("/t/b".into(), None)];
        let plan = build_trial_configs(&cfg, &tasks, uuid::Uuid::nil());
        let names: Vec<(String, &str)> = plan.iter().map(|c| (c.task.display_name(), c.agent.name())).collect();
        assert_eq!(names.len(), 8);
        assert_eq!(&names[..4], &[("a".into(), "oracle"), ("a".into(), "nop"), ("b".into(), "oracle"), ("b".into(), "nop")]);
        assert_eq!(identity(&plan[0]), identity(&plan[4]));
        assert_ne!(identity(&plan[0]), identity(&plan[1]));
        assert!(plan[0].trial_name.starts_with("a__") && plan[0].trial_name.len() == 10);
    }
}
