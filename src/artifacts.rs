//! Artifact collection + re-materialization (port of upstream's ArtifactHandler).
//!
//! Host layout under the trial's `artifacts/` dir: source-derived entries mirror
//! their absolute path (`/var/log/x` -> `artifacts/var/log/x`, so the agent's
//! `/logs/artifacts` lands at `artifacts/logs/artifacts/`); entries with a
//! destination land there; `manifest.json` records every attempt.

use crate::env::DockerEnv;
use crate::task::{ArtifactSpec, MAIN};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestEntry {
    pub source: String,
    pub destination: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub service: Option<String>,
    pub exclude: Option<Vec<String>>,
}

fn rel(p: &str) -> PathBuf {
    Path::new(p).components().filter_map(|c| if let Component::Normal(s) = c { Some(s) } else { None }).collect()
}

pub fn host_path(artifacts_dir: &Path, a: &ArtifactSpec) -> PathBuf {
    match &a.destination {
        Some(d) => artifacts_dir.join(rel(d)),
        None => artifacts_dir.join(rel(&a.source)),
    }
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

/// Collection pass state: host paths claimed so far (first claimant wins).
#[derive(Default)]
pub struct Claims(Vec<(PathBuf, String)>);

/// Best-effort download of `specs` (restricted to `services` if given); appends to manifest.json.
pub async fn download(
    env: &DockerEnv,
    artifacts_dir: &Path,
    specs: &[ArtifactSpec],
    services: &[&str],
    claims: &mut Claims,
) -> Result<Vec<ManifestEntry>> {
    std::fs::create_dir_all(artifacts_dir)?;
    let mut entries = vec![];
    for a in specs.iter().filter(|a| services.contains(&a.service())) {
        let target = host_path(artifacts_dir, a);
        let dest = match target.strip_prefix(artifacts_dir) {
            Ok(r) if r.as_os_str().is_empty() => "artifacts".to_string(),
            Ok(r) => format!("artifacts/{}", r.display()),
            Err(_) => "artifacts".into(),
        };
        let mut entry = ManifestEntry {
            source: a.source.clone(),
            destination: dest,
            kind: String::new(),
            status: String::new(),
            service: a.service.clone(),
            exclude: None,
        };
        if claims.0.iter().any(|(t, s)| *s != a.source && overlaps(t, &target)) {
            entry.kind = if Path::new(&a.source).extension().is_some() { "file" } else { "directory" }.into();
            entry.status = "skipped".into();
            entry.exclude = Some(if entry.kind == "directory" { a.exclude.clone() } else { vec![] });
            entries.push(entry);
            continue;
        }
        claims.0.push((target.clone(), a.source.clone()));
        let is_dir = match env.is_dir(a.service(), &a.source).await {
            Ok(d) => d,
            Err(_) => Path::new(&a.source).extension().is_none(),
        };
        let res = if is_dir {
            if a.exclude.is_empty() {
                env.download_dir(a.service(), &a.source, &target).await
            } else {
                env.download_dir_excluding(a.service(), &a.source, &target, &a.exclude).await
            }
        } else {
            env.download_file(a.service(), &a.source, &target).await
        };
        entry.kind = if is_dir { "directory" } else { "file" }.into();
        entry.exclude = Some(if is_dir { a.exclude.clone() } else { vec![] });
        entry.status = if res.is_ok() { "ok" } else { "failed" }.into();
        // upstream's mounted convention dir is recorded "empty" when nothing was published.
        if a.is_convention()
            && a.destination.is_none()
            && a.exclude.is_empty()
            && res.is_ok()
            && std::fs::read_dir(&target).map(|mut d| d.next().is_none()).unwrap_or(true)
        {
            entry.status = "empty".into();
        }
        entries.push(entry);
    }
    let path = artifacts_dir.join("manifest.json");
    let mut all: Vec<ManifestEntry> = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    all.extend(entries.clone());
    if !all.is_empty() {
        std::fs::write(&path, serde_json::to_string_pretty(&all)?)?;
    }
    Ok(entries)
}

/// Re-create collected artifacts at their original source paths in a verifier env.
pub async fn upload(target: &DockerEnv, artifacts_dir: &Path, specs: &[ArtifactSpec]) -> Result<()> {
    for a in specs {
        let host = host_path(artifacts_dir, a);
        if !host.exists() {
            continue;
        }
        if host.is_dir() {
            target.empty_dirs(&[a.source.as_str()], true).await?;
            target.upload_dir(&host, &a.source).await?;
        } else {
            target.upload_file(&host, &a.source).await?;
        }
    }
    Ok(())
}

pub fn sidecar_services(specs: &[ArtifactSpec]) -> Vec<String> {
    let mut v: Vec<String> = specs.iter().map(|a| a.service().to_string()).filter(|s| s != MAIN).collect();
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_paths() {
        let d = Path::new("/t/artifacts");
        let s = |src: &str, dst: Option<&str>| ArtifactSpec {
            source: src.into(),
            destination: dst.map(String::from),
            exclude: vec![],
            service: None,
        };
        assert_eq!(host_path(d, &s("/logs/artifacts", None)), Path::new("/t/artifacts/logs/artifacts"));
        assert_eq!(host_path(d, &s("/app/hello.txt", Some("ws/h.txt"))), Path::new("/t/artifacts/ws/h.txt"));
        assert!(overlaps(Path::new("/a/b"), Path::new("/a")) && !overlaps(Path::new("/a/b"), Path::new("/a/c")));
    }
}
