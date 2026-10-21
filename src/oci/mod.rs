//! Datasets as OCI artifacts, read from a registry (`oci://<registry>/<namespace>`) or from OCI image layouts on
//! disk (`oci-layout:<dir>`), and published to both ([`publish`]). `-d name@version` is the manifest tagged
//! `<version>` in repository `<namespace>/<name>` (in a layout dir, the layout `<dir>/<name>`). Tasks unpack into
//! `<cache>/tasks/sha256/<layer>/<task>` and registry blobs land in `<cache>/blobs/sha256/<digest>`, both checked
//! against their digests.

pub mod credentials;
pub mod descriptor;
pub mod format;
pub mod lockfile;
pub mod pack;
pub mod publish;
mod store;
mod unpack;

#[cfg(test)]
mod tests;

use crate::config::{DatasetConfig, OciTask, TaskRef};
use anyhow::{Context, Result, bail};
use format::{Dataset, Layer};
use futures_util::StreamExt;
use lockfile::LockMode;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use store::Store;

pub const REGISTRY_SCHEME: &str = "oci://";
pub const LAYOUT_SCHEME: &str = "oci-layout:";
const PARALLEL_DOWNLOADS: usize = 8;

/// A parsed `--registry`: `registry` is a registry host, or `oci-layout:<absolute dir>`; datasets are
/// repositories under `namespace`.
#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub registry: String,
    pub namespace: String,
}

fn is_repository_component(s: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*$").unwrap()).is_match(s)
}

/// A repository path: components of lowercase letters and digits, separated by `/`.
pub fn is_repository(s: &str) -> bool {
    s.split('/').all(is_repository_component)
}

fn is_tag(s: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[A-Za-z0-9_][A-Za-z0-9._-]{0,127}$").unwrap()).is_match(s)
}

impl Location {
    pub fn parse(spec: &str) -> Result<Location> {
        if let Some(dir) = spec.strip_prefix(LAYOUT_SCHEME) {
            if dir.is_empty() {
                bail!("{spec}: name a directory, oci-layout:<dir>");
            }
            let dir = std::path::absolute(dir).with_context(|| format!("{spec}: resolving the directory"))?;
            return Ok(Location { registry: format!("{LAYOUT_SCHEME}{}", dir.display()), namespace: String::new() });
        }
        let Some(rest) = spec.strip_prefix(REGISTRY_SCHEME) else {
            bail!(
                "--registry takes oci://<registry>/<namespace> or oci-layout:<dir>, not {spec:?} (a registry.json goes in --registry-url or --registry-path)"
            );
        };
        let (registry, namespace) = rest.trim_end_matches('/').split_once('/').unwrap_or((rest.trim_end_matches('/'), ""));
        let valid_host = !registry.is_empty() && registry.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c));
        if !valid_host {
            bail!("{spec}: {registry:?} is not a registry host");
        }
        if !namespace.is_empty() && !is_repository(namespace) {
            bail!("{spec}: {namespace:?} is not a repository namespace (lowercase letters and digits, separated by . _ - or /)");
        }
        Ok(Location { registry: registry.to_string(), namespace: namespace.to_string() })
    }

    /// The `--registry` form, with a layout dir made absolute.
    pub fn spec(&self) -> String {
        match (self.registry.starts_with(LAYOUT_SCHEME), self.namespace.is_empty()) {
            (true, _) => self.registry.clone(),
            (false, true) => format!("{REGISTRY_SCHEME}{}", self.registry),
            (false, false) => format!("{REGISTRY_SCHEME}{}/{}", self.registry, self.namespace),
        }
    }

    pub fn repository(&self, name: &str) -> Result<String> {
        if !is_repository_component(name) {
            bail!("dataset name {name:?} is not an OCI repository name (lowercase letters and digits, separated by . _ or -)");
        }
        Ok(if self.namespace.is_empty() { name.to_string() } else { format!("{}/{name}", self.namespace) })
    }
}

/// Root of the OCI cache: `download_dir` when a dataset sets one, else `~/.cache/hbench`.
fn cache_root(download_dir: Option<&Path>) -> PathBuf {
    download_dir.map(Path::to_path_buf).unwrap_or_else(crate::env::cache_root)
}

/// Where an OCI task unpacks: `<cache>/tasks/sha256/<layer hex>/<task>`.
pub fn task_dir(t: &TaskRef, oci: &OciTask) -> PathBuf {
    let hex = oci.layer_digest.trim_start_matches("sha256:");
    cache_root(t.download_dir.as_deref()).join("tasks/sha256").join(hex).join(t.path.clone().unwrap_or_default())
}

pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", crate::task::hex(&Sha256::digest(bytes)))
}

pub struct Sha256Reader<R> {
    inner: R,
    hasher: Sha256,
    len: u64,
}

impl<R: Read> Sha256Reader<R> {
    pub fn new(inner: R) -> Self {
        Sha256Reader { inner, hasher: Sha256::new(), len: 0 }
    }
    /// (`sha256:<hex>`, bytes read).
    pub fn finish(self) -> (String, u64) {
        (format!("sha256:{}", crate::task::hex(&self.hasher.finalize())), self.len)
    }
}

impl<R: Read> Read for Sha256Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }
}

pub struct Sha256Writer<W> {
    inner: W,
    hasher: Sha256,
    len: u64,
}

impl<W: Write> Sha256Writer<W> {
    pub fn new(inner: W) -> Self {
        Sha256Writer { inner, hasher: Sha256::new(), len: 0 }
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    /// (`sha256:<hex>`, bytes written), once the inner writer is flushed.
    pub fn finish(mut self) -> std::io::Result<(String, u64)> {
        self.inner.flush()?;
        Ok((format!("sha256:{}", crate::task::hex(&self.hasher.finalize())), self.len))
    }
}

impl<W: Write> Write for Sha256Writer<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// The file at `path` is `size` bytes whose sha256 is `digest`.
pub fn verify_file(path: &Path, digest: &str, size: u64) -> Result<()> {
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hashed = Sha256Reader::new(file.take(size + 1));
    std::io::copy(&mut hashed, &mut std::io::sink())?;
    let (got, len) = hashed.finish();
    if got != digest || len != size {
        bail!("{} does not match its digest {digest} ({size} bytes)", path.display());
    }
    Ok(())
}

fn require_version(d: &DatasetConfig, name: &str) -> Result<String> {
    let version = d.version.clone().with_context(|| format!("an OCI dataset needs a version: {name}@<version>"))?;
    if !is_tag(&version) {
        bail!("{name}@{version}: the version is not an OCI tag");
    }
    Ok(version)
}

/// Set each OCI dataset's `digest` to the manifest its tag resolves to, through `./hbench.lock` as `mode` says.
/// A dataset that already has a digest keeps it.
pub async fn pin_datasets(datasets: &mut [DatasetConfig], mode: LockMode) -> Result<()> {
    pin_datasets_at(Path::new(lockfile::FILE), datasets, mode).await
}

async fn pin_datasets_at(path: &Path, datasets: &mut [DatasetConfig], mode: LockMode) -> Result<()> {
    let mut oci: Vec<&mut DatasetConfig> = datasets.iter_mut().filter(|d| d.registry.is_some()).collect();
    if oci.is_empty() {
        return Ok(());
    }
    let lock = lockfile::read(path)?;
    if mode == LockMode::Locked && lock.is_none() {
        bail!("--locked: there is no {} here (write one with --lock-write)", lockfile::FILE);
    }
    let mut locked = lock.map(|l| l.datasets).unwrap_or_default();
    let mut changed = false;
    for d in oci.iter_mut() {
        let name = d.name.clone().context("an OCI dataset needs a name")?;
        let version = require_version(d, &name)?;
        let key = format!("{name}@{version}");
        let in_lock = locked.get(&key).cloned();
        let location = Location::parse(d.registry.as_deref().unwrap_or_default())?;
        d.registry = Some(location.spec());
        let digest = match d.digest.clone() {
            Some(digest) => digest,
            None => {
                let store = Store::open(&location.registry, &cache_root(d.download_dir.as_deref()))?;
                let resolved = store.resolve_tag(&location.repository(&name)?, &version).await;
                match (resolved, &in_lock, mode) {
                    (Ok(digest), _, _) => digest,
                    (Err(e), Some(pinned), LockMode::Read) => {
                        eprintln!("warning: {key}: {e:#}; using {pinned} from {}", lockfile::FILE);
                        pinned.clone()
                    }
                    (Err(e), _, _) => return Err(e),
                }
            }
        };
        let digest = match (mode, in_lock) {
            (LockMode::Locked, None) => bail!("--locked: {key} is not in {}", lockfile::FILE),
            (LockMode::Locked, Some(pinned)) if pinned != digest => {
                bail!("--locked: {key} resolves to {digest}, but {} locks it at {pinned}", lockfile::FILE)
            }
            (LockMode::Write, pinned) if pinned.as_ref() != Some(&digest) => {
                eprintln!("{}: {key} -> {digest}", lockfile::FILE);
                locked.insert(key, digest.clone());
                changed = true;
                digest
            }
            (LockMode::Read, Some(pinned)) if pinned != digest && d.digest.is_none() => {
                eprintln!(
                    "warning: {key} now resolves to {digest}; running {pinned} from {} (--lock-write records the new one)",
                    lockfile::FILE
                );
                pinned
            }
            _ => digest,
        };
        d.digest = Some(digest);
    }
    if changed {
        lockfile::write(path, &locked)?;
    }
    Ok(())
}

/// The dataset at `repository@digest`: its manifest and config, checked against each other.
async fn open(store: &Store, repository: &str, digest: &str) -> Result<Dataset> {
    let reference = store.reference(repository, digest);
    let bytes = store.manifest(repository, digest).await?;
    let manifest = format::parse_manifest(&bytes).with_context(|| reference.clone())?;
    let config = manifest.config();
    let config = store
        .blob_bytes(repository, config.digest().as_ref(), config.size(), format::MAX_CONFIG_BYTES)
        .await
        .with_context(|| reference.clone())?;
    Dataset::new(&manifest, &config).with_context(|| reference.clone())
}

/// The tasks of OCI dataset `d` (its registry set), each one a layer of the dataset's manifest.
pub async fn dataset_tasks(d: &DatasetConfig) -> Result<Vec<TaskRef>> {
    let mut d = d.clone();
    pin_datasets(std::slice::from_mut(&mut d), LockMode::Read).await?;
    let name = d.name.clone().context("an OCI dataset needs a name")?;
    let version = require_version(&d, &name)?;
    let digest = d.digest.clone().context("an OCI dataset without a digest")?;
    let location = Location::parse(d.registry.as_deref().unwrap_or_default())?;
    let repository = location.repository(&name)?;
    let store = Store::open(&location.registry, &cache_root(d.download_dir.as_deref()))?;
    let dataset = open(&store, &repository, &digest).await?;
    if (dataset.config.name.as_str(), dataset.config.version.as_str()) != (name.as_str(), version.as_str()) {
        bail!(
            "{} is dataset {}@{}, not {name}@{version}",
            store.reference(&repository, &digest),
            dataset.config.name,
            dataset.config.version
        );
    }
    Ok(dataset
        .layers
        .into_iter()
        .map(|l| TaskRef {
            path: Some(l.name.into()),
            overwrite: d.overwrite,
            download_dir: d.download_dir.clone(),
            source: Some(name.clone()),
            oci: Some(OciTask {
                registry: location.registry.clone(),
                repository: repository.clone(),
                manifest_digest: digest.clone(),
                layer_digest: l.digest,
            }),
            ..Default::default()
        })
        .collect())
}

/// Unpack every OCI task that is not in the cache already, or whose cached copy no longer matches its package
/// digest.
pub async fn download(tasks: &[&TaskRef]) -> Result<()> {
    let mut groups: BTreeMap<(String, String, String, Option<PathBuf>), Vec<&TaskRef>> = BTreeMap::new();
    for t in tasks {
        let Some(o) = &t.oci else { continue };
        let key = (o.registry.clone(), o.repository.clone(), o.manifest_digest.clone(), t.download_dir.clone());
        groups.entry(key).or_default().push(t);
    }
    for ((registry, repository, digest, download_dir), ts) in groups {
        let store = Store::open(&registry, &cache_root(download_dir.as_deref()))?;
        let dataset = open(&store, &repository, &digest).await?;
        let mut needed: Vec<(Layer, PathBuf)> = vec![];
        for t in ts {
            let o = t.oci.as_ref().unwrap();
            let name = t.display_name();
            let layer = dataset
                .layers
                .iter()
                .find(|l| l.digest == o.layer_digest && l.name == name)
                .with_context(|| format!("task {name} ({}) is not in {}", o.layer_digest, store.reference(&repository, &digest)))?;
            let dir = task_dir(t, o);
            if dir.is_dir() && !t.overwrite && crate::task::package_digest(&dir) == layer.package_digest {
                continue;
            }
            if dir.exists() && !t.overwrite {
                eprintln!("task {name}: the cached copy at {} differs from its package digest; unpacking it again", dir.display());
            }
            if !needed.iter().any(|(_, d)| *d == dir) {
                needed.push((layer.clone(), dir));
            }
        }
        if needed.is_empty() {
            continue;
        }
        eprintln!("downloading {} task(s) from {}", needed.len(), store.reference(&repository, &digest));
        let blobs: Vec<Result<PathBuf>> =
            futures_util::stream::iter(needed.iter().map(|(l, _)| store.blob_file(&repository, &l.digest, l.size)))
                .buffered(PARALLEL_DOWNLOADS)
                .collect()
                .await;
        for ((layer, dir), blob) in needed.iter().zip(blobs) {
            unpack::unpack(&blob?, layer, dir)?;
            if crate::task::package_digest(dir) != layer.package_digest {
                bail!("task {}: {} does not match its package digest after unpacking", layer.name, dir.display());
            }
        }
    }
    Ok(())
}

/// `datasets list` for an OCI layout dir: every tagged dataset in it, verified.
pub async fn list(spec: &str) -> Result<Vec<Dataset>> {
    let location = Location::parse(spec)?;
    let Some(dir) = location.registry.strip_prefix(LAYOUT_SCHEME) else {
        bail!("an OCI registry can't be listed; name a dataset with -d <name>@<version> (oci-layout: dirs can be listed)");
    };
    let store = Store::open(&location.registry, &crate::env::cache_root())?;
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {dir}"))?
        .flatten()
        .filter(|e| e.path().join("index.json").is_file())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    names.sort();
    let mut out = vec![];
    for name in names {
        let text = std::fs::read_to_string(Path::new(dir).join(&name).join("index.json"))?;
        let index: oci_spec::image::ImageIndex = serde_json::from_str(&text).with_context(|| format!("{dir}/{name}/index.json"))?;
        for m in index.manifests().iter().filter(|m| m.annotations().as_ref().is_some_and(|a| a.contains_key(format::REF_NAME))) {
            out.push(open(&store, &name, m.digest().as_ref()).await?);
        }
    }
    Ok(out)
}
