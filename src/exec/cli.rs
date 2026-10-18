//! CLI arguments and entry point.

use super::*;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};

const EXPERIMENTAL_WARNING: &str = "`hbench exec` is experimental; flags and behavior may change.";

/// Compile paths into tasks and run a job.
#[derive(clap::Args, Debug, Default)]
pub struct ExecArgs {
    /// ExecConfig YAML, JSON, or TOML path. Cannot be combined with flags mode.
    #[arg(short = 'c', long = "config", help_heading = "Config")]
    pub config: Option<PathBuf>,
    /// File, directory, or glob pattern to copy into the task environment. Repeatable.
    #[arg(short = 'p', long = "path", help_heading = "Task Compilation")]
    pub paths: Vec<String>,
    /// Fan out paths into tasks. Globs produce one task per match; directories produce one task per immediate subdirectory.
    #[arg(long, overrides_with = "no_scan", help_heading = "Task Compilation")]
    pub scan: bool,
    #[arg(long, overrides_with = "scan", hide = true)]
    pub no_scan: bool,
    /// Maximum number of scanned path matches to include.
    #[arg(short = 'l', long, value_parser = clap::value_parser!(u64).range(1..), help_heading = "Task Compilation")]
    pub limit: Option<u64>,
    /// Inline task instruction. If --artifact is not set, artifacts are auto-inferred from file paths mentioned in the instruction.
    #[arg(short = 'i', long = "instruction", visible_alias = "prompt", help_heading = "Task Compilation")]
    pub instruction: Option<String>,
    /// Markdown or text path containing the task instruction.
    #[arg(long, help_heading = "Task Compilation")]
    pub instruction_path: Option<String>,
    /// Optional task template directory.
    #[arg(long, help_heading = "Task Compilation")]
    pub task_template: Option<String>,
    /// Task Docker image. Defaults to ubuntu:latest in flags mode.
    #[arg(long, help_heading = "Task Compilation")]
    pub image: Option<String>,
    /// Task working directory. Defaults to /app in flags mode.
    #[arg(long, help_heading = "Task Compilation")]
    pub workdir: Option<String>,
    /// Artifact path to collect after each trial. Repeatable. Overrides auto-inferred artifacts. If unset, artifacts are auto-inferred from file paths mentioned in the instruction; use --print-config to inspect them.
    #[arg(short = 'f', long = "artifact", help_heading = "Task Compilation")]
    pub artifact: Vec<String>,
    /// Artifact path to promote to reward.json after verification. The file must be a JSON object mapping string keys to numbers; those keys become reward labels. Also collected as an artifact.
    #[arg(long, help_heading = "Task Compilation")]
    pub reward_artifact: Option<String>,
    /// Disable existence-only artifact verification and task-template verification.
    #[arg(long, help_heading = "Task Compilation")]
    pub disable_verification: bool,
    /// Directory to save compiled tasks after task compilation. If omitted, compiled tasks are ephemeral and cleaned up after execution.
    #[arg(long, help_heading = "Task Compilation")]
    pub tasks_dir: Option<String>,
    /// Execution environment type, e.g. docker or daytona.
    #[arg(short = 'e', long = "env", help_heading = "Jobs")]
    pub environment: Option<String>,
    /// Maximum concurrent trials.
    #[arg(short = 'n', long = "n-concurrent", help_heading = "Jobs")]
    pub n_concurrent: Option<i64>,
    /// Maximum number of retry attempts.
    #[arg(short = 'r', long = "max-retries", help_heading = "Jobs")]
    pub max_retries: Option<u32>,
    /// Directory to store map and reduce job results. Defaults to jobs.
    #[arg(long, help_heading = "Jobs")]
    pub jobs_dir: Option<String>,
    /// Suppress trial progress.
    #[arg(short = 'q', long, help_heading = "Jobs")]
    pub quiet: bool,
    /// Agent to run.
    #[arg(short = 'a', long, help_heading = "Map Job")]
    pub agent: Option<String>,
    /// Model name for the agent. Repeatable.
    #[arg(short = 'm', long = "model", help_heading = "Map Job")]
    pub models: Vec<String>,
    /// Agent kwarg key=value. Repeatable.
    #[arg(long = "ak", visible_alias = "agent-kwarg", help_heading = "Map Job")]
    pub agent_kwargs: Vec<String>,
    /// Agent env var KEY=VALUE. Repeatable.
    #[arg(long = "ae", visible_alias = "agent-env", help_heading = "Map Job")]
    pub agent_env: Vec<String>,
    /// Agent execution timeout in seconds (overrides task default).
    #[arg(long = "agent-timeout", help_heading = "Map Job")]
    pub agent_timeout_sec: Option<f64>,
    /// Attempts per task.
    #[arg(short = 'k', long = "n-attempts", help_heading = "Map Job")]
    pub n_attempts: Option<i64>,
    /// Map job name.
    #[arg(long, help_heading = "Map Job")]
    pub job_name: Option<String>,
    /// Inline reducer task instruction. If --reduce-artifact is not set, reducer artifacts are auto-inferred from file paths mentioned in the reducer instruction.
    #[arg(long = "ri", visible_aliases = ["reduce-instruction", "reduce-prompt"], help_heading = "Reduce Task")]
    pub reduce_instruction: Option<String>,
    /// Markdown or text path containing the reducer instruction.
    #[arg(long, help_heading = "Reduce Task")]
    pub reduce_instruction_path: Option<String>,
    /// Optional task template directory for the reducer.
    #[arg(long, help_heading = "Reduce Task")]
    pub reduce_task_template: Option<String>,
    /// Reducer Docker image. Defaults to ubuntu:latest.
    #[arg(long, help_heading = "Reduce Task")]
    pub reduce_image: Option<String>,
    /// Reducer working directory. Defaults to /app.
    #[arg(long, help_heading = "Reduce Task")]
    pub reduce_workdir: Option<String>,
    /// Reducer artifact path to collect. Repeatable. Overrides auto-inferred reducer artifacts.
    #[arg(long, help_heading = "Reduce Task")]
    pub reduce_artifact: Vec<String>,
    /// Reducer artifact path to promote to reward.json after verification. Also collected as a reducer artifact.
    #[arg(long, help_heading = "Reduce Task")]
    pub reduce_reward_artifact: Option<String>,
    /// Reducer agent to run. Defaults to the map job.
    #[arg(long, help_heading = "Reduce Job")]
    pub reduce_agent: Option<String>,
    /// Reducer model name. Repeatable. Defaults to the map job.
    #[arg(long = "reduce-model", help_heading = "Reduce Job")]
    pub reduce_models: Vec<String>,
    /// Reducer agent kwarg key=value. Repeatable. Defaults to the map job.
    #[arg(long = "reduce-ak", visible_alias = "reduce-agent-kwarg", help_heading = "Reduce Job")]
    pub reduce_agent_kwargs: Vec<String>,
    /// Reducer agent env var KEY=VALUE. Repeatable. Defaults to the map job.
    #[arg(long = "reduce-ae", visible_alias = "reduce-agent-env", help_heading = "Reduce Job")]
    pub reduce_agent_env: Vec<String>,
    /// Reducer job name.
    #[arg(long, help_heading = "Reduce Job")]
    pub reduce_job_name: Option<String>,
    /// Attempts for the reducer task.
    #[arg(long = "rk", visible_alias = "n-reduce-attempts", value_parser = clap::value_parser!(i64).range(1..), help_heading = "Reduce Job")]
    pub reduce_n_attempts: Option<i64>,
    /// Job plugin import path (Python plugins; not supported by hbench).
    #[arg(long = "plugin", help_heading = "Integrations")]
    pub job_plugin: Vec<String>,
    /// Plugin kwarg (not supported by hbench).
    #[arg(long = "pk", visible_alias = "plugin-kwarg", help_heading = "Integrations")]
    pub plugin_kwargs: Vec<String>,
    /// Print resolved ExecConfig JSON, including auto-inferred artifacts, and exit.
    #[arg(long, help_heading = "Config")]
    pub print_config: bool,
}

impl ExecArgs {
    pub(super) fn scan(&self) -> Option<bool> {
        if self.scan {
            Some(true)
        } else if self.no_scan {
            Some(false)
        } else {
            None
        }
    }
}

pub async fn run(a: ExecArgs) -> Result<()> {
    let config = resolve_config(&a)?;
    if a.print_config {
        println!("{}", serde_json::to_string_pretty(&print_value(&config))?);
        return Ok(());
    }
    eprintln!("Warning: {EXPERIMENTAL_WARNING}");
    execute_and_render(config, a.config.is_none() && a.tasks_dir.is_none()).await
}

pub(super) fn resolve_config(a: &ExecArgs) -> Result<ExecConfig> {
    if !a.job_plugin.is_empty() || !a.plugin_kwargs.is_empty() {
        // Job plugins are Python classes; add a Rust hook if one is ever needed.
        bail!("--plugin/--pk load Python job plugins, which hbench does not support.");
    }
    match &a.config {
        Some(p) => {
            reject_config_flag_mix(a)?;
            load_config(p)
        }
        None => config_from_flags(a),
    }
}

fn reject_config_flag_mix(a: &ExecArgs) -> Result<()> {
    let passed = !a.paths.is_empty()
        || a.scan().is_some()
        || a.limit.is_some()
        || a.instruction.is_some()
        || a.instruction_path.is_some()
        || a.task_template.is_some()
        || a.image.is_some()
        || a.workdir.is_some()
        || !a.artifact.is_empty()
        || a.reward_artifact.is_some()
        || a.disable_verification
        || a.tasks_dir.is_some()
        || a.agent.is_some()
        || !a.models.is_empty()
        || !a.agent_kwargs.is_empty()
        || !a.agent_env.is_empty()
        || a.agent_timeout_sec.is_some()
        || a.environment.is_some()
        || a.n_attempts.is_some()
        || a.n_concurrent.is_some()
        || a.max_retries.is_some()
        || a.job_name.is_some()
        || a.jobs_dir.is_some()
        || a.quiet
        || reduce_flags_passed(a);
    if passed {
        bail!("--config cannot be combined with flags mode options.");
    }
    Ok(())
}

pub(super) fn reduce_flags_passed(a: &ExecArgs) -> bool {
    a.reduce_instruction.is_some()
        || a.reduce_instruction_path.is_some()
        || a.reduce_task_template.is_some()
        || a.reduce_image.is_some()
        || a.reduce_workdir.is_some()
        || !a.reduce_artifact.is_empty()
        || a.reduce_reward_artifact.is_some()
        || reduce_agent_flags_passed(a)
        || a.reduce_job_name.is_some()
        || a.reduce_n_attempts.is_some()
}

pub(super) fn reduce_agent_flags_passed(a: &ExecArgs) -> bool {
    a.reduce_agent.is_some() || !a.reduce_models.is_empty() || !a.reduce_agent_kwargs.is_empty() || !a.reduce_agent_env.is_empty()
}

fn load_config(path: &Path) -> Result<ExecConfig> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let v: Value = match ext.as_str() {
        "yaml" | "yml" => serde_yaml_ng::from_str(&text)?,
        "json" => serde_json::from_str(&text)?,
        "toml" => Value::Object(parse_toml(&text)?),
        _ => bail!("Unsupported exec config file format: .{ext}. Use .yaml, .yml, .json, or .toml."),
    };
    let cfg: ExecConfig = serde_json::from_value(v).map_err(|e| anyhow!("Invalid exec config: {e}"))?;
    cfg.validate()?;
    Ok(cfg)
}
