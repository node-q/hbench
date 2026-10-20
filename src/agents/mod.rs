//! Agent abstraction and the factory mapping names to agents.

pub mod computer1;
pub mod dspy_rlm;
pub mod installed;
pub mod llm;
pub mod model_connection;
pub mod nop;
pub mod oracle;
pub mod pricing;
pub mod terminus2;

use crate::config::AgentConfig;
use crate::env::{DockerEnv, ExecResult};
use crate::task::Task;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelUsage {
    pub n_input_tokens: i64,
    pub n_cache_tokens: i64,
    pub n_output_tokens: i64,
    pub cost_usd: Option<f64>,
}

/// Agent context (token counts include cached tokens in n_input_tokens).
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentContext {
    pub n_input_tokens: Option<i64>,
    pub n_cache_tokens: Option<i64>,
    pub n_output_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub model_usage: Option<BTreeMap<String, ModelUsage>>,
    #[serde(default)]
    pub rollout_details: Option<Vec<Value>>,
    pub metadata: Option<Value>,
}

impl AgentContext {
    pub fn is_empty(&self) -> bool {
        *self == AgentContext::default()
    }
    pub fn add_usage(&mut self, input: i64, cache: i64, output: i64, cost: Option<f64>) {
        self.n_input_tokens = Some(self.n_input_tokens.unwrap_or(0) + input);
        self.n_cache_tokens = Some(self.n_cache_tokens.unwrap_or(0) + cache);
        self.n_output_tokens = Some(self.n_output_tokens.unwrap_or(0) + output);
        if let Some(c) = cost {
            self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + c);
        }
    }
}

/// What an agent sees of the environment during setup/run: execs run as the
/// phase's default user with env layering (per-exec env < agent `env` overlay).
pub struct AgentCtx<'a> {
    pub env: &'a DockerEnv,
    pub user: Option<String>,
    pub overlay: BTreeMap<String, String>,
    /// Host dir mirrored to /logs/agent.
    pub logs_dir: PathBuf,
    pub step: Option<String>,
}

impl AgentCtx<'_> {
    /// Run as the phase's default user. Non-zero exit is returned, not raised.
    pub async fn exec(&self, cmd: &str, env: &BTreeMap<String, String>, timeout: Option<Duration>) -> Result<ExecResult> {
        self.exec_as(cmd, self.user.as_deref(), env, timeout).await
    }
    pub async fn exec_as(
        &self,
        cmd: &str,
        user: Option<&str>,
        env: &BTreeMap<String, String>,
        timeout: Option<Duration>,
    ) -> Result<ExecResult> {
        let mut e = env.clone();
        e.extend(self.overlay.clone());
        self.env.exec(cmd, user, &e, timeout).await
    }
    pub async fn exec_root(&self, cmd: &str, env: &BTreeMap<String, String>) -> Result<ExecResult> {
        self.exec_as(cmd, Some("root"), env, None).await
    }
}

#[async_trait::async_trait]
pub trait Agent: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> Option<String>;
    fn setup_timeout_sec(&self) -> f64 {
        360.0
    }
    /// Native session resume across multi-step tasks.
    fn supports_resume(&self) -> bool {
        false
    }
    /// Can seed a session from a native (`atif=false`) or ATIF (`atif=true`) trajectory.
    fn supports_load(&self, _atif: bool) -> bool {
        false
    }
    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()>;
    /// `resume`: continue the previous step's session (multi-step + resume_trajectory).
    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, ctx: &mut AgentContext, resume: bool) -> Result<()>;
    /// The agent's (possibly normalized) env overlay applied to its setup/run execs;
    /// None = the configured `agent.env` as-is.
    fn extra_env(&self) -> Option<BTreeMap<String, String>> {
        None
    }
    /// ACP server launch command (target agents behind the simulated-user bridge). None = unsupported.
    fn acp_command(&self) -> Option<Vec<String>> {
        None
    }
    /// Host-derived env the ACP server needs.
    fn acp_env(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
    async fn acp_install(&mut self, _cx: &AgentCtx<'_>) -> Result<()> {
        Ok(())
    }
    async fn acp_teardown(&mut self, _cx: &AgentCtx<'_>) -> Result<()> {
        Ok(())
    }
    /// Fill usage from downloaded logs when run() left the context empty.
    fn collect_usage(&self, _logs_dir: &std::path::Path, _ctx: &mut AgentContext) {}
}

/// Common per-agent inputs.
#[derive(Clone)]
pub struct AgentArgs {
    pub model_name: Option<String>,
    pub kwargs: BTreeMap<String, Value>,
    pub extra_env: BTreeMap<String, String>,
    pub mcp_servers: Vec<crate::task::McpServer>,
    pub skills: Vec<String>,
    pub logs_dir: PathBuf,
    pub task: Arc<Task>,
    /// Trajectory to seed the first run's session from (native .jsonl or ATIF .json).
    pub load_trajectory: Option<PathBuf>,
}

impl AgentArgs {
    /// kwargs value as string (numbers/bools stringified).
    pub fn kw(&self, k: &str) -> Option<String> {
        match self.kwargs.get(k)? {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            v => Some(v.to_string()),
        }
    }
    /// kwargs value as a boolean ([`installed::common::opts::lax_bool`]); None when absent, null or not a boolean.
    pub fn kw_bool(&self, k: &str) -> Option<bool> {
        self.kwargs.get(k).and_then(installed::common::opts::lax_bool)
    }
    /// Env lookup: agent `env` (--ae) > host environment.
    pub fn get_env(&self, k: &str) -> Option<String> {
        self.extra_env.get(k).cloned().or_else(|| std::env::var(k).ok()).filter(|v| !v.is_empty())
    }
    /// `provider/model` -> (provider, model).
    pub fn provider_model(&self) -> (Option<String>, Option<String>) {
        match &self.model_name {
            Some(m) => match m.split_once('/') {
                Some((p, n)) => (Some(p.to_string()), Some(n.to_string())),
                None => (None, Some(m.clone())),
            },
            None => (None, None),
        }
    }
    /// `provider/model` -> (provider, model), split at the first `/`, for agents that need both parts.
    pub fn require_provider_model(&self) -> Result<(&str, &str)> {
        self.model_name.as_deref().and_then(|m| m.split_once('/')).context("Model name must be in the format provider/model_name")
    }
    pub fn version_kw(&self) -> Option<String> {
        self.kw("version")
    }
    /// Apply `prompt_template_path` (Jinja2 with `instruction`) if configured.
    pub fn render_instruction(&self, instruction: &str) -> Result<String> {
        let Some(p) = self.kw("prompt_template_path") else { return Ok(instruction.to_string()) };
        let tpl = std::fs::read_to_string(&p).map_err(|e| anyhow::anyhow!("Template file not found: {p}: {e}"))?;
        if !tpl.contains("instruction") {
            bail!("Prompt template {p} must use the 'instruction' variable");
        }
        let env = minijinja::Environment::new();
        Ok(env.render_str(&tpl, minijinja::context! { instruction => instruction })?)
    }
}

pub type AgentBox = Box<dyn Agent>;

/// Every agent name the factory knows.
pub fn agent_names() -> Vec<&'static str> {
    let mut v = vec!["oracle", "nop", "terminus", "terminus-1", "terminus-2", "computer-1", "dspy-rlm"];
    v.extend(installed::names());
    v
}

/// Reject kwargs outside the agent's options schema, and `config` unless the agent supports it natively.
pub fn check_kwargs(name: &str, kwargs: &BTreeMap<String, Value>) -> Result<()> {
    let Some(options) = crate::schemas::options(name) else { return Ok(()) };
    let unknown: Vec<String> = kwargs.keys().filter(|k| !options.keys.contains(*k)).map(|k| format!("  Unknown option '{k}'.")).collect();
    if !unknown.is_empty() {
        bail!("Invalid kwargs for agent '{name}':\n{}\nSee `hbench agents schema {name}` for available options.", unknown.join("\n"));
    }
    if !options.native_config && kwargs.get("config").is_some_and(|v| !v.is_null()) {
        bail!("Agent '{name}' does not support config. You can implement it.");
    }
    Ok(())
}

pub fn create(cfg: &AgentConfig, logs_dir: PathBuf, task: Arc<Task>, load_trajectory: Option<PathBuf>) -> Result<AgentBox> {
    check_kwargs(cfg.name(), &cfg.kwargs)?;
    let extra_env = crate::task::resolve_env_vars(&cfg.env)?;
    crate::redact::register_env(&extra_env);
    crate::redact::register_json(&serde_json::to_value(&cfg.kwargs)?);
    let args = AgentArgs {
        model_name: cfg.model_name.clone(),
        kwargs: cfg.kwargs.clone(),
        extra_env,
        mcp_servers: {
            let mut m = task.config.environment.mcp_servers.clone();
            m.extend(cfg.mcp_servers.clone());
            m
        },
        skills: cfg.skills.clone(),
        logs_dir,
        task,
        load_trajectory,
    };
    Ok(match cfg.name() {
        n if n.starts_with("acp:") => installed::acp::create_from_agent_name(n, args)?,
        "oracle" => Box::new(oracle::Oracle::new(args)),
        "nop" => Box::new(nop::Nop),
        "terminus" | "terminus-2" | "terminus-1" => Box::new(terminus2::Terminus2::new(args)?),
        "computer-1" => computer1::create(args)?,
        "dspy-rlm" => dspy_rlm::create(args)?,
        other => match installed::create(other, args)? {
            Some(a) => a,
            None => bail!("Unknown agent {other:?}. Known agents: {}", agent_names().join(", ")),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_come_from_the_schema() {
        let kw = |v: Value| -> BTreeMap<String, Value> { serde_json::from_value(v).unwrap() };
        for name in agent_names().into_iter().filter(|n| !matches!(*n, "oracle" | "nop")) {
            let err = check_kwargs(name, &kw(serde_json::json!({"bogus": 1, "nope": 2}))).unwrap_err().to_string();
            assert!(err.contains("Unknown option 'bogus'.\n  Unknown option 'nope'."), "{name}: {err}");
            let native = crate::schemas::options(name).unwrap().native_config;
            let config = check_kwargs(name, &kw(serde_json::json!({"config": {}})));
            assert_eq!(config.is_ok(), native, "{name}: {config:?}");
        }
        assert!(check_kwargs("cline-cli", &kw(serde_json::json!({"cline-timeout": 60}))).is_ok());
        assert!(check_kwargs("terminus", &kw(serde_json::json!({"max_episodes": 5}))).is_ok());
        assert!(check_kwargs("acp:goose", &kw(serde_json::json!({"config": {}}))).is_err());
    }
}
