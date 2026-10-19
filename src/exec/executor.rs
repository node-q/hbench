//! Executor: runs the map job, then the optional reduce job.

use super::*;
use crate::config::JobConfig;
use crate::job::{self, JobResult};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::path::{Path, PathBuf};

struct Phase {
    task_dirs: Vec<PathBuf>,
    job_dir: PathBuf,
    result: JobResult,
}

pub(super) fn job_config(j: &ExecJobConfig, task_dirs: &[PathBuf]) -> Result<JobConfig> {
    let mut v = json!({
        "n_attempts": j.n_attempts,
        "n_concurrent_trials": j.n_concurrent_trials,
        "quiet": j.quiet,
        "retry": j.retry,
        "environment": j.environment,
        "verifier": j.verifier,
        "metrics": j.metrics,
        "agents": j.agents,
        "tasks": task_dirs.iter().map(|d| json!({"path": d})).collect::<Vec<_>>(),
    });
    if let Some(n) = &j.job_name {
        v["job_name"] = n.clone().into();
    }
    if let Some(d) = &j.jobs_dir {
        v["jobs_dir"] = d.0.clone().into();
    }
    let cfg: JobConfig = serde_json::from_value(v).context("invalid exec job config")?;
    cfg.validate()?;
    crate::preflight_agents(&cfg)?;
    Ok(cfg)
}

async fn run_phase(mut cfg: JobConfig, task_dirs: Vec<PathBuf>, cancel: &tokio::sync::watch::Receiver<bool>) -> Result<Phase> {
    let job_dir = cfg.job_dir();
    // Env.rs resolves the compose dir against the task's environment/ dir, so a relative
    // jobs_dir breaks `docker compose` (also in `hbench run`); hand the runner an absolute one.
    cfg.jobs_dir = std::path::absolute(&cfg.jobs_dir)?;
    let result = job::run_job(cfg, None, &[], cancel.clone()).await?;
    Ok(Phase { task_dirs, job_dir, result })
}

async fn execute(cfg: &ExecConfig) -> Result<(Phase, Option<Phase>)> {
    let map_dirs = compile(&cfg.map.compile)?;
    let map_job = job_config(&cfg.map.job, &map_dirs)?;
    crate::env::preflight().await?;
    // Force-quit cleanup covers the map job's trials; the reduce job is one trial.
    let map_dir = std::path::absolute(map_job.job_dir())?;
    let rx = crate::cancel_on_signal(move || crate::trial_names(std::slice::from_ref(&map_dir)));
    let map = run_phase(map_job, map_dirs, &rx).await?;
    let Some(r) = &cfg.reduce else { return Ok((map, None)) };
    if *rx.borrow() {
        bail!("cancelled before the reduce phase");
    }
    let reduce_dir = compile_reduce_task(&r.task, &map.job_dir)?;
    let reduce_job = job_config(&r.job, std::slice::from_ref(&reduce_dir))?;
    let reduce = run_phase(reduce_job, vec![reduce_dir], &rx).await?;
    Ok((map, Some(reduce)))
}

/// Trial results of a finished job, ordered by trial name. Not completion
/// order, since JobResult has no trial list; read the trial dirs instead.
fn trial_results(job_dir: &Path) -> Result<Vec<crate::trial::TrialResult>> {
    let mut out: Vec<crate::trial::TrialResult> = vec![];
    for e in std::fs::read_dir(job_dir)?.flatten() {
        if let Ok(s) = std::fs::read_to_string(e.path().join("result.json"))
            && let Ok(r) = serde_json::from_str(&s)
        {
            out.push(r);
        }
    }
    out.sort_by(|a, b| a.trial_name.cmp(&b.trial_name));
    Ok(out)
}

pub(super) fn compile_reduce_task(task: &ExecReduceTaskConfig, map_job_dir: &Path) -> Result<PathBuf> {
    let trials = trial_results(map_job_dir)?;
    if trials.is_empty() {
        bail!("Cannot reduce a map job with no trial results.");
    }
    let staging = temp_root().join(format!("hbench-exec-reduce-{}", uuid::Uuid::new_v4().simple()));
    let res = stage_and_compile(task, &trials, &staging);
    let _ = std::fs::remove_dir_all(&staging);
    let dirs = res?;
    if dirs.len() != 1 {
        bail!("Reduce task compilation must produce exactly one task.");
    }
    Ok(dirs.into_iter().next().unwrap_or_default())
}

fn stage_and_compile(task: &ExecReduceTaskConfig, trials: &[crate::trial::TrialResult], staging: &Path) -> Result<Vec<PathBuf>> {
    let staged = staging.join("artifacts");
    std::fs::create_dir_all(&staged)?;
    let mut n = 0;
    for (i, t) in trials.iter().enumerate() {
        let Some(dir) = t.trial_uri.strip_prefix("file://") else { bail!("Expected file trial URI, got: {}", t.trial_uri) };
        let artifacts = Path::new(dir).join("artifacts");
        if !artifacts.is_dir() {
            continue;
        }
        let s = slug(if t.trial_name.is_empty() { "trial" } else { &t.trial_name });
        let s = if s.is_empty() { "trial".to_string() } else { s };
        copy_tree(&artifacts, &staged.join(format!("{:04}-{s}", i + 1)))?;
        n += 1;
    }
    if n == 0 {
        bail!("Cannot reduce a map job with no trial artifacts.");
    }
    let dirs = compile(&CompileConfig {
        task_name_prefix: Some(task.task_name.clone()),
        output_dir: Some(task.output_dir.clone()),
        task_template: task.task_template.clone(),
        artifacts: task.artifacts.clone(),
        instructions: vec![task.instruction.clone()],
        environments: vec![CompileEnvironment {
            path: None,
            paths: vec![],
            docker_image: Some(task.environment.docker_image.clone()),
            workdir: Some(task.environment.workdir.clone()),
        }],
        verifiers: task.verifier.clone().into_iter().collect(),
        ..Default::default()
    })?;
    if dirs.len() != 1 {
        bail!("Reduce task compilation must produce exactly one task.");
    }
    let target = dirs[0].join("environment").join("artifacts");
    std::fs::create_dir_all(&target)?;
    for e in std::fs::read_dir(&staged)?.flatten() {
        replace_dir(&e.path(), &target.join(e.file_name()))?;
    }
    Ok(dirs)
}

pub(super) async fn execute_and_render(config: ExecConfig, cleanup: bool) -> Result<()> {
    if !cleanup {
        let (m, r) = execute(&config).await?;
        render(&m, r.as_ref(), false);
        return Ok(());
    }
    let tmp = temp_root().join(format!("hbench-exec-map-tasks-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::create_dir_all(&tmp)?;
    let mut run = config;
    let out = NormPath(tmp.to_string_lossy().into_owned());
    run.map.compile.output_dir = Some(out.clone());
    if let Some(r) = run.reduce.as_mut() {
        r.task.output_dir = out;
    }
    let res = execute(&run).await;
    let _ = std::fs::remove_dir_all(&tmp);
    let (m, r) = res?;
    render(&m, r.as_ref(), true);
    Ok(())
}

fn is_temporary(p: &Path) -> bool {
    let tmp = temp_root().canonicalize().unwrap_or_else(|_| temp_root());
    resolve(&p.to_string_lossy()).starts_with(tmp)
}

fn render(map: &Phase, reduce: Option<&Phase>, cleaned_up: bool) {
    let tasks_parent = map.task_dirs[0].parent().unwrap_or(Path::new("")).to_path_buf();
    let temporary = cleaned_up || is_temporary(&tasks_parent);
    println!("Map Results");
    job::print_summary(&map.result, &map.job_dir);
    if cleaned_up {
        println!("Compiled tasks used a temporary directory and were cleaned up.");
    } else if temporary {
        println!("Compiled tasks used a temporary directory.");
    } else {
        println!("Map tasks written to {}", tasks_parent.display());
    }
    println!("Map job written to {}", map.job_dir.display());
    let Some(r) = reduce else {
        println!("Inspect results by running `hbench view -p {}`", map.job_dir.parent().unwrap_or(Path::new(".")).display());
        println!();
        return;
    };
    println!("Reduce Results");
    job::print_summary(&r.result, &r.job_dir);
    if !temporary {
        println!("Reduce task written to {}", r.task_dirs[0].display());
    }
    println!("Reduce job written to {}", r.job_dir.display());
    println!("Inspect results by running `hbench view -p {}`", r.job_dir.parent().unwrap_or(Path::new(".")).display());
    println!();
}
