//! The reproducible packer. A task's layer depends only on its files' paths, contents, exec bits and symlink
//! targets: entries sorted by path, mode 0755 or 0644, uid, gid and mtime 0, no user or group names, and gzip
//! with no file name and mtime 0. A dataset is written as an OCI image layout, `<dir>/<name>`, tagged with its
//! version.

use super::Sha256Writer;
use super::format::{
    self, ARTIFACT_TYPE, CONFIG_MEDIA_TYPE, CREATED, ConfigBlob, INDEX_MEDIA_TYPE, LAYER_MEDIA_TYPE, LICENSES, MANIFEST_MEDIA_TYPE,
    PACKAGE_DIGEST, REF_NAME, REVISION, SOURCE, TITLE, TaskEntry, Timeouts, Upstream, VERSION,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const GZIP_OS_UNIX: u8 = 3;
const LAYOUT_VERSION: &str = "1.0.0";

enum Kind {
    Dir,
    File { executable: bool, len: u64 },
    Symlink(PathBuf),
}

/// What a dataset's config and manifest say about it beyond its tasks.
#[derive(Debug, Clone, Default)]
pub struct DatasetInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub license: String,
    pub upstream: Option<Upstream>,
    pub canary: Option<String>,
    pub created: Option<String>,
}

/// Every entry under `dir` as (`/`-separated relative path, kind), sorted by path.
fn entries(dir: &Path) -> Result<Vec<(String, Kind)>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Kind)>) -> Result<()> {
        for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let p = e?.path();
            let rel = p.strip_prefix(root)?.to_str().with_context(|| format!("{}: not a UTF-8 path", p.display()))?.to_string();
            let meta = p.symlink_metadata()?;
            let kind = if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&p)?;
                if !super::unpack::stays_inside(Path::new(&rel), &target) {
                    bail!("{}: symlink to {} leaves the task directory", p.display(), target.display());
                }
                Kind::Symlink(target)
            } else if meta.is_dir() {
                walk(root, &p, out)?;
                Kind::Dir
            } else if meta.is_file() {
                Kind::File { executable: meta.permissions().mode() & 0o111 != 0, len: meta.len() }
            } else {
                bail!("{}: only files, directories and symlinks can be packed", p.display());
            };
            out.push((rel, kind));
        }
        Ok(())
    }
    let mut out = vec![];
    walk(dir, dir, &mut out)?;
    super::unpack::check_symlinks(dir, dir)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Write the task at `dir` to `out` as a reproducible tar+gzip layer.
pub fn pack_task(dir: &Path, out: impl Write) -> Result<()> {
    let gz = flate2::GzBuilder::new().mtime(0).operating_system(GZIP_OS_UNIX).write(out, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    for (rel, kind) in entries(dir)? {
        let mut h = tar::Header::new_gnu();
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_username("")?;
        h.set_groupname("")?;
        h.set_size(0);
        match kind {
            Kind::Dir => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(0o755);
                tar.append_data(&mut h, &rel, std::io::empty())?;
            }
            Kind::File { executable, len } => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(if executable { 0o755 } else { 0o644 });
                h.set_size(len);
                let file = std::fs::File::open(dir.join(&rel)).with_context(|| format!("reading {rel}"))?;
                tar.append_data(&mut h, &rel, std::io::Read::take(file, len))?;
            }
            Kind::Symlink(target) => {
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_mode(0o777);
                tar.append_link(&mut h, &rel, target)?;
            }
        }
    }
    tar.into_inner()?.finish()?.flush()?;
    Ok(())
}

/// Write `contents` into `blobs` under its digest; (digest, size).
fn write_blob(blobs: &Path, contents: impl FnOnce(&mut Sha256Writer<std::fs::File>) -> Result<()>) -> Result<(String, u64)> {
    let tmp = blobs.join(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
    let mut w = Sha256Writer::new(std::fs::File::create(&tmp)?);
    let written = contents(&mut w).and_then(|_| Ok(w.finish()?));
    let (digest, size) = match written {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    std::fs::rename(&tmp, blobs.join(digest.trim_start_matches("sha256:")))?;
    Ok((digest, size))
}

fn task_entry(dir: &Path, name: &str, layer_digest: String) -> Result<TaskEntry> {
    let task = crate::task::Task::load(dir)?;
    let package_digest = crate::task::package_digest(dir);
    if package_digest.is_empty() {
        bail!("{}: no package digest", dir.display());
    }
    let docker_image =
        task.config.environment.docker_image.clone().filter(|i| i.split_once('@').is_some_and(|(_, d)| format::is_sha256_digest(d)));
    let timeouts = Timeouts {
        agent_sec: task.config.agent.timeout_sec,
        verifier_sec: Some(task.config.verifier.timeout_sec.unwrap_or(crate::task::DEFAULT_VERIFIER_TIMEOUT_SEC)),
        build_sec: Some(task.config.environment.build_timeout_sec),
    };
    Ok(TaskEntry { name: name.to_string(), layer_digest, package_digest, docker_image, platforms: None, timeouts: Some(timeouts) })
}

/// Write `tasks` as dataset `info` into the OCI image layout `<root>/<name>`, tagged `<version>` (replacing an
/// earlier manifest with that tag); the manifest's digest.
pub fn write_layout(root: &Path, info: &DatasetInfo, tasks: &[PathBuf]) -> Result<String> {
    let layout = root.join(&info.name);
    let blobs = layout.join("blobs/sha256");
    std::fs::create_dir_all(&blobs)?;
    let (mut entries, mut layers) = (vec![], vec![]);
    for dir in tasks {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        if !format::is_task_name(&name) {
            bail!("{}: {name:?} is not a valid task name", dir.display());
        }
        let (digest, size) = write_blob(&blobs, |w| pack_task(dir, w)).with_context(|| format!("packing {}", dir.display()))?;
        let entry = task_entry(dir, &name, digest.clone())?;
        layers.push(json!({
            "mediaType": LAYER_MEDIA_TYPE, "digest": digest, "size": size,
            "annotations": {TITLE: name, PACKAGE_DIGEST: entry.package_digest},
        }));
        entries.push(entry);
    }
    let config = ConfigBlob {
        name: info.name.clone(),
        version: info.version.clone(),
        description: info.description.clone(),
        license: info.license.clone(),
        upstream: info.upstream.clone(),
        canary: info.canary.clone(),
        tasks: entries,
    };
    let config_bytes = serde_json::to_vec(&config)?;
    let (config_digest, config_size) = write_blob(&blobs, |w| Ok(w.write_all(&config_bytes)?))?;
    let mut annotations = serde_json::Map::new();
    annotations.insert(VERSION.into(), info.version.clone().into());
    annotations.insert(LICENSES.into(), info.license.clone().into());
    if let Some(u) = &info.upstream {
        annotations.insert(SOURCE.into(), u.url.clone().into());
        if let Some(r) = &u.revision {
            annotations.insert(REVISION.into(), r.clone().into());
        }
    }
    if let Some(c) = &info.created {
        annotations.insert(CREATED.into(), c.clone().into());
    }
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST_MEDIA_TYPE,
        "artifactType": ARTIFACT_TYPE,
        "config": {"mediaType": CONFIG_MEDIA_TYPE, "digest": config_digest, "size": config_size},
        "layers": layers,
        "annotations": annotations,
    });
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    format::Dataset::new(&format::parse_manifest(&manifest_bytes)?, &config_bytes).context("the packed dataset does not verify")?;
    let (digest, size) = write_blob(&blobs, |w| Ok(w.write_all(&manifest_bytes)?))?;
    let index_path = layout.join("index.json");
    let mut manifests: Vec<Value> = match std::fs::read_to_string(&index_path) {
        Ok(s) => serde_json::from_str::<Value>(&s)?["manifests"].as_array().cloned().unwrap_or_default(),
        Err(_) => vec![],
    };
    manifests.retain(|m| m["annotations"][REF_NAME] != info.version.as_str());
    manifests.push(json!({
        "mediaType": MANIFEST_MEDIA_TYPE, "artifactType": ARTIFACT_TYPE, "digest": digest, "size": size,
        "annotations": {REF_NAME: info.version},
    }));
    manifests.sort_by(|a, b| a["annotations"][REF_NAME].as_str().cmp(&b["annotations"][REF_NAME].as_str()));
    let index = json!({"schemaVersion": 2, "mediaType": INDEX_MEDIA_TYPE, "manifests": manifests});
    std::fs::write(layout.join("oci-layout"), json!({"imageLayoutVersion": LAYOUT_VERSION}).to_string())?;
    crate::trial::write_atomic(&index_path, serde_json::to_vec_pretty(&index)?)?;
    Ok(digest)
}
