//! One agent attempt on one task (upstream's Trial / SingleStepTrial / MultiStepTrial).

use crate::agents::{self, AgentBox, AgentContext, AgentCtx};
use crate::artifacts::{self, Claims};
use crate::config::TrialConfig;
use crate::env::{DockerEnv, EnvSpec, LOG_DIRS, TrialLog, error_kind, kind_err};
use crate::task::{
    Artifact, ArtifactSpec, EnvironmentConfig, MAIN, MinReward, NetworkMode, NetworkPolicy, RewardStrategy, StepConfig, Task, VerifierMode,
    normalize_hosts, resolve_env_vars, with_convention,
};
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, watch};

pub type Rewards = BTreeMap<String, Number>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimingInfo {
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl TimingInfo {
    fn start() -> TimingInfo {
        TimingInfo { started_at: Some(Utc::now()), finished_at: None }
    }
    fn finish(mut self) -> TimingInfo {
        self.finished_at = Some(Utc::now());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExceptionInfo {
    pub exception_type: String,
    pub exception_message: String,
    pub exception_traceback: String,
    pub occurred_at: DateTime<Utc>,
}

impl ExceptionInfo {
    pub fn from(e: &anyhow::Error) -> ExceptionInfo {
        ExceptionInfo { exception_type: error_kind(e).into(), exception_message: format!("{e:#}"), exception_traceback: format!("{e:?}"), occurred_at: Utc::now() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub version: String,
    pub model_info: Option<ModelInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerifierResult {
    pub rewards: Option<Rewards>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub step_name: String,
    pub agent_result: Option<AgentContext>,
    pub verifier_result: Option<VerifierResult>,
    pub exception_info: Option<ExceptionInfo>,
    pub agent_execution: Option<TimingInfo>,
    pub verifier: Option<TimingInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialResult {
    pub id: uuid::Uuid,
    pub task_name: String,
    pub trial_name: String,
    pub trial_uri: String,
    pub task_id: Value,
    pub source: Option<String>,
    pub task_checksum: String,
    pub config: TrialConfig,
    pub agent_info: AgentInfo,
    pub agent_result: Option<AgentContext>,
    pub verifier_result: Option<VerifierResult>,
    #[serde(default)]
    pub verifier_environment_mode: Option<VerifierMode>,
    pub exception_info: Option<ExceptionInfo>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub environment_setup: Option<TimingInfo>,
    pub agent_setup: Option<TimingInfo>,
    pub agent_execution: Option<TimingInfo>,
    pub verifier: Option<TimingInfo>,
    #[serde(default)]
    pub step_results: Option<Vec<StepResult>>,
}

impl TrialResult {
    pub fn reward(&self) -> Option<&Rewards> {
        self.verifier_result.as_ref()?.rewards.as_ref()
    }
    /// (input, cache, output, cost) summed over the trial or its steps.
    pub fn usage_totals(&self) -> (Option<i64>, Option<i64>, Option<i64>, Option<f64>) {
        let ctxs: Vec<&AgentContext> = match (&self.agent_result, &self.step_results) {
            (Some(a), _) => vec![a],
            (None, Some(s)) => s.iter().filter_map(|s| s.agent_result.as_ref()).collect(),
            _ => vec![],
        };
        let sum = |f: fn(&AgentContext) -> Option<i64>| ctxs.iter().filter_map(|c| f(c)).reduce(|a, b| a + b);
        (
            sum(|c| c.n_input_tokens),
            sum(|c| c.n_cache_tokens),
            sum(|c| c.n_output_tokens),
            ctxs.iter().filter_map(|c| c.cost_usd).reduce(|a, b| a + b),
        )
    }
}

pub fn model_info(model: &Option<String>) -> Option<ModelInfo> {
    model.as_ref().map(|m| match m.split_once('/') {
        Some((p, n)) => ModelInfo { name: n.into(), provider: Some(p.into()) },
        None => ModelInfo { name: m.clone(), provider: None },
    })
}

/// upstream's task id: `{path}` (local) or `{git_url, git_commit_id, path}`.
pub fn task_id(cfg: &TrialConfig) -> Value {
    let t = &cfg.task;
    let mut m = serde_json::Map::new();
    if let Some(u) = &t.git_url {
        m.insert("git_url".into(), u.clone().into());
        m.insert("git_commit_id".into(), t.git_commit_id.clone().into());
    }
    m.insert("path".into(), t.path.as_ref().map(|p| p.display().to_string()).into());
    Value::Object(m)
}

/// Agent errors that are recorded while the verifier still runs (upstream's
/// AgentTimeoutError + NonZeroAgentExitCodeError family).
pub fn is_agent_run_error(kind: &str) -> bool {
    kind == "AgentTimeoutError"
        || kind == "NonZeroAgentExitCodeError"
        || kind == "AgentAuthenticationError"
        || kind == "ModelNotFoundError"
        || kind == "UnsupportedModelParametersError"
        || kind == "NetworkConnectionError"
        || kind.starts_with("Api")
        || matches!(kind, "OutputTokenExceededError" | "ContextWindowExceededError" | "UnknownApiError" | "AgentSafetyRefusalError")
}

/// Per-phase network policies.
#[derive(Clone)]
struct NetPlan {
    agent_env_baseline: NetworkPolicy,
    agent_phase: NetworkPolicy,
    verifier_env_baseline: Option<NetworkPolicy>,
    verifier_phase: NetworkPolicy,
}

fn merge_hosts(p: NetworkPolicy, extra: &[String]) -> NetworkPolicy {
    if extra.is_empty() || p.mode == NetworkMode::Public {
        return p;
    }
    let mut hosts = p.allowed_hosts.clone();
    for h in extra {
        if !hosts.contains(h) {
            hosts.push(h.clone());
        }
    }
    NetworkPolicy { mode: NetworkMode::Allowlist, allowed_hosts: hosts }
}

pub struct Trial {
    pub cfg: TrialConfig,
    task: Arc<Task>,
    dir: PathBuf,
    log: Arc<TrialLog>,
    agent: Option<AgentBox>,
    env: Option<DockerEnv>,
    env_stopped: bool,
    result: TrialResult,
    agent_permit: Option<Arc<Semaphore>>,
    shared_verifier_dirty: bool,
}

impl Trial {
    pub fn new(cfg: TrialConfig, task: Arc<Task>, agent_permit: Option<Arc<Semaphore>>) -> Result<Trial> {
        let dir = cfg.trials_dir.join(&cfg.trial_name);
        for d in ["agent", "verifier", "artifacts"] {
            std::fs::create_dir_all(dir.join(d))?;
        }
        std::fs::write(dir.join("config.json"), serde_json::to_string_pretty(&cfg)?)?;
        std::fs::write(dir.join("lock.json"), serde_json::to_string_pretty(&trial_lock(&cfg))?)?;
        let log = TrialLog::create(&dir.join("trial.log"))?;
        let result = TrialResult {
            id: uuid::Uuid::new_v4(),
            task_name: task.name(),
            trial_name: cfg.trial_name.clone(),
            trial_uri: format!("file://{}", dir.canonicalize().unwrap_or(dir.clone()).display()),
            task_id: task_id(&cfg),
            source: cfg.task.source.clone(),
            task_checksum: task.checksum().unwrap_or_default(),
            config: cfg.clone(),
            agent_info: AgentInfo { name: cfg.agent.name().into(), version: "unknown".into(), model_info: model_info(&cfg.agent.model_name) },
            agent_result: None,
            verifier_result: None,
            verifier_environment_mode: (!task.has_steps()).then(|| task.verifier_mode(None)),
            exception_info: None,
            started_at: Some(Utc::now()),
            finished_at: None,
            environment_setup: None,
            agent_setup: None,
            agent_execution: None,
            verifier: None,
            step_results: None,
        };
        Ok(Trial { cfg, task, dir, log, agent: None, env: None, env_stopped: false, result, agent_permit, shared_verifier_dirty: false })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn secs(&self, base: f64, max: Option<f64>, mult: Option<f64>) -> Duration {
        let b = max.map_or(base, |m| base.min(m));
        Duration::from_secs_f64((b * self.cfg.multiplier(mult)).max(0.001))
    }
    fn agent_timeout(&self, step: Option<&StepConfig>) -> Option<Duration> {
        let base = self.cfg.agent.override_timeout_sec.or(step.and_then(|s| s.agent.timeout_sec)).or(self.task.config.agent.timeout_sec)?;
        Some(self.secs(base, self.cfg.agent.max_timeout_sec, self.cfg.agent_timeout_multiplier))
    }
    fn verifier_timeout(&self, step: Option<&StepConfig>) -> Duration {
        let base = self.cfg.verifier.override_timeout_sec.or(step.and_then(|s| s.verifier.timeout_sec)).or(self.task.config.verifier.timeout_sec).unwrap_or(600.0);
        self.secs(base, self.cfg.verifier.max_timeout_sec, self.cfg.verifier_timeout_multiplier)
    }
    fn build_timeout(&self, ec: &EnvironmentConfig) -> Duration {
        self.secs(ec.build_timeout_sec, None, self.cfg.environment_build_timeout_multiplier)
    }
    fn agent_user(&self, step: Option<&StepConfig>) -> Option<String> {
        step.and_then(|s| s.agent.user.clone()).or(self.task.config.agent.user.clone()).map(|u| u.to_string())
    }
    fn verifier_user(&self, step: Option<&StepConfig>) -> Option<String> {
        step.and_then(|s| s.verifier.user.clone()).or(self.task.config.verifier.user.clone()).map(|u| u.to_string())
    }

    fn net_plan(&self, step: Option<&StepConfig>) -> Result<NetPlan> {
        let tc = &self.task.config;
        let env_hosts = normalize_hosts(&self.cfg.environment.extra_allowed_hosts)?;
        let agent_env_baseline = merge_hosts(tc.environment.baseline(), &env_hosts);
        let explicit_agent = step.and_then(|s| s.agent.explicit_policy()).or_else(|| tc.agent.explicit_policy());
        let agent_phase = merge_hosts(explicit_agent.unwrap_or_else(|| agent_env_baseline.clone()), &normalize_hosts(&self.cfg.agent.extra_allowed_hosts)?);
        let verifier_env_baseline = match self.task.verifier_env_definition(step) {
            None => None,
            Some((ec, _, _)) => {
                let inherits = step.is_none_or(|s| s.verifier.environment.is_none()) && tc.verifier.environment.is_none();
                Some(if inherits { merge_hosts(ec.baseline(), &env_hosts) } else { ec.baseline() })
            }
        };
        let explicit_verifier = step.and_then(|s| s.verifier.explicit_policy()).or_else(|| tc.verifier.explicit_policy());
        let verifier_phase = explicit_verifier.unwrap_or_else(|| verifier_env_baseline.clone().unwrap_or_else(|| agent_env_baseline.clone()));
        Ok(NetPlan { agent_env_baseline, agent_phase, verifier_env_baseline, verifier_phase })
    }

    fn artifact_specs(&self, step: Option<&StepConfig>) -> Vec<ArtifactSpec> {
        let mut all: Vec<Artifact> = self.task.config.artifacts.clone();
        all.extend(self.cfg.artifacts.clone());
        if let Some(s) = step {
            all.extend(s.artifacts.clone());
        }
        with_convention(&all)
    }

    fn env_spec(&self, project: String, env_dir: PathBuf, config: EnvironmentConfig, baseline: NetworkPolicy, phases: Vec<NetworkPolicy>, agent_env: bool) -> Result<EnvSpec> {
        let e = &self.cfg.environment;
        Ok(EnvSpec {
            compose_dir: self.dir.join(if agent_env { "compose".to_string() } else { format!("compose-{project}") }),
            project,
            env_dir,
            build_timeout: self.build_timeout(&config),
            config,
            force_build: agent_env && e.force_build,
            persistent_env: resolve_env_vars(&e.env)?,
            override_cpus: e.override_cpus,
            override_memory_mb: e.override_memory_mb,
            override_gpus: e.override_gpus,
            mounts: if agent_env { e.mounts.clone().unwrap_or_default() } else { vec![] },
            extra_compose: if agent_env { e.extra_docker_compose.clone() } else { vec![] },
            baseline,
            phase_policies: phases,
            upload_env_dir: true,
            keep_containers: e.keep_containers,
            delete: e.delete,
            log_dirs: if agent_env { LOG_DIRS.to_vec() } else { vec!["/logs/verifier"] },
        })
    }

    fn record(&mut self, e: &anyhow::Error) {
        self.log.line(&format!("exception: {e:?}"));
        if self.result.exception_info.is_some() {
            return;
        }
        let _ = std::fs::write(self.dir.join("exception.txt"), format!("{e:?}"));
        self.result.exception_info = Some(ExceptionInfo::from(e));
    }

    fn env(&self) -> &DockerEnv {
        self.env.as_ref().expect("environment started")
    }

    fn agent_ctx(&self, user: Option<String>, step: Option<&StepConfig>) -> Result<AgentCtx<'_>> {
        Ok(AgentCtx {
            env: self.env(),
            user,
            overlay: resolve_env_vars(&self.cfg.agent.env)?,
            logs_dir: self.dir.join("agent"),
            task: &self.task,
            step: step.map(|s| s.name.clone()),
        })
    }

    /// Run to completion. Never fails: errors land in exception_info.
    pub async fn run(mut self, cancel: watch::Receiver<bool>) -> TrialResult {
        let outcome = cancellable(cancel, self.run_inner()).await;
        if let Err(e) = outcome {
            self.record(&e);
            // Recover what we can for debugging, then tear down.
            if self.env.is_some() && !self.env_stopped {
                let _ = self.download_agent_logs(&self.dir.join("agent")).await;
            }
        }
        self.stop_env().await;
        if let Some(a) = &self.agent {
            let mut ctx = self.result.agent_result.take();
            if let Some(c) = ctx.as_mut().filter(|c| c.is_empty()) {
                a.populate_context_post_run(&self.dir.join("agent"), c);
            }
            self.result.agent_result = ctx;
            self.result.agent_info.version = a.version().unwrap_or_else(|| "unknown".into());
        }
        if self.task.has_steps() {
            cleanup_empty(&self.dir);
        }
        self.result.finished_at = Some(Utc::now());
        let _ = std::fs::write(self.dir.join("result.json"), serde_json::to_string_pretty(&self.result).unwrap());
        self.result
    }

    async fn stop_env(&mut self) {
        if let Some(env) = &self.env
            && !self.env_stopped
        {
            self.env_stopped = true;
            if let Err(e) = env.stop().await {
                self.log.line(&format!("env stop failed: {e:#}"));
                self.record(&e);
            }
        }
    }

    async fn run_inner(&mut self) -> Result<()> {
        if self.cfg.source_trial.is_some() {
            return crate::regrade::run(self).await;
        }
        let task = self.task.clone();
        self.agent = Some(agents::create(&self.cfg.agent, self.dir.join("agent"), task.clone())?);
        if task.has_steps() && self.cfg.agent.resume_trajectory && task.steps().len() > 1 && !self.agent.as_ref().unwrap().supports_resume() {
            bail!("Agent '{}' does not support resume; cannot honor agent.resume_trajectory", self.cfg.agent.name());
        }

        // Every policy the agent env may switch to decides whether egress control is needed.
        let steps: Vec<Option<&StepConfig>> = if task.has_steps() { task.steps().iter().map(Some).collect() } else { vec![None] };
        let mut phases = vec![];
        for s in &steps {
            let p = self.net_plan(*s)?;
            phases.push(p.agent_phase.clone());
            if task.verifier_mode(*s) == VerifierMode::Shared {
                phases.push(p.verifier_phase.clone());
            }
        }
        let baseline = self.net_plan(steps[0])?.agent_env_baseline;
        let spec = self.env_spec(crate::env::sanitize(&self.cfg.trial_name, false), task.env_dir(), task.config.environment.clone(), baseline, phases, true)?;
        let env = DockerEnv::new(spec, self.log.clone()).await?;
        self.env = Some(env);

        let t = TimingInfo::start();
        let r = self.env().start().await;
        self.result.environment_setup = Some(t.finish());
        r?;
        if let Some(hc) = &task.config.environment.healthcheck {
            self.env().run_healthcheck(hc).await?;
        }
        self.upload_skills().await?;

        let t = TimingInfo::start();
        let user = self.agent_user(steps[0]);
        let setup_timeout = self.secs(
            self.cfg.agent.override_setup_timeout_sec.unwrap_or(self.agent.as_ref().unwrap().setup_timeout_sec()),
            None,
            self.cfg.agent_setup_timeout_multiplier,
        );
        let mut agent = self.agent.take().unwrap();
        let r = match self.agent_ctx(user, steps[0]) {
            Err(e) => Err(e),
            Ok(cx) => match tokio::time::timeout(setup_timeout, agent.setup(&cx)).await {
                Ok(r) => r,
                Err(_) => Err(kind_err("AgentSetupTimeoutError", format!("Agent setup timed out after {} seconds", setup_timeout.as_secs_f64()))),
            },
        };
        self.agent = Some(agent);
        self.result.agent_setup = Some(t.finish());
        self.result.agent_info.version = self.agent.as_ref().unwrap().version().unwrap_or_else(|| "unknown".into());
        r?;
        if self.cfg.install_only {
            return Ok(());
        }
        if task.has_steps() { self.run_multi_step().await } else { self.run_single_step().await }
    }

    /// Upload `agent.skills` dirs to /hbench/skills/<name>.
    async fn upload_skills(&self) -> Result<()> {
        for s in &self.cfg.agent.skills {
            let p = Path::new(s);
            if !p.is_dir() {
                bail!("skill {s:?} is not a local directory (git/registry skills are not supported)");
            }
            let name = p.file_name().unwrap().to_string_lossy();
            self.env().upload_dir(p, &format!("/hbench/skills/{name}")).await?;
        }
        if !self.cfg.agent.skills.is_empty() {
            self.env().exec_root("chmod -R a+rX /hbench/skills", None).await?;
        }
        Ok(())
    }

    /// Run the agent under its timeout, phase network policy and agent concurrency permit.
    async fn agent_phase(&mut self, instruction: &str, step: Option<&StepConfig>, resume: bool) -> (AgentContext, TimingInfo, Result<()>) {
        let mut ctx = AgentContext::default();
        let t = TimingInfo::start();
        let plan = match self.net_plan(step) {
            Ok(p) => p,
            Err(e) => return (ctx, t.finish(), Err(e)),
        };
        let _permit = match &self.agent_permit {
            Some(s) => Some(s.clone().acquire_owned().await.unwrap()),
            None => None,
        };
        let timeout = self.agent_timeout(step);
        let switch = plan.agent_phase != plan.agent_env_baseline;
        if switch && let Err(e) = self.env().set_network_policy(&plan.agent_phase).await {
            return (ctx, t.finish(), Err(e));
        }
        let mut agent = self.agent.take().expect("agent created");
        let r = match self.agent_ctx(self.agent_user(step), step) {
            Err(e) => Err(e),
            Ok(cx) => {
                let run = agent.run(instruction, &cx, &mut ctx, resume);
                match timeout {
                    Some(d) => match tokio::time::timeout(d, run).await {
                        Ok(r) => r,
                        Err(_) => Err(kind_err("AgentTimeoutError", format!("Agent execution timed out after {} seconds", d.as_secs_f64()))),
                    },
                    None => run.await,
                }
            }
        };
        self.agent = Some(agent);
        if switch && let Err(e) = self.env().set_network_policy(&plan.agent_env_baseline).await {
            return (ctx, t.finish(), Err(e));
        }
        (ctx, t.finish(), r)
    }

    async fn download_agent_logs(&self, target: &Path) -> Result<()> {
        let env = self.env();
        let a = &self.cfg.agent;
        let r = env.download_dir(MAIN, "/logs/agent", target).await;
        if r.is_ok() && (!a.include_logs.is_empty() || !a.exclude_logs.is_empty()) {
            filter_logs(target, &a.include_logs, &a.exclude_logs, &[]);
        }
        if let Err(e) = &r {
            self.log.line(&format!("agent log download failed: {e:#}"));
        }
        r
    }

    /// Download agent logs, fill usage, and push host-side files (e.g. exit-code.txt) back.
    async fn sync_agent_output(&mut self, ctx: &mut AgentContext) {
        let agent_dir = self.dir.join("agent");
        let _ = self.download_agent_logs(&agent_dir).await;
        if ctx.is_empty()
            && let Some(a) = &self.agent
        {
            a.populate_context_post_run(&agent_dir, ctx);
        }
        if let Err(e) = self.env().upload_dir(&agent_dir, "/logs/agent").await {
            self.log.line(&format!("agent log upload failed: {e:#}"));
        }
    }

    async fn collect_artifacts(&self, target_dir: &Path, step: Option<&StepConfig>, stop_main_first: bool) -> Result<()> {
        let env = self.env();
        let specs = self.artifact_specs(step);
        let mut hooks = self.task.config.verifier.collect.clone();
        if let Some(s) = step {
            hooks.extend(s.verifier.collect.clone());
        }
        let mut claims = Claims::default();
        let run_hooks = |svc_main: bool| {
            let hooks = hooks.clone();
            async move {
                for h in hooks.iter().filter(|h| (h.service == MAIN) == svc_main) {
                    let r = env.service_exec(&h.service, &h.command, h.user.as_ref().map(|u| u.to_string()).as_deref(), Some(Duration::from_secs_f64(h.timeout_sec))).await;
                    match r {
                        Ok(r) if r.code != 0 => self.log.line(&format!("collect hook in {} exited {}: {}", h.service, r.code, h.command)),
                        Err(e) => self.log.line(&format!("collect hook in {} failed: {e:#}", h.service)),
                        _ => {}
                    }
                }
            }
        };
        run_hooks(true).await;
        artifacts::download(env, target_dir, &specs, &[MAIN], &mut claims).await?;
        let mut sidecars = artifacts::sidecar_services(&specs);
        for h in hooks.iter().filter(|h| h.service != MAIN) {
            if !sidecars.contains(&h.service) {
                sidecars.push(h.service.clone());
            }
        }
        if sidecars.is_empty() {
            return Ok(());
        }
        if stop_main_first && let Err(e) = env.stop_service(MAIN).await {
            self.log.line(&format!("failed to stop main before sidecar collection: {e:#}"));
        }
        run_hooks(false).await;
        let svc: Vec<&str> = sidecars.iter().map(String::as_str).collect();
        artifacts::download(env, target_dir, &specs, &svc, &mut claims).await?;
        Ok(())
    }

    async fn run_single_step(&mut self) -> Result<()> {
        let mode = self.task.verifier_mode(None);
        let instruction = self.instruction(None)?;
        let (mut ctx, timing, r) = self.agent_phase(&instruction, None, false).await;
        self.result.agent_execution = Some(timing);
        let mut fatal = None;
        if let Err(e) = r {
            if is_agent_run_error(error_kind(&e)) {
                self.record(&e);
            } else {
                fatal = Some(e);
            }
        }
        self.sync_agent_output(&mut ctx).await;
        self.result.agent_result = Some(ctx);
        if let Some(e) = fatal {
            let _ = self.collect_artifacts(&self.dir.join("artifacts"), None, false).await;
            return Err(e);
        }
        let separate = mode == VerifierMode::Separate;
        self.collect_artifacts(&self.dir.join("artifacts"), None, separate).await?;
        if separate {
            self.stop_env().await;
        }
        if !self.cfg.verifier.disable {
            let t = TimingInfo::start();
            let r = self.verify(None, &self.dir.join("artifacts")).await;
            self.result.verifier = Some(t.finish());
            self.result.verifier_result = Some(VerifierResult { rewards: Some(r?) });
        }
        Ok(())
    }

    fn instruction(&self, step: Option<&StepConfig>) -> Result<String> {
        let base = match step {
            Some(s) => self.task.step_instruction(&s.name)?,
            None => self.task.instruction.clone(),
        };
        let mut parts = vec![base];
        for p in &self.cfg.extra_instruction_paths {
            parts.push(std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("Extra instruction file not found: {}: {e}", p.display()))?);
        }
        parts.extend(self.cfg.extra_instructions.clone());
        Ok(parts.join("\n\n"))
    }

    async fn run_multi_step(&mut self) -> Result<()> {
        let task = self.task.clone();
        let steps = task.steps();
        let total = steps.len();
        let mut results: Vec<StepResult> = vec![];
        for (i, step) in steps.iter().enumerate() {
            let index = i + 1;
            let resume = self.cfg.agent.resume_trajectory && index > 1;
            let next_resumes = self.cfg.agent.resume_trajectory && index < total;
            let sdir = self.dir.join("steps").join(&step.name);
            for d in ["agent", "verifier", "artifacts"] {
                std::fs::create_dir_all(sdir.join(d))?;
            }
            let mut sr = StepResult { step_name: step.name.clone(), agent_result: None, verifier_result: None, exception_info: None, agent_execution: None, verifier: None };

            if let Err(e) = self.prepare_step(step, resume).await {
                self.log.line(&format!("step {} preparation failed: {e:#}", step.name));
                sr.exception_info = Some(ExceptionInfo::from(&e));
                archive_step(&self.dir, &sdir, false);
                results.push(sr);
                break;
            }
            let instruction = self.instruction(Some(step))?;
            let (mut ctx, timing, r) = self.agent_phase(&instruction, Some(step), resume).await;
            sr.agent_execution = Some(timing);
            if let Err(e) = r {
                sr.exception_info = Some(ExceptionInfo::from(&e));
            }
            self.sync_agent_output(&mut ctx).await;
            sr.agent_result = Some(ctx);

            let mode = task.verifier_mode(Some(step));
            let last = index == total;
            let separate_last = mode == VerifierMode::Separate && last;
            if let Err(e) = self.collect_artifacts(&sdir.join("artifacts"), Some(step), separate_last).await {
                self.log.line(&format!("artifact collection failed: {e:#}"));
            }
            if separate_last {
                self.stop_env().await;
            }
            if !self.cfg.verifier.disable {
                let t = TimingInfo::start();
                if mode == VerifierMode::Shared {
                    self.env().empty_dirs(&["/logs/verifier"], true).await?;
                    self.env().empty_dirs(&["/tests"], false).await?;
                    self.shared_verifier_dirty = true;
                }
                match self.verify(Some(step), &sdir.join("artifacts")).await {
                    Ok(r) => sr.verifier_result = Some(VerifierResult { rewards: Some(r) }),
                    Err(e) => {
                        if sr.exception_info.is_none() {
                            sr.exception_info = Some(ExceptionInfo::from(&e));
                        }
                    }
                }
                sr.verifier = Some(t.finish());
            }
            archive_step(&self.dir, &sdir, next_resumes);
            let stop = (sr.exception_info.is_some() && sr.verifier_result.is_none()) || (!self.cfg.verifier.disable && min_reward_failure(sr.verifier_result.as_ref(), step.min_reward.as_ref()));
            results.push(sr);
            if stop {
                self.log.line(&format!("step {} failed its gate; aborting remaining steps", step.name));
                break;
            }
        }
        self.result.verifier_result = match task.config.multi_step_reward_strategy {
            Some(RewardStrategy::Final) => results.last().and_then(|r| r.verifier_result.clone()),
            _ => aggregate_step_rewards(&results),
        };
        self.result.step_results = Some(results);
        Ok(())
    }

    async fn prepare_step(&mut self, step: &StepConfig, resume: bool) -> Result<()> {
        if !resume {
            self.env().empty_dirs(&["/logs/agent"], true).await?;
        }
        if self.shared_verifier_dirty {
            // Remove the previous step's grading harness before the agent can see it.
            self.env().empty_dirs(&["/logs/verifier"], true).await?;
            self.env().empty_dirs(&["/tests"], false).await?;
            self.shared_verifier_dirty = false;
        }
        let env = self.env();
        let user = self.agent_user(Some(step));
        let wd = env.exec("pwd", user.as_deref(), &BTreeMap::new(), None).await?.stdout.trim().to_string();
        let wd = if wd.is_empty() { "/".to_string() } else { wd };
        let wdir = self.task.step_dir(&step.name).join("workdir");
        if wdir.exists() {
            env.upload_dir(&wdir, &wd).await?;
            if wdir.join("setup.sh").exists() {
                let script = format!("{}/setup.sh", wd.trim_end_matches('/'));
                let r = env.exec(&format!("bash {}", crate::env::quote(&script)), user.as_deref(), &BTreeMap::new(), None).await?;
                if r.code != 0 {
                    bail!("Step '{}' setup.sh exited with code {}: {}", step.name, r.code, r.stderr);
                }
            }
        }
        if let Some(hc) = &step.healthcheck {
            env.run_healthcheck(hc).await?;
        }
        Ok(())
    }

    /// Shared verifier runs in the agent env; separate builds a fresh env from the verifier definition.
    async fn verify(&self, step: Option<&StepConfig>, artifacts_dir: &Path) -> Result<Rewards> {
        let timeout = self.verifier_timeout(step);
        let fut = async {
            let plan = self.net_plan(step)?;
            let user = self.verifier_user(step);
            match self.task.verifier_env_definition(step) {
                None => {
                    let switch = plan.verifier_phase != plan.agent_env_baseline;
                    if switch {
                        self.env().set_network_policy(&plan.verifier_phase).await?;
                    }
                    let r = self.run_tests(self.env(), step, user.as_deref(), false).await;
                    if switch {
                        self.env().set_network_policy(&plan.agent_env_baseline).await?;
                    }
                    r
                }
                Some((ec, dir, bundled)) => {
                    let key = step.map(|s| s.name.clone()).unwrap_or_else(|| "trial".into());
                    let project = crate::env::sanitize(&format!("{}__verifier__{key}", self.cfg.trial_name), false);
                    let baseline = plan.verifier_env_baseline.clone().unwrap_or_else(NetworkPolicy::public);
                    let mut spec = self.env_spec(project, dir, ec, baseline.clone(), vec![plan.verifier_phase.clone()], false)?;
                    spec.upload_env_dir = !bundled;
                    let venv = DockerEnv::new(spec, self.log.clone()).await?;
                    let r = async {
                        venv.start().await.map_err(|e| {
                            if error_kind(&e) == "EnvironmentStartTimeoutError" {
                                kind_err("EnvironmentStartTimeoutError", format!("Verifier environment start timed out: {e:#}"))
                            } else {
                                e
                            }
                        })?;
                        venv.empty_dirs(&["/logs/verifier"], true).await?;
                        artifacts::upload(&venv, artifacts_dir, &self.artifact_specs(step)).await?;
                        let switch = plan.verifier_phase != baseline;
                        if switch {
                            venv.set_network_policy(&plan.verifier_phase).await?;
                        }
                        self.run_tests(&venv, step, user.as_deref(), bundled).await
                    }
                    .await;
                    if let Err(e) = venv.stop().await {
                        self.log.line(&format!("failed to stop verifier env: {e:#}"));
                    }
                    r
                }
            }
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(r) => r,
            Err(_) => Err(kind_err("VerifierTimeoutError", format!("Verifier execution timed out after {} seconds", timeout.as_secs_f64()))),
        }
    }

    /// Upload tests, run /tests/test.sh, download /logs/verifier, parse rewards.
    async fn run_tests(&self, env: &DockerEnv, step: Option<&StepConfig>, user: Option<&str>, bundled: bool) -> Result<Rewards> {
        let shared = self.task.tests_dir();
        let script = if bundled {
            "/tests/test.sh".to_string()
        } else {
            let mut sources = vec![];
            let mut script_rel = None;
            match step {
                None => {
                    if !shared.join("test.sh").exists() {
                        bail!("No test script found in: {}", shared.display());
                    }
                    sources.push(shared.clone());
                    script_rel = Some("test.sh");
                }
                Some(s) => {
                    let sdir = self.task.step_dir(&s.name).join("tests");
                    if shared.exists() {
                        sources.push(shared.clone());
                    }
                    if sdir.exists() {
                        sources.push(sdir.clone());
                    }
                    if sdir.join("test.sh").exists() || shared.join("test.sh").exists() {
                        script_rel = Some("test.sh");
                    }
                }
            }
            let Some(rel) = script_rel else { bail!("No test script found for step {:?}", step.map(|s| &s.name)) };
            for src in &sources {
                env.upload_dir(src, "/tests").await.map_err(|e| kind_err("AddTestsDirError", format!("Failed to add tests directory to environment: {e:#}")))?;
            }
            format!("/tests/{rel}")
        };
        env.exec_root(&format!("chmod +x {script}"), None).await?;
        let mut venv = resolve_env_vars(&self.task.config.verifier.env)?;
        if let Some(s) = step {
            venv.extend(resolve_env_vars(&s.verifier.env)?);
        }
        venv.extend(resolve_env_vars(&self.cfg.verifier.env)?);
        env.exec(&format!("({script}) > /logs/verifier/test-stdout.txt 2>&1"), user, &venv, None).await?;
        let vdir = self.dir.join("verifier");
        env.download_dir(MAIN, "/logs/verifier", &vdir)
            .await
            .map_err(|e| kind_err("DownloadVerifierDirError", format!("Failed to download verifier directory from environment: {e:#}")))?;
        let v = &self.cfg.verifier;
        if !v.include_logs.is_empty() || !v.exclude_logs.is_empty() {
            filter_logs(&vdir, &v.include_logs, &v.exclude_logs, &["reward.txt", "reward.json"]);
        }
        parse_rewards(&vdir)
    }

    pub fn task(&self) -> &Arc<Task> {
        &self.task
    }
    pub fn result_mut(&mut self) -> &mut TrialResult {
        &mut self.result
    }
    pub fn trial_dir(&self) -> PathBuf {
        self.dir.clone()
    }
    pub fn log(&self) -> &TrialLog {
        &self.log
    }
    /// Separate-verifier helper for regrades.
    pub async fn regrade_verify(&self, step: Option<&StepConfig>, artifacts_dir: &Path) -> Result<Rewards> {
        self.verify(step, artifacts_dir).await
    }
}

/// Reproducibility record of the resolved run input (upstream's lock.json, simplified).
pub fn trial_lock(c: &TrialConfig) -> Value {
    let local = crate::registry::local_path(&c.task);
    let digest = |p: &Path| crate::task::hash_dir(p).map(|h| format!("sha256:{h}")).unwrap_or_default();
    let file_digest = |p: &Path| std::fs::read(p).map(|b| format!("sha256:{}", crate::task::hex(&sha2::Digest::finalize(<sha2::Sha256 as sha2::Digest>::new_with_prefix(&b))))).unwrap_or_default();
    serde_json::json!({
        "schema_version": 2,
        "task": {
            "name": Task::load(&local).map(|t| t.name()).unwrap_or_default(),
            "version": Task::load(&local).ok().and_then(|t| t.config.task.and_then(|p| p.version)),
            "type": if c.task.git_url.is_some() { "git" } else { "local" },
            "digest": digest(&local),
            "source": c.task.source,
            "path": c.task.path,
            "git_url": c.task.git_url,
            "git_commit_id": c.task.git_commit_id,
        },
        "install_only": c.install_only,
        "timeout_multiplier": c.timeout_multiplier,
        "agent_timeout_multiplier": c.agent_timeout_multiplier,
        "verifier_timeout_multiplier": c.verifier_timeout_multiplier,
        "agent_setup_timeout_multiplier": c.agent_setup_timeout_multiplier,
        "environment_build_timeout_multiplier": c.environment_build_timeout_multiplier,
        "extra_instructions": c.extra_instruction_paths.iter().map(|p| serde_json::json!({"path": p, "digest": file_digest(p)}))
            .chain(c.extra_instructions.iter().map(|s| serde_json::json!({"digest": format!("sha256:{}", crate::task::hex(&<sha2::Sha256 as sha2::Digest>::digest(s.as_bytes())))})))
            .collect::<Vec<_>>(),
        "agent": c.agent,
        "skills": c.agent.skills.iter().map(|s| serde_json::json!({"name": Path::new(s).file_name().map(|n| n.to_string_lossy().into_owned()), "source": s, "digest": digest(Path::new(s))})).collect::<Vec<_>>(),
        "environment": c.environment,
        "extra_docker_compose": c.environment.extra_docker_compose.iter().map(|p| serde_json::json!({"path": p, "digest": file_digest(p)})).collect::<Vec<_>>(),
        "verifier": c.verifier,
        "source_trial": c.source_trial,
    })
}

async fn cancellable<T>(mut cancel: watch::Receiver<bool>, fut: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::select! {
        r = fut => r,
        _ = cancel.wait_for(|c| *c) => Err(kind_err("CancelledError", "Trial cancelled")),
    }
}

/// Move root agent/verifier/artifacts contents into steps/<name>/ (agent copied when the next step resumes).
fn archive_step(trial_dir: &Path, sdir: &Path, preserve_agent: bool) {
    for d in ["agent", "verifier", "artifacts"] {
        let src = trial_dir.join(d);
        let dst = sdir.join(d);
        let Ok(rd) = std::fs::read_dir(&src) else { continue };
        let _ = std::fs::create_dir_all(&dst);
        for e in rd.flatten() {
            let target = dst.join(e.file_name());
            if d == "agent" && preserve_agent {
                let _ = copy_tree(&e.path(), &target);
            } else {
                if target.exists() {
                    let _ = if target.is_dir() { std::fs::remove_dir_all(&target) } else { std::fs::remove_file(&target) };
                }
                let _ = std::fs::rename(e.path(), &target);
            }
        }
    }
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// Remove root mount dirs that hold nothing but empty directories (multi-step trials).
fn cleanup_empty(trial_dir: &Path) {
    fn empty(p: &Path) -> bool {
        std::fs::read_dir(p).map(|rd| rd.flatten().all(|e| e.path().is_dir() && empty(&e.path()))).unwrap_or(false)
    }
    for d in ["agent", "verifier", "artifacts"] {
        let p = trial_dir.join(d);
        if p.is_dir() && empty(&p) {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

fn min_reward_failure(v: Option<&VerifierResult>, min: Option<&MinReward>) -> bool {
    let Some(min) = min else { return false };
    let rewards = v.and_then(|v| v.rewards.as_ref());
    let thresholds: Vec<(String, f64)> = match min {
        MinReward::One(x) => vec![("reward".into(), *x)],
        MinReward::Keys(m) => m.iter().map(|(k, v)| (k.clone(), *v)).collect(),
    };
    thresholds.iter().any(|(k, t)| rewards.and_then(|r| r.get(k)).and_then(Number::as_f64).unwrap_or(f64::NEG_INFINITY) < *t)
}

/// Per-key mean over steps that have a verifier result (missing keys count 0).
pub fn aggregate_step_rewards(results: &[StepResult]) -> Option<VerifierResult> {
    let valid: Vec<Rewards> = results.iter().filter_map(|r| r.verifier_result.as_ref()).map(|v| v.rewards.clone().unwrap_or_default()).collect();
    if valid.is_empty() {
        return None;
    }
    let keys: std::collections::BTreeSet<&String> = valid.iter().flat_map(|r| r.keys()).collect();
    if keys.is_empty() {
        return None;
    }
    let n = valid.len() as f64;
    let rewards = keys
        .into_iter()
        .map(|k| {
            let s: f64 = valid.iter().map(|r| r.get(k).and_then(Number::as_f64).unwrap_or(0.0)).sum();
            (k.clone(), Number::from_f64(s / n).unwrap_or_else(|| Number::from(0)))
        })
        .collect();
    Some(VerifierResult { rewards: Some(rewards) })
}

/// Keep files matching `include` (if any) and not `exclude`; `protect` names always stay.
fn filter_logs(dir: &Path, include: &[String], exclude: &[String], protect: &[&str]) {
    fn walk(root: &Path, p: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let ep = e.path();
                if ep.is_dir() {
                    walk(root, &ep, out);
                } else {
                    out.push(ep.strip_prefix(root).unwrap().to_path_buf());
                }
            }
        }
    }
    let mut files = vec![];
    walk(dir, dir, &mut files);
    for f in files {
        let s = f.to_string_lossy().replace('\\', "/");
        if protect.contains(&s.as_str()) {
            continue;
        }
        let keep = (include.is_empty() || include.iter().any(|g| crate::glob_match(g, &s))) && !exclude.iter().any(|g| crate::glob_match(g, &s));
        if !keep {
            let _ = std::fs::remove_file(dir.join(&f));
        }
    }
}

/// reward.json (object of finite numbers) takes precedence over reward.txt (one float).
pub fn parse_rewards(vdir: &Path) -> Result<Rewards> {
    let (json_p, txt_p) = (vdir.join("reward.json"), vdir.join("reward.txt"));
    if json_p.exists() {
        let raw = std::fs::read_to_string(&json_p)?;
        if raw.is_empty() {
            return Err(kind_err("RewardFileEmptyError", format!("Reward file is empty at {}", json_p.display())));
        }
        let parse_err = |m: String| kind_err("VerifierOutputParseError", m);
        let v: Value = serde_json::from_str(&raw).map_err(|e| parse_err(format!("Failed to parse rewards from JSON file {}: {e}", json_p.display())))?;
        let Value::Object(map) = v else { return Err(parse_err(format!("Rewards in {} must be a JSON object", json_p.display()))) };
        let mut out = Rewards::new();
        for (k, v) in map {
            match v {
                Value::Number(n) if n.as_f64().is_some_and(f64::is_finite) => {
                    out.insert(k, n);
                }
                other => return Err(parse_err(format!("Non-numeric reward {other} in JSON file {}", json_p.display()))),
            }
        }
        Ok(out)
    } else if txt_p.exists() {
        let raw = std::fs::read_to_string(&txt_p)?;
        if raw.is_empty() {
            return Err(kind_err("RewardFileEmptyError", format!("Reward file is empty at {}", txt_p.display())));
        }
        let v: f64 = raw.trim().parse().map_err(|_| kind_err("VerifierOutputParseError", format!("Failed to parse rewards from text file {}", txt_p.display())))?;
        let n = Number::from_f64(v).filter(|_| v.is_finite()).ok_or_else(|| kind_err("VerifierOutputParseError", format!("Non-finite reward in text file {}", txt_p.display())))?;
        Ok([("reward".to_string(), n)].into())
    } else {
        Err(kind_err("RewardFileNotFoundError", format!("No reward file found at {} or {}", txt_p.display(), json_p.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hbench-test-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn reward_parsing() {
        let d = tmp("rw");
        assert_eq!(error_kind(&parse_rewards(&d).unwrap_err()), "RewardFileNotFoundError");
        std::fs::write(d.join("reward.txt"), "").unwrap();
        assert_eq!(error_kind(&parse_rewards(&d).unwrap_err()), "RewardFileEmptyError");
        std::fs::write(d.join("reward.txt"), "1\n").unwrap();
        assert_eq!(serde_json::to_string(&parse_rewards(&d).unwrap()).unwrap(), r#"{"reward":1.0}"#);
        for bad in ["abc", "inf", "nan"] {
            std::fs::write(d.join("reward.txt"), bad).unwrap();
            assert_eq!(error_kind(&parse_rewards(&d).unwrap_err()), "VerifierOutputParseError", "{bad}");
        }
        std::fs::write(d.join("reward.json"), r#"{"acc": 1, "f1": 0.5}"#).unwrap();
        assert_eq!(serde_json::to_string(&parse_rewards(&d).unwrap()).unwrap(), r#"{"acc":1,"f1":0.5}"#);
        std::fs::write(d.join("reward.json"), r#"{"acc": "1"}"#).unwrap();
        assert_eq!(error_kind(&parse_rewards(&d).unwrap_err()), "VerifierOutputParseError");
        std::fs::write(d.join("reward.json"), "0.5").unwrap();
        assert_eq!(error_kind(&parse_rewards(&d).unwrap_err()), "VerifierOutputParseError");
    }

    fn sr(r: Option<&str>) -> StepResult {
        StepResult {
            step_name: "s".into(),
            agent_result: None,
            verifier_result: r.map(|r| VerifierResult { rewards: Some(serde_json::from_str(r).unwrap()) }),
            exception_info: None,
            agent_execution: None,
            verifier: None,
        }
    }

    #[test]
    fn step_aggregation_and_gates() {
        let agg = aggregate_step_rewards(&[sr(Some(r#"{"reward":1,"x":1}"#)), sr(Some(r#"{"reward":0}"#)), sr(None)]).unwrap();
        let r = agg.rewards.unwrap();
        assert_eq!((r["reward"].as_f64(), r["x"].as_f64()), (Some(0.5), Some(0.5)));
        assert!(aggregate_step_rewards(&[sr(None)]).is_none());
        let v = VerifierResult { rewards: Some(serde_json::from_str(r#"{"reward":0.4,"a":1}"#).unwrap()) };
        assert!(min_reward_failure(Some(&v), Some(&MinReward::One(0.5))));
        assert!(!min_reward_failure(Some(&v), Some(&MinReward::One(0.4))));
        assert!(min_reward_failure(Some(&v), Some(&MinReward::Keys([("b".into(), 0.0)].into()))));
        assert!(min_reward_failure(None, Some(&MinReward::One(0.0))));
        assert!(!min_reward_failure(None, None));
    }

    #[test]
    fn archive_and_filters() {
        let d = tmp("arch");
        std::fs::create_dir_all(d.join("agent/sub")).unwrap();
        std::fs::write(d.join("agent/sub/a.txt"), "a").unwrap();
        std::fs::create_dir_all(d.join("verifier")).unwrap();
        std::fs::write(d.join("verifier/reward.txt"), "1").unwrap();
        let s = d.join("steps/one");
        archive_step(&d, &s, true);
        assert!(s.join("agent/sub/a.txt").exists() && d.join("agent/sub/a.txt").exists());
        assert!(s.join("verifier/reward.txt").exists() && !d.join("verifier/reward.txt").exists());
        archive_step(&d, &d.join("steps/two"), false);
        assert!(!d.join("agent/sub").exists());
        cleanup_empty(&d);
        assert!(!d.join("agent").exists() && !d.join("verifier").exists());

        let f = tmp("filt");
        for n in ["keep.log", "drop.txt", "reward.txt"] {
            std::fs::write(f.join(n), "x").unwrap();
        }
        filter_logs(&f, &["*.log".into()], &[], &["reward.txt"]);
        assert!(f.join("keep.log").exists() && !f.join("drop.txt").exists() && f.join("reward.txt").exists());
    }

    #[test]
    fn host_merging() {
        let p = NetworkPolicy { mode: NetworkMode::NoNetwork, allowed_hosts: vec![] };
        let m = merge_hosts(p, &["pypi.org".into()]);
        assert_eq!((m.mode, m.allowed_hosts), (NetworkMode::Allowlist, vec!["pypi.org".to_string()]));
        assert_eq!(merge_hosts(NetworkPolicy::public(), &["x.com".into()]), NetworkPolicy::public());
    }
}
