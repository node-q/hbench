//! Task sources a `taskbuild` repository builds into tasks, run without building them by hand.
//!
//! A dataset repository such as hbench-datasets-internal keeps each task as a source folder, `tasks/<dataset>/<task>/`
//! with a `task.yaml` (what the task is and asks), `expected.json` and its files, and builds the runnable tasks
//! (`task.toml`, instruction, environment, tests) with `python -m taskbuild build datasets/<dataset> <out>`. A local
//! task or dataset path that points at such sources (one task's folder, or the dataset's `tasks/<dataset>` folder) is
//! built that way into hbench's cache first, and the run uses what it built. The build is the dataset's current version,
//! from the sources as they are now, so an edited `task.yaml` is what runs.
//!
//! The build runs on `$HBENCH_TASKBUILD_PYTHON` if set, else the repository's `.venv`, else `uv run` with the
//! repository's `requirements.txt` on Python 3.12, else `python3`.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Env naming the Python that runs `taskbuild`.
pub const PYTHON_ENV: &str = "HBENCH_TASKBUILD_PYTHON";

/// Task sources found at a path: the repository, the dataset, and one task if the path was a task's folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Sources {
    pub root: PathBuf,
    pub dataset: String,
    pub task: Option<String>,
}

fn name(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Whether `dir` holds task sources rather than tasks, and where their repository is.
pub fn sources(dir: &Path) -> Option<Sources> {
    if dir.join("task.toml").is_file() {
        return None;
    }
    let is_source = |d: &Path| d.join("task.yaml").is_file() && !d.join("task.toml").is_file();
    let (dataset_dir, task) = if is_source(dir) {
        (dir.parent()?, Some(name(dir)?))
    } else if std::fs::read_dir(dir).ok()?.flatten().any(|e| is_source(&e.path())) {
        (dir, None)
    } else {
        return None;
    };
    let tasks_dir = dataset_dir.parent()?;
    if name(tasks_dir)? != "tasks" {
        return None;
    }
    let root = tasks_dir.parent()?.to_path_buf();
    let dataset = name(dataset_dir)?;
    let buildable = root.join("taskbuild/__main__.py").is_file() && root.join("datasets").join(&dataset).join("dataset.yaml").is_file();
    buildable.then_some(Sources { root, dataset, task })
}

/// The command that runs `python -m taskbuild` in `root`.
fn python(root: &Path) -> tokio::process::Command {
    if let Some(p) = std::env::var_os(PYTHON_ENV).filter(|p| !p.is_empty()) {
        return tokio::process::Command::new(p);
    }
    let venv = root.join(if cfg!(windows) { ".venv/Scripts/python.exe" } else { ".venv/bin/python" });
    if venv.is_file() {
        return tokio::process::Command::new(venv);
    }
    let requirements = root.join("requirements.txt");
    if requirements.is_file() && which("uv") {
        let mut c = tokio::process::Command::new("uv");
        c.args(["run", "--quiet", "--no-project", "--python", "3.12", "--with-requirements"]).arg(&requirements).arg("python");
        return c;
    }
    tokio::process::Command::new(if cfg!(windows) { "python" } else { "python3" })
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(program).is_file() || d.join(format!("{program}.exe")).is_file()))
}

/// Build `s`'s dataset into the cache and return the folder of built tasks. Builds with the same result share one
/// folder (named by their content), so a run never has its tasks rewritten under it by a later build.
pub async fn build(s: &Sources) -> Result<PathBuf> {
    let base = crate::env::cache_root().join("taskbuild");
    std::fs::create_dir_all(&base)?;
    let stage = base.join(format!(".build-{}", uuid::Uuid::new_v4().simple()));
    eprintln!("building {} from its task sources in {} (taskbuild)", s.dataset, s.root.display());
    let out = python(&s.root)
        .current_dir(&s.root)
        .args(["-m", "taskbuild", "build"])
        .arg(Path::new("datasets").join(&s.dataset))
        .arg(&stage)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("running taskbuild in {} (set {PYTHON_ENV} to the Python that has its requirements)", s.root.display()))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&stage);
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect();
        bail!(
            "taskbuild couldn't build {} from {}:\n{}\n(it runs on {PYTHON_ENV}, the repository's .venv, `uv run` with its requirements.txt, or python3)",
            s.dataset,
            s.root.display(),
            tail.join("\n")
        );
    }
    let digest = tree_digest(&stage)?;
    let dir = base.join(format!("{}-{}", s.dataset, &digest[..12]));
    if dir.is_dir() {
        let _ = std::fs::remove_dir_all(&stage);
    } else if let Err(e) = std::fs::rename(&stage, &dir) {
        // Another run finished the same build first.
        let _ = std::fs::remove_dir_all(&stage);
        if !dir.is_dir() {
            return Err(e).with_context(|| format!("keeping the build in {}", dir.display()));
        }
    }
    Ok(dir)
}

/// The built tasks for `s`: its dataset's folder, or that task's own folder in it.
pub async fn built(s: &Sources) -> Result<PathBuf> {
    let dir = build(s).await?;
    match &s.task {
        None => Ok(dir),
        Some(t) => {
            let task = dir.join(t);
            if !task.join("task.toml").is_file() {
                bail!("taskbuild built no task {t} for {}: is it in the dataset's current version?", s.dataset);
            }
            Ok(task)
        }
    }
}

/// SHA-256 of every file's relative path and content under `root`, in path order.
fn tree_digest(root: &Path) -> Result<String> {
    let mut files = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let p = e.path();
            if e.file_type()?.is_dir() {
                stack.push(p);
            } else {
                files.push(p);
            }
        }
    }
    files.sort();
    let mut h = Sha256::new();
    for f in files {
        h.update(crate::fsutil::slash_path(f.strip_prefix(root)?).as_bytes());
        h.update([0]);
        h.update(std::fs::read(&f)?);
        h.update([0]);
    }
    Ok(crate::task::hex(&h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        let root = std::env::temp_dir().join(format!("hbench-taskbuild-{}", uuid::Uuid::new_v4().simple()));
        for d in ["taskbuild", "datasets/ds", "tasks/ds/t1", "tasks/ds/t2/files", "tasks/other/t3"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("taskbuild/__main__.py"), "").unwrap();
        std::fs::write(root.join("datasets/ds/dataset.yaml"), "current: '0.1'\n").unwrap();
        for t in ["tasks/ds/t1", "tasks/ds/t2", "tasks/other/t3"] {
            std::fs::write(root.join(t).join("task.yaml"), "name: x\n").unwrap();
        }
        root
    }

    #[test]
    fn finds_a_datasets_sources_and_one_tasks() {
        let root = repo();
        let at = |p: &str| sources(&root.join(p));
        assert_eq!(at("tasks/ds"), Some(Sources { root: root.clone(), dataset: "ds".into(), task: None }));
        assert_eq!(at("tasks/ds/t1"), Some(Sources { root: root.clone(), dataset: "ds".into(), task: Some("t1".into()) }));
        // No datasets/other/dataset.yaml, a folder of files, a built task: not sources.
        assert_eq!(at("tasks/other"), None);
        assert_eq!(at("tasks/ds/t2/files"), None);
        std::fs::write(root.join("tasks/ds/t1/task.toml"), "").unwrap();
        assert_eq!(at("tasks/ds/t1"), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Builds real sources: `HBENCH_TASKBUILD_SOURCES=<repo>/tasks/<dataset> cargo test taskbuild -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn builds_real_sources() {
        let Some(dir) = std::env::var_os("HBENCH_TASKBUILD_SOURCES").map(PathBuf::from) else { return };
        let s = sources(&dir).expect("task sources");
        let built = built(&s).await.unwrap();
        let n = std::fs::read_dir(&built).unwrap().flatten().filter(|e| e.path().join("task.toml").is_file()).count();
        assert!(n > 0, "{}", built.display());
        // The same sources build to the same folder.
        assert_eq!(build(&s).await.unwrap(), built);
        if let Some(t) = std::fs::read_dir(&dir).unwrap().flatten().find(|e| e.path().join("task.yaml").is_file()) {
            let one = sources(&t.path()).unwrap();
            assert!(built_one_ok(&one).await);
        }
    }

    async fn built_one_ok(s: &Sources) -> bool {
        built(s).await.is_ok_and(|p| p.join("task.toml").is_file())
    }

    #[test]
    fn the_digest_follows_paths_and_contents() {
        let root = repo();
        let d = |p: &str| tree_digest(&root.join(p)).unwrap();
        let before = d("tasks/ds");
        assert_eq!(before, d("tasks/ds"));
        std::fs::write(root.join("tasks/ds/t1/task.yaml"), "name: y\n").unwrap();
        assert_ne!(before, d("tasks/ds"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
