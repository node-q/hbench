//! `sweeps run`: successive jobs, dropping tasks that already succeeded.
//! Success means `verifier_result.rewards.reward > 0`.

use crate::config::{JobConfig, TaskRef};
use anyhow::Result;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub fn succeeded_tasks(job_dir: &Path) -> BTreeSet<String> {
    let mut ok = BTreeSet::new();
    for e in std::fs::read_dir(job_dir).into_iter().flatten().flatten() {
        let Some(v) = std::fs::read_to_string(e.path().join("result.json")).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok())
        else {
            continue;
        };
        if v["verifier_result"]["rewards"]["reward"].as_f64().is_some_and(|r| r > 0.0) {
            // Match by task dir name (the name sweeps track tasks by).
            if let Some(p) = v["config"]["task"]["path"].as_str() {
                ok.insert(Path::new(p).file_name().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    ok
}

pub async fn run(
    base: JobConfig,
    max_sweeps: usize,
    trials_per_task: usize,
    hint: Option<String>,
    hints: BTreeMap<String, String>,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<Vec<PathBuf>> {
    let mut base = base;
    base.n_attempts = trials_per_task;
    if let Some(h) = hint {
        for a in base.agents.iter_mut() {
            a.kwargs.insert("hint".into(), Value::String(h.clone()));
        }
    }
    let mut remaining: Vec<TaskRef> = crate::job::resolve_tasks(&base)?;
    base.datasets.clear();
    let mut job_dirs = vec![];
    for idx in 1..=max_sweeps {
        if remaining.is_empty() {
            eprintln!("[sweeps] All tasks succeeded; stopping early.");
            break;
        }
        let mut succeeded = BTreeSet::new();
        let groups: Vec<(String, Vec<TaskRef>)> = if hints.is_empty() {
            vec![(format!("{}.sweep-{idx}", base.job_name), remaining.clone())]
        } else {
            remaining.iter().map(|t| (format!("{}.sweep-{idx}.{}", base.job_name, t.display_name()), vec![t.clone()])).collect()
        };
        eprintln!("[sweeps] Starting sweep {idx} with {} tasks, {} trials/task", remaining.len(), base.n_attempts);
        for (name, tasks) in groups {
            if *cancel.borrow() {
                return Ok(job_dirs);
            }
            let mut cfg = base.clone();
            cfg.job_name = name;
            if let Some(h) = tasks.first().and_then(|t| hints.get(&t.display_name())) {
                for a in cfg.agents.iter_mut() {
                    a.kwargs.insert("hint".into(), Value::String(h.clone()));
                }
            }
            cfg.tasks = tasks;
            crate::job::run_job(cfg.clone(), None, &[], cancel.clone()).await?;
            succeeded.extend(succeeded_tasks(&cfg.job_dir()));
            job_dirs.push(cfg.job_dir());
        }
        let before = remaining.len();
        remaining.retain(|t| !succeeded.contains(&t.display_name()));
        eprintln!("[sweeps] Sweep {idx} complete. Tasks: {before} -> {} remaining", remaining.len());
    }
    Ok(job_dirs)
}
