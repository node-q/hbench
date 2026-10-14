//! `run --diff <source job>`: plan reuse / regrade / rerun against earlier jobs (upstream's job_diff).

use crate::config::{SourceTrial, TrialConfig};
use crate::trial::{TrialResult, trial_lock};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    Reuse,
    Regrade,
    Rerun,
}

pub struct Source {
    pub dir: PathBuf,
    pub result: TrialResult,
    pub lock: Value,
}

impl Source {
    fn version(&self) -> Option<&str> {
        self.lock["task"]["version"].as_str()
    }
    fn completed_successfully(&self) -> bool {
        let r = &self.result;
        if r.finished_at.is_none() || r.exception_info.is_some() {
            return false;
        }
        if r.step_results.iter().flatten().any(|s| s.exception_info.is_some()) {
            return false;
        }
        self.lock["verifier"]["disable"] == true || r.reward().is_some()
    }
}

pub struct TrialDiff {
    pub action: Action,
    pub target_trial_name: String,
    pub task_name: String,
    pub source: Option<usize>,
    pub reason: String,
    pub downgraded: bool,
}

pub fn load_sources(job_dirs: &[PathBuf]) -> Result<Vec<Source>> {
    let mut out = vec![];
    for jd in job_dirs {
        if !jd.join("config.json").exists() {
            bail!("{} is not a job directory (missing config.json).", jd.display());
        }
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(jd)?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for d in dirs {
            if !d.join("result.json").exists() {
                continue;
            }
            let lock = std::fs::read_to_string(d.join("lock.json"))
                .with_context(|| format!("Source trial '{}' has a result.json but no lock.json.", d.display()))?;
            let result = serde_json::from_str(&std::fs::read_to_string(d.join("result.json"))?)
                .with_context(|| format!("unreadable result.json in {}", d.display()))?;
            out.push(Source { dir: d.canonicalize()?, result, lock: serde_json::from_str(&lock)? });
        }
    }
    Ok(out)
}

type Semver = (u64, u64, u64, String);

fn semver(v: &str) -> Result<Semver> {
    let (core, rest) = match v.find(['-', '+']) {
        Some(i) => (&v[..i], &v[i..]),
        None => (v, ""),
    };
    let p: Vec<&str> = core.split('.').collect();
    let n = |s: &str| s.parse::<u64>().map_err(|_| anyhow::anyhow!("Invalid semantic version {v:?}"));
    if p.len() != 3 {
        bail!("Invalid semantic version {v:?}");
    }
    Ok((n(p[0])?, n(p[1])?, n(p[2])?, rest.to_string()))
}

/// Same version: reuse; patch bump: reuse; minor: regrade; major or pre-release/build: rerun.
pub fn version_action(source: &str, target: &str) -> Result<Action> {
    if source == target {
        return Ok(Action::Reuse);
    }
    let (s, t) = (semver(source)?, semver(target)?);
    Ok(if s.0 != t.0 {
        Action::Rerun
    } else if s.1 != t.1 {
        Action::Regrade
    } else if s.2 != t.2 {
        Action::Reuse
    } else {
        Action::Rerun
    })
}

fn without_task(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("task");
        o.remove("source_trial");
    }
    v
}

pub fn compare(targets: &[TrialConfig], sources: &[Source]) -> Result<Vec<TrialDiff>> {
    let locks: Vec<Value> = targets.iter().map(trial_lock).collect();
    for (c, l) in targets.iter().zip(&locks) {
        let name = l["task"]["name"].as_str().unwrap_or_default();
        if let Some(v) = l["task"]["version"].as_str() {
            semver(v)?;
            for s in sources.iter().filter(|s| s.result.task_name == name && s.version() == Some(v)) {
                if s.lock["task"]["digest"] != l["task"]["digest"] {
                    bail!("Task '{name}' has version {v} in both source and target locks, but the digests differ.");
                }
            }
        }
        let _ = c;
    }
    let mut consumed: HashSet<usize> = HashSet::new();
    let mut regradable: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
    let mut out = vec![];
    for (c, l) in targets.iter().zip(&locks) {
        let task_name = l["task"]["name"].as_str().unwrap_or_default().to_string();
        let rerun = |reason: String, downgraded: bool| TrialDiff {
            action: Action::Rerun,
            target_trial_name: c.trial_name.clone(),
            task_name: task_name.clone(),
            source: None,
            reason,
            downgraded,
        };
        let Some(tv) = l["task"]["version"].as_str() else {
            out.push(rerun("target task has no semantic version".into(), false));
            continue;
        };
        let task_sources: Vec<usize> = (0..sources.len())
            .filter(|&i| sources[i].result.task_name == task_name && sources[i].version().is_some() && !consumed.contains(&i))
            .collect();
        let compatible: Vec<usize> = task_sources.iter().copied().filter(|&i| without_task(&sources[i].lock) == without_task(l)).collect();
        let mut cands: Vec<(Action, bool, usize)> = vec![];
        for &i in compatible.iter().filter(|&&i| sources[i].completed_successfully()) {
            let sv = sources[i].version().unwrap();
            cands.push((version_action(sv, tv)?, sv != tv, i));
        }
        cands.sort();
        let Some(&(action, _, i)) = cands.first() else {
            let reason = if !compatible.is_empty() {
                "source trials failed or are incomplete"
            } else if !task_sources.is_empty() {
                "non-task trial inputs differ"
            } else {
                "no matching source trial"
            };
            out.push(rerun(reason.into(), false));
            continue;
        };
        if action == Action::Rerun {
            out.push(rerun(format!("major or pre-release version change {} -> {tv}", sources[i].version().unwrap()), false));
            continue;
        }
        if action == Action::Regrade {
            let dir = crate::registry::local_path(&c.task);
            let why = regradable
                .entry(dir.clone())
                .or_insert_with(|| crate::task::Task::load(&dir).ok().and_then(|t| crate::regrade::check_task_regradable(&t)))
                .clone();
            if let Some(why) = why {
                out.push(rerun(why, true));
                continue;
            }
        }
        consumed.insert(i);
        let sv = sources[i].version().unwrap();
        out.push(TrialDiff {
            action,
            target_trial_name: c.trial_name.clone(),
            task_name,
            source: Some(i),
            reason: if sv == tv { "same task version".into() } else { format!("task version {sv} -> {tv}") },
            downgraded: false,
        });
    }
    Ok(out)
}

/// Trials to actually run: regrades (seeded from the source) and reruns; reuses are skipped.
pub fn trial_configs(targets: Vec<TrialConfig>, diffs: &[TrialDiff], sources: &[Source]) -> Vec<TrialConfig> {
    targets
        .into_iter()
        .zip(diffs)
        .filter_map(|(mut c, d)| match d.action {
            Action::Reuse => None,
            Action::Rerun => Some(c),
            Action::Regrade => {
                let s = &sources[d.source.unwrap()];
                c.source_trial = Some(SourceTrial {
                    action: "regrade".into(),
                    kind: "local".into(),
                    trial_id: Some(s.result.id.to_string()),
                    path: Some(s.dir.clone()),
                });
                Some(c)
            }
        })
        .collect()
}

pub fn print_preview(diffs: &[TrialDiff], sources: &[Source]) {
    let count = |a: Action| diffs.iter().filter(|d| d.action == a).count();
    println!(
        "Reuse: {}/{n}  Regrade: {}/{n}  Rerun: {}/{n}",
        count(Action::Reuse),
        count(Action::Regrade),
        count(Action::Rerun),
        n = diffs.len()
    );
    for d in diffs {
        let src = d.source.map(|i| sources[i].dir.file_name().unwrap().to_string_lossy().into_owned()).unwrap_or_default();
        println!("  {:<8} {:<40} {:<20} {src} ({})", format!("{:?}", d.action).to_lowercase(), d.task_name, d.target_trial_name, d.reason);
    }
    let down = diffs.iter().filter(|d| d.downgraded).count();
    if down > 0 {
        println!("{down} trial(s) had minor task changes but could not be regraded, so they must be rerun.");
    }
}

pub fn source_dirs(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    paths.iter().map(|p| Path::new(p).canonicalize().with_context(|| format!("source job {}", p.display()))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_actions() {
        assert_eq!(version_action("1.2.3", "1.2.3").unwrap(), Action::Reuse);
        assert_eq!(version_action("1.2.3", "1.2.4").unwrap(), Action::Reuse);
        assert_eq!(version_action("1.2.3", "1.3.0").unwrap(), Action::Regrade);
        assert_eq!(version_action("1.2.3", "2.0.0").unwrap(), Action::Rerun);
        assert_eq!(version_action("1.2.3", "1.2.3-rc1").unwrap(), Action::Rerun);
        assert!(version_action("1.2", "1.2.3").is_err());
    }
}
