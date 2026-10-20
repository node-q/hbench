//! Regrade: re-run (separate) verification against a recorded trial's outputs
//! without re-running the agent.

use crate::artifacts::{ManifestEntry, host_path};
use crate::env::{ErrorKind, kind_err};
use crate::task::{Artifact, Task, VerifierMode, with_convention};
use crate::trial::{ExceptionInfo, StepResult, TimingInfo, Trial, TrialResult, VerifierResult, aggregate_step_rewards};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

fn err(msg: impl Into<String>) -> anyhow::Error {
    kind_err(ErrorKind::Regrade, msg)
}

/// None if every verifier in the task runs in separate mode, else the reason it can't be regraded.
pub fn check_task_regradable(task: &Task) -> Option<String> {
    if task.has_steps() {
        let shared: Vec<&str> =
            task.steps().iter().filter(|s| task.verifier_mode(Some(s)) != VerifierMode::Separate).map(|s| s.name.as_str()).collect();
        if !shared.is_empty() {
            return Some(format!(
                "Task '{}' has steps with shared verifiers ({}); regrade needs separate verifiers",
                task.name(),
                shared.join(", ")
            ));
        }
        None
    } else if task.verifier_mode(None) != VerifierMode::Separate {
        Some(format!("Task '{}' uses a shared verifier; regrade needs [verifier] environment_mode = \"separate\"", task.name()))
    } else {
        None
    }
}

fn read_manifest(artifacts_dir: &Path) -> Result<Vec<ManifestEntry>> {
    let p = artifacts_dir.join("manifest.json");
    if !p.exists() {
        return Ok(vec![]);
    }
    serde_json::from_str(&std::fs::read_to_string(&p)?).map_err(|e| err(format!("unreadable artifact manifest {}: {e}", p.display())))
}

/// The new verifier's declared artifact inputs must be present in the source record.
fn coverage_problems(src_artifacts: &Path, record_dir: &Path, declared: &[Artifact]) -> Result<Vec<String>> {
    let entries = read_manifest(src_artifacts)?;
    let mut problems = vec![];
    for a in with_convention(declared) {
        let key_src = a.source.trim_end_matches('/');
        let cands: Vec<&ManifestEntry> = entries
            .iter()
            .filter(|e| e.service.as_deref().unwrap_or("main") == a.service() && e.source.trim_end_matches('/') == key_src)
            .collect();
        if cands.is_empty() {
            problems.push(format!("{}: never collected (no manifest entry); the verifier would silently grade without it", a.source));
            continue;
        }
        let replay = host_path(src_artifacts, &a);
        let dest = replay.strip_prefix(record_dir).map(|p| p.display().to_string()).unwrap_or_default();
        let Some(e) = cands.iter().rev().find(|c| c.destination == dest) else {
            problems.push(format!("{}: recorded at a different destination than the new declaration ({dest})", a.source));
            continue;
        };
        match e.status.as_str() {
            "failed" => problems.push(format!("{}: collection failed in the source trial; the record does not contain it", a.source)),
            "skipped" => problems.push(format!("{}: skipped in the source trial (host-path collision)", a.source)),
            "empty" => {}
            _ => {
                if e.kind == "directory" && e.exclude.as_ref().is_some_and(|x| *x != a.exclude) {
                    problems.push(format!(
                        "{}: recorded with exclusion filters {:?}, but the new declaration uses {:?}",
                        a.source, e.exclude, a.exclude
                    ));
                } else if !replay.exists() {
                    problems.push(format!("{}: collected artifact no longer exists on disk", a.source));
                } else if (e.kind == "directory") != replay.is_dir() {
                    problems.push(format!("{}: artifact type no longer matches the recorded {}", a.source, e.kind));
                }
            }
        }
    }
    Ok(problems)
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

fn move_contents(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    if let Ok(rd) = std::fs::read_dir(src) {
        for e in rd.flatten() {
            let t = dst.join(e.file_name());
            if t.exists() {
                if t.is_dir() { std::fs::remove_dir_all(&t)? } else { std::fs::remove_file(&t)? }
            }
            std::fs::rename(e.path(), t)?;
        }
    }
    Ok(())
}

/// A source exception that happened before the source verifier started is agent-phase evidence.
fn source_agent_exception(s: &StepResult) -> Option<ExceptionInfo> {
    let e = s.exception_info.clone()?;
    if s.verifier_result.is_some() {
        return Some(e);
    }
    match &s.verifier {
        None => Some(e),
        Some(t) if t.started_at.is_some_and(|st| e.occurred_at <= st) => Some(e),
        _ => None,
    }
}

/// Regrades `trial` from its `source_trial`: copies the source's agent/ and artifacts/ (per step for a
/// multi-step task), keeps its agent result, and runs only the verifier, which must be separate.
pub async fn run(trial: &mut Trial) -> Result<()> {
    let src_dir: PathBuf =
        trial.cfg.source_trial.as_ref().and_then(|s| s.path.clone()).ok_or_else(|| err("regrade needs source_trial.path"))?;
    let task = trial.task().clone();
    let source: TrialResult = std::fs::read_to_string(src_dir.join("result.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .ok_or_else(|| err(format!("Source trial '{}' has no readable result.json; nothing to regrade.", src_dir.display())))?;
    if trial.cfg.verifier.disable {
        bail!(err("Regrade with verification disabled does nothing."));
    }
    if let Some(e) = check_task_regradable(&task) {
        bail!(err(e));
    }
    if source.task_name != task.name() {
        bail!(err(format!("Task name mismatch: source trial ran task '{}' but the provided task is '{}'.", source.task_name, task.name())));
    }
    let dir = trial.trial_dir();
    let base: Vec<Artifact> = task.config.artifacts.iter().chain(trial.cfg.artifacts.iter()).cloned().collect();
    {
        let r = trial.result_mut();
        r.agent_info = source.agent_info.clone();
        r.agent_result = source.agent_result.clone();
    }

    if !task.has_steps() {
        if src_dir.join("steps").exists() {
            bail!(err("Source trial is multi-step; it cannot be regraded with a single-step task."));
        }
        let problems = coverage_problems(&src_dir.join("artifacts"), &src_dir, &base)?;
        if !problems.is_empty() {
            bail!(err(format!("Task '{}' cannot regrade source trial: {}", task.name(), problems.join("; "))));
        }
        copy_tree(&src_dir.join("agent"), &dir.join("agent"))?;
        copy_tree(&src_dir.join("artifacts"), &dir.join("artifacts"))?;
        let t = TimingInfo { started_at: Some(chrono::Utc::now()), finished_at: None };
        let r = trial.regrade_verify(None, &dir.join("artifacts")).await;
        let res = trial.result_mut();
        res.verifier = Some(TimingInfo { finished_at: Some(chrono::Utc::now()), ..t });
        res.verifier_result = Some(VerifierResult { rewards: Some(r?) });
        return Ok(());
    }

    let src_steps =
        source.step_results.clone().ok_or_else(|| err("Source trial has no step_results; it is not a recorded multi-step trial."))?;
    let expected: Vec<&str> = task.steps().iter().map(|s| s.name.as_str()).collect();
    let recorded: Vec<&str> = src_steps.iter().map(|s| s.step_name.as_str()).collect();
    if expected != recorded {
        bail!(err(format!("Step mismatch: the record contains {recorded:?} but the provided task defines {expected:?}.")));
    }
    let mut problems = vec![];
    for s in task.steps() {
        let mut decl = base.clone();
        decl.extend(s.artifacts.clone());
        let sd = src_dir.join("steps").join(&s.name);
        problems.extend(coverage_problems(&sd.join("artifacts"), &sd, &decl)?.into_iter().map(|p| format!("step '{}': {p}", s.name)));
    }
    if !problems.is_empty() {
        bail!(err(format!("Task '{}' cannot regrade source trial: {}", task.name(), problems.join("; "))));
    }
    let mut results = vec![];
    let mut failures = vec![];
    for (s, src) in task.steps().iter().zip(src_steps.iter()) {
        let sd = dir.join("steps").join(&s.name);
        copy_tree(&src_dir.join("steps").join(&s.name).join("agent"), &sd.join("agent"))?;
        copy_tree(&src_dir.join("steps").join(&s.name).join("artifacts"), &sd.join("artifacts"))?;
        std::fs::create_dir_all(sd.join("verifier"))?;
        let mut sr = StepResult {
            step_name: s.name.clone(),
            agent_result: src.agent_result.clone(),
            verifier_result: None,
            exception_info: None,
            agent_execution: src.agent_execution.clone(),
            verifier: None,
        };
        let t0 = chrono::Utc::now();
        match trial.regrade_verify(Some(s), &sd.join("artifacts")).await {
            Ok(r) => {
                sr.verifier_result = Some(VerifierResult { rewards: Some(r) });
                sr.exception_info = source_agent_exception(src);
            }
            Err(e) => {
                sr.exception_info = Some(ExceptionInfo::from(&e));
                failures.push(s.name.clone());
            }
        }
        sr.verifier = Some(TimingInfo { started_at: Some(t0), finished_at: Some(chrono::Utc::now()) });
        move_contents(&dir.join("verifier"), &sd.join("verifier"))?;
        results.push(sr);
    }
    let res = trial.result_mut();
    if failures.is_empty() {
        res.verifier_result = match task.config.multi_step_reward_strategy {
            Some(crate::task::RewardStrategy::Final) => results.last().and_then(|r| r.verifier_result.clone()),
            _ => aggregate_step_rewards(&results),
        };
        res.step_results = Some(results);
        Ok(())
    } else {
        res.step_results = Some(results);
        Err(err(format!("Fresh verification failed for regrade step(s) {}; no trial reward was produced.", failures.join(", "))))
    }
}
