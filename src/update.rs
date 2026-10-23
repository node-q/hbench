//! Tells an interactive user when a newer hbench release is out. Reads the latest release from a file under
//! ~/.cache/hbench and, at most once a day, refreshes that file with a background curl, so the check never waits on
//! the network. `HBENCH_NO_UPDATE_CHECK=1` or `CI` turns it off.

use crate::build_info::VERSION;
use std::io::IsTerminal;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

const LATEST_URL: &str = "https://api.github.com/repos/node-q/hbench/releases/latest";
const INSTALL: &str = "curl -fsSL https://github.com/node-q/hbench/releases/latest/download/install.sh | sh";
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Prints a notice on stderr when the cached latest release is newer than this build, then refreshes a stale cache.
pub fn notify() {
    if std::env::var_os("HBENCH_NO_UPDATE_CHECK").is_some_and(|v| v != "0")
        || std::env::var_os("CI").is_some()
        || !std::io::stderr().is_terminal()
    {
        return;
    }
    let path = crate::env::cache_root().join("latest-release.json");
    let latest = std::fs::read_to_string(&path).ok().and_then(|s| tag(&s));
    if let Some(latest) = latest.filter(|l| newer(l, VERSION)) {
        eprintln!("hbench {latest} is available (you have {VERSION}); run `hbench update`");
    }
    let age = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
    if age.is_none_or(|a| a > MAX_AGE) && std::fs::create_dir_all(path.parent().unwrap()).is_ok() {
        // ponytail: curl runs on after hbench exits; a failed fetch leaves an empty file and retries tomorrow.
        if let Ok(out) = std::fs::File::create(&path) {
            let _ = Command::new("curl")
                .args(["-fsSL", "--max-time", "10", LATEST_URL])
                .stdin(Stdio::null())
                .stdout(out)
                .stderr(Stdio::null())
                .spawn();
        }
    }
}

/// `hbench update`: runs the install script, which replaces this binary with the latest release.
pub fn run() -> anyhow::Result<std::process::ExitCode> {
    let status = Command::new("sh").args(["-c", INSTALL]).status()?;
    Ok(if status.success() { std::process::ExitCode::SUCCESS } else { std::process::ExitCode::FAILURE })
}

/// The release's version, `tag_name` without its `v`.
fn tag(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(v["tag_name"].as_str()?.trim_start_matches('v').to_string())
}

/// Whether `a` is a later `major.minor.patch` than `b`; a `-suffix` is ignored.
fn newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Option<Vec<u64>> { s.split('-').next()?.split('.').map(|p| p.parse().ok()).collect() };
    matches!((parse(a), parse(b)), (Some(a), Some(b)) if a > b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.4.0", "0.3.0"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1.0.0", "0.3.0-rc.1"));
        assert!(!newer("0.3.0", "0.3.0"));
        assert!(!newer("0.2.9", "0.3.0"));
        assert!(!newer("garbage", "0.3.0"));
    }

    #[test]
    fn reads_the_tag() {
        assert_eq!(tag(r#"{"tag_name":"v0.4.0","name":"x"}"#).as_deref(), Some("0.4.0"));
        assert_eq!(tag(""), None);
    }
}
