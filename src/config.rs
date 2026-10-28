//! Job / trial configuration (JobConfig / TrialConfig), loaded from YAML/JSON job files.

use crate::task::{Artifact, McpServer, normalize_hosts};
use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Deserialize `T`, failing on any key `T` does not define: a misspelt key would otherwise be dropped
/// silently, and for keys like `enforce_allowlist` that changes what a run is allowed to do.
pub fn strict<'de, T, D>(d: D) -> Result<T>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
    D::Error: Send + Sync + 'static,
{
    let mut unknown = vec![];
    let v = serde_ignored::deserialize(d, |path| unknown.push(dotted(&path)))?;
    if !unknown.is_empty() {
        bail!("unknown key{} {}", if unknown.len() == 1 { "" } else { "s" }, unknown.join(", "));
    }
    Ok(v)
}

fn dotted(path: &serde_ignored::Path) -> String {
    use serde_ignored::Path;
    let join = |parent: &Path, seg: String| match dotted(parent) {
        p if p.is_empty() => seg,
        p => format!("{p}.{seg}"),
    };
    match path {
        Path::Root => String::new(),
        Path::Seq { parent, index } => join(parent, index.to_string()),
        Path::Map { parent, key } => join(parent, key.clone()),
        Path::Some { parent } | Path::NewtypeStruct { parent } | Path::NewtypeVariant { parent } => dotted(parent),
    }
}

/// [`strict`] for a JSON value, also checking `user_agent`, whose flattened agent fields serde cannot check.
pub fn strict_value<T: DeserializeOwned>(v: Value) -> Result<T> {
    if let Some(Value::Object(ua)) = v.get("user_agent") {
        let agent = ua.iter().filter(|(k, _)| !UserAgentConfig::OWN_KEYS.contains(&k.as_str()));
        strict::<AgentConfig, _>(Value::Object(agent.map(|(k, v)| (k.clone(), v.clone())).collect())).context("user_agent")?;
    }
    strict(v)
}

fn one() -> f64 {
    1.0
}

/// `env` accepts a mapping or a legacy list of `KEY=VALUE` strings.
fn env_map<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum E {
        Map(BTreeMap<String, String>),
        List(Vec<String>),
    }
    Ok(match Option::<E>::deserialize(d)? {
        None => BTreeMap::new(),
        Some(E::Map(m)) => m,
        Some(E::List(l)) => {
            let mut m = BTreeMap::new();
            for item in l {
                let (k, v) = item
                    .split_once('=')
                    .ok_or_else(|| serde::de::Error::custom(format!("Invalid env var format: {item:?}. Expected KEY=VALUE")))?;
                m.insert(k.trim().to_string(), v.trim().to_string());
            }
            m
        }
    })
}

pub fn is_sensitive_key(k: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    // KEY|SECRET|TOKEN (not TOKENS)|PASSWORD|CREDENTIAL|AUTH.
    RE.get_or_init(|| regex::Regex::new(r"(?i)(KEY|SECRET|PASSWORD|CREDENTIAL|AUTH|TOKEN$|TOKEN[^S])").unwrap()).is_match(k)
}

fn is_template(v: &str) -> bool {
    v.starts_with("${") && v.ends_with('}')
}

/// Whether a variable or kwarg holds a model endpoint (`*_BASE_URL`, `*_API_BASE`, `*_ENDPOINT`, `*_HOST`,
/// `base_url`), whose value can reveal private infrastructure.
pub fn is_endpoint_key(k: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"(?i)(BASE_?URL|API_BASE|ENDPOINT(_URL)?|_HOST)$").unwrap());
    re.is_match(k) && !k.to_ascii_uppercase().contains("ALLOW")
}

/// The written form of an endpoint value: its `${KEY}` template when equal to the host value, else `[set]`.
fn endpoint_placeholder(k: &str, v: &str) -> String {
    if v.is_empty() || is_template(v) {
        v.to_string()
    } else if std::env::var(k).ok().as_deref() == Some(v) {
        format!("${{{k}}}")
    } else {
        ENDPOINT_SET.into()
    }
}

/// What a written config holds in place of an endpoint it was given.
pub const ENDPOINT_SET: &str = "[set]";

/// Refuses an endpoint that a written config withheld as `[set]`, as a resumed job would read it.
pub fn check_endpoint_given(k: &str, v: &str) -> Result<()> {
    if v == ENDPOINT_SET && is_endpoint_key(k) {
        bail!("{k} was not written to the job's config (it reads [set]); pass endpoints as ${{VAR}} templates to resume a job");
    }
    Ok(())
}

/// Persist secrets as `${KEY}` templates (when equal to the host value) or redacted, and endpoints as
/// [`endpoint_placeholder`]s.
fn templatize<S: serde::Serializer>(env: &BTreeMap<String, String>, s: S) -> Result<S::Ok, S::Error> {
    let out: BTreeMap<&String, String> = env
        .iter()
        .map(|(k, v)| {
            let v = if is_endpoint_key(k) {
                endpoint_placeholder(k, v)
            } else if is_template(v) || !is_sensitive_key(k) {
                v.clone()
            } else if std::env::var(k).ok().as_deref() == Some(v.as_str()) {
                format!("${{{k}}}")
            } else if v.chars().count() <= 8 {
                "****".into()
            } else {
                let chars: Vec<char> = v.chars().collect();
                format!("{}****{}", chars[..4].iter().collect::<String>(), chars[chars.len() - 3..].iter().collect::<String>())
            };
            (k, v)
        })
        .collect();
    out.serialize(s)
}

/// Agent kwargs as written: endpoint strings (`base_url`, `api_base`, ...) become `[set]`.
fn mask_endpoint_kwargs<S: serde::Serializer>(kwargs: &BTreeMap<String, Value>, s: S) -> Result<S::Ok, S::Error> {
    let out: BTreeMap<&String, Value> = kwargs
        .iter()
        .map(|(k, v)| match v {
            Value::String(u) if is_endpoint_key(k) && !is_template(u) => (k, Value::String(ENDPOINT_SET.into())),
            _ => (k, v.clone()),
        })
        .collect();
    out.serialize(s)
}

/// A `model_served_by` / `model_quantization` label: 1-64 of `A-Z a-z 0-9 . _ - : +`, so no URL or
/// credential fits in one.
fn check_model_label(field: &str, v: &str) -> Result<()> {
    let plain = |c: char| c.is_ascii_alphanumeric() || "._-:+".contains(c);
    if v.is_empty() || v.len() > 64 || !v.chars().all(plain) {
        bail!("agent {field} must be 1-64 characters of letters, digits and . _ - : + (got {} characters)", v.chars().count());
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentConfig {
    /// None means oracle, serialized as "oracle".
    #[serde(serialize_with = "ser_agent_name")]
    pub name: Option<String>,
    pub import_path: Option<String>,
    pub model_name: Option<String>,
    pub n_concurrent: Option<usize>,
    pub concurrency_group: Option<String>,
    pub skills: Vec<String>,
    pub override_timeout_sec: Option<f64>,
    pub override_setup_timeout_sec: Option<f64>,
    pub max_timeout_sec: Option<f64>,
    pub resume_trajectory: bool,
    /// Seed the agent's session from a native (.jsonl) or ATIF (.json) trajectory.
    pub load_trajectory: Option<String>,
    pub extra_allowed_hosts: Vec<String>,
    /// Make `extra_allowed_hosts` also restrict a task whose agent phase is public, instead of being
    /// ignored there. The agent phase then reaches only those hosts (none, if the list is empty).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub enforce_allowlist: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include_logs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_logs: Vec<String>,
    #[serde(serialize_with = "mask_endpoint_kwargs")]
    pub kwargs: BTreeMap<String, Value>,
    #[serde(deserialize_with = "env_map", serialize_with = "templatize", skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub mcp_servers: Vec<McpServer>,
    /// What served the model, as the run declares it (e.g. `lmstudio`, `vllm`, `openrouter:together`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_served_by: Option<String>,
    /// The served model's quantization, as the run declares it (e.g. `q4_k_m`, `fp8`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_quantization: Option<String>,
}

fn ser_agent_name<S: serde::Serializer>(n: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(n.as_deref().unwrap_or("oracle"))
}

impl AgentConfig {
    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or("oracle")
    }
    pub fn check_model_labels(&self) -> Result<()> {
        for (field, v) in [("model_served_by", &self.model_served_by), ("model_quantization", &self.model_quantization)] {
            if let Some(v) = v {
                check_model_label(field, v)?;
            }
        }
        Ok(())
    }
    /// Agents sharing a key share an n_concurrent pool.
    pub fn concurrency_key(&self) -> String {
        match &self.concurrency_group {
            Some(g) => format!("group:{g}"),
            None => {
                let mut c = self.clone();
                c.n_concurrent = None;
                format!("agent:{}", serde_json::to_string(&c).unwrap())
            }
        }
    }
}

/// Simulated user: an agent that drives the target agent through a bridge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserAgentConfig {
    #[serde(flatten)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub user_persona_path: Option<PathBuf>,
    #[serde(default)]
    pub user_prompt_template_path: Option<PathBuf>,
    pub bridge: BridgeConfig,
}

impl UserAgentConfig {
    const OWN_KEYS: &[&str] = &["user_persona_path", "user_prompt_template_path", "bridge"];
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BridgeConfig {
    pub kind: String,
    #[serde(default)]
    pub prompt_path: Option<PathBuf>,
    #[serde(default)]
    pub kwargs: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EnvConfig {
    /// None means docker, serialized as "docker".
    #[serde(rename = "type", serialize_with = "ser_env_type")]
    pub kind: Option<String>,
    pub import_path: Option<String>,
    pub force_build: bool,
    pub delete: bool,
    pub stream: bool,
    pub cpu_enforcement_policy: String,
    pub memory_enforcement_policy: String,
    pub override_cpus: Option<u32>,
    pub override_memory_mb: Option<u64>,
    pub override_storage_mb: Option<u64>,
    pub override_gpus: Option<u32>,
    pub override_tpu: Option<Value>,
    pub mounts: Option<Vec<Value>>,
    pub extra_docker_compose: Vec<PathBuf>,
    #[serde(deserialize_with = "env_map", serialize_with = "templatize", skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Docker kwargs; `keep_containers: true` stops instead of removing containers.
    pub kwargs: BTreeMap<String, Value>,
    pub extra_allowed_hosts: Vec<String>,
    /// Start the main container from an image of it as agent setup left it, committed by the first trial with
    /// the same image, agent, options and hbench version (`--cache-agent-setup`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cache_agent_setup: bool,
}

fn ser_env_type<S: serde::Serializer>(k: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(k.as_deref().unwrap_or("docker"))
}

impl EnvConfig {
    pub fn keep_containers(&self) -> bool {
        self.kwargs.get("keep_containers").is_some_and(|v| v == true || v == "true")
    }
}

impl Default for EnvConfig {
    fn default() -> Self {
        EnvConfig {
            kind: None,
            import_path: None,
            force_build: false,
            delete: true,
            stream: false,
            cpu_enforcement_policy: "auto".into(),
            memory_enforcement_policy: "auto".into(),
            override_cpus: None,
            override_memory_mb: None,
            override_storage_mb: None,
            override_gpus: None,
            override_tpu: None,
            mounts: None,
            extra_docker_compose: vec![],
            env: BTreeMap::new(),
            kwargs: BTreeMap::new(),
            extra_allowed_hosts: vec![],
            cache_agent_setup: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct VerifierRunConfig {
    pub override_timeout_sec: Option<f64>,
    pub max_timeout_sec: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include_logs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_logs: Vec<String>,
    #[serde(deserialize_with = "env_map", serialize_with = "templatize", skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_path: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub kwargs: BTreeMap<String, Value>,
    pub disable: bool,
}

/// Trial-level task reference: a local path, a git-hosted task, or a task of an OCI dataset.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TaskRef {
    pub path: Option<PathBuf>,
    pub git_url: Option<String>,
    pub git_commit_id: Option<String>,
    pub name: Option<String>,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub overwrite: bool,
    pub download_dir: Option<PathBuf>,
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oci: Option<OciTask>,
}

/// A task of an OCI dataset: its layer, and the dataset manifest that lists it. `registry` is the registry host,
/// or `oci-layout:<dir>` for a dataset read from an OCI image layout `<dir>/<repository>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OciTask {
    pub registry: String,
    pub repository: String,
    pub manifest_digest: String,
    pub layer_digest: String,
}

impl TaskRef {
    pub fn local(path: PathBuf, source: Option<String>) -> TaskRef {
        TaskRef { path: Some(path), source, ..Default::default() }
    }
    pub fn validate(&self) -> Result<()> {
        if self.name.is_some() {
            bail!("package tasks (name/ref) need a hosted package registry, which hbench doesn't support; use path or git_url+path");
        }
        if self.path.is_none() {
            bail!("task config needs 'path'");
        }
        if self.git_commit_id.is_some() && self.git_url.is_none() {
            bail!("'git_commit_id' requires 'git_url' to be set.");
        }
        if let Some(o) = &self.oci {
            let plain_name = self.path.as_deref().is_some_and(|p| p.to_str().is_some_and(crate::oci::format::is_task_name));
            if self.git_url.is_some() || !plain_name || !crate::oci::is_repository(&o.repository) {
                bail!("an OCI task's path is its task name and its repository a repository path, and it has no git_url");
            }
            if ![&o.manifest_digest, &o.layer_digest].iter().all(|d| crate::oci::format::is_sha256_digest(d)) {
                bail!("an OCI task's manifest_digest and layer_digest must be sha256 digests");
            }
        }
        Ok(())
    }
    /// A task directory on the host, rather than one downloaded into the cache.
    pub fn is_local(&self) -> bool {
        self.git_url.is_none() && self.oci.is_none()
    }
    /// Short name used in trial names / filters.
    pub fn display_name(&self) -> String {
        self.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DatasetConfig {
    pub path: Option<PathBuf>,
    pub name: Option<String>,
    pub version: Option<String>,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub registry_url: Option<String>,
    pub registry_path: Option<PathBuf>,
    /// An OCI registry, `oci://<registry>/<namespace>`, or OCI image layouts, `oci-layout:<dir>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// The manifest digest an OCI dataset runs at, recorded when its tag is resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    pub repo: Option<String>,
    pub overwrite: bool,
    pub download_dir: Option<PathBuf>,
    pub task_names: Option<Vec<String>>,
    pub exclude_task_names: Option<Vec<String>>,
    pub n_tasks: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MetricConfig {
    #[serde(rename = "type", default)]
    pub kind: MetricKind,
    #[serde(default)]
    pub kwargs: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MetricKind {
    #[default]
    Mean,
    Sum,
    Min,
    Max,
    /// `uv run` on the script in `kwargs.script_path`.
    UvScript,
}

impl MetricKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MetricKind::Mean => "mean",
            MetricKind::Sum => "sum",
            MetricKind::Min => "min",
            MetricKind::Max => "max",
            MetricKind::UvScript => "uv-script",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub include_exceptions: Option<BTreeSet<String>>,
    pub exclude_exceptions: Option<BTreeSet<String>>,
    pub wait_multiplier: f64,
    pub min_wait_sec: f64,
    pub max_wait_sec: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        RetryConfig {
            max_retries: 0,
            include_exceptions: None,
            exclude_exceptions: Some(
                [
                    "AgentTimeoutError",
                    "VerifierTimeoutError",
                    "RewardFileNotFoundError",
                    "RewardFileEmptyError",
                    "VerifierOutputParseError",
                    "ApiUsageLimitError",
                    "AgentSafetyRefusalError",
                    "AgentAuthenticationError",
                    "ModelNotFoundError",
                    "UnsupportedModelParametersError",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
            ),
            wait_multiplier: 1.0,
            min_wait_sec: 1.0,
            max_wait_sec: 60.0,
        }
    }
}

impl RetryConfig {
    pub fn should_retry(&self, kind: &str) -> bool {
        if kind == crate::env::ErrorKind::Cancelled.as_str() {
            return false;
        }
        if self.exclude_exceptions.as_ref().is_some_and(|s| s.contains(kind)) {
            return false;
        }
        if let Some(inc) = &self.include_exceptions
            && !inc.is_empty()
            && !inc.contains(kind)
        {
            return false;
        }
        true
    }
    pub fn backoff_sec(&self, attempt: u32) -> f64 {
        (self.min_wait_sec * self.wait_multiplier.powi(attempt as i32)).min(self.max_wait_sec)
    }
}

/// Regrade source: re-verify a recorded trial's outputs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceTrial {
    pub action: SourceTrialAction,
    #[serde(rename = "type")]
    pub kind: SourceTrialKind,
    #[serde(default)]
    pub trial_id: Option<String>,
    #[serde(default)]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceTrialAction {
    Regrade,
}

/// Where the source trial is: a trial dir on this host.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceTrialKind {
    Local,
}

impl SourceTrial {
    pub fn regrade_local(trial_id: String, path: PathBuf) -> SourceTrial {
        SourceTrial { action: SourceTrialAction::Regrade, kind: SourceTrialKind::Local, trial_id: Some(trial_id), path: Some(path) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialConfig {
    pub task: TaskRef,
    pub trial_name: String,
    pub trials_dir: PathBuf,
    #[serde(default)]
    pub install_only: bool,
    #[serde(default = "one")]
    pub timeout_multiplier: f64,
    #[serde(default)]
    pub agent_timeout_multiplier: Option<f64>,
    #[serde(default)]
    pub verifier_timeout_multiplier: Option<f64>,
    #[serde(default)]
    pub agent_setup_timeout_multiplier: Option<f64>,
    #[serde(default)]
    pub environment_build_timeout_multiplier: Option<f64>,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub user_agent: Option<UserAgentConfig>,
    #[serde(default)]
    pub environment: EnvConfig,
    #[serde(default)]
    pub verifier: VerifierRunConfig,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    #[serde(default)]
    pub extra_instruction_paths: Vec<PathBuf>,
    #[serde(default)]
    pub extra_instructions: Vec<String>,
    #[serde(default)]
    pub job_id: Option<uuid::Uuid>,
    #[serde(default)]
    pub source_trial: Option<SourceTrial>,
}

impl TrialConfig {
    /// A trial of `task` with every other setting at its default.
    pub fn new(task: TaskRef, trial_name: String, trials_dir: PathBuf) -> TrialConfig {
        TrialConfig {
            task,
            trial_name,
            trials_dir,
            install_only: false,
            timeout_multiplier: one(),
            agent_timeout_multiplier: None,
            verifier_timeout_multiplier: None,
            agent_setup_timeout_multiplier: None,
            environment_build_timeout_multiplier: None,
            agent: AgentConfig::default(),
            user_agent: None,
            environment: EnvConfig::default(),
            verifier: VerifierRunConfig::default(),
            artifacts: vec![],
            extra_instruction_paths: vec![],
            extra_instructions: vec![],
            job_id: None,
            source_trial: None,
        }
    }

    pub fn multiplier(&self, specific: Option<f64>) -> f64 {
        specific.unwrap_or(self.timeout_multiplier)
    }

    /// Load a YAML or JSON trial config, refusing unknown keys as job configs do.
    pub fn load(path: &Path) -> Result<TrialConfig> {
        strict_value(read_config(path)?).with_context(|| format!("invalid trial config {}", path.display()))
    }
}

/// A YAML or JSON config file as a value, legacy keys rewritten ([`migrate_legacy`]).
fn read_config(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut v: Value = if path.extension().is_some_and(|e| e == "json") {
        serde_json::from_str(&raw)?
    } else {
        serde_yaml_ng::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
    };
    migrate_legacy(&mut v);
    Ok(v)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JobConfig {
    pub job_name: String,
    pub jobs_dir: PathBuf,
    pub n_attempts: usize,
    pub install_only: bool,
    pub timeout_multiplier: f64,
    pub agent_timeout_multiplier: Option<f64>,
    pub verifier_timeout_multiplier: Option<f64>,
    pub agent_setup_timeout_multiplier: Option<f64>,
    pub environment_build_timeout_multiplier: Option<f64>,
    pub debug: bool,
    pub n_concurrent_trials: usize,
    pub quiet: bool,
    pub retry: RetryConfig,
    pub environment: EnvConfig,
    pub verifier: VerifierRunConfig,
    pub metrics: Vec<MetricConfig>,
    pub agents: Vec<AgentConfig>,
    pub user_agent: Option<UserAgentConfig>,
    pub datasets: Vec<DatasetConfig>,
    pub tasks: Vec<TaskRef>,
    pub artifacts: Vec<Artifact>,
    pub extra_instruction_paths: Vec<PathBuf>,
    pub extra_instructions: Vec<String>,
    pub source_jobs: Vec<SourceJob>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceJob {
    pub action: String,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<uuid::Uuid>,
    pub path: PathBuf,
}

impl Default for JobConfig {
    fn default() -> Self {
        JobConfig {
            job_name: chrono::Local::now().format("%Y-%m-%d__%H-%M-%S").to_string(),
            jobs_dir: "jobs".into(),
            n_attempts: 1,
            install_only: false,
            timeout_multiplier: 1.0,
            agent_timeout_multiplier: None,
            verifier_timeout_multiplier: None,
            agent_setup_timeout_multiplier: None,
            environment_build_timeout_multiplier: None,
            debug: false,
            n_concurrent_trials: 4,
            quiet: false,
            retry: RetryConfig::default(),
            environment: EnvConfig::default(),
            verifier: VerifierRunConfig::default(),
            metrics: vec![],
            agents: vec![AgentConfig::default()],
            user_agent: None,
            datasets: vec![],
            tasks: vec![],
            artifacts: vec![],
            extra_instruction_paths: vec![],
            extra_instructions: vec![],
            source_jobs: vec![],
        }
    }
}

/// Unread settings that nothing known sends with a value, so setting one is refused rather than ignored.
pub const REFUSED_UNREAD_SETTINGS: [&str; 4] =
    ["environment.stream", "environment.override_tpu", "environment.import_path", "verifier.import_path"];

impl JobConfig {
    pub fn job_dir(&self) -> PathBuf {
        self.jobs_dir.join(&self.job_name)
    }

    /// Load a YAML or JSON job config (incl. legacy `orchestrator:`).
    pub fn load(path: &Path) -> Result<JobConfig> {
        let cfg: JobConfig = strict_value(read_config(path)?).with_context(|| format!("invalid job config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Settings hbench parses but never acts on, where they differ from the default. Those in
    /// [`REFUSED_UNREAD_SETTINGS`] stop the job; the others only get a warning, because hbench's viewer
    /// or node-q/hpg send them.
    pub fn unread_settings(&self) -> Vec<&'static str> {
        let (e, v, default) = (&self.environment, &self.verifier, EnvConfig::default());
        [
            ("debug", self.debug),
            ("environment.stream", e.stream),
            ("environment.cpu_enforcement_policy", e.cpu_enforcement_policy != default.cpu_enforcement_policy),
            ("environment.memory_enforcement_policy", e.memory_enforcement_policy != default.memory_enforcement_policy),
            ("environment.override_storage_mb", e.override_storage_mb.is_some()),
            ("environment.override_tpu", e.override_tpu.as_ref().is_some_and(|t| !t.is_null())),
            ("environment.import_path", e.import_path.is_some()),
            ("verifier.import_path", v.import_path.is_some()),
            ("verifier.kwargs", !v.kwargs.is_empty()),
        ]
        .into_iter()
        .filter_map(|(key, set)| set.then_some(key))
        .collect()
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(key) = self.unread_settings().into_iter().find(|k| REFUSED_UNREAD_SETTINGS.contains(k)) {
            bail!("{key} is not supported by hbench (nothing reads it); remove it from the job config");
        }
        if self.n_attempts < 1 || self.n_concurrent_trials < 1 {
            bail!("n_attempts and n_concurrent_trials must be >= 1");
        }
        if let Some(k) = &self.environment.kind
            && k != "docker"
        {
            bail!("environment type {k:?} is not supported by hbench (only 'docker')");
        }
        normalize_hosts(&self.environment.extra_allowed_hosts)?;
        let mut pools: BTreeMap<String, Option<usize>> = BTreeMap::new();
        for a in &self.agents {
            if a.import_path.is_some() {
                bail!("agent import_path is not supported; use a built-in agent name");
            }
            normalize_hosts(&a.extra_allowed_hosts)?;
            a.check_model_labels()?;
            if let Some(n) = a.n_concurrent
                && n > self.n_concurrent_trials
            {
                bail!("Agent n_concurrent ({n}) cannot exceed n_concurrent_trials ({}).", self.n_concurrent_trials);
            }
            if a.concurrency_group.is_some() && a.n_concurrent.is_none() {
                bail!("Agents in a concurrency_group must set n_concurrent.");
            }
            let key = a.concurrency_key();
            if let Some(prev) = pools.insert(key, a.n_concurrent)
                && prev != a.n_concurrent
            {
                bail!("Agents in the same concurrency pool must use the same n_concurrent value.");
            }
        }
        for t in &self.tasks {
            t.validate()?;
        }
        if let Some(u) = &self.user_agent {
            if u.bridge.kind != "acp" {
                bail!("Unsupported bridge kind: {}", u.bridge.kind);
            }
            if self.agents.iter().any(|a| a.load_trajectory.is_some()) {
                bail!("agent.load_trajectory cannot be combined with user_agent.");
            }
        }
        for d in &self.datasets {
            if d.repo.is_some() || d.reference.is_some() || d.name.as_deref().is_some_and(|n| n.contains('/')) {
                bail!(
                    "package/repo datasets need a hosted package registry, which hbench does not support; use a registry.json dataset or a local path"
                );
            }
            match (&d.path, &d.name) {
                (None, None) => bail!("dataset needs 'path' or 'name'"),
                (Some(_), Some(_)) => bail!("Cannot set both 'path' and 'name'."),
                _ => {}
            }
            if let Some(spec) = &d.registry {
                crate::oci::Location::parse(spec)?;
                if d.registry_url.is_some() || d.registry_path.is_some() {
                    bail!("a dataset sets registry or registry_url/registry_path, not both");
                }
                if d.version.is_none() {
                    bail!("an OCI dataset needs a version");
                }
            }
            if let Some(digest) = &d.digest
                && (d.registry.is_none() || !crate::oci::format::is_sha256_digest(digest))
            {
                bail!("a dataset's digest is the sha256 manifest digest of an OCI dataset (one with a registry)");
            }
        }
        for a in &self.artifacts {
            a.spec().validate()?;
        }
        Ok(())
    }
}

/// Removes fields equal to their defaults, recursing into nested configs and into list elements whose
/// element type has a default (how config.json is written). Done on the `Value` rather than with
/// `skip_serializing_if` because the defaults are contextual (list-element defaults, `job_name` kept, agent
/// defaults under `user_agent`/`source_trial`) and the full serialization is still needed elsewhere.
fn drop_defaults(v: &mut Value, d: &Value) {
    let (Value::Object(m), Value::Object(dm)) = (v, d) else { return };
    let elem_default = |k: &str| -> Option<Value> {
        match k {
            "tasks" => serde_json::to_value(TaskRef::default()).ok(),
            "datasets" => serde_json::to_value(DatasetConfig::default()).ok(),
            "agents" => serde_json::to_value(AgentConfig::default()).ok(),
            "metrics" => Some(serde_json::json!({"type": "mean", "kwargs": {}})),
            _ => None,
        }
    };
    let keys: Vec<String> = m.keys().cloned().collect();
    for k in keys {
        let Some(dv) = dm.get(&k) else {
            if let (Some(ed), Some(Value::Array(items))) = (elem_default(&k), m.get_mut(&k)) {
                items.iter_mut().for_each(|i| drop_defaults(i, &ed));
            }
            continue;
        };
        if m[&k] == *dv {
            m.remove(&k);
            continue;
        }
        match (m.get_mut(&k).unwrap(), elem_default(&k)) {
            (Value::Array(items), Some(ed)) => items.iter_mut().for_each(|i| drop_defaults(i, &ed)),
            (Value::Object(_), _) if dv.is_object() => drop_defaults(m.get_mut(&k).unwrap(), dv),
            (Value::Object(_), _) if k == "user_agent" || k == "source_trial" => {
                if let Ok(ad) = serde_json::to_value(AgentConfig::default()) {
                    drop_defaults(m.get_mut(&k).unwrap(), &ad);
                }
            }
            _ => {}
        }
    }
}

impl TrialConfig {
    /// config.json as written by the job runner (fields at their default omitted).
    pub fn config_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        let d = TrialConfig::new(TaskRef::default(), String::new(), "trials".into());
        drop_defaults(&mut v, &serde_json::to_value(d).unwrap_or_default());
        v
    }
}

impl JobConfig {
    /// config.json as written by the job runner (fields at their default omitted; job_name is always kept).
    pub fn config_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        let mut d = serde_json::to_value(JobConfig::default()).unwrap_or_default();
        d.as_object_mut().map(|o| o.remove("job_name"));
        drop_defaults(&mut v, &d);
        v
    }
}

/// Rewrite legacy config keys to their current form, so the strict parse sees only
/// current keys: `orchestrator.{n_concurrent_trials,quiet,retry}` move to the top level, `plugins` and
/// `environment.suppress_override_warnings` (both no-ops) go, `environment.mounts_json` becomes `mounts`,
/// and a dataset's `registry: {url, path}` becomes `registry_url` / `registry_path` (a string `registry` names an
/// OCI registry and is kept).
pub fn migrate_legacy(v: &mut Value) {
    let Some(obj) = v.as_object_mut() else { return };
    obj.remove("plugins");
    if let Some(Value::Object(orch)) = obj.remove("orchestrator") {
        for (k, val) in orch.into_iter().filter(|(k, _)| ["n_concurrent_trials", "quiet", "retry"].contains(&k.as_str())) {
            obj.entry(k).or_insert(val);
        }
    }
    if let Some(Value::Object(env)) = obj.get_mut("environment") {
        env.remove("suppress_override_warnings");
        if let Some(m) = env.remove("mounts_json") {
            env.entry("mounts").or_insert(m);
        }
    }
    for d in obj.get_mut("datasets").and_then(Value::as_array_mut).into_iter().flatten() {
        let Some(d) = d.as_object_mut() else { continue };
        if !d.get("registry").is_some_and(Value::is_object) {
            continue;
        }
        if let Some(Value::Object(reg)) = d.remove("registry") {
            for (from, to) in [("url", "registry_url"), ("path", "registry_path")] {
                if let Some(val) = reg.get(from) {
                    d.entry(to).or_insert(val.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_yaml_job() {
        let dir = std::env::temp_dir().join(format!("hbench-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("job.yaml");
        std::fs::write(
            &p,
            "jobs_dir: jobs\nn_attempts: 2\norchestrator:\n  type: local\n  n_concurrent_trials: 3\n\
             environment:\n  type: docker\n  force_build: true\n  delete: true\n  env:\n    - FOO=bar\n\
             agents:\n  - name: claude-code\n    model_name: anthropic/x\n    n_concurrent: 2\n    kwargs: {max_turns: 5}\n\
             datasets:\n  - path: examples/tasks\n    task_names: ['hello-*']\n\
             metrics:\n  - type: sum\nartifacts:\n  - /app/hello.txt\n  - source: /app\n    destination: ws\n",
        )
        .unwrap();
        let c = JobConfig::load(&p).unwrap();
        assert_eq!((c.n_attempts, c.n_concurrent_trials), (2, 3));
        assert_eq!(c.environment.env["FOO"], "bar");
        assert!(c.environment.force_build);
        assert_eq!(c.agents[0].name(), "claude-code");
        assert_eq!(c.agents[0].kwargs["max_turns"], 5);
        assert_eq!(c.datasets[0].task_names.as_ref().unwrap()[0], "hello-*");
        assert_eq!(c.metrics[0].kind, MetricKind::Sum);
        assert_eq!(c.artifacts.len(), 2);
        std::fs::write(&p, "agents:\n  - name: a\n    n_concurrent: 9\n").unwrap();
        assert!(JobConfig::load(&p).is_err());
        std::fs::write(&p, "environment:\n  type: daytona\n").unwrap();
        assert!(JobConfig::load(&p).is_err());
    }

    #[test]
    fn unknown_keys_are_errors() {
        let load = |v: Value| -> Result<JobConfig> {
            let mut v = v;
            migrate_legacy(&mut v);
            strict_value(v)
        };
        let err = |v: Value| format!("{:#}", load(v).unwrap_err());
        assert_eq!(
            err(serde_json::json!({"agents": [{"name": "nop", "enforce_allow_list": true}]})),
            "unknown key agents.0.enforce_allow_list"
        );
        assert_eq!(
            err(serde_json::json!({"n_attempt": 2, "environment": {"forcebuild": true}})),
            "unknown keys n_attempt, environment.forcebuild"
        );
        assert!(
            err(serde_json::json!({"user_agent": {"name": "nop", "modle_name": "m", "bridge": {"kind": "acp"}}})).contains("modle_name")
        );
        assert!(err(serde_json::json!({"artifacts": [{"source": "/a", "dest": "b"}]})).contains("Artifact"));
        let ok = load(serde_json::json!({
            "plugins": ["x"],
            "orchestrator": {"type": "local", "n_concurrent_trials": 2, "kwargs": {}},
            "environment": {"suppress_override_warnings": true, "mounts_json": [{"type": "bind"}]},
            "datasets": [{"name": "terminal-bench", "registry": {"url": "https://r/registry.json"}}],
            "user_agent": {"name": "nop", "user_persona_path": "p.md", "bridge": {"kind": "acp"}},
            "source_jobs": [{"action": "regrade", "type": "local", "path": "jobs/j"}],
        }))
        .unwrap();
        assert_eq!(ok.n_concurrent_trials, 2);
        assert_eq!(ok.environment.mounts.as_ref().map(Vec::len), Some(1));
        assert_eq!(ok.datasets[0].registry_url.as_deref(), Some("https://r/registry.json"));
    }

    #[test]
    fn oci_datasets_take_a_registry_string() {
        let load = |v: Value| -> Result<JobConfig> {
            let mut v = v;
            migrate_legacy(&mut v);
            let cfg: JobConfig = strict_value(v)?;
            cfg.validate()?;
            Ok(cfg)
        };
        let digest = format!("sha256:{}", "0".repeat(64));
        let oci =
            load(serde_json::json!({"datasets": [{"name": "hello", "version": "1.0", "registry": "oci://ghcr.io/ns", "digest": digest}]}))
                .unwrap();
        assert_eq!(
            (oci.datasets[0].registry.as_deref(), oci.datasets[0].digest.as_deref()),
            (Some("oci://ghcr.io/ns"), Some(digest.as_str()))
        );
        assert_eq!(
            oci.config_json()["datasets"],
            serde_json::json!([{"name": "hello", "version": "1.0", "registry": "oci://ghcr.io/ns", "digest": digest}])
        );
        let legacy = load(serde_json::json!({"datasets": [{"name": "hello", "registry": {"path": "registry.json"}}]})).unwrap();
        assert_eq!(
            (legacy.datasets[0].registry_path.as_deref(), legacy.datasets[0].registry.as_deref()),
            (Some(Path::new("registry.json")), None)
        );
        assert_eq!(legacy.config_json()["datasets"], serde_json::json!([{"name": "hello", "registry_path": "registry.json"}]));
        let err = |v: Value| format!("{:#}", load(v).unwrap_err());
        assert!(
            err(serde_json::json!({"datasets": [{"name": "hello", "version": "1.0", "registry": "https://r/registry.json"}]}))
                .contains("--registry-url")
        );
        assert!(
            err(serde_json::json!({"datasets": [{"name": "hello", "version": "1.0", "registry": "oci://r/ns", "registry_url": "https://r"}]}))
                .contains("not both")
        );
        assert!(err(serde_json::json!({"datasets": [{"name": "hello", "registry": "oci://r/ns"}]})).contains("needs a version"));
        assert!(
            err(serde_json::json!({"datasets": [{"name": "hello", "registry_url": "https://r", "digest": digest}]})).contains("digest")
        );
        assert!(serde_json::to_value(TaskRef::default()).unwrap().get("oci").is_none());
    }

    #[test]
    fn trial_configs_refuse_unknown_keys() {
        let p = std::env::temp_dir().join(format!("hbench-trial-{}.yaml", uuid::Uuid::new_v4()));
        std::fs::write(&p, "task: {path: /t/a}\ntrial_name: a__1\ntrials_dir: trials\nagent: {name: nop, modle_name: m}\n").unwrap();
        assert!(format!("{:#}", TrialConfig::load(&p).unwrap_err()).contains("unknown key agent.modle_name"));
        std::fs::write(&p, "task: {path: /t/a}\ntrial_name: a__1\ntrials_dir: trials\nenvironment: {mounts_json: []}\n").unwrap();
        assert_eq!(TrialConfig::load(&p).unwrap().environment.mounts, Some(vec![]));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn secrets_are_templated_or_redacted() {
        // SAFETY: unique var name.
        unsafe { std::env::set_var("HBENCH_T_API_KEY", "sk-live-1234567890") };
        let a = AgentConfig {
            env: [
                ("HBENCH_T_API_KEY".into(), "sk-live-1234567890".into()),
                ("OTHER_TOKEN".into(), "abcdefghijkl".into()),
                ("MAX_TOKENS".into(), "100".into()),
                ("REGION".into(), "us".into()),
            ]
            .into(),
            ..Default::default()
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["env"]["HBENCH_T_API_KEY"], "${HBENCH_T_API_KEY}");
        assert_eq!(v["env"]["OTHER_TOKEN"], "abcd****jkl");
        assert_eq!(v["env"]["MAX_TOKENS"], "100");
        assert_eq!(v["env"]["REGION"], "us");
    }

    #[test]
    fn endpoints_are_written_as_set_or_templated() {
        // SAFETY: unique var name.
        unsafe { std::env::set_var("HBENCH_T_BASE_URL", "http://10.1.2.3:8000/v1") };
        let a = AgentConfig {
            env: [
                ("HBENCH_T_BASE_URL".into(), "http://10.1.2.3:8000/v1".into()),
                ("OPENAI_BASE_URL".into(), "http://user:pass@100.67.202.9:1234/v1".into()),
                ("OLLAMA_HOST".into(), "gpu-box.corp.example:11434".into()),
                ("AZURE_OPENAI_ENDPOINT".into(), "${AZ_ENDPOINT}".into()),
                ("HBENCH_ALLOW_INSECURE_MODEL_BASE_URL".into(), "true".into()),
            ]
            .into(),
            kwargs: serde_json::from_value(serde_json::json!({"api_base": "http://10.0.0.7/v1", "base_url": "${GW}", "max_turns": 3}))
                .unwrap(),
            ..Default::default()
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["env"]["HBENCH_T_BASE_URL"], "${HBENCH_T_BASE_URL}");
        assert_eq!(v["env"]["OPENAI_BASE_URL"], "[set]");
        assert_eq!(v["env"]["OLLAMA_HOST"], "[set]");
        assert_eq!(v["env"]["AZURE_OPENAI_ENDPOINT"], "${AZ_ENDPOINT}");
        assert_eq!(v["env"]["HBENCH_ALLOW_INSECURE_MODEL_BASE_URL"], "true");
        assert_eq!(v["kwargs"], serde_json::json!({"api_base": "[set]", "base_url": "${GW}", "max_turns": 3}));
        assert!(check_endpoint_given("OPENAI_BASE_URL", "[set]").unwrap_err().to_string().contains("${VAR}"));
        assert!(check_endpoint_given("OPENAI_BASE_URL", "http://h").is_ok() && check_endpoint_given("NOTE", "[set]").is_ok());
        for k in ["openrouter_base_url", "LANGSMITH_ENDPOINT", "DATABRICKS_HOST", "OPENAI_API_BASE"] {
            assert!(is_endpoint_key(k), "{k}");
        }
        for k in ["EGRESS_CONTROL_INITIAL_ALLOWED_HOSTS", "HOSTNAME", "MODEL"] {
            assert!(!is_endpoint_key(k), "{k}");
        }
    }

    #[test]
    fn model_labels_are_short_plain_strings() {
        let labels = |served: &str, quant: &str| AgentConfig {
            model_served_by: Some(served.into()),
            model_quantization: Some(quant.into()),
            ..Default::default()
        };
        assert!(labels("lmstudio", "q4_k_m").check_model_labels().is_ok());
        assert!(labels("openrouter:together", "fp8").check_model_labels().is_ok());
        for bad in ["", "http://10.0.0.1/v1", "user@host", "lm studio", "a?key=x", &"x".repeat(65), "naïve"] {
            let e = labels(bad, "fp8").check_model_labels().unwrap_err().to_string();
            assert!(e.contains("model_served_by") && (bad.is_empty() || !e.contains(bad)), "{bad}: {e}");
        }
        assert!(labels("vllm", "q4/k").check_model_labels().unwrap_err().to_string().contains("model_quantization"));
        let j: Result<JobConfig> =
            strict_value(serde_json::json!({"agents": [{"name": "terminus-2", "model_served_by": "vllm", "model_quantization": "fp8"}]}));
        assert_eq!(j.unwrap().agents[0].model_served_by.as_deref(), Some("vllm"));
    }

    #[test]
    fn config_json_excludes_defaults() {
        let t: TrialConfig = serde_json::from_value(serde_json::json!({"task": {"path": "/t/hello-world"}, "trial_name": "hello-world__x", "trials_dir": "/j", "job_id": "04eeb23d-d3b1-4af2-8b96-5fd962b43b6e"})).unwrap();
        assert_eq!(
            t.config_json(),
            serde_json::json!({"task": {"path": "/t/hello-world"}, "trial_name": "hello-world__x", "trials_dir": "/j", "job_id": "04eeb23d-d3b1-4af2-8b96-5fd962b43b6e"})
        );
        let full = serde_json::to_value(&t).unwrap();
        for k in ["install_only", "user_agent", "artifacts", "source_trial", "extra_instructions"] {
            assert!(full.get(k).is_some(), "{k} must be serialized in result.json config");
        }
        assert_eq!(full["agent"]["name"], "oracle");
        assert_eq!(full["environment"]["type"], "docker");
        let j: JobConfig = serde_json::from_value(serde_json::json!({"job_name": "j", "jobs_dir": "/x", "agents": [{"name": "oracle"}], "tasks": [{"path": "/t/a"}], "environment": {"type": "docker", "delete": true}})).unwrap();
        assert_eq!(j.config_json(), serde_json::json!({"job_name": "j", "jobs_dir": "/x", "tasks": [{"path": "/t/a"}]}));
        let j2: JobConfig = serde_json::from_value(
            serde_json::json!({"job_name": "j", "agents": [{"name": "claude-code", "model_name": "m"}], "n_attempts": 3}),
        )
        .unwrap();
        assert_eq!(
            j2.config_json(),
            serde_json::json!({"job_name": "j", "n_attempts": 3, "agents": [{"name": "claude-code", "model_name": "m"}]})
        );
        let cached: JobConfig =
            serde_json::from_value(serde_json::json!({"job_name": "j", "environment": {"cache_agent_setup": true}})).unwrap();
        assert!(cached.environment.cache_agent_setup);
        assert_eq!(cached.config_json()["environment"]["cache_agent_setup"], true);
        assert!(serde_json::to_value(JobConfig::default()).unwrap()["environment"].get("cache_agent_setup").is_none());
    }

    #[test]
    fn new_trial_configs_match_deserialized_defaults() {
        let parsed: TrialConfig =
            serde_json::from_value(serde_json::json!({"task": {"path": "/t/a"}, "trial_name": "a__1", "trials_dir": "j"})).unwrap();
        let new = TrialConfig::new(TaskRef::local("/t/a".into(), None), "a__1".into(), "j".into());
        assert_eq!(serde_json::to_value(new).unwrap(), serde_json::to_value(parsed).unwrap());
        let regrade = serde_json::to_value(SourceTrial::regrade_local("id".into(), "/j/a__1".into())).unwrap();
        assert_eq!(regrade, serde_json::json!({"action": "regrade", "type": "local", "trial_id": "id", "path": "/j/a__1"}));
    }

    #[test]
    fn retry_policy() {
        let r = RetryConfig { max_retries: 2, ..Default::default() };
        assert!(!r.should_retry("AgentTimeoutError") && !r.should_retry("CancelledError"));
        assert!(r.should_retry("ApiRateLimitError") && r.should_retry("RuntimeError"));
        let inc = RetryConfig { include_exceptions: Some(["ApiRateLimitError".to_string()].into()), ..Default::default() };
        assert!(inc.should_retry("ApiRateLimitError") && !inc.should_retry("RuntimeError"));
        let b = RetryConfig { wait_multiplier: 2.0, max_wait_sec: 5.0, ..Default::default() };
        assert_eq!((b.backoff_sec(0), b.backoff_sec(2), b.backoff_sec(9)), (1.0, 4.0, 5.0));
    }

    #[test]
    fn unread_settings_are_refused_or_reported() {
        let cfg = |v: Value| -> JobConfig { strict_value(v).unwrap() };
        for key in REFUSED_UNREAD_SETTINGS {
            let (section, field) = key.split_once('.').unwrap();
            let value = if field == "stream" { serde_json::json!(true) } else { serde_json::json!("x") };
            let err = cfg(serde_json::json!({ section: { field: value } })).validate().unwrap_err().to_string();
            assert!(err.starts_with(&format!("{key} is not supported")), "{err}");
        }
        let viewer_and_hpg = cfg(serde_json::json!({
            "debug": true,
            "environment": {"cpu_enforcement_policy": "strict", "override_storage_mb": 10240},
            "verifier": {"kwargs": {"x": 1}},
        }));
        assert!(viewer_and_hpg.validate().is_ok());
        assert_eq!(
            viewer_and_hpg.unread_settings(),
            ["debug", "environment.cpu_enforcement_policy", "environment.override_storage_mb", "verifier.kwargs"]
        );
        assert!(JobConfig::default().unread_settings().is_empty());
    }
}
