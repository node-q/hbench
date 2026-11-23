//! hbench's own local log, for tracing a user's error later: one line per event (see the `locallog` crate),
//! redacted, in a file of at most 5 × 1 MB. It is written for any command given `--log-file <path>` (or
//! `HBENCH_LOG_FILE`, which the desktop app sets), and by default for the long ones (runs, sweeps,
//! `datasets list`) to `<data dir>/hbench/logs/hbench.log`. Stdout and stderr are unchanged. Nothing is sent.
//!
//! Events never carry task content, prompts or answers: only what ran, how it ended, and why it failed.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;
use std::time::Instant;

pub const ENV: &str = "HBENCH_LOG_FILE";

/// Commands logged without `--log-file`: the ones that run for a while and fail in ways worth tracing.
const LONG: &[&str] = &["run", "jobs start", "jobs resume", "sweeps run", "datasets list", "trials start"];

struct Started {
    command: String,
    at: Instant,
}

static STARTED: OnceLock<Started> = OnceLock::new();

/// `datasets list` from the parsed command line: the subcommand names, never their arguments.
pub fn command_name(m: &clap::ArgMatches) -> String {
    let mut parts = vec![];
    let mut cur = m;
    while let Some((name, sub)) = cur.subcommand() {
        parts.push(name.to_string());
        cur = sub;
    }
    parts.join(" ")
}

/// The default log: `<data dir>/hbench/logs/hbench.log`.
pub fn default_path() -> Option<PathBuf> {
    locallog::data_dir("hbench").map(|d| d.join("logs").join("hbench.log"))
}

/// Where this command logs, if anywhere: the flag, then the environment, then the default for a long command.
fn path_for(command: &str, flag: Option<&Path>, env: Option<PathBuf>) -> Option<PathBuf> {
    flag.map(Path::to_path_buf).or(env).or_else(|| LONG.contains(&command).then(default_path).flatten())
}

/// Start logging for this command, if it logs, and record its start.
pub fn start(m: &clap::ArgMatches, flag: Option<&Path>) {
    let command = command_name(m);
    let env = std::env::var_os(ENV).filter(|v| !v.is_empty()).map(PathBuf::from);
    let Some(path) = path_for(&command, flag, env) else { return };
    let home = crate::fsutil::home_dir().map(|h| h.to_string_lossy().into_owned());
    let sink = locallog::Sink::new(locallog::Rotating::new(path), format!("hbench/{}", env!("CARGO_PKG_VERSION")), home)
        .with_scrubber(crate::redact::text);
    if locallog::init(sink) {
        tracing::info!(event = "command.start", command = %command);
        let _ = STARTED.set(Started { command, at: Instant::now() });
    }
}

/// Record how the command ended.
pub fn end(code: &ExitCode, error: Option<&anyhow::Error>) {
    let Some(s) = STARTED.get() else { return };
    let ms = s.at.elapsed().as_millis() as u64;
    match error {
        Some(e) => tracing::error!(event = "command.end", command = %s.command, ms, outcome = "failed", error = %format!("{e:#}")),
        None => {
            let outcome = if *code == ExitCode::SUCCESS { "ok" } else { "failed" };
            tracing::info!(event = "command.end", command = %s.command, ms, outcome)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_commands_log_by_default_and_any_command_logs_when_asked() {
        let default = default_path();
        assert_eq!(path_for("run", None, None), default);
        assert_eq!(path_for("datasets list", None, None), default);
        assert_eq!(path_for("jobs resume", None, None), default);
        assert_eq!(path_for("datasets show", None, None), None);
        assert_eq!(path_for("view", None, None), None);
        let flag = PathBuf::from("/tmp/x.log");
        assert_eq!(path_for("view", Some(&flag), Some("/tmp/env.log".into())), Some(flag));
        assert_eq!(path_for("view", None, Some("/tmp/env.log".into())), Some("/tmp/env.log".into()));
        assert!(default.unwrap().ends_with("hbench/logs/hbench.log"));
    }

    #[test]
    fn the_command_name_has_no_arguments() {
        use clap::{CommandFactory, FromArgMatches};
        let m = crate::Cli::command().try_get_matches_from(["hbench", "datasets", "list", "--registry", "secret-registry"]).unwrap();
        assert!(crate::Cli::from_arg_matches(&m).is_ok());
        assert_eq!(command_name(&m), "datasets list");
        let m = crate::Cli::command().try_get_matches_from(["hbench", "--log-file", "/tmp/a.log", "view", "jobs"]).unwrap();
        assert_eq!(command_name(&m), "view");
    }
}
