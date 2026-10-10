//! `hbench exec` (experimental): compile paths + an instruction into upstream tasks, run a map
//! job over them, and optionally compile one reducer task over the map artifacts and run it.
//! Port of upstream's `hbench exec` (cli/exec.py, compile/compiler.py, exec/executor.py,
//! models/exec, models/compile). `--print-config` output and compiled task dirs match upstream.

use crate::config::{AgentConfig, EnvConfig, JobConfig, MetricConfig, RetryConfig, VerifierRunConfig};
use crate::job::{self, JobResult};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

const TPL_AUTO_VERIFY: &str = include_str!("../assets/compile/compiled-task-template/tests/auto-verify-artifacts.sh");
const TPL_AUTO_VERIFY_SCHEMAS: &str = include_str!("../assets/compile/compiled-task-template/tests/auto-verify-artifacts-with-schemas.sh");
const TPL_SCHEMA_VALIDATOR: &str = include_str!("../assets/compile/compiled-task-template/tests/validate_artifact_schemas.py");
const TPL_PROMOTE_REWARD: &str = include_str!("../assets/compile/compiled-task-template/tests/promote_reward_artifact.py");

const DEFAULT_IMAGE: &str = "ubuntu:latest";
const DEFAULT_WORKDIR: &str = "/app";
const EXPERIMENTAL_WARNING: &str = "`hbench exec` is experimental; flags and behavior may change.";

const KNOWN_EXTS: &[&str] = &[
    "md", "txt", "pdf", "png", "jpg", "jpeg", "gif", "svg", "csv", "json", "jsonl", "xml", "yaml", "yml", "toml", "py", "js", "ts", "tsx", "jsx", "sh", "html", "css", "sql", "rs", "go",
    "java", "c", "cpp", "h", "rb", "ipynb", "parquet", "xlsx", "docx", "pptx", "zip", "tar", "gz", "lock", "cfg", "ini", "log",
];
const KNOWN_DOTFILES: &[&str] = &["gitignore", "gitattributes", "env", "envrc", "bashrc", "zshrc", "editorconfig", "npmrc", "prettierrc"];
/// upstream's EnvironmentType values (accepted for config parity; hbench only runs docker).
const ENVIRONMENT_TYPES: &[&str] = &[
    "docker", "podman", "daytona", "e2b", "modal", "runloop", "langsmith", "ec2", "gke", "ack", "openshift", "novita", "apple-container", "singularity", "islo", "tensorlake", "cwsandbox",
    "use-computer", "cua-cloud", "blaxel", "opensandbox", "beam", "skypilot", "hf-sandbox", "hyperbrowser", "vercel", "runta", "prime", "kata", "mosaic", "smol", "sail",
];

// ---------------------------------------------------------------- CLI

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
    fn scan(&self) -> Option<bool> {
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

fn resolve_config(a: &ExecArgs) -> Result<ExecConfig> {
    if !a.job_plugin.is_empty() || !a.plugin_kwargs.is_empty() {
        // ponytail: upstream job plugins are Python classes; add a Rust hook if one is ever needed.
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

fn reduce_flags_passed(a: &ExecArgs) -> bool {
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

fn reduce_agent_flags_passed(a: &ExecArgs) -> bool {
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

// ---------------------------------------------------------------- models

/// Python `pathlib.Path` string: serialized the way pydantic dumps a Path.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PyPath(String);

impl PyPath {
    fn new(s: &str) -> PyPath {
        PyPath(py_path(s))
    }
}

impl<'de> Deserialize<'de> for PyPath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(PyPath::new(&String::deserialize(d)?))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompileInstruction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PyPath>,
}

impl CompileInstruction {
    fn text(t: &str) -> CompileInstruction {
        CompileInstruction { text: Some(t.into()), path: None }
    }
    fn validate(&self) -> Result<()> {
        if self.text.as_deref() == Some("") {
            bail!("1 validation error for CompileInstruction\ntext\n  String should have at least 1 character");
        }
        if self.text.is_none() == self.path.is_none() {
            bail!("1 validation error for CompileInstruction\n  Value error, Exactly one of text or path is required.");
        }
        Ok(())
    }
}

fn is_default_image(v: &Option<String>) -> bool {
    v.as_deref().is_none_or(|s| s == DEFAULT_IMAGE)
}
fn is_default_workdir(v: &Option<String>) -> bool {
    v.as_deref().is_none_or(|s| s == DEFAULT_WORKDIR)
}

/// `docker_image`/`workdir` are Options so "explicitly set" (pydantic `model_fields_set`) is known.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompileEnvironment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PyPath>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "is_default_image")]
    pub docker_image: Option<String>,
    #[serde(default, skip_serializing_if = "is_default_workdir")]
    pub workdir: Option<String>,
}

impl CompileEnvironment {
    fn image(&self) -> &str {
        self.docker_image.as_deref().unwrap_or(DEFAULT_IMAGE)
    }
    fn wd(&self) -> &str {
        self.workdir.as_deref().unwrap_or(DEFAULT_WORKDIR)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AutoVerifier {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_artifacts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reward_artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub artifact_json_schemas: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompileVerifier {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PyPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_verifier: Option<AutoVerifier>,
}

fn is_v1(s: &String) -> bool {
    s == "1.0"
}
fn v1() -> String {
    "1.0".into()
}

/// Bare strings in `instructions` mean `{text: ...}`.
fn coerce_instructions<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<CompileInstruction>, D::Error> {
    let raw = Vec::<Value>::deserialize(d)?;
    raw.into_iter()
        .map(|v| match v {
            Value::String(s) => Ok(CompileInstruction::text(&s)),
            v => serde_json::from_value(v).map_err(serde::de::Error::custom),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompileConfig {
    #[serde(default = "v1", skip_serializing_if = "is_v1")]
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_name_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<PyPath>,
    #[serde(default, deserialize_with = "coerce_instructions", skip_serializing_if = "Vec::is_empty")]
    pub instructions: Vec<CompileInstruction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_template: Option<PyPath>,
    /// `str | ArtifactConfig` entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty", serialize_with = "ser_artifacts")]
    pub artifacts: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environments: Vec<CompileEnvironment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verifiers: Vec<CompileVerifier>,
}

impl Default for CompileConfig {
    fn default() -> Self {
        CompileConfig {
            schema_version: v1(),
            dataset_name: None,
            task_name_prefix: None,
            output_dir: None,
            instructions: vec![],
            task_template: None,
            artifacts: vec![],
            environments: vec![],
            verifiers: vec![],
        }
    }
}

/// ArtifactConfig dumped with `exclude_defaults`.
fn ser_artifacts<S: serde::Serializer>(a: &[Value], s: S) -> Result<S::Ok, S::Error> {
    let v: Vec<Value> = a
        .iter()
        .map(|x| match artifact_value(x) {
            Value::Object(mut m) => {
                if m.get("exclude").is_some_and(|e| e.as_array().is_some_and(Vec::is_empty)) {
                    m.shift_remove("exclude");
                }
                Value::Object(m)
            }
            v => v,
        })
        .collect();
    v.serialize(s)
}

fn validate_artifacts(a: &[Value]) -> Result<()> {
    for x in a {
        match x {
            Value::String(_) => {}
            Value::Object(m) if m.get("source").is_some_and(Value::is_string) => {}
            _ => bail!("Invalid artifact entry {x}: expected a path string or {{source, destination?, exclude?, service?}}"),
        }
    }
    Ok(())
}

impl CompileConfig {
    fn validate(&self) -> Result<()> {
        for i in &self.instructions {
            i.validate()?;
        }
        for e in &self.environments {
            if e.path.is_some() && !e.paths.is_empty() {
                bail!("1 validation error for CompileEnvironment\n  Value error, path and paths are mutually exclusive.");
            }
        }
        for v in &self.verifiers {
            validate_verifier(v)?;
        }
        validate_artifacts(&self.artifacts)
    }
}

fn validate_verifier(v: &CompileVerifier) -> Result<()> {
    if v.path.is_none() == v.auto_verifier.is_none() {
        bail!("1 validation error for CompileVerifier\n  Value error, Exactly one of path or auto_verifier is required.");
    }
    Ok(())
}

fn one() -> i64 {
    1
}
fn four() -> i64 {
    4
}
fn default_agents() -> Vec<Map<String, Value>> {
    vec![Map::new()]
}

/// Job settings for one phase. Job sub-configs are kept as raw JSON (validated against hbench's
/// types) so CLI/file key order survives into `--print-config`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecJobConfig {
    #[serde(default)]
    pub job_name: Option<String>,
    #[serde(default)]
    pub jobs_dir: Option<PyPath>,
    #[serde(default = "one")]
    pub n_attempts: i64,
    #[serde(default = "four")]
    pub n_concurrent_trials: i64,
    #[serde(default)]
    pub quiet: bool,
    #[serde(default)]
    pub retry: Map<String, Value>,
    #[serde(default)]
    pub environment: Map<String, Value>,
    #[serde(default)]
    pub verifier: Map<String, Value>,
    #[serde(default)]
    pub metrics: Vec<Value>,
    #[serde(default = "default_agents")]
    pub agents: Vec<Map<String, Value>>,
}

impl Default for ExecJobConfig {
    fn default() -> Self {
        serde_json::from_value(json!({})).expect("default exec job")
    }
}

impl ExecJobConfig {
    fn validate(&self) -> Result<()> {
        for (k, v) in [("n_attempts", self.n_attempts), ("n_concurrent_trials", self.n_concurrent_trials)] {
            if v < 1 {
                bail!("1 validation error for ExecJobConfig\n{k}\n  Input should be greater than or equal to 1");
            }
        }
        serde_json::from_value::<RetryConfig>(Value::Object(self.retry.clone())).context("retry")?;
        serde_json::from_value::<EnvConfig>(Value::Object(self.environment.clone())).context("environment")?;
        serde_json::from_value::<VerifierRunConfig>(Value::Object(self.verifier.clone())).context("verifier")?;
        serde_json::from_value::<Vec<MetricConfig>>(Value::Array(self.metrics.clone())).context("metrics")?;
        for a in &self.agents {
            serde_json::from_value::<AgentConfig>(Value::Object(a.clone())).context("agents")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecMapConfig {
    pub compile: CompileConfig,
    #[serde(default)]
    pub job: ExecJobConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecReduceEnvironment {
    #[serde(default = "image_default")]
    pub docker_image: String,
    #[serde(default = "workdir_default")]
    pub workdir: String,
}
fn image_default() -> String {
    DEFAULT_IMAGE.into()
}
fn workdir_default() -> String {
    DEFAULT_WORKDIR.into()
}
impl Default for ExecReduceEnvironment {
    fn default() -> Self {
        ExecReduceEnvironment { docker_image: image_default(), workdir: workdir_default() }
    }
}
fn is_default_reduce_env(e: &ExecReduceEnvironment) -> bool {
    *e == ExecReduceEnvironment::default()
}
fn reduce_name() -> String {
    "reduce".into()
}
fn is_reduce_name(s: &String) -> bool {
    s == "reduce"
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecReduceTaskConfig {
    #[serde(default = "reduce_name", skip_serializing_if = "is_reduce_name")]
    pub task_name: String,
    pub output_dir: PyPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_template: Option<PyPath>,
    pub instruction: CompileInstruction,
    #[serde(default, skip_serializing_if = "Vec::is_empty", serialize_with = "ser_artifacts")]
    pub artifacts: Vec<Value>,
    #[serde(default, skip_serializing_if = "is_default_reduce_env", serialize_with = "ser_reduce_env")]
    pub environment: ExecReduceEnvironment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier: Option<CompileVerifier>,
}

fn ser_reduce_env<S: serde::Serializer>(e: &ExecReduceEnvironment, s: S) -> Result<S::Ok, S::Error> {
    let mut m = Map::new();
    if e.docker_image != DEFAULT_IMAGE {
        m.insert("docker_image".into(), e.docker_image.clone().into());
    }
    if e.workdir != DEFAULT_WORKDIR {
        m.insert("workdir".into(), e.workdir.clone().into());
    }
    m.serialize(s)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecReduceConfig {
    pub task: ExecReduceTaskConfig,
    #[serde(default)]
    pub job: ExecJobConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecConfig {
    #[serde(default = "v1")]
    pub schema_version: String,
    pub map: ExecMapConfig,
    #[serde(default)]
    pub reduce: Option<ExecReduceConfig>,
}

impl ExecConfig {
    fn validate(&self) -> Result<()> {
        self.map.compile.validate()?;
        self.map.job.validate()?;
        if let Some(r) = &self.reduce {
            if r.task.task_name.is_empty() {
                bail!("1 validation error for ExecReduceTaskConfig\ntask_name\n  String should have at least 1 character");
            }
            r.task.instruction.validate()?;
            validate_artifacts(&r.task.artifacts)?;
            if let Some(v) = &r.task.verifier {
                validate_verifier(v)?;
            }
            r.job.validate()?;
            if self.map.compile.artifacts.is_empty() {
                bail!(
                    "1 validation error for ExecConfig\n  Value error, reduce requires map.compile.artifacts so prior map artifacts can be used as implicit reducer inputs."
                );
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- --print-config

/// `ExecConfig.model_dump(mode="json", exclude_defaults=True)`.
fn print_value(c: &ExecConfig) -> Value {
    let mut root = Map::new();
    if c.schema_version != "1.0" {
        root.insert("schema_version".into(), c.schema_version.clone().into());
    }
    let mut map = Map::new();
    map.insert("compile".into(), serde_json::to_value(&c.map.compile).unwrap_or_default());
    if let Some(j) = job_print(&c.map.job) {
        map.insert("job".into(), j);
    }
    root.insert("map".into(), Value::Object(map));
    if let Some(r) = &c.reduce {
        let mut red = Map::new();
        red.insert("task".into(), serde_json::to_value(&r.task).unwrap_or_default());
        if let Some(j) = job_print(&r.job) {
            red.insert("job".into(), j);
        }
        root.insert("reduce".into(), Value::Object(red));
    }
    Value::Object(root)
}

/// Drop keys equal to their default (or absent-from-default and empty).
fn prune(v: Value, default: &Value) -> Map<String, Value> {
    let Value::Object(m) = v else { return Map::new() };
    m.into_iter()
        .filter(|(k, val)| match default.get(k) {
            Some(d) => val != d,
            None => !(val.is_null() || val.as_array().is_some_and(Vec::is_empty) || val.as_object().is_some_and(Map::is_empty)),
        })
        .collect()
}

/// Round-trip through hbench's type (upstream field order, float coercion, env templating), then
/// restore the caller's key order for free-form maps.
fn canonical<T: serde::de::DeserializeOwned + Serialize + Default>(raw: &Map<String, Value>) -> Value {
    let mut v = serde_json::from_value::<T>(Value::Object(raw.clone())).ok().and_then(|t| serde_json::to_value(t).ok()).unwrap_or_default();
    for key in ["env", "kwargs"] {
        if let (Some(Value::Object(orig)), Some(Value::Object(out))) = (raw.get(key), v.get_mut(key)) {
            let mut ordered = Map::new();
            for k in orig.keys() {
                if let Some(x) = out.shift_remove(k) {
                    ordered.insert(k.clone(), x);
                }
            }
            ordered.append(out);
            *out = ordered;
        }
    }
    v
}

fn default_value<T: Serialize + Default>() -> Value {
    serde_json::to_value(T::default()).unwrap_or_default()
}

fn agent_print(raw: &Map<String, Value>) -> Value {
    let mut m = prune(canonical::<AgentConfig>(raw), &default_value::<AgentConfig>());
    // upstream's field default for `name` is None; the validator fills "oracle", so it always shows.
    let name = if raw.get("import_path").is_some_and(|v| !v.is_null()) && raw.get("name").is_none_or(Value::is_null) {
        None
    } else {
        Some(raw.get("name").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| "oracle".into()))
    };
    let mut out = Map::new();
    if let Some(n) = name {
        out.insert("name".into(), n);
    }
    m.shift_remove("name");
    out.append(&mut m);
    Value::Object(out)
}

/// upstream's field default for `type` is None; the validator fills "docker" unless an
/// import_path is set, so `type` shows whenever the environment differs from the default.
fn env_print(raw: &Map<String, Value>) -> Map<String, Value> {
    let mut m = prune(canonical::<EnvConfig>(raw), &default_value::<EnvConfig>());
    m.shift_remove("type");
    if m.is_empty() && raw.get("type").is_none_or(|t| t.is_null() || t == "docker") {
        return m;
    }
    let mut out = Map::new();
    match raw.get("type").filter(|t| !t.is_null()) {
        Some(t) => {
            out.insert("type".into(), t.clone());
        }
        None if raw.get("import_path").is_none_or(Value::is_null) => {
            out.insert("type".into(), "docker".into());
        }
        None => {}
    }
    out.append(&mut m);
    out
}

fn job_print(j: &ExecJobConfig) -> Option<Value> {
    let mut m = Map::new();
    if let Some(n) = &j.job_name {
        m.insert("job_name".into(), n.clone().into());
    }
    if let Some(d) = &j.jobs_dir {
        m.insert("jobs_dir".into(), d.0.clone().into());
    }
    if j.n_attempts != 1 {
        m.insert("n_attempts".into(), j.n_attempts.into());
    }
    if j.n_concurrent_trials != 4 {
        m.insert("n_concurrent_trials".into(), j.n_concurrent_trials.into());
    }
    if j.quiet {
        m.insert("quiet".into(), true.into());
    }
    let sections = [
        ("retry", prune(canonical::<RetryConfig>(&j.retry), &default_value::<RetryConfig>())),
        ("environment", env_print(&j.environment)),
        ("verifier", prune(canonical::<VerifierRunConfig>(&j.verifier), &default_value::<VerifierRunConfig>())),
    ];
    for (k, v) in sections {
        if !v.is_empty() {
            m.insert(k.into(), Value::Object(v));
        }
    }
    if !j.metrics.is_empty() {
        let d = json!({"type": "mean", "kwargs": {}});
        m.insert("metrics".into(), j.metrics.iter().map(|x| Value::Object(prune(x.clone(), &d))).collect());
    }
    let agents: Vec<Value> = j.agents.iter().map(agent_print).collect();
    if agents != [json!({"name": "oracle"})] {
        m.insert("agents".into(), agents.into());
    }
    (!m.is_empty()).then_some(Value::Object(m))
}

// ---------------------------------------------------------------- flags mode

fn config_from_flags(a: &ExecArgs) -> Result<ExecConfig> {
    let map_instructions = compile_instructions(a.instruction.as_deref(), a.instruction_path.as_deref())?;
    validate_map_task_source(&map_instructions, a.task_template.as_deref())?;

    let map_workdir = a.workdir.clone().unwrap_or_else(workdir_default);
    let reward = a.reward_artifact.as_deref().map(|r| prompt_file_to_artifact_path(r, &map_workdir));
    if reward.is_some() && a.disable_verification {
        bail!("--reward-artifact cannot be used with --disable-verification.");
    }
    let artifacts = with_reward_artifact(compile_artifacts(&a.artifact, a.instruction.as_deref(), &map_workdir), reward.as_deref());
    let verify = !a.disable_verification;
    let verifiers = compile_verifiers(&artifacts, verify, reward.as_deref());
    let tasks_output_dir = match &a.tasks_dir {
        Some(d) => PyPath::new(d),
        None => PyPath::new(&temp_root().join(format!("hbench-exec-map-tasks-{}", uuid::Uuid::new_v4().simple())).to_string_lossy()),
    };
    let timestamp = chrono::Local::now().format("%Y-%m-%d__%H-%M-%S").to_string();
    let mut retry = Map::new();
    if let Some(r) = a.max_retries {
        retry.insert("max_retries".into(), r.into());
    }
    let agents = agent_configs(a.agent.as_deref(), &a.models, &a.agent_kwargs, &a.agent_env, a.agent_timeout_sec)?;
    let environment = environment_config(a.environment.as_deref())?;
    let map_job = ExecJobConfig {
        job_name: Some(a.job_name.clone().unwrap_or_else(|| format!("{timestamp}-map"))),
        jobs_dir: Some(PyPath::new(a.jobs_dir.as_deref().unwrap_or("jobs"))),
        n_attempts: a.n_attempts.unwrap_or(1),
        n_concurrent_trials: a.n_concurrent.unwrap_or(4),
        quiet: a.quiet,
        retry,
        environment,
        verifier: verifier_config(!verifiers.is_empty(), a.task_template.as_deref(), verify),
        metrics: vec![],
        agents,
    };
    map_job.validate()?;

    let compile = CompileConfig {
        task_name_prefix: a.job_name.clone(),
        output_dir: Some(tasks_output_dir.clone()),
        instructions: map_instructions,
        task_template: a.task_template.as_deref().map(PyPath::new),
        artifacts: artifacts.into_iter().map(Value::String).collect(),
        environments: compile_environments(&a.paths, a.scan(), a.limit, a.image.as_deref(), a.workdir.as_deref())?,
        verifiers,
        ..Default::default()
    };
    let reduce = reduce_config_from_flags(a, verify, &format!("{timestamp}-reduce"), &map_job, &tasks_output_dir)?;
    let cfg = ExecConfig { schema_version: v1(), map: ExecMapConfig { compile, job: map_job }, reduce };
    cfg.validate()?;
    Ok(cfg)
}

fn reduce_config_from_flags(a: &ExecArgs, verify: bool, default_job_name: &str, map_job: &ExecJobConfig, output_dir: &PyPath) -> Result<Option<ExecReduceConfig>> {
    if !reduce_flags_passed(a) {
        return Ok(None);
    }
    let workdir = a.reduce_workdir.clone().unwrap_or_else(workdir_default);
    let reward = a.reduce_reward_artifact.as_deref().map(|r| prompt_file_to_artifact_path(r, &workdir));
    if reward.is_some() && !verify {
        bail!("--reduce-reward-artifact cannot be used with --disable-verification.");
    }
    let artifacts = with_reward_artifact(compile_artifacts(&a.reduce_artifact, a.reduce_instruction.as_deref(), &workdir), reward.as_deref());
    let verifier = compile_verifiers(&artifacts, verify, reward.as_deref()).into_iter().next();
    let instruction = match (&a.reduce_instruction, &a.reduce_instruction_path) {
        (Some(_), Some(_)) => bail!("--reduce-instruction and --reduce-instruction-path are mutually exclusive."),
        (Some(t), None) => CompileInstruction::text(t),
        (None, Some(p)) => CompileInstruction { text: None, path: Some(PyPath::new(p)) },
        (None, None) => bail!("Reducer requires --reduce-instruction/--reduce-prompt or --reduce-instruction-path."),
    };
    instruction.validate()?;
    let job = ExecJobConfig {
        job_name: Some(a.reduce_job_name.clone().unwrap_or_else(|| default_job_name.to_string())),
        jobs_dir: map_job.jobs_dir.clone(),
        n_attempts: a.reduce_n_attempts.unwrap_or(1),
        n_concurrent_trials: map_job.n_concurrent_trials,
        quiet: map_job.quiet,
        retry: map_job.retry.clone(),
        environment: map_job.environment.clone(),
        verifier: verifier_config(verifier.is_some(), a.reduce_task_template.as_deref(), verify),
        metrics: map_job.metrics.clone(),
        agents: reduce_agent_configs(a, &map_job.agents)?,
    };
    job.validate()?;
    Ok(Some(ExecReduceConfig {
        task: ExecReduceTaskConfig {
            task_name: reduce_name(),
            output_dir: output_dir.clone(),
            task_template: a.reduce_task_template.as_deref().map(PyPath::new),
            instruction,
            artifacts: artifacts.into_iter().map(Value::String).collect(),
            environment: ExecReduceEnvironment {
                docker_image: a.reduce_image.clone().unwrap_or_else(image_default),
                workdir: a.reduce_workdir.clone().unwrap_or_else(workdir_default),
            },
            verifier,
        },
        job,
    }))
}

fn compile_instructions(instruction: Option<&str>, path: Option<&str>) -> Result<Vec<CompileInstruction>> {
    let v = match (instruction, path) {
        (Some(_), Some(_)) => bail!("--instruction and --instruction-path are mutually exclusive."),
        (Some(t), None) => vec![CompileInstruction::text(t)],
        (None, Some(p)) => vec![CompileInstruction { text: None, path: Some(PyPath::new(p)) }],
        (None, None) => vec![],
    };
    for i in &v {
        i.validate()?;
    }
    Ok(v)
}

fn validate_map_task_source(instructions: &[CompileInstruction], template: Option<&str>) -> Result<()> {
    if !instructions.is_empty() {
        return Ok(());
    }
    let Some(t) = template else {
        bail!("Pass --instruction/--prompt, --instruction-path, or --task-template.");
    };
    let dir = PathBuf::from(expanduser(t));
    if dir.is_dir() && !dir.join("instruction.md").is_file() {
        bail!("--task-template must contain instruction.md when no --instruction or --instruction-path is provided.");
    }
    Ok(())
}

fn compile_artifacts(artifact: &[String], instruction: Option<&str>, workdir: &str) -> Vec<String> {
    if !artifact.is_empty() {
        return artifact.to_vec();
    }
    instruction.map(|i| infer_artifacts_from_prompt(i, workdir)).unwrap_or_default()
}

fn with_reward_artifact(mut artifacts: Vec<String>, reward: Option<&str>) -> Vec<String> {
    if let Some(r) = reward
        && !artifacts.iter().any(|a| a == r)
    {
        artifacts.push(r.to_string());
    }
    artifacts
}

pub fn infer_artifacts_from_prompt(prompt: &str, workdir: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for m in file_mentions(prompt) {
        let a = prompt_file_to_artifact_path(&m, workdir);
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out
}

fn prompt_file_to_artifact_path(filename: &str, workdir: &str) -> String {
    let f = filename.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'']);
    if f.starts_with('/') {
        return normpath(f);
    }
    let wd = if workdir.starts_with('/') { workdir.to_string() } else { format!("/{workdir}") };
    normpath(&posix_join(&wd, f))
}

fn compile_environments(paths: &[String], scan: Option<bool>, limit: Option<u64>, image: Option<&str>, workdir: Option<&str>) -> Result<Vec<CompileEnvironment>> {
    let env = |p: Vec<String>| CompileEnvironment {
        path: None,
        paths: p,
        docker_image: Some(image.unwrap_or(DEFAULT_IMAGE).into()),
        workdir: Some(workdir.unwrap_or(DEFAULT_WORKDIR).into()),
    };
    if !should_scan_paths(paths, scan) {
        if limit.is_some() {
            bail!("--limit only applies to scanned paths.");
        }
        return Ok(vec![env(paths.to_vec())]);
    }
    if paths.is_empty() {
        bail!("--scan requires at least one --path.");
    }
    Ok(scan_paths(paths, limit)?.into_iter().map(|p| env(vec![p])).collect())
}

fn should_scan_paths(paths: &[String], scan: Option<bool>) -> bool {
    if let Some(s) = scan {
        return s;
    }
    paths.len() == 1 && (has_magic(&paths[0]) || Path::new(&expanduser(&paths[0])).is_dir())
}

/// One entry per glob match / immediate subdirectory, deduped by resolved path.
pub fn scan_paths(values: &[String], limit: Option<u64>) -> Result<Vec<String>> {
    let mut out = vec![];
    let mut seen = std::collections::HashSet::new();
    for v in values {
        for p in scan_one_path(v)? {
            let resolved = std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(&p));
            if seen.insert(resolved) {
                out.push(p);
            }
        }
    }
    if out.is_empty() {
        bail!("--scan did not discover any paths.");
    }
    if let Some(l) = limit {
        out.truncate(l as usize);
    }
    Ok(out)
}

fn scan_one_path(value: &str) -> Result<Vec<String>> {
    if has_magic(value) {
        let mut matches: Vec<PathBuf> = py_glob(&py_path(&expanduser(value))).iter().map(|m| PathBuf::from(py_path(m))).collect();
        matches.sort();
        if matches.is_empty() {
            bail!("Path pattern did not match anything: {value}");
        }
        return Ok(matches.into_iter().map(|p| p.to_string_lossy().into_owned()).collect());
    }
    let base = py_path(&expanduser(value));
    let path = Path::new(&base);
    if path.is_dir() {
        let mut kids: Vec<PathBuf> = std::fs::read_dir(path)?
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| if base == "." { PathBuf::from(e.file_name()) } else { path.join(e.file_name()) })
            .collect();
        kids.sort();
        if kids.is_empty() {
            bail!("Path directory has no subdirectories to scan: {value}");
        }
        return Ok(kids.into_iter().map(|p| p.to_string_lossy().into_owned()).collect());
    }
    if path.exists() {
        return Ok(vec![base]);
    }
    bail!("Path does not exist: {value}")
}

fn compile_verifiers(artifacts: &[String], verify: bool, reward: Option<&str>) -> Vec<CompileVerifier> {
    if verify && (!artifacts.is_empty() || reward.is_some()) {
        return vec![CompileVerifier {
            path: None,
            auto_verifier: Some(AutoVerifier { required_artifacts: Some(artifacts.to_vec()), reward_artifact: reward.map(String::from), artifact_json_schemas: Map::new() }),
        }];
    }
    vec![]
}

/// upstream's `parse_kwargs`: JSON values, Python literals True/False/None, else strings.
fn parse_kwargs(items: &[String]) -> Result<Map<String, Value>> {
    let mut m = Map::new();
    for kw in items {
        let (k, v) = kw.split_once('=').ok_or_else(|| anyhow!("Invalid kwarg format: {kw}. Expected key=value"))?;
        let v = v.trim();
        let val = serde_json::from_str(v).unwrap_or_else(|_| match v {
            "True" => Value::Bool(true),
            "False" => Value::Bool(false),
            "None" => Value::Null,
            _ => Value::String(v.to_string()),
        });
        m.insert(k.trim().to_string(), val);
    }
    Ok(m)
}

fn parse_env_vars(items: &[String]) -> Result<Map<String, Value>> {
    let mut m = Map::new();
    for it in items {
        let (k, v) = it.split_once('=').ok_or_else(|| anyhow!("Invalid env var format: {it}. Expected KEY=VALUE"))?;
        m.insert(k.trim().to_string(), Value::String(v.trim().to_string()));
    }
    Ok(m)
}

fn agent_configs(agent: Option<&str>, models: &[String], kwargs: &[String], env: &[String], timeout: Option<f64>) -> Result<Vec<Map<String, Value>>> {
    if agent.is_none() && (!models.is_empty() || !kwargs.is_empty() || !env.is_empty()) {
        bail!("--agent is required when passing agent-specific flags.");
    }
    let kwargs = parse_kwargs(kwargs)?;
    let env = parse_env_vars(env)?;
    let build = |model: Option<&String>| {
        let mut m = Map::new();
        if let Some(a) = agent {
            m.insert("name".into(), a.into());
        }
        if let Some(model) = model {
            m.insert("model_name".into(), model.clone().into());
        }
        if let Some(t) = timeout {
            m.insert("override_timeout_sec".into(), t.into());
        }
        if agent.is_some() {
            m.insert("kwargs".into(), Value::Object(kwargs.clone()));
            m.insert("env".into(), Value::Object(env.clone()));
        }
        m
    };
    if agent.is_some() && !models.is_empty() {
        return Ok(models.iter().map(|m| build(Some(m))).collect());
    }
    Ok(vec![build(None)])
}

fn reduce_agent_configs(a: &ExecArgs, map_agents: &[Map<String, Value>]) -> Result<Vec<Map<String, Value>>> {
    let inherited: Vec<Map<String, Value>> = if map_agents.is_empty() { vec![Map::new()] } else { map_agents.to_vec() };
    if !reduce_agent_flags_passed(a) {
        return Ok(inherited);
    }
    let kwargs = (!a.reduce_agent_kwargs.is_empty()).then(|| parse_kwargs(&a.reduce_agent_kwargs)).transpose()?;
    let env = (!a.reduce_agent_env.is_empty()).then(|| parse_env_vars(&a.reduce_agent_env)).transpose()?;
    let mut models: Vec<Value> = if a.reduce_models.is_empty() {
        map_agents.iter().map(|m| m.get("model_name").cloned().unwrap_or(Value::Null)).collect()
    } else {
        a.reduce_models.iter().map(|m| Value::String(m.clone())).collect()
    };
    if models.is_empty() {
        models.push(Value::Null);
    }
    Ok(models
        .into_iter()
        .enumerate()
        .map(|(i, model)| {
            let mut c = inherited[i % inherited.len()].clone();
            c.insert("model_name".into(), model);
            if let Some(n) = &a.reduce_agent {
                c.insert("name".into(), n.clone().into());
            }
            if let Some(k) = &kwargs {
                c.insert("kwargs".into(), Value::Object(k.clone()));
            }
            if let Some(e) = &env {
                c.insert("env".into(), Value::Object(e.clone()));
            }
            c
        })
        .collect())
}

fn environment_config(spec: Option<&str>) -> Result<Map<String, Value>> {
    let mut m = Map::new();
    let Some(s) = spec else { return Ok(m) };
    if s.contains(':') {
        m.insert("import_path".into(), s.into());
    } else if ENVIRONMENT_TYPES.contains(&s) {
        m.insert("type".into(), s.into());
    } else {
        bail!(
            "Invalid value: Unknown environment '{s}'. Valid types: {}. Pass a custom environment as an import path (module.path:ClassName).",
            ENVIRONMENT_TYPES.join(", ")
        );
    }
    Ok(m)
}

fn verifier_config(has_compile_verifier: bool, template: Option<&str>, verify: bool) -> Map<String, Value> {
    let template_has_tests = template.is_some_and(|t| Path::new(&expanduser(t)).join("tests").is_dir());
    let mut m = Map::new();
    if !verify || (!has_compile_verifier && !template_has_tests) {
        m.insert("disable".into(), true.into());
    }
    m
}

// ---------------------------------------------------------------- file mention extraction

/// Tiny backtracking matcher: upstream's FILE_MENTION_PATTERN uses lookbehind/lookahead, which
/// the `regex` crate lacks, so the pattern is hand-built from these nodes with Python `re`'s
/// exploration order (greedy, alternatives left to right).
enum Node {
    Class(fn(char) -> bool),
    Lit(&'static str),
    /// Case-insensitive alternatives, tried in order.
    Words(&'static [&'static str]),
    Seq(Vec<Node>),
    Alt(Vec<Node>),
    Star(Box<Node>),
    Opt(Box<Node>),
    NotAhead(Box<Node>),
    /// Python `$` (no MULTILINE): end, or before a final newline.
    End,
}

type K<'a> = dyn FnMut(usize) -> bool + 'a;

impl Node {
    fn m(&self, s: &[char], i: usize, k: &mut K<'_>) -> bool {
        match self {
            Node::Class(f) => i < s.len() && f(s[i]) && k(i + 1),
            Node::Lit(l) => {
                let n = l.chars().count();
                i + n <= s.len() && s[i..i + n].iter().copied().eq(l.chars()) && k(i + n)
            }
            Node::Words(ws) => ws.iter().any(|w| {
                let n = w.chars().count();
                i + n <= s.len() && s[i..i + n].iter().zip(w.chars()).all(|(a, b)| a.to_lowercase().eq(b.to_lowercase())) && k(i + n)
            }),
            Node::Seq(ns) => seq(ns, s, i, k),
            Node::Alt(ns) => ns.iter().any(|n| n.m(s, i, k)),
            Node::Star(n) => star(n, s, i, k),
            Node::Opt(n) => n.m(s, i, k) || k(i),
            Node::NotAhead(n) => !n.m(s, i, &mut |_| true) && k(i),
            Node::End => (i == s.len() || (i + 1 == s.len() && s[i] == '\n')) && k(i),
        }
    }
}

fn seq(ns: &[Node], s: &[char], i: usize, k: &mut K<'_>) -> bool {
    match ns.split_first() {
        None => k(i),
        Some((first, rest)) => first.m(s, i, &mut |j| seq(rest, s, j, k)),
    }
}

fn star(n: &Node, s: &[char], i: usize, k: &mut K<'_>) -> bool {
    n.m(s, i, &mut |j| j > i && star(n, s, j, k)) || k(i)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn file_mention_pattern() -> Node {
    use Node::*;
    let w = || Class(is_word);
    let w_dash = || Class(|c| is_word(c) || c == '-');
    let w_dot_dash = || Class(|c| is_word(c) || c == '.' || c == '-');
    let upper = || Class(|c| c.is_ascii_uppercase());
    let dot = || Lit(".");
    Seq(vec![
        // (?!(?-i:[A-Z]\.[A-Z])(?:$|[^\w/.-]|\.(?!\w)))  D.C. / U.S. initials
        NotAhead(Box::new(Seq(vec![
            upper(),
            dot(),
            upper(),
            Alt(vec![End, Class(|c| !(is_word(c) || c == '/' || c == '.' || c == '-')), Seq(vec![dot(), NotAhead(Box::new(w()))])]),
        ]))),
        Opt(Box::new(Lit("/"))),
        Star(Box::new(Seq(vec![dot(), Opt(Box::new(dot())), Lit("/")]))),
        Star(Box::new(Seq(vec![w(), Star(Box::new(w_dot_dash())), Lit("/")]))),
        Alt(vec![
            Seq(vec![w(), Star(Box::new(w_dash())), Star(Box::new(Seq(vec![dot(), w_dash(), Star(Box::new(w_dash()))]))), dot(), Words(KNOWN_EXTS)]),
            Seq(vec![dot(), Words(KNOWN_DOTFILES), Star(Box::new(Seq(vec![dot(), w(), Star(Box::new(w()))])))]),
        ]),
        NotAhead(Box::new(Class(|c| is_word(c) || c == '/' || c == '-'))),
        NotAhead(Box::new(Seq(vec![dot(), w()]))),
    ])
}

/// `FILE_MENTION_PATTERN.finditer(text)` group 1 values.
pub fn file_mentions(text: &str) -> Vec<String> {
    let s: Vec<char> = text.chars().collect();
    let pat = file_mention_pattern();
    let mut out = vec![];
    let mut pos = 0;
    while pos < s.len() {
        let found = (pos..s.len()).find_map(|start| {
            // (?<![\w~./-])
            if start > 0 && (is_word(s[start - 1]) || "~./-".contains(s[start - 1])) {
                return None;
            }
            let mut end = None;
            pat.m(&s, start, &mut |e| {
                end = Some(e);
                true
            });
            end.map(|e| (start, e))
        });
        let Some((start, end)) = found else { break };
        out.push(s[start..end].iter().collect());
        pos = end;
    }
    out
}

// ---------------------------------------------------------------- python path helpers

fn expanduser(s: &str) -> String {
    if (s == "~" || s.starts_with("~/"))
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{}{}", home.trim_end_matches('/'), &s[1..]);
    }
    s.to_string()
}

/// `str(pathlib.PurePosixPath(s))`.
fn py_path(s: &str) -> String {
    if s.is_empty() {
        return ".".into();
    }
    let root = if s.starts_with("//") && !s.starts_with("///") {
        "//"
    } else if s.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    let joined = format!("{root}{}", parts.join("/"));
    if joined.is_empty() { ".".into() } else { joined }
}

/// `posixpath.normpath`.
fn normpath(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let initial = if p.starts_with("//") && !p.starts_with("///") {
        2
    } else if p.starts_with('/') {
        1
    } else {
        0
    };
    let mut out: Vec<&str> = vec![];
    for c in p.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c != ".." || (initial == 0 && out.is_empty()) || out.last() == Some(&"..") {
            out.push(c);
        } else if !out.is_empty() {
            out.pop();
        }
    }
    let s = format!("{}{}", "/".repeat(initial), out.join("/"));
    if s.is_empty() { ".".into() } else { s }
}

fn posix_join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

fn posix_split(p: &str) -> (String, String) {
    let i = p.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (head, tail) = p.split_at(i);
    let head = if !head.is_empty() && head.chars().any(|c| c != '/') { head.trim_end_matches('/') } else { head };
    (head.to_string(), tail.to_string())
}

fn has_magic(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Python's `tempfile.gettempdir()`.
fn temp_root() -> PathBuf {
    PathBuf::from(py_path(&std::env::temp_dir().to_string_lossy()))
}

/// `Path(p).expanduser().resolve()` (non-strict).
fn resolve(p: &str) -> PathBuf {
    let p = PathBuf::from(expanduser(p));
    let abs = if p.is_absolute() { p } else { std::env::current_dir().unwrap_or_default().join(p) };
    let abs = PathBuf::from(normpath(&abs.to_string_lossy()));
    if let Ok(c) = abs.canonicalize() {
        return c;
    }
    // Canonicalize the longest existing ancestor, keep the rest as-is.
    let mut tail = vec![];
    let mut cur = abs.as_path();
    while let Some(parent) = cur.parent() {
        tail.push(cur.file_name().unwrap_or_default().to_owned());
        if let Ok(c) = parent.canonicalize() {
            return tail.iter().rev().fold(c, |acc, t| acc.join(t));
        }
        cur = parent;
    }
    abs
}

/// `glob.glob(pattern, recursive=True)` (unsorted, hidden entries skipped unless the pattern
/// component starts with '.').
fn py_glob(pattern: &str) -> Vec<String> {
    let mut out = iglob(pattern, false);
    if (pattern.is_empty() || pattern.starts_with("**")) && out.first().is_some_and(String::is_empty) {
        out.remove(0);
    }
    out
}

fn iglob(pathname: &str, dironly: bool) -> Vec<String> {
    let (dirname, basename) = posix_split(pathname);
    if !has_magic(pathname) {
        let p = Path::new(pathname);
        let hit = if !basename.is_empty() { p.symlink_metadata().is_ok() } else { Path::new(&dirname).is_dir() };
        return if hit { vec![pathname.to_string()] } else { vec![] };
    }
    if dirname.is_empty() {
        return if basename == "**" { glob2("", &basename, dironly) } else { glob1("", &basename, dironly) };
    }
    let dirs = if dirname != pathname && has_magic(&dirname) { iglob(&dirname, true) } else { vec![dirname.clone()] };
    let mut out = vec![];
    for d in dirs {
        let names = if !has_magic(&basename) {
            glob0(&d, &basename)
        } else if basename == "**" {
            glob2(&d, &basename, dironly)
        } else {
            glob1(&d, &basename, dironly)
        };
        out.extend(names.into_iter().map(|n| posix_join(&d, &n)));
    }
    out
}

fn listdir(dirname: &str, dironly: bool) -> Vec<String> {
    let dir = if dirname.is_empty() { "." } else { dirname };
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    rd.flatten().filter(|e| !dironly || e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()
}

fn glob0(dirname: &str, basename: &str) -> Vec<String> {
    let hit = if !basename.is_empty() { Path::new(&posix_join(dirname, basename)).symlink_metadata().is_ok() } else { Path::new(dirname).is_dir() };
    if hit { vec![basename.to_string()] } else { vec![] }
}

fn glob1(dirname: &str, pattern: &str, dironly: bool) -> Vec<String> {
    listdir(dirname, dironly).into_iter().filter(|n| (pattern.starts_with('.') || !n.starts_with('.')) && crate::glob_match(pattern, n)).collect()
}

fn glob2(dirname: &str, _pattern: &str, dironly: bool) -> Vec<String> {
    let mut out = vec![];
    if dirname.is_empty() || Path::new(dirname).is_dir() {
        out.push(String::new());
    }
    rlistdir(dirname, dironly, &mut out, "");
    out
}

fn rlistdir(dirname: &str, dironly: bool, out: &mut Vec<String>, prefix: &str) {
    for x in listdir(dirname, dironly) {
        if x.starts_with('.') {
            continue;
        }
        let rel = if prefix.is_empty() { x.clone() } else { posix_join(prefix, &x) };
        out.push(rel.clone());
        let path = if dirname.is_empty() { x.clone() } else { posix_join(dirname, &x) };
        rlistdir(&path, dironly, out, &rel);
    }
}

// ---------------------------------------------------------------- compiler

/// `re.sub(r"[^A-Za-z0-9_.-]+", "-", v.strip().lower()).strip("-")`; may be empty.
fn slug(v: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"[^A-Za-z0-9_.-]+").unwrap());
    re.replace_all(&v.trim().to_lowercase(), "-").trim_matches('-').to_string()
}

fn slugify(v: &str) -> String {
    let s = slug(v);
    if s.is_empty() { "task".into() } else { s }
}

/// `shutil.copytree` (follows symlinks); `dst` must not exist.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        bail!("[Errno 17] File exists: '{}'", dst.display());
    }
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let p = e?.path();
        let to = dst.join(p.file_name().unwrap_or_default());
        if p.is_dir() {
            copy_tree(&p, &to)?;
        } else {
            std::fs::copy(&p, &to).with_context(|| format!("copying {}", p.display()))?;
        }
    }
    Ok(())
}

fn replace_dir(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }
    copy_tree(src, dst)
}

fn write_file(p: &Path, content: &str, mode: u32) -> Result<()> {
    std::fs::write(p, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))?;
    }
    let _ = mode;
    Ok(())
}

/// Compile a CompileConfig into task dirs (instruction x environment x verifier cross product).
pub fn compile(cfg: &CompileConfig) -> Result<Vec<PathBuf>> {
    let out = cfg.output_dir.as_ref().ok_or_else(|| anyhow!("CompileConfig.output_dir is required."))?;
    let out = resolve(&out.0);
    std::fs::create_dir_all(&out)?;
    let instructions: Vec<Option<&CompileInstruction>> = if cfg.instructions.is_empty() { vec![None] } else { cfg.instructions.iter().map(Some).collect() };
    let environments: Vec<Option<&CompileEnvironment>> = if cfg.environments.is_empty() { vec![None] } else { cfg.environments.iter().map(Some).collect() };
    let verifiers: Vec<Option<&CompileVerifier>> = if cfg.verifiers.is_empty() { vec![None] } else { cfg.verifiers.iter().map(Some).collect() };
    let base = slugify(cfg.task_name_prefix.as_deref().or(cfg.dataset_name.as_deref()).unwrap_or("task"));
    let mut tasks = vec![];
    for i in &instructions {
        for e in &environments {
            for v in &verifiers {
                let dir = out.join(format!("{base}-{:04}", tasks.len() + 1));
                compile_task(cfg, &dir, *i, *e, *v)?;
                tasks.push(dir);
            }
        }
    }
    Ok(tasks)
}

fn compile_task(cfg: &CompileConfig, dir: &Path, instruction: Option<&CompileInstruction>, environment: Option<&CompileEnvironment>, verifier: Option<&CompileVerifier>) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    if let Some(t) = &cfg.task_template {
        let t = resolve(&t.0);
        if !t.is_dir() {
            bail!("task_template must be a directory: {}", t.display());
        }
        copy_tree(&t, dir)?;
    }
    std::fs::create_dir_all(dir)?;
    let env_dir = dir.join("environment");
    let tests_dir = dir.join("tests");

    if let Some(i) = instruction {
        if let Some(text) = &i.text {
            std::fs::write(dir.join("instruction.md"), if text.ends_with('\n') { text.clone() } else { format!("{text}\n") })?;
        } else {
            let p = resolve(&i.path.as_ref().ok_or_else(|| anyhow!("Instruction must define text or path."))?.0);
            if !p.is_file() {
                bail!("Instruction path must be a file: {}", p.display());
            }
            std::fs::copy(&p, dir.join("instruction.md"))?;
        }
    }

    match environment {
        None => std::fs::create_dir_all(&env_dir)?,
        Some(CompileEnvironment { path: Some(p), .. }) => {
            let src = resolve(&p.0);
            if !src.is_dir() {
                bail!("Environment path must be a directory: {}", src.display());
            }
            replace_dir(&src, &env_dir)?;
        }
        Some(e) if e.paths.is_empty() => std::fs::create_dir_all(&env_dir)?,
        Some(e) => {
            if env_dir.exists() {
                std::fs::remove_dir_all(&env_dir)?;
            }
            std::fs::create_dir_all(&env_dir)?;
            for src in expand_input_paths(&e.paths)? {
                let to = env_dir.join(src.file_name().unwrap_or_default());
                if src.is_dir() {
                    replace_dir(&src, &to)?;
                } else if src.is_file() {
                    std::fs::copy(&src, &to)?;
                } else {
                    bail!("Input path must be a file or directory: {}", src.display());
                }
            }
        }
    }

    if let Some(v) = verifier {
        if let Some(p) = &v.path {
            let src = resolve(&p.0);
            if !src.is_dir() {
                bail!("Verifier path must be a directory: {}", src.display());
            }
            replace_dir(&src, &tests_dir)?;
        } else {
            let av = v.auto_verifier.as_ref().ok_or_else(|| anyhow!("Verifier must define path or auto_verifier."))?;
            write_auto_verifier(cfg, &tests_dir, av)?;
        }
    }

    write_task_config(cfg, dir, environment)?;

    if !is_valid_dir(dir, true) {
        bail!("Compiled task is not a valid task directory: {}", dir.display());
    }
    if tests_dir.exists() && !is_valid_dir(dir, false) {
        bail!("Compiled task has a tests/ directory but no OS-compatible verifier script for its shared verifier environment.");
    }
    Ok(())
}

fn expand_input_paths(patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = vec![];
    for pat in patterns {
        let resolved = resolve(pat).to_string_lossy().into_owned();
        let mut paths: Vec<PathBuf> = py_glob(&resolved).iter().map(|p| PathBuf::from(py_path(p))).collect();
        paths.sort();
        if paths.is_empty() {
            bail!("Input path pattern did not match anything: {pat}");
        }
        out.extend(paths.into_iter().map(|p| p.canonicalize().unwrap_or(p)));
    }
    Ok(out)
}

fn write_auto_verifier(cfg: &CompileConfig, tests_dir: &Path, av: &AutoVerifier) -> Result<()> {
    if tests_dir.exists() {
        std::fs::remove_dir_all(tests_dir)?;
    }
    std::fs::create_dir_all(tests_dir)?;
    let mut required = av.required_artifacts.clone().unwrap_or_else(|| {
        cfg.artifacts.iter().map(|a| a.as_str().or_else(|| a.get("source").and_then(Value::as_str)).unwrap_or_default().to_string()).collect()
    });
    if let Some(r) = &av.reward_artifact
        && !required.contains(r)
    {
        required.push(r.clone());
    }
    let script = if av.artifact_json_schemas.is_empty() {
        TPL_AUTO_VERIFY
    } else {
        let schemas = tests_dir.join("schemas");
        std::fs::create_dir_all(&schemas)?;
        let mut checks = vec![];
        for (i, (artifact, schema)) in av.artifact_json_schemas.iter().enumerate() {
            let src = resolve(&py_path(schema.as_str().unwrap_or_default()));
            if !src.is_file() {
                bail!("Artifact schema path must be a file: {}", src.display());
            }
            let name = format!("schema-{:04}.json", i + 1);
            std::fs::copy(&src, schemas.join(&name))?;
            checks.push(json!({"artifact_path": artifact, "schema_path": format!("/tests/schemas/{name}")}));
        }
        write_file(&tests_dir.join("validate_artifact_schemas.py"), TPL_SCHEMA_VALIDATOR, 0o644)?;
        std::fs::write(tests_dir.join("artifact-schema-checks.json"), ascii_json(&serde_json::to_string_pretty(&checks)?) + "\n")?;
        TPL_AUTO_VERIFY_SCHEMAS
    };
    std::fs::write(tests_dir.join("required-artifacts.txt"), required.iter().map(|a| format!("{a}\n")).collect::<String>())?;
    if let Some(r) = &av.reward_artifact {
        std::fs::write(tests_dir.join("reward-artifact.txt"), format!("{r}\n"))?;
        write_file(&tests_dir.join("promote_reward_artifact.py"), TPL_PROMOTE_REWARD, 0o644)?;
    }
    write_file(&tests_dir.join("test.sh"), script, 0o755)
}

/// Python `json.dumps` default `ensure_ascii=True`.
fn ascii_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

fn write_task_config(cfg: &CompileConfig, dir: &Path, environment: Option<&CompileEnvironment>) -> Result<()> {
    let path = dir.join("task.toml");
    let exists = path.exists();
    let raw: Map<String, Value> = if exists { parse_toml(&std::fs::read_to_string(&path)?).with_context(|| format!("parsing {}", path.display()))? } else { Map::new() };
    let mut tc = hbench_task_config(&raw);
    if !cfg.artifacts.is_empty() {
        tc.insert("artifacts".into(), cfg.artifacts.iter().map(artifact_value).collect());
    }
    let fresh_env = |image: &str, workdir: &str| Value::Object(env_cfg(json!({"docker_image": image, "workdir": workdir}).as_object().unwrap()));
    match environment {
        None if !exists => {
            tc.insert("environment".into(), fresh_env(DEFAULT_IMAGE, DEFAULT_WORKDIR));
        }
        None => {}
        Some(e) if e.path.is_none() => {
            tc.insert("environment".into(), fresh_env(e.image(), e.wd()));
        }
        Some(e) => {
            if let Some(Value::Object(env)) = tc.get_mut("environment") {
                if let Some(i) = &e.docker_image {
                    env.insert("docker_image".into(), i.clone().into());
                }
                if let Some(w) = &e.workdir {
                    env.insert("workdir".into(), w.clone().into());
                }
                // model_copy(update=...) keeps field order.
                *env = env_cfg(env);
            }
        }
    }
    std::fs::write(&path, task_config_toml(&tc)?)?;
    Ok(())
}

/// `Task.is_valid_dir`. ponytail: separate-verifier detection is "tests/ ships a Dockerfile or
/// compose file"; upstream also resolves [verifier.environment] definitions.
fn is_valid_dir(dir: &Path, disable_verification: bool) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join("task.toml")) else { return false };
    if !dir.join("environment").exists() {
        return false;
    }
    let Ok(raw) = parse_toml(&text) else { return false };
    let tc = Value::Object(hbench_task_config(&raw));
    let steps: Vec<String> = tc.get("steps").and_then(Value::as_array).map(|s| s.iter().filter_map(|x| x.get("name")?.as_str().map(String::from)).collect()).unwrap_or_default();
    let has_instructions = if steps.is_empty() { dir.join("instruction.md").exists() } else { steps.iter().all(|s| dir.join("steps").join(s).join("instruction.md").exists()) };
    if disable_verification || !has_instructions {
        return has_instructions;
    }
    let windows = tc.pointer("/environment/os").and_then(Value::as_str) == Some("windows");
    let script = if windows { "test.bat" } else { "test.sh" };
    let separate = tc.pointer("/verifier/environment_mode").and_then(Value::as_str) == Some("separate") || tc.pointer("/verifier/environment").is_some();
    let bundled = |tests: &Path| separate && ["Dockerfile", "docker-compose.yaml", "docker-compose.yml"].iter().any(|f| tests.join(f).exists());
    let tests = dir.join("tests");
    if steps.is_empty() {
        return bundled(&tests) || tests.join(script).is_file();
    }
    steps.iter().all(|s| {
        let st = dir.join("steps").join(s).join("tests");
        bundled(&st) || bundled(&tests) || st.join(script).is_file() || tests.join(script).is_file()
    })
}

// ---------------------------------------------------------------- upstream TaskConfig (task.toml)

/// TOML -> JSON in document order (Python `tomllib` dicts keep insertion order; hbench's `toml`
/// build sorts keys, so reorder by each key's first span).
fn parse_toml(text: &str) -> Result<Map<String, Value>> {
    let mut v: Map<String, Value> = toml::from_str(text)?;
    let de = toml::de::DeTable::parse(text)?;
    reorder_table(&mut v, de.get_ref());
    Ok(v)
}

fn reorder_table(m: &mut Map<String, Value>, de: &toml::de::DeTable<'_>) {
    let mut keys: Vec<_> = de.iter().collect();
    keys.sort_by_key(|(k, _)| k.span().start);
    let mut out = Map::new();
    for (k, dv) in keys {
        if let Some(mut x) = m.shift_remove(k.get_ref().as_ref()) {
            reorder_value(&mut x, dv.get_ref());
            out.insert(k.get_ref().to_string(), x);
        }
    }
    out.append(m);
    *m = out;
}

fn reorder_value(v: &mut Value, de: &toml::de::DeValue<'_>) {
    match (v, de) {
        (Value::Object(m), toml::de::DeValue::Table(t)) => reorder_table(m, t),
        (Value::Array(a), toml::de::DeValue::Array(da)) => a.iter_mut().zip(da.iter()).for_each(|(x, d)| reorder_value(x, d.get_ref())),
        _ => {}
    }
}

/// Builds one pydantic model's `model_dump(mode="json")` minus None, in field order.
struct Model<'a> {
    inp: &'a Map<String, Value>,
    out: Map<String, Value>,
}

impl<'a> Model<'a> {
    fn new(inp: &'a Map<String, Value>) -> Self {
        Model { inp, out: Map::new() }
    }
    fn get(&self, k: &str) -> Option<&'a Value> {
        self.inp.get(k).filter(|v| !v.is_null())
    }
    fn opt(mut self, k: &str) -> Self {
        if let Some(v) = self.get(k) {
            self.out.insert(k.into(), v.clone());
        }
        self
    }
    fn val(mut self, k: &str, d: Value) -> Self {
        self.out.insert(k.into(), self.get(k).cloned().unwrap_or(d));
        self
    }
    fn float(mut self, k: &str, d: Option<f64>) -> Self {
        if let Some(v) = self.get(k).map(as_float).or(d.map(f64_value)) {
            self.out.insert(k.into(), v);
        }
        self
    }
    fn sub(mut self, k: &str, f: fn(&Map<String, Value>) -> Map<String, Value>, always: bool) -> Self {
        match self.get(k).and_then(Value::as_object) {
            Some(m) => {
                self.out.insert(k.into(), Value::Object(f(m)));
            }
            None if always => {
                self.out.insert(k.into(), Value::Object(f(&Map::new())));
            }
            None => {}
        }
        self
    }
    fn list(mut self, k: &str, f: fn(&Value) -> Value, always: bool) -> Self {
        match self.get(k).and_then(Value::as_array) {
            Some(a) => {
                self.out.insert(k.into(), a.iter().map(f).collect());
            }
            None if always => {
                self.out.insert(k.into(), json!([]));
            }
            None => {}
        }
        self
    }
    fn done(self) -> Map<String, Value> {
        self.out
    }
}

fn f64_value(f: f64) -> Value {
    serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
}

fn as_float(v: &Value) -> Value {
    match v.as_f64() {
        Some(f) if v.is_number() => f64_value(f),
        _ => v.clone(),
    }
}

fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// `TaskConfig.model_validate(toml).model_dump(mode="json")` minus None values.
fn hbench_task_config(raw: &Map<String, Value>) -> Map<String, Value> {
    let mut raw = raw.clone();
    if let Some(v) = raw.shift_remove("version") {
        raw.entry("schema_version").or_insert(v);
    }
    Model::new(&raw)
        .val("schema_version", "1.4".into())
        .sub("task", package_cfg, false)
        .val("metadata", json!({}))
        .sub("verifier", verifier_cfg, true)
        .sub("agent", agent_cfg, true)
        .sub("environment", env_cfg, true)
        .sub("solution", |m| Model::new(m).val("env", json!({})).done(), true)
        .opt("source")
        .opt("multi_step_reward_strategy")
        .list("steps", |v| Value::Object(step_cfg(&obj(v))), false)
        .list("artifacts", artifact_value, true)
        .done()
}

fn package_cfg(m: &Map<String, Value>) -> Map<String, Value> {
    Model::new(m)
        .opt("name")
        .opt("version")
        .val("description", "".into())
        .list("authors", |v| Value::Object(Model::new(&obj(v)).opt("name").opt("email").done()), true)
        .val("keywords", json!([]))
        .done()
}

fn agent_cfg(m: &Map<String, Value>) -> Map<String, Value> {
    Model::new(m).opt("network_mode").opt("allowed_hosts").float("timeout_sec", None).opt("user").done()
}

fn verifier_cfg(m: &Map<String, Value>) -> Map<String, Value> {
    Model::new(m)
        .opt("network_mode")
        .opt("allowed_hosts")
        .float("timeout_sec", Some(600.0))
        .val("env", json!({}))
        .opt("user")
        .opt("environment_mode")
        .sub("environment", env_cfg, false)
        .list("collect", |v| Value::Object(Model::new(&obj(v)).opt("command").val("service", "main".into()).float("timeout_sec", Some(60.0)).opt("user").done()), true)
        .done()
}

fn size_to_mb(s: &str) -> Option<i64> {
    let s = s.trim().to_uppercase();
    let num = |t: &str| t.parse::<f64>().ok();
    if let Some(n) = s.strip_suffix('G') {
        num(n).map(|f| (f * 1024.0) as i64)
    } else if let Some(n) = s.strip_suffix('M') {
        num(n).map(|f| f as i64)
    } else if let Some(n) = s.strip_suffix('K') {
        num(n).map(|f| (f / 1024.0) as i64)
    } else {
        None
    }
}

fn env_cfg(m: &Map<String, Value>) -> Map<String, Value> {
    let mut m = m.clone();
    for (legacy, field) in [("memory", "memory_mb"), ("storage", "storage_mb")] {
        if let Some(v) = m.shift_remove(legacy)
            && let Some(mb) = v.as_str().and_then(size_to_mb)
        {
            m.entry(field).or_insert(mb.into());
        }
    }
    // Deprecated allow_internet maps onto network_mode unless a policy was given.
    if let Some(allow) = m.get("allow_internet").and_then(Value::as_bool)
        && !m.contains_key("network_mode")
        && m.get("allowed_hosts").is_none_or(Value::is_null)
    {
        m.insert("network_mode".into(), (if allow { "public" } else { "no-network" }).into());
    }
    if let Some(Value::String(os)) = m.get_mut("os") {
        *os = os.to_lowercase();
    }
    Model::new(&m)
        .val("network_mode", "public".into())
        .opt("allowed_hosts")
        .float("build_timeout_sec", Some(600.0))
        .opt("docker_image")
        .val("os", "linux".into())
        .opt("cpus")
        .opt("memory_mb")
        .opt("storage_mb")
        .opt("gpus")
        .opt("gpu_types")
        .sub("tpu", |t| Model::new(t).opt("type").opt("topology").done(), false)
        .list(
            "mcp_servers",
            |v| {
                let mut s = obj(v);
                if s.get("transport").and_then(Value::as_str) == Some("http") {
                    s.insert("transport".into(), "streamable-http".into());
                }
                Value::Object(Model::new(&s).opt("name").val("transport", "sse".into()).opt("url").opt("command").val("args", json!([])).done())
            },
            true,
        )
        .val("env", json!({}))
        .opt("skills_dir")
        .sub("healthcheck", healthcheck_cfg, false)
        .opt("workdir")
        .done()
}

fn healthcheck_cfg(m: &Map<String, Value>) -> Map<String, Value> {
    Model::new(m)
        .opt("command")
        .float("interval_sec", Some(5.0))
        .float("timeout_sec", Some(30.0))
        .float("start_period_sec", Some(0.0))
        .float("start_interval_sec", Some(5.0))
        .val("retries", 3.into())
        .done()
}

fn step_cfg(m: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Model::new(m).opt("name").sub("agent", agent_cfg, true).sub("verifier", verifier_cfg, true).done();
    if let Some(r) = m.get("min_reward").filter(|v| !v.is_null()) {
        let r = match r {
            Value::Object(d) => Value::Object(d.iter().map(|(k, v)| (k.clone(), as_float(v))).collect()),
            v => as_float(v),
        };
        out.insert("min_reward".into(), r);
    }
    let rest = Model::new(m).sub("healthcheck", healthcheck_cfg, false).list("artifacts", artifact_value, true).done();
    out.extend(rest);
    out
}

/// `str | ArtifactConfig` dumped in json mode (None dropped).
fn artifact_value(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(Model::new(m).opt("source").opt("destination").val("exclude", json!([])).opt("service").done()),
        v => v.clone(),
    }
}

/// `TaskConfig.model_dump_toml()`.
fn task_config_toml(data: &Map<String, Value>) -> Result<String> {
    const LEADING: [&str; 3] = ["schema_version", "source", "multi_step_reward_strategy"];
    const SECTIONS: [&str; 7] = ["task", "steps", "metadata", "verifier", "agent", "environment", "solution"];
    let table_like = |v: &Value| v.is_object() || v.as_array().is_some_and(|a| a.iter().any(Value::is_object));
    let mut root = Map::new();
    for f in LEADING {
        if let Some(v) = data.get(f).filter(|v| !v.is_object()) {
            root.insert(f.into(), v.clone());
        }
    }
    for (k, v) in data {
        if !LEADING.contains(&k.as_str()) && k != "artifacts" && !SECTIONS.contains(&k.as_str()) && !table_like(v) {
            root.insert(k.clone(), v.clone());
        }
    }
    if let Some(v) = data.get("artifacts").filter(|v| !v.is_object()) {
        root.insert("artifacts".into(), v.clone());
    }
    let mut parts = vec![];
    let mut emitted: Vec<String> = vec![];
    if !root.is_empty() {
        parts.push(toml_dumps(&root)?);
        emitted.extend(root.keys().cloned());
    }
    for s in ["task", "steps", "metadata", "verifier", "agent", "environment", "solution"] {
        if let Some(v) = data.get(s) {
            let mut one = Map::new();
            one.insert(s.into(), v.clone());
            parts.push(toml_dumps(&one)?);
            emitted.push(s.into());
        }
    }
    for (k, v) in data {
        if !emitted.contains(k) {
            let mut one = Map::new();
            one.insert(k.clone(), v.clone());
            parts.push(toml_dumps(&one)?);
            emitted.push(k.clone());
        }
    }
    let parts: Vec<&str> = parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
    Ok(parts.join("\n\n") + "\n")
}

// ---------------------------------------------------------------- python `toml` 0.10 encoder

/// `toml.dumps(o)` from the Python `toml` package, quirks included (upstream writes task.toml with it).
fn toml_dumps(o: &Map<String, Value>) -> Result<String> {
    let (mut retval, mut sections) = dump_sections(o, "")?;
    while !sections.is_empty() {
        let mut next: Vec<(String, Map<String, Value>)> = vec![];
        for (section, table) in &sections {
            let (add, subs) = dump_sections(table, section)?;
            if !add.is_empty() || subs.is_empty() {
                if !retval.is_empty() && !retval.ends_with("\n\n") {
                    retval.push('\n');
                }
                retval.push_str(&format!("[{section}]\n"));
                retval.push_str(&add);
            }
            next.extend(subs.into_iter().map(|(k, v)| (format!("{section}.{k}"), v)));
        }
        sections = next;
    }
    Ok(retval)
}

fn bare_key(k: &str) -> String {
    if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') { k.to_string() } else { dump_str(k) }
}

type Sections = Vec<(String, Map<String, Value>)>;

fn dump_sections(o: &Map<String, Value>, sup: &str) -> Result<(String, Sections)> {
    let sup = if !sup.is_empty() && !sup.ends_with('.') { format!("{sup}.") } else { sup.to_string() };
    let mut retstr = String::new();
    let mut retdict: Sections = vec![];
    let mut arraystr = String::new();
    for (key, val) in o {
        let q = bare_key(key);
        match val {
            Value::Object(m) => retdict.push((q, m.clone())),
            Value::Array(items) if items.iter().any(Value::is_object) => {
                for a in items {
                    let Value::Object(a) = a else { bail!("TypeError: cannot dump mixed table/value array {key:?} to TOML") };
                    let mut arraytabstr = String::from("\n");
                    arraystr.push_str(&format!("[[{sup}{q}]]\n"));
                    let (s, mut d) = dump_sections(a, &format!("{sup}{q}"))?;
                    if !s.is_empty() {
                        if s.starts_with('[') {
                            arraytabstr.push_str(&s);
                        } else {
                            arraystr.push_str(&s);
                        }
                    }
                    while !d.is_empty() {
                        let mut newd = vec![];
                        for (dsec, table) in &d {
                            let (s1, d1) = dump_sections(table, &format!("{sup}{q}.{dsec}"))?;
                            if !s1.is_empty() {
                                arraytabstr.push_str(&format!("[{sup}{q}.{dsec}]\n"));
                                arraytabstr.push_str(&s1);
                            }
                            newd.extend(d1.into_iter().map(|(k, v)| (format!("{dsec}.{k}"), v)));
                        }
                        d = newd;
                    }
                    arraystr.push_str(&arraytabstr);
                }
            }
            Value::Null => {}
            v => retstr.push_str(&format!("{q} = {}\n", dump_value(v))),
        }
    }
    retstr.push_str(&arraystr);
    Ok((retstr, retdict))
}

fn dump_value(v: &Value) -> String {
    match v {
        Value::String(s) => dump_str(s),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) if n.is_f64() => dump_float(n.as_f64().unwrap_or_default()),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => format!("[{}]", a.iter().map(|x| format!(" {},", dump_value(x))).collect::<String>()),
        // A dict nested in a list is iterated like one (its keys).
        Value::Object(m) => format!("[{}]", m.keys().map(|k| format!(" {},", dump_str(k))).collect::<String>()),
        Value::Null => dump_str("None"),
    }
}

/// Python `str(float)` with the encoder's exponent tweak.
fn dump_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let a = f.abs();
    if a != 0.0 && !(1e-4..1e16).contains(&a) {
        let s = format!("{f:e}");
        let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        return format!("{mant}e{}{}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    let s = format!("{f}");
    if s.contains('.') { s } else { format!("{s}.0") }
}

/// Python `repr(str)`.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if !is_printable(c) => {
                let n = c as u32;
                if n <= 0xff {
                    out.push_str(&format!("\\x{n:02x}"));
                } else if n <= 0xffff {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    out.push_str(&format!("\\U{n:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// ponytail: approximates Python's `str.isprintable` (control, line/para separators, common
/// format chars, non-ASCII spaces); exact needs Unicode category tables.
fn is_printable(c: char) -> bool {
    !(c.is_control()
        || matches!(c as u32, 0xa0 | 0xad | 0x1680 | 0x2000..=0x200f | 0x2028..=0x202f | 0x205f..=0x2064 | 0x3000 | 0xfeff | 0xd800..=0xdfff | 0xe000..=0xf8ff))
}

/// `toml.encoder._dump_str`, including its `\x` and backslash handling.
fn dump_str(s: &str) -> String {
    let r = py_repr(s);
    let single = r.starts_with('\'');
    let mut v = r[1..r.len() - 1].to_string();
    if single {
        v = v.replace("\\'", "'").replace('"', "\\\"");
    }
    let mut parts: Vec<String> = v.split("\\x").map(String::from).collect();
    while parts.len() > 1 {
        if parts[0].is_empty() {
            parts.remove(0);
            if parts.len() < 2 {
                break;
            }
        }
        parts[0] = parts[0].replace("\\\\", "\\");
        let chars: Vec<char> = parts[0].chars().collect();
        let mut i = chars.len();
        let mut joinx = chars.last() != Some(&'\\');
        while i > 1 && chars[i - 1] == '\\' {
            joinx = !joinx;
            i -= 1;
        }
        let joiner = if joinx { "x" } else { "u00" };
        let merged = format!("{}{joiner}{}", parts[0], parts[1]);
        parts.splice(0..2, [merged]);
    }
    format!("\"{}\"", parts[0])
}

// ---------------------------------------------------------------- executor

struct Phase {
    task_dirs: Vec<PathBuf>,
    job_dir: PathBuf,
    result: JobResult,
}

fn job_config(j: &ExecJobConfig, task_dirs: &[PathBuf]) -> Result<JobConfig> {
    let mut v = json!({
        "n_attempts": j.n_attempts,
        "n_concurrent_trials": j.n_concurrent_trials,
        "quiet": j.quiet,
        "retry": j.retry,
        "environment": j.environment,
        "verifier": j.verifier,
        "metrics": j.metrics,
        "agents": j.agents,
        "tasks": task_dirs.iter().map(|d| json!({"path": d})).collect::<Vec<_>>(),
    });
    if let Some(n) = &j.job_name {
        v["job_name"] = n.clone().into();
    }
    if let Some(d) = &j.jobs_dir {
        v["jobs_dir"] = d.0.clone().into();
    }
    let cfg: JobConfig = serde_json::from_value(v).context("invalid exec job config")?;
    cfg.validate()?;
    crate::preflight_agents(&cfg)?;
    Ok(cfg)
}

async fn run_phase(mut cfg: JobConfig, task_dirs: Vec<PathBuf>, cancel: &tokio::sync::watch::Receiver<bool>) -> Result<Phase> {
    let job_dir = cfg.job_dir();
    // ponytail: env.rs resolves the compose dir against the task's environment/ dir, so a relative
    // jobs_dir breaks `docker compose` (also in `hbench run`); hand the runner an absolute one.
    cfg.jobs_dir = std::path::absolute(&cfg.jobs_dir)?;
    let result = job::run_job(cfg, None, &[], cancel.clone()).await?;
    Ok(Phase { task_dirs, job_dir, result })
}

async fn execute(cfg: &ExecConfig) -> Result<(Phase, Option<Phase>)> {
    let map_dirs = compile(&cfg.map.compile)?;
    let map_job = job_config(&cfg.map.job, &map_dirs)?;
    crate::env::preflight().await?;
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\ncancelling: tearing down containers (Ctrl-C again to force quit)...");
            let _ = tx.send(true);
            if tokio::signal::ctrl_c().await.is_ok() {
                std::process::exit(130);
            }
        }
    });
    let map = run_phase(map_job, map_dirs, &rx).await?;
    let Some(r) = &cfg.reduce else { return Ok((map, None)) };
    if *rx.borrow() {
        bail!("cancelled before the reduce phase");
    }
    let reduce_dir = compile_reduce_task(&r.task, &map.job_dir)?;
    let reduce_job = job_config(&r.job, std::slice::from_ref(&reduce_dir))?;
    let reduce = run_phase(reduce_job, vec![reduce_dir], &rx).await?;
    Ok((map, Some(reduce)))
}

/// Trial results of a finished job, ordered by trial name. ponytail: upstream uses its JobResult's
/// completion order; hbench's JobResult has no trial list, so read the trial dirs.
fn trial_results(job_dir: &Path) -> Result<Vec<crate::trial::TrialResult>> {
    let mut out: Vec<crate::trial::TrialResult> = vec![];
    for e in std::fs::read_dir(job_dir)?.flatten() {
        if let Ok(s) = std::fs::read_to_string(e.path().join("result.json"))
            && let Ok(r) = serde_json::from_str(&s)
        {
            out.push(r);
        }
    }
    out.sort_by(|a, b| a.trial_name.cmp(&b.trial_name));
    Ok(out)
}

fn compile_reduce_task(task: &ExecReduceTaskConfig, map_job_dir: &Path) -> Result<PathBuf> {
    let trials = trial_results(map_job_dir)?;
    if trials.is_empty() {
        bail!("Cannot reduce a map job with no trial results.");
    }
    let staging = temp_root().join(format!("hbench-exec-reduce-{}", uuid::Uuid::new_v4().simple()));
    let res = stage_and_compile(task, &trials, &staging);
    let _ = std::fs::remove_dir_all(&staging);
    let dirs = res?;
    if dirs.len() != 1 {
        bail!("Reduce task compilation must produce exactly one task.");
    }
    Ok(dirs.into_iter().next().unwrap_or_default())
}

fn stage_and_compile(task: &ExecReduceTaskConfig, trials: &[crate::trial::TrialResult], staging: &Path) -> Result<Vec<PathBuf>> {
    let staged = staging.join("artifacts");
    std::fs::create_dir_all(&staged)?;
    let mut n = 0;
    for (i, t) in trials.iter().enumerate() {
        let Some(dir) = t.trial_uri.strip_prefix("file://") else { bail!("Expected file trial URI, got: {}", t.trial_uri) };
        let artifacts = Path::new(dir).join("artifacts");
        if !artifacts.is_dir() {
            continue;
        }
        let s = slug(if t.trial_name.is_empty() { "trial" } else { &t.trial_name });
        let s = if s.is_empty() { "trial".to_string() } else { s };
        copy_tree(&artifacts, &staged.join(format!("{:04}-{s}", i + 1)))?;
        n += 1;
    }
    if n == 0 {
        bail!("Cannot reduce a map job with no trial artifacts.");
    }
    let dirs = compile(&CompileConfig {
        task_name_prefix: Some(task.task_name.clone()),
        output_dir: Some(task.output_dir.clone()),
        task_template: task.task_template.clone(),
        artifacts: task.artifacts.clone(),
        instructions: vec![task.instruction.clone()],
        environments: vec![CompileEnvironment { path: None, paths: vec![], docker_image: Some(task.environment.docker_image.clone()), workdir: Some(task.environment.workdir.clone()) }],
        verifiers: task.verifier.clone().into_iter().collect(),
        ..Default::default()
    })?;
    if dirs.len() != 1 {
        bail!("Reduce task compilation must produce exactly one task.");
    }
    let target = dirs[0].join("environment").join("artifacts");
    std::fs::create_dir_all(&target)?;
    for e in std::fs::read_dir(&staged)?.flatten() {
        replace_dir(&e.path(), &target.join(e.file_name()))?;
    }
    Ok(dirs)
}

async fn execute_and_render(config: ExecConfig, cleanup: bool) -> Result<()> {
    if !cleanup {
        let (m, r) = execute(&config).await?;
        render(&m, r.as_ref(), false);
        return Ok(());
    }
    let tmp = temp_root().join(format!("hbench-exec-map-tasks-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::create_dir_all(&tmp)?;
    let mut run = config;
    let out = PyPath(tmp.to_string_lossy().into_owned());
    run.map.compile.output_dir = Some(out.clone());
    if let Some(r) = run.reduce.as_mut() {
        r.task.output_dir = out;
    }
    let res = execute(&run).await;
    let _ = std::fs::remove_dir_all(&tmp);
    let (m, r) = res?;
    render(&m, r.as_ref(), true);
    Ok(())
}

fn is_temporary(p: &Path) -> bool {
    let tmp = temp_root().canonicalize().unwrap_or_else(|_| temp_root());
    resolve(&p.to_string_lossy()).starts_with(tmp)
}

fn render(map: &Phase, reduce: Option<&Phase>, cleaned_up: bool) {
    let tasks_parent = map.task_dirs[0].parent().unwrap_or(Path::new("")).to_path_buf();
    let temporary = cleaned_up || is_temporary(&tasks_parent);
    println!("Map Results");
    job::print_summary(&map.result, &map.job_dir);
    if cleaned_up {
        println!("Compiled tasks used a temporary directory and were cleaned up.");
    } else if temporary {
        println!("Compiled tasks used a temporary directory.");
    } else {
        println!("Map tasks written to {}", tasks_parent.display());
    }
    println!("Map job written to {}", map.job_dir.display());
    let Some(r) = reduce else {
        println!("Inspect results by running `hbench view -p {}`", map.job_dir.parent().unwrap_or(Path::new(".")).display());
        println!();
        return;
    };
    println!("Reduce Results");
    job::print_summary(&r.result, &r.job_dir);
    if !temporary {
        println!("Reduce task written to {}", r.task_dirs[0].display());
    }
    println!("Reduce job written to {}", r.job_dir.display());
    println!("Inspect results by running `hbench view -p {}`", r.job_dir.parent().unwrap_or(Path::new(".")).display());
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        a: ExecArgs,
    }

    fn args(v: &[&str]) -> ExecArgs {
        Cli::try_parse_from(std::iter::once("exec").chain(v.iter().copied())).expect("cli parse").a
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hbench-exec-{tag}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Writes `{"files": {rel: content}}`; `*.sh` get 0755 like the oracle.
    fn write_fixture(spec: &Value, root: &Path) {
        for (rel, content) in spec["files"].as_object().unwrap() {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            write_file(&p, content.as_str().unwrap(), if rel.ends_with(".sh") { 0o755 } else { 0o644 }).unwrap();
        }
    }

    /// upstream's FILE_MENTION_PATTERN_SUITE (tests/unit/cli/test_exec.py) plus extra edge cases
    /// checked against Python `re`.
    #[test]
    fn file_mention_pattern_suite() {
        let suite: &[(&str, &[&str])] = &[
            ("Open notes.md when you can", &["notes.md"]),
            ("notes.md starts the sentence", &["notes.md"]),
            ("the file is notes/lessons/2026-07-05.md", &["notes/lessons/2026-07-05.md"]),
            ("two files: a.py and b.py", &["a.py", "b.py"]),
            ("absolute /etc/hosts.cfg path", &["/etc/hosts.cfg"]),
            ("run ./build.sh then ../deploy.sh", &["./build.sh", "../deploy.sh"]),
            ("deep ../../scripts/build.sh path", &["../../scripts/build.sh"]),
            ("home ~/Developer/lessons/foo.md path", &[]),
            ("edit .gitignore and src/.env.local", &[".gitignore", "src/.env.local"]),
            ("the .env file holds secrets", &[".env"]),
            ("unknown dotfile .venv should NOT match", &[]),
            (".NET is a framework not a file", &[]),
            ("unpack foo.tar.gz and app.config.py", &["foo.tar.gz", "app.config.py"]),
            ("release v1.2.3.tar.gz is out", &["v1.2.3.tar.gz"]),
            ("jquery.min.js is legacy", &["jquery.min.js"]),
            ("outputs events.jsonl layout.xml notebook.ipynb data.parquet", &["events.jsonl", "layout.xml", "notebook.ipynb", "data.parquet"]),
            ("see README.MD and photo.JPG", &["README.MD", "photo.JPG"]),
            ("I renamed foo.txt.", &["foo.txt"]),
            ("is it in foo.md?", &["foo.md"]),
            ("files (see notes.md) here", &["notes.md"]),
            ("he said \"read plan.md\".", &["plan.md"]),
            ("check [design.md] and `impl.py`", &["design.md", "impl.py"]),
            ("link: [lesson](notes/2026-07-05.md)", &["notes/2026-07-05.md"]),
            ("foo.md,bar.md are both stale", &["foo.md", "bar.md"]),
            ("error at src/app.py:42 today", &["src/app.py"]),
            ("see notes.md#history for context", &["notes.md"]),
            ("foo.md's contents changed", &["foo.md"]),
            ("semicolon test notes.md; done", &["notes.md"]),
            ("python 3.14 and semver 1.2.3 are versions", &[]),
            ("use e.g. numpy.array or os.path.join", &[]),
            ("born in the U.S. in 1990, i.e. long ago", &[]),
            ("visit example.com or www.google.org", &[]),
            ("email alexgshaw64@gmail.com please", &[]),
            ("call obj.method() and this.props.name", &[]),
            ("Washington D.C. is not a path", &[]),
            ("the amount is 1,234.56 dollars", &[]),
            ("unknown exts data.feather notes.org skip", &[]),
            ("extension prefix trap: notes.mdx file.pyc", &[]),
            ("urls https://example.com/report.pdf skip", &[]),
            ("open My Document.pdf now", &["Document.pdf"]),
            ("Makefile and Dockerfile have no extension", &[]),
            ("windows C:\\Users\\alex\\file.txt path", &["file.txt"]),
            ("ellipsis right before...foo.md blocks it", &[]),
            ("bare extension mention: .md files", &[]),
            ("résumé.pdf and 中文.txt exist", &["résumé.pdf", "中文.txt"]),
            ("A.B\n", &[]),
            ("x.json.", &["x.json"]),
            ("a.c.cpp b.jsonl.", &["a.c.cpp", "b.jsonl"]),
            ("/abs/.env.local.bak ok", &["/abs/.env.local.bak"]),
            ("../../a/b.c/d.md", &["../../a/b.c/d.md"]),
        ];
        for (text, want) in suite {
            assert_eq!(file_mentions(text), *want, "{text:?}");
        }
    }

    #[test]
    fn artifact_inference_matches_hbench() {
        let p = "Write result.json, reports/summary.txt, /tmp/output.PNG, ../out/data.csv, docs/README.md, and .env.local.";
        assert_eq!(infer_artifacts_from_prompt(p, "/app"), ["/app/result.json", "/app/reports/summary.txt", "/tmp/output.PNG", "/out/data.csv", "/app/docs/README.md", "/app/.env.local"]);
        assert_eq!(infer_artifacts_from_prompt("Write result.json and reports/summary.txt.", "workspace"), ["/workspace/result.json", "/workspace/reports/summary.txt"]);
        assert_eq!(infer_artifacts_from_prompt("a.md then ./a.md and x/../a.md", "/w"), ["/w/a.md"]);
        assert!(infer_artifacts_from_prompt("Run python3.12 and explain the result.", "/app").is_empty());
        assert_eq!(prompt_file_to_artifact_path("out.json)\"", "/"), "/out.json");
    }

    #[test]
    fn python_path_helpers() {
        assert_eq!(normpath("/work/../up.csv"), "/up.csv");
        assert_eq!(normpath("/../a//./b/"), "/a/b");
        assert_eq!(normpath("../a/../../b"), "../../b");
        assert_eq!(normpath("//x/y"), "//x/y");
        assert_eq!(py_path("./td/"), "td");
        assert_eq!(py_path(""), ".");
        assert_eq!(py_path("///a//b"), "/a/b");
        assert_eq!(posix_split("a/b/"), ("a/b".into(), "".into()));
        assert_eq!(posix_split("/x"), ("/".into(), "x".into()));
        assert_eq!(slugify(" My Job! "), "my-job");
        assert_eq!(slugify("--"), "task");
    }

    /// Python `toml` encoder quirks (outputs captured from toml 0.10.2).
    #[test]
    fn python_toml_encoder() {
        assert_eq!(dump_str("q\"'\\ \t\n\u{1}é中"), "\"q\\\"'\\ \\t\\nx01é中\"");
        assert_eq!(dump_str("it's \"x\""), "\"it's \\\"x\\\"\"");
        assert_eq!(dump_str("a\\b"), "\"a\\\\b\"");
        for (f, s) in [(1e-5, "1e-5"), (1e16, "1e+16"), (1.5e300, "1.5e+300"), (0.1, "0.1"), (600.0, "600.0"), (-3.0, "-3.0")] {
            assert_eq!(dump_float(f), s);
        }
        assert_eq!(dump_value(&json!([[1, 2], ["a"]])), "[ [ 1, 2,], [ \"a\",],]");
        let m = json!({"a": 1e-5, "k y": "v", "m": {"x": {"y": 1}, "z": [{"q": 1, "w": {"r": 2}}]}, "t": true});
        assert_eq!(toml_dumps(m.as_object().unwrap()).unwrap(), "a = 1e-5\n\"k y\" = \"v\"\nt = true\n\n[m]\n[[m.z]]\nq = 1\n\n[m.z.w]\nr = 2\n\n[m.x]\ny = 1\n");
    }

    fn print_output(a: &ExecArgs) -> String {
        match resolve_config(a) {
            Ok(c) => serde_json::to_string_pretty(&print_value(&c)).unwrap(),
            Err(e) => format!("ERROR Error: {}", format!("{e:#}").split_whitespace().collect::<Vec<_>>().join(" ")),
        }
    }

    /// `hbench exec ... --print-config` goldens (tests/fixtures/exec, produced by upstream 0.24.0).
    #[test]
    fn print_config_matches_hbench_golden() {
        let spec: Value = serde_json::from_str(include_str!("../tests/fixtures/exec/print_config_cases.json")).unwrap();
        let golden = include_str!("../tests/fixtures/exec/print_config_golden.txt");
        let root = tmp("print");
        write_fixture(&spec, &root);
        let r = root.to_string_lossy().into_owned();
        let want: Vec<(&str, &str)> = golden.split("### ").filter(|b| !b.is_empty()).map(|b| b.split_once('\n').unwrap()).collect();
        let cases = spec["cases"].as_array().unwrap();
        assert_eq!(cases.len(), want.len());
        for (case, (head, body)) in cases.iter().zip(want) {
            let argv: Vec<String> = case.as_array().unwrap().iter().map(|a| a.as_str().unwrap().replace("{ROOT}", &r)).collect();
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            let got = print_output(&args(&argv)).replace(&r, "{ROOT}");
            assert_eq!(got, body.trim_end(), "case {head}");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn default_job_names_and_temp_task_dir() {
        let c = resolve_config(&args(&["-i", "make a.md", "--ri", "combine"])).unwrap();
        let (m, r) = (c.map.job.job_name.clone().unwrap(), c.reduce.as_ref().unwrap().job.job_name.clone().unwrap());
        assert!(m.ends_with("-map") && r.ends_with("-reduce"));
        assert_eq!(m.trim_end_matches("-map"), r.trim_end_matches("-reduce"));
        let out = c.map.compile.output_dir.clone().unwrap().0;
        assert!(out.starts_with(&format!("{}/hbench-exec-map-tasks-", temp_root().display())), "{out}");
        assert_eq!(c.reduce.unwrap().task.output_dir.0, out);
    }

    /// Dump a task dir the way the oracle script does.
    fn dump_tree(dir: &Path, out: &mut Vec<String>) {
        use std::os::unix::fs::PermissionsExt;
        let assets = [
            (TPL_AUTO_VERIFY, "auto-verify-artifacts.sh"),
            (TPL_AUTO_VERIFY_SCHEMAS, "auto-verify-artifacts-with-schemas.sh"),
            (TPL_SCHEMA_VALIDATOR, "validate_artifact_schemas.py"),
            (TPL_PROMOTE_REWARD, "promote_reward_artifact.py"),
        ];
        fn walk(base: &Path, d: &Path, acc: &mut Vec<(String, Option<PathBuf>)>) {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                if p.is_dir() {
                    acc.push((rel, None));
                    walk(base, &p, acc);
                } else {
                    acc.push((rel, Some(p)));
                }
            }
        }
        let mut entries = vec![];
        walk(dir, dir, &mut entries);
        entries.sort();
        for (rel, file) in entries {
            match file {
                None => out.push(format!("DIR {rel}")),
                Some(p) => {
                    let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                    out.push(format!("== {rel} 0o{mode:o}"));
                    let text = std::fs::read_to_string(&p).unwrap();
                    match assets.iter().find(|(c, _)| *c == text) {
                        Some((_, n)) => out.push(format!("<asset {n}>")),
                        None => out.push(text.trim_end_matches('\n').to_string()),
                    }
                }
            }
        }
    }

    /// Compiled task trees match upstream's Compiler byte for byte (tests/fixtures/exec goldens).
    #[test]
    fn compile_matches_hbench_golden() {
        let spec: Value = serde_json::from_str(include_str!("../tests/fixtures/exec/compile_cases.json")).unwrap();
        let golden = include_str!("../tests/fixtures/exec/compile_golden.txt");
        let root = tmp("compile");
        write_fixture(&spec, &root);
        let r = root.to_string_lossy().into_owned();
        let mut out = vec![];
        for (name, case) in spec["cases"].as_object().unwrap() {
            out.push(format!("## {name}"));
            let mut cfg: Value = serde_json::from_str(&case.to_string().replace("{ROOT}", &r)).unwrap();
            cfg["output_dir"] = root.join("out").join(name).to_string_lossy().into_owned().into();
            let cfg: CompileConfig = serde_json::from_value(cfg).unwrap();
            cfg.validate().unwrap();
            match compile(&cfg) {
                Err(e) => out.push(format!("ERROR {}", e.to_string().replace(&r, "{ROOT}").lines().next().unwrap_or_default())),
                Ok(tasks) => {
                    for t in tasks {
                        out.push(format!("TASK {}", t.file_name().unwrap().to_string_lossy()));
                        dump_tree(&t, &mut out);
                    }
                }
            }
        }
        let got = out.join("\n");
        for (i, (g, w)) in got.lines().zip(golden.lines()).enumerate() {
            assert_eq!(g, w, "first differing line {}", i + 1);
        }
        assert_eq!(got.trim_end(), golden.trim_end());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn compile_rejects_bad_inputs() {
        let root = tmp("bad");
        let out = Some(PyPath(root.join("o").to_string_lossy().into_owned()));
        assert!(compile(&CompileConfig::default()).unwrap_err().to_string().contains("output_dir"));
        let missing = CompileConfig { output_dir: out.clone(), instructions: vec![CompileInstruction { text: None, path: Some(PyPath::new("/nonexistent/i.md")) }], ..Default::default() };
        assert!(compile(&missing).unwrap_err().to_string().starts_with("Instruction path must be a file"));
        let no_instruction = CompileConfig { output_dir: out, ..Default::default() };
        assert!(compile(&no_instruction).unwrap_err().to_string().starts_with("Compiled task is not a valid task directory"));
        let both: CompileInstruction = serde_json::from_value(json!({"text": "a", "path": "b"})).unwrap();
        assert!(both.validate().is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    fn write_trial(job: &Path, name: &str, artifacts: &[(&str, &str)]) {
        let dir = job.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, content) in artifacts {
            let p = dir.join("artifacts").join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        let r = json!({
            "id": uuid::Uuid::nil(), "task_name": "t", "trial_name": name, "trial_uri": format!("file://{}", dir.display()),
            "task_id": {"path": "t"}, "task_checksum": "", "agent_info": {"name": "oracle", "version": "1", "model_info": null},
            "config": {"task": {"path": "t"}, "trial_name": name, "trials_dir": job},
        });
        std::fs::write(dir.join("result.json"), r.to_string()).unwrap();
    }

    /// Mirrors upstream's test_executor_stages_map_artifacts_into_reduce_task_environment.
    #[test]
    fn reduce_task_stages_map_artifacts() {
        let root = tmp("reduce");
        let job = root.join("jobs/map");
        write_trial(&job, "Task B__1", &[("out.md", "b")]);
        write_trial(&job, "task-a__1", &[("x/y.txt", "a")]);
        write_trial(&job, "task-c__1", &[]);
        let task = ExecReduceTaskConfig {
            task_name: "reduce".into(),
            output_dir: PyPath(root.join("tasks").to_string_lossy().into_owned()),
            task_template: None,
            instruction: CompileInstruction::text("Combine into /app/final.md"),
            artifacts: vec!["/app/final.md".into()],
            environment: ExecReduceEnvironment::default(),
            verifier: None,
        };
        let dir = compile_reduce_task(&task, &job).unwrap();
        assert_eq!(dir.file_name().unwrap(), "reduce-0001");
        let staged = dir.join("environment/artifacts");
        assert_eq!(std::fs::read_to_string(staged.join("0001-task-b__1/out.md")).unwrap(), "b");
        assert_eq!(std::fs::read_to_string(staged.join("0002-task-a__1/x/y.txt")).unwrap(), "a");
        assert_eq!(std::fs::read_dir(&staged).unwrap().count(), 2);
        assert_eq!(std::fs::read_to_string(dir.join("instruction.md")).unwrap(), "Combine into /app/final.md\n");

        let empty = root.join("jobs/empty");
        write_trial(&empty, "t__1", &[]);
        assert_eq!(compile_reduce_task(&task, &empty).unwrap_err().to_string(), "Cannot reduce a map job with no trial artifacts.");
        std::fs::create_dir_all(root.join("jobs/none")).unwrap();
        assert_eq!(compile_reduce_task(&task, &root.join("jobs/none")).unwrap_err().to_string(), "Cannot reduce a map job with no trial results.");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn job_config_feeds_existing_runner() {
        let c = resolve_config(&args(&["-i", "make a.md", "-a", "oracle", "-n", "2", "-k", "3", "-r", "1", "--job-name", "J", "--jobs-dir", "jd", "--tasks-dir", "td", "--ae", "K=V"])).unwrap();
        let cfg = job_config(&c.map.job, &[PathBuf::from("/t/a")]).unwrap();
        assert_eq!((cfg.job_name.as_str(), cfg.jobs_dir.as_path(), cfg.n_attempts, cfg.n_concurrent_trials, cfg.retry.max_retries), ("J", Path::new("jd"), 3, 2, 1));
        assert_eq!(cfg.agents[0].env["K"], "V");
        assert_eq!(cfg.tasks[0].path.as_deref(), Some(Path::new("/t/a")));
        assert!(!cfg.verifier.disable);
        let daytona = resolve_config(&args(&["-i", "x", "-e", "daytona", "--tasks-dir", "td"])).unwrap();
        assert!(job_config(&daytona.map.job, &[PathBuf::from("/t/a")]).is_err());
    }
}
