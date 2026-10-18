//! Config models for exec and compile.

use super::*;
use crate::config::{AgentConfig, EnvConfig, MetricConfig, RetryConfig, VerifierRunConfig};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};

/// A path string normalized on construction (repeated and trailing `/` and `.` segments dropped; empty -> `.`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NormPath(pub(super) String);

impl NormPath {
    pub(super) fn new(s: &str) -> NormPath {
        NormPath(posix_normalize(s))
    }
}

impl<'de> Deserialize<'de> for NormPath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(NormPath::new(&String::deserialize(d)?))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompileInstruction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<NormPath>,
}

impl CompileInstruction {
    pub(super) fn text(t: &str) -> CompileInstruction {
        CompileInstruction { text: Some(t.into()), path: None }
    }
    pub(super) fn validate(&self) -> Result<()> {
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
    pub path: Option<NormPath>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "is_default_image")]
    pub docker_image: Option<String>,
    #[serde(default, skip_serializing_if = "is_default_workdir")]
    pub workdir: Option<String>,
}

impl CompileEnvironment {
    pub(super) fn image(&self) -> &str {
        self.docker_image.as_deref().unwrap_or(DEFAULT_IMAGE)
    }
    pub(super) fn wd(&self) -> &str {
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
    pub path: Option<NormPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_verifier: Option<AutoVerifier>,
}

fn is_v1(s: &String) -> bool {
    s == "1.0"
}
pub(super) fn v1() -> String {
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
    pub output_dir: Option<NormPath>,
    #[serde(default, deserialize_with = "coerce_instructions", skip_serializing_if = "Vec::is_empty")]
    pub instructions: Vec<CompileInstruction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_template: Option<NormPath>,
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
    pub(super) fn validate(&self) -> Result<()> {
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
    pub jobs_dir: Option<NormPath>,
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
    pub(super) fn validate(&self) -> Result<()> {
        for (k, v) in [("n_attempts", self.n_attempts), ("n_concurrent_trials", self.n_concurrent_trials)] {
            if v < 1 {
                bail!("1 validation error for ExecJobConfig\n{k}\n  Input should be greater than or equal to 1");
            }
        }
        crate::config::strict_value::<RetryConfig>(Value::Object(self.retry.clone())).context("retry")?;
        crate::config::strict_value::<EnvConfig>(Value::Object(self.environment.clone())).context("environment")?;
        crate::config::strict_value::<VerifierRunConfig>(Value::Object(self.verifier.clone())).context("verifier")?;
        crate::config::strict_value::<Vec<MetricConfig>>(Value::Array(self.metrics.clone())).context("metrics")?;
        for a in &self.agents {
            crate::config::strict_value::<AgentConfig>(Value::Object(a.clone())).context("agents")?;
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
pub(super) fn image_default() -> String {
    DEFAULT_IMAGE.into()
}
pub(super) fn workdir_default() -> String {
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
pub(super) fn reduce_name() -> String {
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
    pub output_dir: NormPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_template: Option<NormPath>,
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
    pub(super) fn validate(&self) -> Result<()> {
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
