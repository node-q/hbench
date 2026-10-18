//! Dataset registry (registry.json) + git-backed task download.

use crate::config::{DatasetConfig, TaskRef};
use crate::task::{Task, hex};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const DEFAULT_REGISTRY_URL: &str = "https://raw.githubusercontent.com/node-q/hbench-datasets/main/registry.json";

#[derive(Debug, Deserialize)]
pub struct Dataset {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub tasks: Vec<RegistryTask>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegistryTask {
    pub name: String,
    #[serde(default)]
    pub git_url: Option<String>,
    #[serde(default)]
    pub git_commit_id: Option<String>,
    pub path: String,
}

pub fn load(url: Option<&str>, path: Option<&Path>) -> Result<Vec<Dataset>> {
    let raw = match path {
        Some(p) => std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
        None => {
            let url = url.unwrap_or(DEFAULT_REGISTRY_URL);
            // Curl instead of an HTTP client crate.
            let out = Command::new("curl").args(["-fsSL", "--max-time", "60", url]).output().context("running curl")?;
            if !out.status.success() {
                bail!("fetching registry {url}: {}", String::from_utf8_lossy(&out.stderr));
            }
            String::from_utf8(out.stdout)?
        }
    };
    serde_json::from_str(&raw).context("parsing registry json")
}

/// `name@version` / name + version, or `name` when the registry has exactly one version of it.
pub fn find<'a>(datasets: &'a [Dataset], name: &str, version: Option<&str>) -> Result<&'a Dataset> {
    let matches: Vec<&Dataset> = datasets.iter().filter(|d| d.name == name && version.is_none_or(|v| d.version == v)).collect();
    match matches.as_slice() {
        [d] => Ok(d),
        [] => bail!(
            "dataset {name:?}{} not found in registry (try `hbench datasets list`)",
            version.map(|v| format!("@{v}")).unwrap_or_default()
        ),
        many => bail!(
            "dataset {name:?} has several versions ({}); use {name}@<version>",
            many.iter().map(|d| d.version.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// fnmatch-style glob (`*`, `?`, `[...]`).
pub fn fnmatch(pat: &str, s: &str) -> bool {
    crate::glob_match(pat, s)
}

pub fn filter_names<T>(items: Vec<T>, name: impl Fn(&T) -> String, d: &DatasetConfig) -> Result<Vec<T>> {
    let all: Vec<String> = items.iter().map(&name).collect();
    let mut v = items;
    if let Some(inc) = d.task_names.as_ref().filter(|i| !i.is_empty()) {
        v.retain(|t| inc.iter().any(|g| fnmatch(g, &name(t))));
        if v.is_empty() {
            let mut sorted = all.clone();
            sorted.sort();
            bail!(
                "No tasks matched the filter(s) {inc:?}. There are {} tasks available in this dataset. Example task names: {:?}",
                all.len(),
                &sorted[..sorted.len().min(5)]
            );
        }
    }
    if let Some(exc) = &d.exclude_task_names {
        v.retain(|t| !exc.iter().any(|g| fnmatch(g, &name(t))));
    }
    if let Some(n) = d.n_tasks {
        v.truncate(n);
    }
    Ok(v)
}

/// Resolve a dataset config into task refs (local dirs, or git refs from a registry).
pub fn dataset_tasks(d: &DatasetConfig, disable_verification: bool) -> Result<Vec<TaskRef>> {
    if let Some(p) = &d.path {
        let p = p.canonicalize().with_context(|| format!("dataset path {}", p.display()))?;
        let source = p.file_name().map(|n| n.to_string_lossy().into_owned());
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&p)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
        dirs.sort();
        // Silently skip dirs that aren't runnable tasks.
        dirs.retain(|t| {
            t.join("environment").is_dir() && Task::load(t).is_ok_and(|task| disable_verification || task.validate_tests().is_ok())
        });
        let dirs = filter_names(dirs, |t| t.file_name().unwrap().to_string_lossy().into_owned(), d)?;
        return Ok(dirs.into_iter().map(|t| TaskRef { path: Some(t), source: source.clone(), ..Default::default() }).collect());
    }
    let name = d.name.as_deref().context("dataset needs path or name")?;
    let regs = load(d.registry_url.as_deref(), d.registry_path.as_deref())?;
    let ds = find(&regs, name, d.version.as_deref())?;
    let tasks = filter_names(ds.tasks.clone(), |t| t.name.clone(), d)?;
    Ok(tasks
        .into_iter()
        .map(|t| TaskRef {
            path: Some(t.path.into()),
            git_url: t.git_url,
            git_commit_id: t.git_commit_id,
            overwrite: d.overwrite,
            download_dir: d.download_dir.clone(),
            source: Some(name.to_string()),
            ..Default::default()
        })
        .collect())
}

fn cache_dir(t: &TaskRef) -> PathBuf {
    let url = t.git_url.as_deref().unwrap_or_default();
    let commit = t.git_commit_id.as_deref().unwrap_or("HEAD");
    let key = hex(&Sha256::digest(format!("{url}@{commit}")))[..16].to_string();
    t.download_dir.clone().unwrap_or_else(|| crate::env::cache_root().join("tasks")).join(key)
}

/// Host path of a task (git tasks resolve into the download cache).
pub fn local_path(t: &TaskRef) -> PathBuf {
    let p = t.path.clone().unwrap_or_default();
    match &t.git_url {
        Some(_) => cache_dir(t).join(p),
        None => p,
    }
}

/// Ceiling for a single git call (the shallow, blob-less fetch is the slow one).
const GIT_TIMEOUT: Duration = Duration::from_secs(600);

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git")?;
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > GIT_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git {} timed out after {}s", args.join(" "), GIT_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if !status.success() {
        let mut err = String::new();
        let _ = child.stderr.take().map(|mut e| e.read_to_string(&mut err));
        bail!("git {} failed: {err}", args.join(" "));
    }
    Ok(())
}

/// Fresh sparse, shallow checkout of `paths` into a temp dir beside `dir`, then swapped into place, so a
/// failed fetch never leaves a half-initialised cache and other jobs never see `dir` half-deleted.
fn fetch(dir: &Path, url: &str, commit: &str, paths: &[String]) -> Result<()> {
    let name = dir.file_name().context("task cache dir has no name")?.to_string_lossy().into_owned();
    let sibling = |tag: &str| dir.with_file_name(format!(".{name}.{tag}-{}", uuid::Uuid::new_v4().simple()));
    let tmp = sibling("tmp");
    std::fs::create_dir_all(&tmp)?;
    let r = (|| {
        git(&tmp, &["init", "-q"])?;
        git(&tmp, &["remote", "add", "origin", url])?;
        let mut sparse = vec!["sparse-checkout", "set", "--no-cone"];
        sparse.extend(paths.iter().map(String::as_str));
        git(&tmp, &sparse)?;
        git(&tmp, &["fetch", "-q", "--depth", "1", "--filter=blob:none", "origin", commit])?;
        git(&tmp, &["checkout", "-q", "FETCH_HEAD"])
    })();
    if r.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
        return r;
    }
    let old = sibling("old");
    let _ = std::fs::rename(dir, &old);
    // Losing a race to a concurrent download is fine: the caller checks the tasks are there.
    if std::fs::rename(&tmp, dir).is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Sparse, shallow checkout of each (repo, commit) group of git tasks.
pub fn download(tasks: &[TaskRef]) -> Result<()> {
    let mut groups: BTreeMap<PathBuf, Vec<&TaskRef>> = BTreeMap::new();
    for t in tasks.iter().filter(|t| t.git_url.is_some()) {
        groups.entry(cache_dir(t)).or_default().push(t);
    }
    let sparse_path = |t: &&TaskRef| format!("/{}/", t.path.as_ref().unwrap().display().to_string().trim_matches('/'));
    for (dir, ts) in groups {
        let url = ts[0].git_url.clone().unwrap();
        let commit = ts[0].git_commit_id.clone().unwrap_or_else(|| "HEAD".into());
        let missing: Vec<String> = ts.iter().filter(|t| !local_path(t).join("task.toml").exists()).map(sparse_path).collect();
        let refetch = commit == "HEAD" || ts.iter().any(|t| t.overwrite);
        // Widen the existing sparse checkout; keeps tasks other jobs may be using.
        let widened = !missing.is_empty() && !refetch && dir.join(".git").exists() && {
            let mut add = vec!["sparse-checkout", "add"];
            add.extend(missing.iter().map(String::as_str));
            git(&dir, &add).is_ok()
        };
        if !widened && (!missing.is_empty() || refetch) {
            // HEAD moves, so it is always refetched; `overwrite` forces a refetch.
            if let Some(p) = dir.parent() {
                std::fs::create_dir_all(p)?;
            }
            eprintln!("downloading {} task(s) from {url}@{commit}", ts.len());
            fetch(&dir, &url, &commit, &ts.iter().map(sparse_path).collect::<Vec<_>>())?;
        }
        for t in ts {
            if !local_path(t).join("task.toml").exists() {
                bail!("task {} missing at {} after download", t.display_name(), local_path(t).display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn failed_fetch_leaves_no_cache() {
        let root = std::env::temp_dir().join(format!("hbench-reg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let missing = root.join("no-such-repo");
        assert!(super::fetch(&root.join("cache"), &missing.to_string_lossy(), "HEAD", &["/t/".into()]).is_err());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
