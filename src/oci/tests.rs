use super::format::{self, Layer};
use super::lockfile::LockMode;
use super::pack::{DatasetInfo, pack_task, write_layout};
use super::{Location, dataset_tasks, download, pin_datasets_at, sha256_digest, unpack::unpack};
use crate::config::{DatasetConfig, OciTask, TaskRef, TrialConfig};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

mod publish;

/// A temp dir, removed when it goes out of scope.
struct Scratch(PathBuf);

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(tag: &str) -> Scratch {
    let d = std::env::temp_dir().join(format!("hbench-oci-{tag}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    Scratch(d)
}

fn example_tasks() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/tasks");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root).unwrap().flatten().map(|e| e.path()).collect();
    dirs.sort();
    assert_eq!(dirs.len(), 3, "{dirs:?}");
    dirs
}

fn example(name: &str) -> PathBuf {
    example_tasks().into_iter().find(|d| d.ends_with(name)).unwrap()
}

fn info(name: &str, version: &str) -> DatasetInfo {
    DatasetInfo {
        name: name.into(),
        version: version.into(),
        license: "Apache-2.0".into(),
        description: "examples".into(),
        ..Default::default()
    }
}

fn layout_dataset(root: &Path, cache: &Path, name: &str, version: &str) -> DatasetConfig {
    DatasetConfig {
        name: Some(name.into()),
        version: Some(version.into()),
        registry: Some(format!("oci-layout:{}", root.display())),
        download_dir: Some(cache.to_path_buf()),
        ..Default::default()
    }
}

async fn resolve(lock: &Path, mut d: DatasetConfig) -> anyhow::Result<Vec<TaskRef>> {
    pin_datasets_at(lock, std::slice::from_mut(&mut d), LockMode::Read).await?;
    dataset_tasks(&d).await
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), target).unwrap();
        }
    }
}

fn modes(dir: &Path) -> Vec<(String, u32)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u32)>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let rel = e.path().strip_prefix(root).unwrap().display().to_string();
            out.push((rel, e.metadata().unwrap().permissions().mode() & 0o111));
            if e.file_type().unwrap().is_dir() {
                walk(root, &e.path(), out);
            }
        }
    }
    let mut out = vec![];
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn assert_identical(source: &Path, unpacked: &Path) {
    assert_eq!(crate::task::hash_dir(source).unwrap(), crate::task::hash_dir(unpacked).unwrap(), "{}", source.display());
    assert_eq!(crate::task::package_digest(source), crate::task::package_digest(unpacked), "{}", source.display());
    assert!(!crate::task::package_digest(unpacked).is_empty());
    assert_eq!(modes(source), modes(unpacked), "{}", source.display());
}

/// Rewrite the manifest tagged `version` in the layout `layout`, and its config; the new manifest digest.
fn rewrite(layout: &Path, version: &str, edit: impl FnOnce(&mut Value, &mut Value)) -> String {
    let blob = |digest: &str| layout.join("blobs/sha256").join(digest.trim_start_matches("sha256:"));
    let put = |bytes: &[u8]| {
        let digest = sha256_digest(bytes);
        std::fs::write(blob(&digest), bytes).unwrap();
        (digest, bytes.len())
    };
    let mut index: Value = serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    let entry = index["manifests"].as_array_mut().unwrap().iter_mut().find(|m| m["annotations"][format::REF_NAME] == version).unwrap();
    let mut manifest: Value = serde_json::from_slice(&std::fs::read(blob(entry["digest"].as_str().unwrap())).unwrap()).unwrap();
    let mut config: Value = serde_json::from_slice(&std::fs::read(blob(manifest["config"]["digest"].as_str().unwrap())).unwrap()).unwrap();
    edit(&mut manifest, &mut config);
    let (digest, size) = put(&serde_json::to_vec(&config).unwrap());
    manifest["config"]["digest"] = digest.into();
    manifest["config"]["size"] = size.into();
    let (digest, size) = put(&serde_json::to_vec(&manifest).unwrap());
    entry["digest"] = digest.clone().into();
    entry["size"] = size.into();
    std::fs::write(layout.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
    digest
}

#[tokio::test]
async fn example_tasks_unpack_byte_identical_from_an_oci_layout() {
    let (root, cache, locks) = (scratch("layout"), scratch("cache"), scratch("lock"));
    let lock = locks.join("hbench.lock");
    let digest = write_layout(&root, &info("examples", "1.0"), &example_tasks()).unwrap();
    assert_eq!(write_layout(&scratch("again"), &info("examples", "1.0"), &example_tasks()).unwrap(), digest, "packing is reproducible");
    let tasks = resolve(&lock, layout_dataset(&root, &cache, "examples", "1.0")).await.unwrap();
    assert_eq!(tasks.iter().map(TaskRef::display_name).collect::<Vec<_>>(), ["hello-alpine", "hello-workdir", "hello-world"]);
    let oci = tasks[0].oci.as_ref().unwrap();
    assert_eq!((oci.registry.clone(), oci.repository.as_str()), (format!("oci-layout:{}", root.display()), "examples"));
    assert_eq!(oci.manifest_digest, digest);
    download(&tasks.iter().collect::<Vec<_>>()).await.unwrap();
    for (t, source) in tasks.iter().zip(example_tasks()) {
        let dir = crate::registry::local_path(t);
        let hex = t.oci.as_ref().unwrap().layer_digest.trim_start_matches("sha256:").to_string();
        assert_eq!(dir, cache.join("tasks/sha256").join(hex).join(t.display_name()));
        assert_identical(&source, &dir);
        crate::task::Task::load(&dir).unwrap().validate_tests().unwrap();
    }

    let instruction = crate::registry::local_path(&tasks[2]).join("instruction.md");
    std::fs::write(&instruction, "edited in the cache\n").unwrap();
    download(&tasks.iter().collect::<Vec<_>>()).await.unwrap();
    assert_identical(&example("hello-world"), &crate::registry::local_path(&tasks[2]));

    let only = DatasetConfig { task_names: Some(vec!["hello-w*".into()]), ..layout_dataset(&root, &cache, "examples", "1.0") };
    let mut pinned = only.clone();
    pin_datasets_at(&lock, std::slice::from_mut(&mut pinned), LockMode::Read).await.unwrap();
    let names: Vec<String> = crate::registry::dataset_tasks(&pinned, false).await.unwrap().iter().map(TaskRef::display_name).collect();
    assert_eq!(names, ["hello-workdir", "hello-world"]);
    let listed = super::list(&format!("oci-layout:{}", root.display())).await.unwrap();
    assert_eq!((listed[0].config.name.as_str(), listed[0].config.version.as_str(), listed[0].layers.len()), ("examples", "1.0", 3));
}

#[test]
fn layers_depend_only_on_paths_contents_and_exec_bits() {
    let source = example("hello-world");
    let pack = |dir: &Path| {
        let mut out = vec![];
        pack_task(dir, &mut out).unwrap();
        out
    };
    let original = pack(&source);
    let copies = scratch("copy");
    let copy = copies.join("hello-world");
    copy_dir(&source, &copy);
    let instruction = std::fs::File::options().write(true).open(copy.join("instruction.md")).unwrap();
    instruction.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000)).unwrap();
    std::fs::set_permissions(copy.join("task.toml"), std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(pack(&copy), original);
    std::fs::set_permissions(copy.join("solution/solve.sh"), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_ne!(pack(&copy), original);
    let gzip_header = &original[..10];
    assert_eq!(gzip_header[4..8], [0, 0, 0, 0], "gzip mtime");
    assert_eq!(gzip_header[3] & 0x08, 0, "no gzip file name");
}

#[tokio::test]
async fn a_changed_layer_byte_is_refused() {
    let (root, cache, locks) = (scratch("layout"), scratch("cache"), scratch("lock"));
    let lock = locks.join("hbench.lock");
    write_layout(&root, &info("hello", "1.0"), &[example("hello-world")]).unwrap();
    let tasks = resolve(&lock, layout_dataset(&root, &cache, "hello", "1.0")).await.unwrap();
    let blob = root.join("hello/blobs/sha256").join(tasks[0].oci.as_ref().unwrap().layer_digest.trim_start_matches("sha256:"));
    let mut bytes = std::fs::read(&blob).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    std::fs::write(&blob, bytes).unwrap();
    let err = format!("{:#}", download(&tasks.iter().collect::<Vec<_>>()).await.unwrap_err());
    assert!(err.contains("does not match its digest"), "{err}");
    assert!(!crate::registry::local_path(&tasks[0]).exists());
}

#[tokio::test]
async fn files_that_differ_from_the_package_digest_are_refused() {
    let (root, cache, locks) = (scratch("layout"), scratch("cache"), scratch("lock"));
    let lock = locks.join("hbench.lock");
    let changes = scratch("changed");
    let changed = changes.join("hello-world");
    copy_dir(&example("hello-world"), &changed);
    std::fs::write(changed.join("tests/test.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    write_layout(&root, &info("hello", "1.0"), &[changed]).unwrap();
    let claimed = crate::task::package_digest(&example("hello-world"));
    rewrite(&root.join("hello"), "1.0", |manifest, config| {
        manifest["layers"][0]["annotations"][format::PACKAGE_DIGEST] = claimed.clone().into();
        config["tasks"][0]["package_digest"] = claimed.clone().into();
    });
    let tasks = resolve(&lock, layout_dataset(&root, &cache, "hello", "1.0")).await.unwrap();
    let err = format!("{:#}", download(&tasks.iter().collect::<Vec<_>>()).await.unwrap_err());
    assert!(err.contains("the unpacked files do not match its package digest"), "{err}");
    let digest_dir = crate::registry::local_path(&tasks[0]).parent().unwrap().to_path_buf();
    assert_eq!(std::fs::read_dir(digest_dir).unwrap().count(), 0, "no temp dir is left behind");
}

#[tokio::test]
async fn manifests_and_configs_must_agree() {
    let locks = scratch("lock");
    let lock = &locks.join("hbench.lock");
    let refused = |edit: fn(&mut Value, &mut Value)| async move {
        let (root, cache) = (scratch("layout"), scratch("cache"));
        write_layout(&root, &info("hello", "1.0"), &[example("hello-world")]).unwrap();
        rewrite(&root.join("hello"), "1.0", edit);
        format!("{:#}", resolve(lock, layout_dataset(&root, &cache, "hello", "1.0")).await.unwrap_err())
    };
    let err = refused(|m, _| m["artifactType"] = "application/vnd.example".into()).await;
    assert!(err.contains("not an hbench dataset"), "{err}");
    let err = refused(|_, c| c["tasks"][0]["name"] = "hello-other".into()).await;
    assert!(err.contains("does not match its manifest layer"), "{err}");
    let err = refused(|_, c| c["license"] = "".into()).await;
    assert!(err.contains("does not match the dataset config schema") && err.contains("license"), "{err}");
    let err = refused(|m, _| m["layers"][0]["mediaType"] = "application/vnd.oci.image.layer.v1.tar+zstd".into()).await;
    assert!(err.contains("layer mediaType"), "{err}");
    let err = refused(|_, c| c["name"] = "other".into()).await;
    assert!(err.contains("is dataset other@1.0, not hello@1.0"), "{err}");
}

/// A raw tar entry: path, type, contents, link target.
type RawEntry = (&'static str, tar::EntryType, &'static str, &'static str);

/// A gzipped tar of raw entries, written without the tar crate's path checks.
fn raw_layer(dir: &Path, entries: &[RawEntry]) -> (PathBuf, Layer) {
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(vec![], flate2::Compression::default()));
    for (path, kind, contents, link) in entries {
        let mut h = tar::Header::new_gnu();
        h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
        h.as_old_mut().linkname[..link.len()].copy_from_slice(link.as_bytes());
        h.set_entry_type(*kind);
        h.set_mode(0o644);
        h.set_size(contents.len() as u64);
        h.set_cksum();
        b.append(&h, contents.as_bytes()).unwrap();
    }
    let bytes = b.into_inner().unwrap().finish().unwrap();
    let blob = dir.join(format!("blob-{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&blob, &bytes).unwrap();
    let layer =
        Layer { name: "t".into(), digest: sha256_digest(&bytes), size: bytes.len() as u64, package_digest: sha256_digest(b"another task") };
    (blob, layer)
}

#[test]
fn entries_that_could_leave_the_task_dir_are_refused() {
    use tar::EntryType::{Char, Directory, Link, Regular, Symlink};
    let cases: &[(&[RawEntry], &str)] = &[
        (&[("../evil", Regular, "x", "")], "absolute paths and .. are not allowed"),
        (&[("/etc/evil", Regular, "x", "")], "absolute paths and .. are not allowed"),
        (&[("a/../../evil", Regular, "x", "")], "absolute paths and .. are not allowed"),
        (&[("link", Symlink, "", "../outside")], "leaves the task directory"),
        (&[("link", Symlink, "", "/etc/passwd")], "leaves the task directory"),
        (&[("here", Symlink, "", "."), ("up", Symlink, "", "here/..")], "symlink leaves the task directory"),
        (&[("sub", Directory, "", ""), ("d", Symlink, "", "sub"), ("d/x", Regular, "x", "")], "a parent is not a directory"),
        (&[("a", Regular, "x", ""), ("h", Link, "", "a")], "Link entries are not allowed"),
        (&[("dev", Char, "", "")], "Char entries are not allowed"),
        (&[("a", Regular, "x", ""), ("a", Regular, "y", "")], "listed twice"),
        (&[("dangling", Symlink, "", "missing")], "symlink points at nothing"),
    ];
    let dir = scratch("raw");
    for (entries, expected) in cases {
        let (blob, layer) = raw_layer(&dir, entries);
        let dest = dir.join("cache").join(uuid::Uuid::new_v4().simple().to_string()).join("t");
        let err = format!("{:#}", unpack(&blob, &layer, &dest).unwrap_err());
        assert!(err.contains(expected), "{entries:?}: {err}");
        assert_eq!(std::fs::read_dir(dest.parent().unwrap()).unwrap().count(), 0, "{entries:?}: nothing is left behind");
    }
    let (blob, layer) = raw_layer(&dir, &[("sub", Directory, "", ""), ("sub/f", Regular, "x", ""), ("link", Symlink, "", "sub/f")]);
    let dest = dir.join("ok/t");
    let err = format!("{:#}", unpack(&blob, &layer, &dest).unwrap_err());
    assert!(err.contains("do not match its package digest"), "a safe layer gets as far as the package digest: {err}");
}

#[tokio::test]
async fn locked_refuses_a_tag_that_moved() {
    let (root, cache, locks) = (scratch("layout"), scratch("cache"), scratch("lock"));
    let lock = locks.join("hbench.lock");
    let first = write_layout(&root, &info("hello", "1.0"), &[example("hello-world")]).unwrap();
    let dataset = || vec![layout_dataset(&root, &cache, "hello", "1.0")];
    let pin = |mode: LockMode| {
        let lock = lock.clone();
        let mut d = dataset();
        async move { pin_datasets_at(&lock, &mut d, mode).await.map(|_| d.remove(0)) }
    };
    let err = pin(LockMode::Locked).await.unwrap_err().to_string();
    assert!(err.contains("there is no hbench.lock"), "{err}");
    assert_eq!(pin(LockMode::Write).await.unwrap().digest.as_deref(), Some(first.as_str()));
    assert!(std::fs::read_to_string(&lock).unwrap().contains(&format!("\"hello@1.0\" = \"{first}\"")));
    assert_eq!(pin(LockMode::Locked).await.unwrap().digest.as_deref(), Some(first.as_str()));

    let moved = write_layout(&root, &info("hello", "1.0"), &[example("hello-alpine"), example("hello-world")]).unwrap();
    assert_ne!(moved, first);
    let err = pin(LockMode::Locked).await.unwrap_err().to_string();
    assert_eq!(err, format!("--locked: hello@1.0 resolves to {moved}, but hbench.lock locks it at {first}"));
    let read = pin(LockMode::Read).await.unwrap();
    assert_eq!(read.digest.as_deref(), Some(first.as_str()), "the lock wins over a moved tag");
    assert_eq!(dataset_tasks(&read).await.unwrap().len(), 1);
    assert_eq!(pin(LockMode::Write).await.unwrap().digest.as_deref(), Some(moved.as_str()));
    assert_eq!(pin(LockMode::Locked).await.unwrap().digest.as_deref(), Some(moved.as_str()));
    let err = pin_datasets_at(&lock, &mut [layout_dataset(&root, &cache, "hello", "2.0")], LockMode::Locked).await.unwrap_err().to_string();
    assert!(err.contains("not found"), "{err}");
}

#[test]
fn registry_specs() {
    let l = Location::parse("oci://ghcr.io/node-q/hbench-datasets/").unwrap();
    assert_eq!((l.registry.as_str(), l.namespace.as_str()), ("ghcr.io", "node-q/hbench-datasets"));
    assert_eq!(l.repository("terminal-bench").unwrap(), "node-q/hbench-datasets/terminal-bench");
    assert_eq!(l.spec(), "oci://ghcr.io/node-q/hbench-datasets");
    let l = Location::parse("oci://localhost:5000").unwrap();
    assert_eq!((l.registry.as_str(), l.repository("hello").unwrap()), ("localhost:5000", "hello".to_string()));
    let l = Location::parse("oci-layout:datasets").unwrap();
    assert_eq!(l.registry, format!("oci-layout:{}", std::env::current_dir().unwrap().join("datasets").display()));
    let err = |spec: &str| Location::parse(spec).unwrap_err().to_string();
    assert!(err("https://example.com/registry.json").contains("--registry-url"));
    assert!(err("oci://ghcr.io/Node-Q").contains("not a repository namespace"));
    assert!(err("oci:///ns").contains("not a registry host"));
    assert!(Location::parse("oci://ghcr.io/ns").unwrap().repository("Terminal-Bench").is_err());
}

#[test]
fn lock_json_records_oci_tasks() {
    let oci = OciTask {
        registry: "ghcr.io".into(),
        repository: "ns/hello".into(),
        manifest_digest: sha256_digest(b"m"),
        layer_digest: sha256_digest(b"l"),
    };
    let t = TaskRef { path: Some("hello-world".into()), source: Some("hello".into()), oci: Some(oci.clone()), ..Default::default() };
    t.validate().unwrap();
    let lock = crate::trial::trial_lock(&TrialConfig::new(t.clone(), "hello-world__1".into(), "jobs/j".into()));
    let task = &lock["task"];
    assert_eq!(task["type"], "oci");
    assert_eq!(task["registry"], "ghcr.io");
    assert_eq!(task["repository"], "ns/hello");
    assert_eq!(task["manifest_digest"], oci.manifest_digest.as_str());
    assert_eq!(task["layer_digest"], oci.layer_digest.as_str());
    assert!(task.get("git_url").is_none() && task.get("git_commit_id").is_none());
    let git = TaskRef { path: Some("tasks/hello".into()), git_url: Some("https://example.com/t.git".into()), ..Default::default() };
    let lock = crate::trial::trial_lock(&TrialConfig::new(git, "hello__1".into(), "jobs/j".into()));
    assert_eq!(lock["task"]["type"], "git");
    assert!(["registry", "repository", "manifest_digest", "layer_digest"].iter().all(|k| lock["task"].get(k).is_none()));
    let escaping = TaskRef { path: Some("../x".into()), ..t.clone() };
    assert!(escaping.validate().is_err());
    let outside = TaskRef { oci: Some(OciTask { repository: "../ns".into(), ..oci.clone() }), ..t.clone() };
    assert!(outside.validate().is_err());
    let bad_digest = TaskRef { oci: Some(OciTask { layer_digest: "sha256:../../x".into(), ..oci }), ..t };
    assert!(bad_digest.validate().is_err());
}

/// Push dataset `name@version` from the layout under `root` to `registry/repository`, moving the tag if it is
/// there; the manifest digest.
async fn push_layout(root: &Path, name: &str, version: &str, registry: &str, repository: &str) -> String {
    let target = super::publish::Target { registry: registry.into(), repository: repository.into(), tag: Some(version.into()) };
    let pushed = super::publish::push(root, name, version, &target, oci_client::secrets::RegistryAuth::Anonymous, true).await.unwrap();
    pushed.copied.digest
}

/// Run with a registry on localhost, e.g. `docker run -d -p 127.0.0.1:5000:5000 registry:3` and
/// `HBENCH_TEST_OCI_REGISTRY=localhost:5000 cargo test oci::`.
#[tokio::test]
async fn example_tasks_unpack_byte_identical_from_a_registry() {
    let Ok(registry) = std::env::var("HBENCH_TEST_OCI_REGISTRY") else {
        eprintln!("skipped: set HBENCH_TEST_OCI_REGISTRY=<host:port> to run against a registry");
        return;
    };
    let (root, cache, locks) = (scratch("layout"), scratch("cache"), scratch("lock"));
    let lock = locks.join("hbench.lock");
    let namespace = format!("hbench-test/{}", uuid::Uuid::new_v4().simple());
    let digest = write_layout(&root, &info("examples", "1.0"), &example_tasks()).unwrap();
    assert_eq!(push_layout(&root, "examples", "1.0", &registry, &format!("{namespace}/examples")).await, digest);
    let d = DatasetConfig { registry: Some(format!("oci://{registry}/{namespace}")), ..layout_dataset(&root, &cache, "examples", "1.0") };
    let tasks = resolve(&lock, d.clone()).await.unwrap();
    let oci = tasks[0].oci.as_ref().unwrap();
    assert_eq!(
        (oci.registry.as_str(), oci.repository.clone(), oci.manifest_digest.as_str()),
        (registry.as_str(), format!("{namespace}/examples"), digest.as_str())
    );
    download(&tasks.iter().collect::<Vec<_>>()).await.unwrap();
    for (t, source) in tasks.iter().zip(example_tasks()) {
        assert_identical(&source, &crate::registry::local_path(t));
        let hex = t.oci.as_ref().unwrap().layer_digest.trim_start_matches("sha256:").to_string();
        assert!(cache.join("blobs/sha256").join(&hex).is_file());
    }
    assert!(cache.join("blobs/sha256").join(digest.trim_start_matches("sha256:")).is_file(), "the manifest is cached by digest");

    let layer = cache.join("blobs/sha256").join(tasks[0].oci.as_ref().unwrap().layer_digest.trim_start_matches("sha256:"));
    std::fs::write(&layer, b"corrupt").unwrap();
    std::fs::remove_dir_all(crate::registry::local_path(&tasks[0])).unwrap();
    download(&tasks.iter().collect::<Vec<_>>()).await.unwrap();
    assert_identical(&example_tasks()[0], &crate::registry::local_path(&tasks[0]));

    let mut locked = vec![d.clone()];
    pin_datasets_at(&lock, &mut locked, LockMode::Write).await.unwrap();
    write_layout(&root, &info("examples", "1.0"), &example_tasks()[..1]).unwrap();
    let moved = push_layout(&root, "examples", "1.0", &registry, &format!("{namespace}/examples")).await;
    let err = pin_datasets_at(&lock, &mut [d.clone()], LockMode::Locked).await.unwrap_err().to_string();
    assert_eq!(err, format!("--locked: examples@1.0 resolves to {moved}, but hbench.lock locks it at {digest}"));
}

/// Put a dataset index listing `entries` (name, version, digest) at `repository:latest` in `store`, staging its
/// blobs under `dir`; the index manifest's digest.
async fn put_index(dir: &Path, store: &super::store::Store, repository: &str, entries: &[(&str, &str, &str)]) -> String {
    let datasets: Vec<Value> = entries.iter().map(|(n, v, d)| serde_json::json!({"name": n, "version": v, "digest": d})).collect();
    let index = serde_json::to_vec(&serde_json::json!({"schema_version": 1, "datasets": datasets})).unwrap();
    let empty = b"{}".to_vec();
    for bytes in [&index, &empty] {
        let path = dir.join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::write(&path, bytes).unwrap();
        store.put_blob(repository, &sha256_digest(bytes), bytes.len() as u64, &path).await.unwrap();
    }
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": format::MANIFEST_MEDIA_TYPE,
        "artifactType": format::DATASET_INDEX_ARTIFACT_TYPE,
        "config": {"mediaType": "application/vnd.oci.empty.v1+json", "digest": sha256_digest(&empty), "size": empty.len()},
        "layers": [{"mediaType": format::DATASET_INDEX_MEDIA_TYPE, "digest": sha256_digest(&index), "size": index.len()}],
    });
    store.put_manifest(repository, format::DATASET_INDEX_TAG, &serde_json::to_vec(&manifest).unwrap()).await.unwrap()
}

#[test]
fn dataset_indexes_are_checked_entry_by_entry() {
    let digest = format!("sha256:{}", "a".repeat(64));
    let index = |entries: Value| serde_json::to_vec(&serde_json::json!({"schema_version": 1, "datasets": entries})).unwrap();
    let ok = format::parse_dataset_index(&index(serde_json::json!([{"name": "terminal-bench", "version": "2.0.1", "digest": digest}])));
    assert_eq!(ok.unwrap().datasets[0].version, "2.0.1");
    let refused = |bytes: Vec<u8>| format::parse_dataset_index(&bytes).unwrap_err().to_string();
    assert!(
        refused(serde_json::to_vec(&serde_json::json!({"schema_version": 2, "datasets": []})).unwrap()).contains("schema_version is 2")
    );
    assert!(refused(index(serde_json::json!([{"name": "a/b", "version": "1.0", "digest": digest}]))).contains("not a dataset name"));
    assert!(refused(index(serde_json::json!([{"name": "index", "version": "1.0", "digest": digest}]))).contains("not a dataset name"));
    assert!(refused(index(serde_json::json!([{"name": "x", "version": "1.0", "digest": "sha256:12"}]))).contains("not a sha256 digest"));
    let twice = serde_json::json!([{"name": "x", "version": "1.0", "digest": digest}, {"name": "x", "version": "1.0", "digest": digest}]);
    assert!(refused(index(twice)).contains("lists x@1.0 twice"));
    assert!(
        refused(index(serde_json::json!([{"name": "x", "version": "1.0", "digest": digest, "extra": 1}]))).contains("not a dataset index")
    );
}

#[tokio::test]
async fn an_index_lists_its_datasets_each_checked_against_its_digest() {
    let (root, staging) = (scratch("layout"), scratch("staging"));
    let one = write_layout(&root, &info("examples", "1.0"), &example_tasks()).unwrap();
    let two = write_layout(&root, &info("examples", "2.0"), &example_tasks()[..1]).unwrap();
    let spec = format!("oci-layout:{}", root.display());
    let store = super::store::Store::open(&spec, &staging).unwrap();
    assert!(super::indexed(&spec).await.unwrap().is_none());

    put_index(&staging, &store, "index", &[("examples", "1.0", &one), ("examples", "2.0", &two)]).await;
    let listed = super::indexed(&spec).await.unwrap().unwrap();
    let ids: Vec<(String, String, usize)> =
        listed.iter().map(|d| (d.config.name.clone(), d.config.version.clone(), d.layers.len())).collect();
    assert_eq!(ids, [("examples".to_string(), "1.0".to_string(), 3), ("examples".to_string(), "2.0".to_string(), 1)]);
    assert_eq!(super::list(&spec).await.unwrap().len(), 2, "a layout lists its datasets, not its index");

    put_index(&staging, &store, "index", &[("examples", "3.0", &one)]).await;
    let err = format!("{:#}", super::indexed(&spec).await.unwrap_err());
    assert!(err.contains(&format!("lists examples@3.0 at {one}, which is examples@1.0")), "{err}");
}

/// Run with a registry on localhost, as `example_tasks_unpack_byte_identical_from_a_registry` says.
#[tokio::test]
async fn datasets_list_reads_a_registrys_index() {
    let Ok(registry) = std::env::var("HBENCH_TEST_OCI_REGISTRY") else {
        eprintln!("skipped: set HBENCH_TEST_OCI_REGISTRY=<host:port> to run against a registry");
        return;
    };
    let (root, staging) = (scratch("layout"), scratch("staging"));
    let namespace = format!("hbench-test/{}", uuid::Uuid::new_v4().simple());
    let spec = format!("oci://{registry}/{namespace}");
    let err = super::list(&spec).await.unwrap_err().to_string();
    assert!(err.contains(&format!("has no dataset index ({registry}/{namespace}/index:latest), or it is not public")), "{err}");

    let digest = write_layout(&root, &info("examples", "1.0"), &example_tasks()).unwrap();
    push_layout(&root, "examples", "1.0", &registry, &format!("{namespace}/examples")).await;
    let store = super::store::Store::open(&registry, &staging).unwrap();
    put_index(&staging, &store, &format!("{namespace}/index"), &[("examples", "1.0", &digest)]).await;
    let listed = super::list(&spec).await.unwrap();
    assert_eq!(listed.iter().map(|d| (d.config.name.as_str(), d.layers.len())).collect::<Vec<_>>(), [("examples", 3)]);
}
