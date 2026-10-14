//! Job / trial configuration, shaped like upstream's JobConfig / TrialConfig so the
//! same YAML/JSON job files work with both.

use crate::task::{Artifact, McpServer, normalize_hosts};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn one() -> f64 {
    1.0
}

/// `env` accepts a mapping or upstream's legacy list of `KEY=VALUE` strings.
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
    // KEY|SECRET|TOKEN (not TOKENS)|PASSWORD|CREDENTIAL|AUTH, as in upstream.
    RE.get_or_init(|| regex::Regex::new(r"(?i)(KEY|SECRET|PASSWORD|CREDENTIAL|AUTH|TOKEN$|TOKEN[^S])").unwrap()).is_match(k)
}

fn is_template(v: &str) -> bool {
    v.starts_with("${") && v.ends_with('}')
}

/// Persist secrets as `${KEY}` templates (when equal to the host value) or redacted.
fn templatize<S: serde::Serializer>(env: &BTreeMap<String, String>, s: S) -> Result<S::Ok, S::Error> {
    let out: BTreeMap<&String, String> = env
        .iter()
        .map(|(k, v)| {
            let v = if is_template(v) || !is_sensitive_key(k) {
                v.clone()
            } else if std::env::var(k).ok().as_deref() == Some(v.as_str()) {
                format!("${{{k}}}")
            } else if v.len() <= 8 {
                "****".into()
            } else {
                format!("{}****{}", &v[..4], &v[v.len() - 3..])
            };
            (k, v)
        })
        .collect();
    out.serialize(s)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentConfig {
    /// None means oracle (upstream's validator), serialized as "oracle".
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include_logs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_logs: Vec<String>,
    pub kwargs: BTreeMap<String, Value>,
    #[serde(deserialize_with = "env_map", serialize_with = "templatize", skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub mcp_servers: Vec<McpServer>,
}

fn ser_agent_name<S: serde::Serializer>(n: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(n.as_deref().unwrap_or("oracle"))
}

impl AgentConfig {
    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or("oracle")
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
    /// None means docker (upstream's validator), serialized as "docker".
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

/// upstream's trial-level TaskConfig: a local path, or a git-hosted task.
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
        Ok(())
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
    pub repo: Option<String>,
    pub overwrite: bool,
    pub download_dir: Option<PathBuf>,
    pub task_names: Option<Vec<String>>,
    pub exclude_task_names: Option<Vec<String>>,
    pub n_tasks: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetricConfig {
    #[serde(rename = "type", default = "mean")]
    pub kind: String,
    #[serde(default)]
    pub kwargs: BTreeMap<String, Value>,
}
fn mean() -> String {
    "mean".into()
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
        if kind == "CancelledError" {
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
    pub action: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub trial_id: Option<String>,
    #[serde(default)]
    pub path: Option<PathBuf>,
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
    pub fn multiplier(&self, specific: Option<f64>) -> f64 {
        specific.unwrap_or(self.timeout_multiplier)
    }
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

impl JobConfig {
    pub fn job_dir(&self) -> PathBuf {
        self.jobs_dir.join(&self.job_name)
    }

    /// Load a YAML or JSON job config (upstream format, incl. legacy `orchestrator:`).
    pub fn load(path: &Path) -> Result<JobConfig> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut v: Value = if path.extension().is_some_and(|e| e == "json") {
            serde_json::from_str(&raw)?
        } else {
            serde_yaml_ng::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
        };
        migrate_legacy(&mut v);
        let cfg: JobConfig = serde_json::from_value(v).with_context(|| format!("invalid job config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
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
                bail!("agent import_path (Python agents) is not supported; use a built-in agent name");
            }
            normalize_hosts(&a.extra_allowed_hosts)?;
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
        }
        for m in &self.metrics {
            if !["mean", "sum", "min", "max", "uv-script"].contains(&m.kind.as_str()) {
                bail!("Unsupported metric type: {}", m.kind);
            }
        }
        for a in &self.artifacts {
            a.spec().validate()?;
        }
        Ok(())
    }
}

/// Pydantic's `exclude_defaults`: drop fields equal to their defaults, recursing into nested
/// models and into list elements whose element type has a default (upstream writes config.json so).
fn prune_defaults(v: &mut Value, d: &Value) {
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
                items.iter_mut().for_each(|i| prune_defaults(i, &ed));
            }
            continue;
        };
        if m[&k] == *dv {
            m.remove(&k);
            continue;
        }
        match (m.get_mut(&k).unwrap(), elem_default(&k)) {
            (Value::Array(items), Some(ed)) => items.iter_mut().for_each(|i| prune_defaults(i, &ed)),
            (Value::Object(_), _) if dv.is_object() => prune_defaults(m.get_mut(&k).unwrap(), dv),
            (Value::Object(_), _) if k == "user_agent" || k == "source_trial" => {
                if let Ok(ad) = serde_json::to_value(AgentConfig::default()) {
                    prune_defaults(m.get_mut(&k).unwrap(), &ad);
                }
            }
            _ => {}
        }
    }
}

impl TrialConfig {
    /// config.json as upstream writes it (`exclude_defaults=True`).
    pub fn config_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        let d: TrialConfig = serde_json::from_value(serde_json::json!({"task": {}, "trial_name": "", "trials_dir": "trials"}))
            .expect("default trial config");
        prune_defaults(&mut v, &serde_json::to_value(d).unwrap_or_default());
        v
    }
}

impl JobConfig {
    /// config.json as upstream writes it (`exclude_defaults=True`; job_name is always kept).
    pub fn config_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        let mut d = serde_json::to_value(JobConfig::default()).unwrap_or_default();
        d.as_object_mut().map(|o| o.remove("job_name"));
        prune_defaults(&mut v, &d);
        v
    }
}

/// upstream moved `orchestrator.{n_concurrent_trials,quiet,retry}` to the top level.
pub fn migrate_legacy(v: &mut Value) {
    let Some(obj) = v.as_object_mut() else { return };
    if let Some(Value::Object(orch)) = obj.remove("orchestrator") {
        for (k, val) in orch {
            if k != "type" && k != "kwargs" {
                obj.entry(k).or_insert(val);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_hbench_yaml_job() {
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
        assert_eq!(c.metrics[0].kind, "sum");
        assert_eq!(c.artifacts.len(), 2);
        std::fs::write(&p, "agents:\n  - name: a\n    n_concurrent: 9\n").unwrap();
        assert!(JobConfig::load(&p).is_err());
        std::fs::write(&p, "environment:\n  type: daytona\n").unwrap();
        assert!(JobConfig::load(&p).is_err());
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
    fn config_json_excludes_defaults_like_hbench() {
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
}
