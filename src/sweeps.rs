//! `sweeps run`: successive jobs, dropping tasks that already succeeded.
//! Success means `verifier_result.rewards.reward > 0`.

use crate::config::{JobConfig, TaskRef};
use crate::job::{JobResult, JobStats};
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

/// Completed and errored trials summed over the sweeps' jobs.
pub fn trial_counts<'a>(jobs: impl IntoIterator<Item = &'a JobResult>) -> JobStats {
    let mut counts = JobStats::default();
    for j in jobs {
        counts.n_completed_trials += j.stats.n_completed_trials;
        counts.n_errored_trials += j.stats.n_errored_trials;
    }
    counts
}

/// The jobs of sweep `idx`: one for every remaining task, or one per task when there are per-task hints,
/// each appending its task's hint to the instruction.
fn sweep_jobs(base: &JobConfig, idx: usize, remaining: &[TaskRef], hints: &BTreeMap<String, String>) -> Vec<JobConfig> {
    let job = |job_name: String, tasks: Vec<TaskRef>| JobConfig { job_name, tasks, ..base.clone() };
    if hints.is_empty() {
        return vec![job(format!("{}.sweep-{idx}", base.job_name), remaining.to_vec())];
    }
    remaining
        .iter()
        .map(|t| {
            let mut cfg = job(format!("{}.sweep-{idx}.{}", base.job_name, t.display_name()), vec![t.clone()]);
            cfg.extra_instructions.extend(hints.get(&t.display_name()).cloned());
            cfg
        })
        .collect()
}

/// Runs the sweeps; returns each job's dir and result, in the order they ran.
pub async fn run(
    base: JobConfig,
    max_sweeps: usize,
    trials_per_task: usize,
    hint: Option<String>,
    hints: BTreeMap<String, String>,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<Vec<(PathBuf, JobResult)>> {
    let mut base = base;
    base.n_attempts = trials_per_task;
    base.extra_instructions.extend(hint);
    let mut remaining: Vec<TaskRef> = crate::job::resolve_tasks(&base)?;
    base.datasets.clear();
    let mut jobs = vec![];
    for idx in 1..=max_sweeps {
        if remaining.is_empty() {
            eprintln!("[sweeps] All tasks succeeded; stopping early.");
            break;
        }
        let mut succeeded = BTreeSet::new();
        eprintln!("[sweeps] Starting sweep {idx} with {} tasks, {} trials/task", remaining.len(), base.n_attempts);
        for cfg in sweep_jobs(&base, idx, &remaining, &hints) {
            if *cancel.borrow() {
                return Ok(jobs);
            }
            let result = crate::job::run_job(cfg.clone(), None, &[], cancel.clone()).await?;
            succeeded.extend(succeeded_tasks(&cfg.job_dir()));
            jobs.push((cfg.job_dir(), result));
        }
        let before = remaining.len();
        remaining.retain(|t| !succeeded.contains(&t.display_name()));
        eprintln!("[sweeps] Sweep {idx} complete. Tasks: {before} -> {} remaining", remaining.len());
    }
    Ok(jobs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_extend_the_instruction_not_the_agent_kwargs() {
        let base = JobConfig {
            job_name: "j".into(),
            agents: vec![crate::config::AgentConfig { name: Some("claude-code".into()), ..Default::default() }],
            extra_instructions: vec!["generic hint".into()],
            ..Default::default()
        };
        let tasks = vec![TaskRef::local("/t/a".into(), None), TaskRef::local("/t/b".into(), None)];
        let hints: BTreeMap<String, String> = [("a".to_string(), "hint for a".to_string())].into();
        let jobs = sweep_jobs(&base, 2, &tasks, &hints);
        let names: Vec<&str> = jobs.iter().map(|j| j.job_name.as_str()).collect();
        assert_eq!(names, ["j.sweep-2.a", "j.sweep-2.b"]);
        assert_eq!(jobs[0].extra_instructions, ["generic hint", "hint for a"]);
        assert_eq!(jobs[1].extra_instructions, ["generic hint"]);
        for j in &jobs {
            assert!(crate::agents::check_kwargs(j.agents[0].name(), &j.agents[0].kwargs).is_ok());
        }
        let jobs = sweep_jobs(&base, 1, &tasks, &BTreeMap::new());
        assert_eq!((jobs.len(), jobs[0].job_name.as_str(), jobs[0].tasks.len()), (1, "j.sweep-1", 2));
    }
}
