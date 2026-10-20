//! A job's manifest.json: the SHA-256 of the job's config, lock, result and log, and of every file its
//! trials wrote, so an edit made after the run shows (a changed reward or model name, a trial removed
//! or added). It is written last, after the job's files are scrubbed of secrets.
//!
//! A manifest shows the files are the ones this build wrote; it cannot show the run was real, since
//! anyone can rebuild hbench.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const FILE: &str = "manifest.json";

/// The job-level files a manifest covers; others in the job dir (analysis.json, check_report.json)
/// are written by later commands.
const JOB_FILES: &[&str] = &["config.json", "lock.json", "result.json", "job.log", "trials.json"];

/// Written into a trial dir after the run, by `hbench analyze`.
const WRITTEN_LATER: &[&str] = &["analysis.json", "analysis.md"];

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub job_id: uuid::Uuid,
    pub created_at: DateTime<Utc>,
    pub hbench: serde_json::Value,
    /// Each file's path in the job dir (`/`-separated) to `sha256:<hex>`; a symlink to `link:<target>`.
    pub files: BTreeMap<String, String>,
}

/// Write `job_dir/manifest.json` for the files there now.
pub fn write(job_dir: &Path, job_id: uuid::Uuid) -> Result<()> {
    let manifest =
        Manifest { schema_version: 1, job_id, created_at: Utc::now(), hbench: crate::build_info::json(), files: digests(job_dir)? };
    crate::trial::write_atomic(&job_dir.join(FILE), serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("writing {}", job_dir.join(FILE).display()))
}

/// What differs between `job_dir` and its manifest: files changed, missing, or not in it. Empty when
/// they match.
pub fn verify(job_dir: &Path) -> Result<Vec<String>> {
    let path = job_dir.join(FILE);
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: Manifest = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let now = digests(job_dir)?;
    let mut problems = vec![];
    for (file, digest) in &manifest.files {
        match now.get(file) {
            None => problems.push(format!("missing: {file}")),
            Some(d) if d != digest => problems.push(format!("changed: {file}")),
            Some(_) => {}
        }
    }
    problems.extend(now.keys().filter(|f| !manifest.files.contains_key(*f)).map(|f| format!("not in the manifest: {f}")));
    Ok(problems)
}

/// The digest of every file the manifest covers in `job_dir`.
fn digests(job_dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = vec![];
    for name in JOB_FILES {
        let p = job_dir.join(name);
        if p.symlink_metadata().is_ok() {
            files.push(p);
        }
    }
    let mut entries: Vec<PathBuf> =
        std::fs::read_dir(job_dir).with_context(|| format!("reading {}", job_dir.display()))?.flatten().map(|e| e.path()).collect();
    entries.sort();
    for trial in entries.iter().filter(|p| p.is_dir() && p.join("config.json").exists()) {
        walk(trial, trial, &mut files)?;
    }
    let mut out = BTreeMap::new();
    for f in files {
        let rel = f.strip_prefix(job_dir).unwrap_or(&f).to_string_lossy().replace('\\', "/");
        out.insert(rel, digest(&f)?);
    }
    Ok(out)
}

fn walk(trial: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<PathBuf> =
        std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let meta = p.symlink_metadata().with_context(|| format!("reading {}", p.display()))?;
        if meta.is_dir() {
            walk(trial, &p, out)?;
        } else if !(dir == trial && WRITTEN_LATER.iter().any(|n| p.file_name().is_some_and(|f| f == *n))) {
            out.push(p);
        }
    }
    Ok(())
}

fn digest(p: &Path) -> Result<String> {
    let meta = p.symlink_metadata().with_context(|| format!("reading {}", p.display()))?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(p).with_context(|| format!("reading {}", p.display()))?;
        return Ok(format!("link:{}", target.to_string_lossy()));
    }
    let mut f = std::fs::File::open(p).with_context(|| format!("reading {}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).with_context(|| format!("reading {}", p.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("sha256:{}", crate::task::hex(&h.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> PathBuf {
        let d = std::env::temp_dir().join(format!("hbench-manifest-{}", uuid::Uuid::new_v4()));
        for (path, text) in [
            ("config.json", "{}"),
            ("result.json", r#"{"id": "j"}"#),
            ("job.log", "job started\n"),
            ("task-a__1/config.json", "{}"),
            ("task-a__1/result.json", r#"{"verifier_result": {"rewards": {"reward": 0.0}}}"#),
            ("task-a__1/agent/trajectory.json", "{}"),
        ] {
            let p = d.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        d
    }

    #[test]
    fn a_fresh_manifest_matches() {
        let d = job();
        write(&d, uuid::Uuid::new_v4()).unwrap();
        assert_eq!(verify(&d).unwrap(), Vec::<String>::new());
        let m: Manifest = serde_json::from_str(&std::fs::read_to_string(d.join(FILE)).unwrap()).unwrap();
        assert_eq!(
            m.files.keys().collect::<Vec<_>>(),
            ["config.json", "job.log", "result.json", "task-a__1/agent/trajectory.json", "task-a__1/config.json", "task-a__1/result.json"]
        );
        assert_eq!(m.hbench["results_epoch"], crate::build_info::RESULTS_EPOCH);
    }

    #[test]
    fn edits_after_the_run_show() {
        let d = job();
        write(&d, uuid::Uuid::new_v4()).unwrap();
        std::fs::write(d.join("task-a__1/result.json"), r#"{"verifier_result": {"rewards": {"reward": 1.0}}}"#).unwrap();
        std::fs::remove_file(d.join("task-a__1/agent/trajectory.json")).unwrap();
        std::fs::create_dir_all(d.join("task-b__1")).unwrap();
        std::fs::write(d.join("task-b__1/config.json"), "{}").unwrap();
        assert_eq!(
            verify(&d).unwrap(),
            ["missing: task-a__1/agent/trajectory.json", "changed: task-a__1/result.json", "not in the manifest: task-b__1/config.json"]
        );
    }

    #[test]
    fn files_written_later_are_not_covered() {
        let d = job();
        write(&d, uuid::Uuid::new_v4()).unwrap();
        std::fs::write(d.join("analysis.json"), "{}").unwrap();
        std::fs::write(d.join("task-a__1/analysis.json"), "{}").unwrap();
        std::fs::write(d.join("task-a__1/analysis.md"), "#").unwrap();
        assert_eq!(verify(&d).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn a_job_without_a_manifest_cannot_be_verified() {
        let err = verify(&job()).unwrap_err();
        assert!(format!("{err:#}").contains("manifest.json"), "{err:#}");
    }
}
