//! Publishing datasets: pack a directory of tasks into an OCI image layout, push it to a registry, convert a
//! `registry.json` dataset, export a registry's dataset into a layout, and check that a packed dataset unpacks to
//! the directories it was packed from.

use super::descriptor::{self, Descriptor};
use super::pack::{DatasetInfo, Kind, Provenance, entries, task_name, write_layout};
use super::store::Store;
use super::{LAYOUT_SCHEME, Location, REGISTRY_SCHEME, Sha256Reader, is_repository, is_tag};
use crate::config::{DatasetConfig, TaskRef};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use oci_client::secrets::RegistryAuth;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const PARALLEL_TRANSFERS: usize = 4;
const UNKNOWN_LICENSE: &str = "NOASSERTION";

/// A push target, `oci://<registry>/<repository>[:<version>]`: the repository's last component is the dataset's
/// name, and the tag is its version.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
}

impl Target {
    pub fn parse(spec: &str) -> Result<Target> {
        const FORM: &str = "oci://<registry>/<repository>:<version>";
        let rest = spec.strip_prefix(REGISTRY_SCHEME).with_context(|| format!("{spec:?}: name a target as {FORM}"))?;
        let (registry, path) = rest.split_once('/').with_context(|| format!("{spec}: no repository; name a target as {FORM}"))?;
        let host = Location::parse(&format!("{REGISTRY_SCHEME}{registry}"))?.registry;
        let (repository, tag) = match path.rsplit_once(':') {
            Some((repository, tag)) => (repository, Some(tag.to_string())),
            None => (path, None),
        };
        if repository.is_empty() || !is_repository(repository) {
            bail!("{spec}: {repository:?} is not a repository (lowercase letters and digits, separated by . _ - or /)");
        }
        if let Some(t) = tag.as_deref().filter(|t| !is_tag(t)) {
            bail!("{spec}: {t:?} is not an OCI tag");
        }
        Ok(Target { registry: host, repository: repository.to_string(), tag })
    }

    /// The target names dataset `name@version`: its repository ends in `name`, and its tag (if any) is `version`.
    pub fn check(&self, name: &str, version: &str) -> Result<()> {
        let last = self.repository.rsplit('/').next().unwrap_or_default();
        if last != name {
            bail!("{}/{}: the repository's last component is the dataset name, {name}", self.registry, self.repository);
        }
        if let Some(t) = self.tag.as_deref().filter(|t| *t != version) {
            bail!("{}/{}:{t}: the tag is the dataset version, {version}", self.registry, self.repository);
        }
        Ok(())
    }
}

/// A directory of its own under `parent`, removed when dropped.
struct Staging(PathBuf);

impl Staging {
    fn new(parent: &Path) -> Result<Staging> {
        let dir = std::path::absolute(parent.join(uuid::Uuid::new_v4().simple().to_string()))?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Staging(dir))
    }
}

impl std::ops::Deref for Staging {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn layout_store(root: &Path) -> Result<Store> {
    let root = std::path::absolute(root)?;
    Store::open(&format!("{LAYOUT_SCHEME}{}", root.display()), &root)
}

/// `name@version`, the version required.
fn name_and_version(dataset: &str) -> Result<(String, String)> {
    match dataset.split_once('@') {
        Some((name, version)) if !name.is_empty() && !version.is_empty() => Ok((name.to_string(), version.to_string())),
        _ => bail!("{dataset:?}: name the dataset as <name>@<version>"),
    }
}

/// Every task directory (one with a task.toml) directly under `dir`, sorted by name.
pub fn task_dirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| crate::task::Task::is_task_dir(p))
        .collect();
    dirs.sort();
    if dirs.is_empty() {
        bail!("{}: no task directories (subdirectories with a task.toml)", dir.display());
    }
    Ok(dirs)
}

#[derive(Debug)]
pub struct Packed {
    pub name: String,
    pub version: String,
    pub digest: String,
    pub tasks: usize,
}

/// Pack the task directories under `tasks_dir` as `descriptor` describes them into the OCI image layout
/// `<layout_root>/<name>`, tagged with the version.
pub fn pack(tasks_dir: &Path, descriptor: &Descriptor, layout_root: &Path) -> Result<Packed> {
    let tasks = task_dirs(tasks_dir)?;
    descriptor.check_tasks(&tasks)?;
    let info = descriptor.info();
    let digest = write_layout(layout_root, &info, &tasks)?;
    Ok(Packed { name: info.name, version: info.version, digest, tasks: tasks.len() })
}

#[derive(Debug)]
pub struct Copied {
    pub reference: String,
    pub digest: String,
    pub blobs: usize,
    pub copied: usize,
    pub copied_bytes: u64,
}

/// Copy the dataset manifest `digest` in `source`'s repository `from`, with its config and layers, into `dest`'s
/// repository `to`, tagged `tag`. Everything is checked against its digest as it is read, and the tag must name
/// the manifest afterwards.
async fn copy(source: &Store, from: &str, digest: &str, dest: &Store, to: &str, tag: &str) -> Result<Copied> {
    let manifest = source.manifest(from, digest).await?;
    let dataset = super::open(source, from, digest).await?;
    let config = super::format::parse_manifest(&manifest)?.config().clone();
    let blobs: Vec<(String, u64)> = std::iter::once((config.digest().to_string(), config.size()))
        .chain(dataset.layers.iter().map(|l| (l.digest.clone(), l.size)))
        .collect();
    let transfers: Vec<Result<Option<u64>>> = futures_util::stream::iter(blobs.iter().map(|(digest, size)| async move {
        let path = source.blob_file(from, digest, *size).await?;
        Ok(dest.put_blob(to, digest, *size, &path).await?.then_some(*size))
    }))
    .buffer_unordered(PARALLEL_TRANSFERS)
    .collect()
    .await;
    let copied: Vec<u64> = transfers.into_iter().collect::<Result<Vec<_>>>()?.into_iter().flatten().collect();
    dest.put_manifest(to, tag, &manifest).await?;
    let reference = dest.reference(to, tag);
    match dest.tagged(to, tag).await? {
        Some(now) if now == digest => {}
        now => bail!("{reference} names {}, not the manifest just put there, {digest}", now.as_deref().unwrap_or("nothing")),
    }
    Ok(Copied { reference, digest: digest.to_string(), blobs: blobs.len(), copied: copied.len(), copied_bytes: copied.iter().sum() })
}

#[derive(Debug)]
pub struct Pushed {
    pub copied: Copied,
    /// The tag named this manifest already, so nothing was pushed.
    pub unchanged: bool,
}

/// Push dataset `name@version` from the OCI image layout `<layout_root>/<name>` to `target`, authenticating with
/// `auth`. A tag that names another manifest is moved only with `allow_overwrite`.
pub async fn push(
    layout_root: &Path,
    name: &str,
    version: &str,
    target: &Target,
    auth: RegistryAuth,
    allow_overwrite: bool,
) -> Result<Pushed> {
    target.check(name, version)?;
    let source = layout_store(layout_root)?;
    let digest = source.resolve_tag(name, version).await?;
    let anonymous = auth == RegistryAuth::Anonymous;
    let dest = Store::open_with_auth(&target.registry, layout_root, auth)?;
    let reference = dest.reference(&target.repository, version);
    let existing = dest.tagged(&target.repository, version).await.map_err(|e| {
        if anonymous {
            e.context(format!("pushing anonymously, as the docker config has no credentials for {}", target.registry))
        } else {
            e
        }
    })?;
    match existing {
        Some(existing) if existing == digest => {
            let copied = Copied { reference, digest, blobs: 0, copied: 0, copied_bytes: 0 };
            return Ok(Pushed { copied, unchanged: true });
        }
        Some(existing) if !allow_overwrite => bail!(
            "{reference} already exists ({existing}), and this push is {digest}; a published version is not moved: publish a new version, or pass --allow-overwrite"
        ),
        _ => {}
    }
    let copied = copy(&source, name, &digest, &dest, &target.repository, version).await?;
    Ok(Pushed { copied, unchanged: false })
}

/// `datasets push`: pack `tasks_dir` as the descriptor at `descriptor` describes it, in a staging dir under
/// `staging`, and push it to `to` with the credentials the docker config at `docker_config` holds for its registry.
pub async fn pack_and_push(
    tasks_dir: &Path,
    descriptor: &Path,
    to: &str,
    allow_overwrite: bool,
    docker_config: Option<&Path>,
    staging: &Path,
) -> Result<(Packed, Pushed)> {
    let descriptor = descriptor::load(descriptor)?;
    let target = Target::parse(to)?;
    target.check(&descriptor.name, &descriptor.version)?;
    let auth = super::credentials::registry_auth(docker_config, &target.registry)?;
    let staging = Staging::new(staging)?;
    let packed = pack(tasks_dir, &descriptor, &staging)?;
    let pushed = push(&staging, &packed.name, &packed.version, &target, auth, allow_overwrite).await?;
    Ok((packed, pushed))
}

/// Copy dataset `name@version` from `registry` (`oci://<registry>/<namespace>` or `oci-layout:<dir>`) into the OCI
/// image layout `<layout_root>/<name>`, tagged with the version, for `oci-layout:<layout_root>` to read offline.
pub async fn export(dataset: &str, registry: &str, layout_root: &Path, auth: RegistryAuth) -> Result<Copied> {
    let (name, version) = name_and_version(dataset)?;
    let location = Location::parse(registry)?;
    let repository = location.repository(&name)?;
    let layout_root = std::path::absolute(layout_root)?;
    let source = Store::open_with_auth(&location.registry, &layout_root.join(&name), auth)?;
    let digest = source.resolve_tag(&repository, &version).await?;
    let config = super::open(&source, &repository, &digest).await?.config;
    if (config.name.as_str(), config.version.as_str()) != (name.as_str(), version.as_str()) {
        bail!("{} is dataset {}@{}, not {dataset}", source.reference(&repository, &digest), config.name, config.version);
    }
    copy(&source, &repository, &digest, &layout_store(&layout_root)?, &name, &version).await
}

pub enum Output {
    Layout(PathBuf),
    Registry(Target),
}

pub struct Convert {
    /// A registry.json path, or an http(s) URL of one.
    pub registry: String,
    pub dataset: String,
    pub descriptor: Option<PathBuf>,
    pub output: Output,
    pub check_equivalence: bool,
    pub allow_overwrite: bool,
    pub docker_config: Option<PathBuf>,
    /// Where git tasks are checked out; the usual task cache when None.
    pub download_dir: Option<PathBuf>,
    pub staging: PathBuf,
}

#[derive(Debug)]
pub struct Converted {
    pub name: String,
    pub version: String,
    pub digest: String,
    pub tasks: usize,
    pub pushed: Option<Pushed>,
}

/// What a dataset converted without a descriptor says about itself: its registry name, version and description,
/// an unknown licence, and the git repository and commit its tasks come from when they all come from one.
fn registry_info(dataset: &crate::registry::Dataset, tasks: &[TaskRef]) -> DatasetInfo {
    let origins: BTreeSet<(Option<&str>, Option<&str>)> =
        tasks.iter().map(|t| (t.git_url.as_deref(), t.git_commit_id.as_deref())).collect();
    let source = match origins.into_iter().collect::<Vec<_>>().as_slice() {
        [(Some(url), commit)] => Some(Provenance { url: url.to_string(), revision: commit.map(String::from) }),
        _ => None,
    };
    DatasetInfo {
        name: dataset.name.clone(),
        version: dataset.version.clone(),
        description: dataset.description.clone(),
        license: UNKNOWN_LICENSE.into(),
        source,
        ..Default::default()
    }
}

/// `datasets convert`: fetch a registry.json dataset's tasks (each git repository and commit once), pack them as
/// the descriptor describes them (or as the registry does), write them to an OCI image layout or push them, and
/// with `check_equivalence` read the result back and compare it with the checkouts.
pub async fn convert(args: &Convert) -> Result<Converted> {
    let (name, version) = name_and_version(&args.dataset)?;
    let is_url = args.registry.starts_with("https://") || args.registry.starts_with("http://");
    let (url, path) = if is_url { (Some(args.registry.as_str()), None) } else { (None, Some(Path::new(&args.registry))) };
    let registries = crate::registry::load(url, path)?;
    let dataset = crate::registry::find(&registries, &name, Some(&version))?;
    let selection = DatasetConfig { download_dir: args.download_dir.clone(), ..Default::default() };
    let mut tasks: Vec<TaskRef> = dataset.tasks.iter().map(|t| crate::registry::task_ref(t.clone(), &selection, &name)).collect();
    tasks.sort_by_key(TaskRef::display_name);
    crate::registry::download(&tasks).await?;
    let dirs: Vec<PathBuf> = tasks.iter().map(crate::registry::local_path).collect();
    let info = match &args.descriptor {
        Some(p) => {
            let d = descriptor::load(p)?;
            if (d.name.as_str(), d.version.as_str()) != (name.as_str(), version.as_str()) {
                eprintln!("packing {name}@{version} as {}@{}, as {} names it", d.name, d.version, p.display());
            }
            d.check_tasks(&dirs)?;
            d.info()
        }
        None => {
            eprintln!("warning: no --descriptor, so {name}@{version} is packed with licence {UNKNOWN_LICENSE}");
            registry_info(dataset, &tasks)
        }
    };
    let staging = Staging::new(&args.staging)?;
    let (digest, pushed, readback, repository) = match &args.output {
        Output::Layout(root) => {
            let digest = write_layout(root, &info, &dirs)?;
            (digest, None, layout_store(root)?, info.name.clone())
        }
        Output::Registry(target) => {
            target.check(&info.name, &info.version)?;
            let auth = super::credentials::registry_auth(args.docker_config.as_deref(), &target.registry)?;
            let layout = staging.join("layout");
            write_layout(&layout, &info, &dirs)?;
            let pushed = push(&layout, &info.name, &info.version, target, auth.clone(), args.allow_overwrite).await?;
            let readback = Store::open_with_auth(&target.registry, &staging.join("cache"), auth)?;
            (pushed.copied.digest.clone(), Some(pushed), readback, target.repository.clone())
        }
    };
    if args.check_equivalence {
        check_equivalence(&readback, &repository, &digest, &dirs, &staging.join("unpacked")).await?;
    }
    Ok(Converted { name: info.name, version: info.version, digest, tasks: dirs.len(), pushed })
}

fn describe(kind: &Kind) -> String {
    match kind {
        Kind::Dir => "a directory".into(),
        Kind::File { executable: true, len } => format!("an executable file of {len} bytes"),
        Kind::File { executable: false, len } => format!("a file of {len} bytes"),
        Kind::Symlink(target) => format!("a symlink to {}", target.display()),
    }
}

fn file_digest(path: &Path) -> Result<String> {
    let mut hashed = Sha256Reader::new(std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?);
    std::io::copy(&mut hashed, &mut std::io::sink())?;
    Ok(hashed.finish().0)
}

/// How the tree at `unpacked` differs from the tree at `source`: entries missing or extra, of another kind,
/// exec bit, size or symlink target, or with other contents.
fn differences(source: &Path, unpacked: &Path) -> Result<Vec<String>> {
    let packed: BTreeMap<String, Kind> = entries(source)?.into_iter().collect();
    let found: BTreeMap<String, Kind> = entries(unpacked)?.into_iter().collect();
    let mut out = vec![];
    for (path, kind) in &packed {
        match found.get(path) {
            None => out.push(format!("{path} is in the source but not unpacked")),
            Some(other) if other != kind => out.push(format!("{path} is {} in the source, {} unpacked", describe(kind), describe(other))),
            Some(Kind::File { .. }) if file_digest(&source.join(path))? != file_digest(&unpacked.join(path))? => {
                out.push(format!("{path} has other contents"))
            }
            _ => {}
        }
    }
    out.extend(found.keys().filter(|p| !packed.contains_key(*p)).map(|p| format!("{p} is unpacked but not in the source")));
    Ok(out)
}

/// Unpack every task of the dataset `repository@digest` in `store` into `scratch` and compare it with the directory
/// it was packed from (`sources`, named like the tasks): the same entries, contents, exec bits and symlink targets,
/// so the same `task_checksum` and package digest. Every difference is reported.
pub async fn check_equivalence(store: &Store, repository: &str, digest: &str, sources: &[PathBuf], scratch: &Path) -> Result<()> {
    let reference = store.reference(repository, digest);
    let dataset = super::open(store, repository, digest).await?;
    let mut problems = vec![];
    let names: Vec<String> = sources.iter().map(|s| task_name(s)).collect();
    problems.extend(dataset.layers.iter().filter(|l| !names.contains(&l.name)).map(|l| format!("{}: not among the sources", l.name)));
    for (source, name) in sources.iter().zip(&names) {
        let Some(layer) = dataset.layers.iter().find(|l| l.name == *name) else {
            problems.push(format!("{name}: not in the dataset"));
            continue;
        };
        let blob = store.blob_file(repository, &layer.digest, layer.size).await?;
        let unpacked = scratch.join(layer.digest.trim_start_matches("sha256:")).join(name);
        super::unpack::unpack(&blob, layer, &unpacked)?;
        problems.extend(differences(source, &unpacked)?.into_iter().map(|d| format!("{name}: {d}")));
        let checksums = (crate::task::hash_dir(source)?, crate::task::hash_dir(&unpacked)?);
        if checksums.0 != checksums.1 {
            problems.push(format!("{name}: task_checksum is {} in the source, {} unpacked", checksums.0, checksums.1));
        }
        let package_digests = (crate::task::package_digest(source), crate::task::package_digest(&unpacked));
        if package_digests.0 != package_digests.1 || package_digests.0 != layer.package_digest {
            problems.push(format!(
                "{name}: the package digest is {} in the source, {} unpacked, {} in the manifest",
                package_digests.0, package_digests.1, layer.package_digest
            ));
        }
    }
    if !problems.is_empty() {
        bail!("{reference} differs from the directories it was packed from:\n  {}", problems.join("\n  "));
    }
    Ok(())
}
