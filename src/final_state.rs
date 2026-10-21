//! A trial's final state: what the agent changed in the task's container, saved with the trial so its
//! verifier can be run again later without the agent.
//!
//! Only for a shared verifier, which grades the agent's own container; a separate verifier grades the
//! trial's artifacts, which are already saved. `final_state/changes.json` lists every path changed since
//! the agent started (by inode change time, which no one in the container can set back) and every path
//! deleted since (by `docker diff`), with its kind, mode and owner. `final_state/files/` holds the content
//! of the changed files, as it was when the verifier started, unless it is a cache or over the limits.
//! The files are plain files, so the trial's secret scrubbing and the job's manifest cover them.

use crate::env::{DockerEnv, quote};
use crate::task::MAIN;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

const DIR: &str = "final_state";
const CHANGES: &str = "changes.json";
const FILES: &str = "files";

const MAX_SAVED_FILES: usize = 500;
const MAX_SAVED_BYTES: u64 = 8 << 20;
const MAX_LISTED: usize = 10_000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const COPY_TIMEOUT: Duration = Duration::from_secs(600);

/// Not the task's state: the kernel's and Docker's mounts and files, and hbench's log dirs.
const NOT_STATE: &[&str] = &["/proc", "/sys", "/dev", "/run", "/logs"];
const DOCKER_FILES: &[&str] = &["/", "/.dockerenv", "/etc/hostname", "/etc/hosts", "/etc/mtab", "/etc/resolv.conf"];
/// Rebuilt by whatever reads them, so their content is listed but not saved.
const CACHES: &[&str] = &["/var/cache/", "/var/lib/apt/lists/", "/var/log/", "/.cache/", "/.npm/", "/__pycache__/"];

const FIELDS: usize = 7;
const LISTING: &str = r"-printf '%y\0%m\0%U\0%G\0%s\0%p\0%l\0'";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Directory,
    Symlink,
    Other,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeftOut {
    Cache,
    Limit,
    Name,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub uid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub gid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub left_out: Option<LeftOut>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    /// The container's clock when the agent started, in Unix seconds.
    pub since: String,
    /// Every change is listed and every changed file but a cache is saved, so the state can be rebuilt.
    pub complete: bool,
    pub not_listed: usize,
    pub changes: Vec<Change>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
}

/// What the container looked like as the agent started.
pub struct Start {
    since: String,
    deleted: BTreeSet<String>,
}

pub async fn start(env: &DockerEnv) -> Result<Start> {
    let r = env.exec_in("date +%s.%N", Some("root"), &BTreeMap::new(), Some(COMMAND_TIMEOUT), Some("/")).await?;
    let since = unix_time(r.stdout.trim()).ok_or_else(|| anyhow::anyhow!("the container's clock printed {:?}", r.stdout.trim()))?;
    Ok(Start { since, deleted: env.deleted_paths(MAIN).await? })
}

/// `date +%s.%N`'s output, or its whole seconds where `date` has no `%N`.
fn unix_time(printed: &str) -> Option<String> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (secs, fraction) = printed.split_once('.').unwrap_or((printed, ""));
    digits(secs).then(|| if digits(fraction) { format!("{secs}.{fraction}") } else { secs.to_string() })
}

/// Saves the final state into `trial_dir/final_state`; a failure is recorded in its changes.json.
pub async fn save(env: &DockerEnv, start: &Start, trial_dir: &Path) -> Result<Changes> {
    let dir = trial_dir.join(DIR);
    std::fs::create_dir_all(&dir)?;
    let changes = match collect(env, start, &dir.join(FILES)).await {
        Ok(c) => c,
        Err(e) => Changes { since: start.since.clone(), complete: false, not_listed: 0, changes: vec![], error: Some(format!("{e:#}")) },
    };
    crate::trial::write_atomic(&dir.join(CHANGES), serde_json::to_vec_pretty(&changes)?)?;
    Ok(changes)
}

async fn collect(env: &DockerEnv, start: &Start, files_dir: &Path) -> Result<Changes> {
    let temp = Temp::new();
    let r = async {
        let mut changes = plan(list(env, start, &temp).await?, deleted_since(env, start).await?, &start.since);
        let paths: Vec<String> = changes.changes.iter().filter(|c| to_save(c)).map(|c| c.path.clone()).collect();
        if !paths.is_empty()
            && let Err(e) = copy_out(env, &paths, files_dir, &temp).await
        {
            let _ = std::fs::remove_dir_all(files_dir);
            changes.complete = false;
            changes.error = Some(format!("{e:#}"));
            for c in changes.changes.iter_mut().filter(|c| to_save(c)) {
                c.left_out = Some(LeftOut::Failed);
            }
        }
        Ok(changes)
    }
    .await;
    temp.remove(env).await;
    r
}

fn to_save(c: &Change) -> bool {
    c.kind == Kind::File && c.left_out.is_none()
}

/// hbench's own files in the container and on the host while it saves a final state.
struct Temp {
    listing: String,
    names: String,
    tar: String,
    host_dir: std::path::PathBuf,
}

impl Temp {
    fn new() -> Temp {
        let id = uuid::Uuid::new_v4().simple();
        let at = |ext: &str| format!("/logs/.hbench-final-state-{id}.{ext}");
        Temp {
            listing: at("listing"),
            names: at("names"),
            tar: at("tar"),
            host_dir: std::env::temp_dir().join(format!("hbench-final-state-{id}")),
        }
    }

    async fn download(&self, env: &DockerEnv, path: &str) -> Result<Vec<u8>> {
        let local = self.host_dir.join(Path::new(path).file_name().unwrap_or_default());
        env.download_file(MAIN, path, &local).await?;
        Ok(std::fs::read(&local)?)
    }

    async fn remove(&self, env: &DockerEnv) {
        let _ = std::fs::remove_dir_all(&self.host_dir);
        let rm = format!("rm -f {} {} {}", quote(&self.listing), quote(&self.names), quote(&self.tar));
        let _ = env.exec_in(&rm, Some("root"), &BTreeMap::new(), Some(COMMAND_TIMEOUT), Some("/")).await;
    }
}

async fn list(env: &DockerEnv, start: &Start, temp: &Temp) -> Result<Vec<Change>> {
    let prune: Vec<String> = NOT_STATE.iter().map(|p| format!("-path {}", quote(p))).collect();
    let cmd =
        format!("find / -xdev \\( {} \\) -prune -o -newerct @{} {LISTING} > {}", prune.join(" -o "), start.since, quote(&temp.listing));
    let r = env.exec_in(&cmd, Some("root"), &BTreeMap::new(), Some(COMMAND_TIMEOUT), Some("/")).await?;
    if r.code != 0 {
        bail!("listing the changed files failed: {}", r.stderr.trim());
    }
    parse_listing(&String::from_utf8_lossy(&temp.download(env, &temp.listing).await?))
}

async fn deleted_since(env: &DockerEnv, start: &Start) -> Result<Vec<String>> {
    Ok(env.deleted_paths(MAIN).await?.difference(&start.deleted).cloned().collect())
}

/// The NUL-separated fields of `find -printf` for each path: kind, mode, uid, gid, size, path, link target.
fn parse_listing(out: &str) -> Result<Vec<Change>> {
    let fields: Vec<&str> = out.split('\0').collect();
    let Some((last, fields)) = fields.split_last() else { return Ok(vec![]) };
    if !last.trim().is_empty() || fields.len() % FIELDS != 0 {
        bail!("the changed-file listing is not {FIELDS} fields per path");
    }
    fields
        .as_chunks::<FIELDS>()
        .0
        .iter()
        .map(|f| {
            let kind = match f[0].trim() {
                "f" => Kind::File,
                "d" => Kind::Directory,
                "l" => Kind::Symlink,
                _ => Kind::Other,
            };
            Ok(Change {
                path: f[5].to_string(),
                kind,
                mode: (kind != Kind::Symlink).then(|| f[1].to_string()),
                uid: Some(f[2].parse()?),
                gid: Some(f[3].parse()?),
                size: (kind == Kind::File).then(|| f[4].parse()).transpose()?,
                target: (kind == Kind::Symlink).then(|| f[6].to_string()),
                left_out: None,
            })
        })
        .collect()
}

fn is_cache(path: &str) -> bool {
    CACHES.iter().any(|c| path.contains(c))
}

fn not_state(path: &str) -> bool {
    DOCKER_FILES.contains(&path) || NOT_STATE.iter().any(|p| path == *p || path.strip_prefix(p).is_some_and(|r| r.starts_with('/')))
}

/// A name hbench can hand to tar and read back unchanged.
fn plain_name(path: &str) -> bool {
    path.starts_with('/') && !path.chars().any(|c| c.is_control() || c == '\\' || c == char::REPLACEMENT_CHARACTER)
}

/// The changes to record, in path order, and which files' content to save.
fn plan(listed: Vec<Change>, deleted: Vec<String>, since: &str) -> Changes {
    let mut changes: Vec<Change> = listed
        .into_iter()
        .chain(deleted.into_iter().map(|path| Change {
            path,
            kind: Kind::Deleted,
            mode: None,
            uid: None,
            gid: None,
            size: None,
            target: None,
            left_out: None,
        }))
        .filter(|c| !not_state(&c.path))
        .collect();
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    let not_listed = changes.len().saturating_sub(MAX_LISTED);
    changes.truncate(MAX_LISTED);
    for c in changes.iter_mut().filter(|c| c.kind == Kind::File) {
        if is_cache(&c.path) {
            c.left_out = Some(LeftOut::Cache);
        } else if !plain_name(&c.path) {
            c.left_out = Some(LeftOut::Name);
        }
    }
    let saving: Vec<&Change> = changes.iter().filter(|c| to_save(c)).collect();
    let bytes: u64 = saving.iter().filter_map(|c| c.size).sum();
    if saving.len() > MAX_SAVED_FILES || bytes > MAX_SAVED_BYTES {
        for c in changes.iter_mut().filter(|c| to_save(c)) {
            c.left_out = Some(LeftOut::Limit);
        }
    }
    let complete = not_listed == 0 && !changes.iter().any(|c| matches!(c.left_out, Some(LeftOut::Limit | LeftOut::Name)));
    Changes { since: since.to_string(), complete, not_listed, changes, error: None }
}

/// Tars the files in the container, then extracts them on the host once the archive holds nothing else.
async fn copy_out(env: &DockerEnv, paths: &[String], files_dir: &Path, temp: &Temp) -> Result<()> {
    let names: Vec<&str> = paths.iter().map(|p| p.trim_start_matches('/')).collect();
    std::fs::create_dir_all(&temp.host_dir)?;
    let local_names = temp.host_dir.join("names");
    std::fs::write(&local_names, format!("{}\0", names.join("\0")))?;
    env.upload_file(&local_names, &temp.names).await?;
    let cmd = format!("tar -C / --no-recursion --hard-dereference --null -T {} -cf {}", quote(&temp.names), quote(&temp.tar));
    let r = env.exec_in(&cmd, Some("root"), &BTreeMap::new(), Some(COPY_TIMEOUT), Some("/")).await?;
    if r.code != 0 {
        bail!("tar in the container failed: {}", r.stderr.trim());
    }
    let local_tar = temp.host_dir.join("files.tar");
    env.download_file(MAIN, &temp.tar, &local_tar).await?;
    check_archive(&local_tar, &names).await?;
    std::fs::create_dir_all(files_dir)?;
    let out = Command::new("tar").arg("-xf").arg(&local_tar).arg("-C").arg(files_dir).output().await?;
    if !out.status.success() {
        bail!("extracting the final state failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// The container wrote the archive, so it holds only regular files, each one asked for.
async fn check_archive(tar: &Path, names: &[&str]) -> Result<()> {
    let out = Command::new("tar").arg("-tvf").arg(tar).output().await?;
    if !out.status.success() {
        bail!("reading the final state's archive failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let listing = String::from_utf8_lossy(&out.stdout);
    if let Some(line) = listing.lines().find(|l| !l.starts_with('-')) {
        bail!("the final state's archive holds more than regular files: {line}");
    }
    let out = Command::new("tar").arg("-tf").arg(tar).output().await?;
    let wanted: BTreeSet<&str> = names.iter().copied().collect();
    let held: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
    if let Some(n) = held.iter().find(|n| !wanted.contains(n.as_str())) {
        bail!("the final state's archive holds a file hbench did not ask for: {n}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(rows: &[[&str; FIELDS]]) -> String {
        rows.iter().map(|r| format!("{}\0", r.join("\0"))).collect()
    }

    fn file(path: &str, size: u64) -> Change {
        Change {
            path: path.into(),
            kind: Kind::File,
            mode: Some("644".into()),
            uid: Some(0),
            gid: Some(0),
            size: Some(size),
            target: None,
            left_out: None,
        }
    }

    #[test]
    fn the_agents_start_is_read_to_the_nanosecond_where_date_can() {
        assert_eq!(unix_time("1792589659.550800818").as_deref(), Some("1792589659.550800818"));
        assert_eq!(unix_time("1792589659.%N").as_deref(), Some("1792589659"));
        assert_eq!(unix_time("1792589659").as_deref(), Some("1792589659"));
        assert_eq!(unix_time("Tue Oct 21"), None);
    }

    #[test]
    fn a_listing_has_each_paths_kind_mode_owner_size_and_target() {
        let out = listing(&[
            ["f", "755", "1000", "1000", "4", "/app/run.sh", ""],
            ["d", "755", "0", "0", "40", "/app", ""],
            ["l", "777", "0", "0", "7", "/app/link", "run.sh"],
            ["p", "644", "0", "0", "0", "/app/fifo", ""],
        ]);
        let got = parse_listing(&out).unwrap();
        assert_eq!(got[0], Change { mode: Some("755".into()), uid: Some(1000), gid: Some(1000), ..file("/app/run.sh", 4) });
        assert_eq!((got[1].kind, got[1].size), (Kind::Directory, None));
        assert_eq!((got[2].kind, got[2].mode.as_deref(), got[2].target.as_deref()), (Kind::Symlink, None, Some("run.sh")));
        assert_eq!(got[3].kind, Kind::Other);
        assert!(parse_listing("").unwrap().is_empty());
        assert!(parse_listing("f\x00644\x00").is_err());
    }

    #[test]
    fn a_plan_lists_changes_and_deletions_in_order_without_dockers_files() {
        let listed = vec![file("/app/b.txt", 1), file("/etc/hosts", 1), file("/logs/agent/x", 1), file("/logsx/y", 1)];
        let got = plan(listed, vec!["/app/a.txt".into(), "/run/lock".into()], "100");
        let paths: Vec<(&str, Kind)> = got.changes.iter().map(|c| (c.path.as_str(), c.kind)).collect();
        assert_eq!(paths, [("/app/a.txt", Kind::Deleted), ("/app/b.txt", Kind::File), ("/logsx/y", Kind::File)]);
        assert!(got.complete && got.since == "100" && got.not_listed == 0);
    }

    #[test]
    fn caches_are_listed_but_not_saved_and_keep_a_state_complete() {
        let got = plan(vec![file("/root/.cache/pip/x", 1), file("/app/__pycache__/m.pyc", 1), file("/app/m.py", 1)], vec![], "0");
        let left: Vec<Option<LeftOut>> = got.changes.iter().map(|c| c.left_out).collect();
        assert_eq!(left, [Some(LeftOut::Cache), None, Some(LeftOut::Cache)]);
        assert!(got.complete);
    }

    #[test]
    fn over_the_limits_no_file_is_saved_and_the_state_is_incomplete() {
        let many: Vec<Change> = (0..=MAX_SAVED_FILES).map(|i| file(&format!("/app/{i}"), 1)).collect();
        let got = plan(many, vec![], "0");
        assert!(!got.complete && got.changes.iter().all(|c| c.left_out == Some(LeftOut::Limit)));
        let big = plan(vec![file("/app/a", MAX_SAVED_BYTES), file("/app/b", 1)], vec![], "0");
        assert!(!big.complete && big.changes.iter().all(|c| c.left_out == Some(LeftOut::Limit)));
        assert!(plan(vec![file("/app/a", MAX_SAVED_BYTES)], vec![], "0").complete);
    }

    #[test]
    fn a_name_tar_could_not_read_back_is_not_saved() {
        let got = plan(vec![file("/app/a\nb", 1), file("/app/c\\d", 1), file("/app/ok", 1)], vec![], "0");
        let left: Vec<Option<LeftOut>> = got.changes.iter().map(|c| c.left_out).collect();
        assert_eq!(left, [Some(LeftOut::Name), Some(LeftOut::Name), None]);
        assert!(!got.complete);
    }

    #[test]
    fn a_long_listing_is_cut_and_the_state_is_incomplete() {
        let many: Vec<Change> =
            (0..MAX_LISTED + 3).map(|i| Change { kind: Kind::Directory, size: None, ..file(&format!("/d{i}"), 0) }).collect();
        let got = plan(many, vec![], "0");
        assert_eq!((got.changes.len(), got.not_listed, got.complete), (MAX_LISTED, 3, false));
    }

    #[tokio::test]
    async fn an_archive_with_a_link_or_an_unasked_file_is_refused() {
        let dir = std::env::temp_dir().join(format!("hbench-final-state-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src/app")).unwrap();
        std::fs::write(dir.join("src/app/a.txt"), "a").unwrap();
        std::fs::write(dir.join("src/app/b.txt"), "b").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.join("src/app/l")).unwrap();
        let tar = |name: &str, members: &[&str]| {
            let out = dir.join(name);
            let ok = std::process::Command::new("tar").arg("-C").arg(dir.join("src")).arg("-cf").arg(&out).args(members).status().unwrap();
            assert!(ok.success());
            out
        };
        assert!(check_archive(&tar("ok.tar", &["app/a.txt"]), &["app/a.txt"]).await.is_ok());
        let e = check_archive(&tar("link.tar", &["app/a.txt", "app/l"]), &["app/a.txt", "app/l"]).await.unwrap_err();
        assert!(format!("{e}").contains("more than regular files"), "{e}");
        let e = check_archive(&tar("extra.tar", &["app/a.txt", "app/b.txt"]), &["app/a.txt"]).await.unwrap_err();
        assert!(format!("{e}").contains("did not ask for"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
