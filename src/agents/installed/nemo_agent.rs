//! NVIDIA NeMo Agent Toolkit installed agent.

use super::common::{installed_dir, upload_asset, uv_install};
use super::pins;
use super::{ensure_system_dependencies, exec_agent, exec_error, exec_root};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use serde_yaml_ng::{Mapping, Value as Yaml};
use std::collections::BTreeMap;
use std::time::Duration;

const WRAPPER: &str = include_str!("../../../assets/agents/nemo-agent/nemo_agent_run_wrapper.py");
const WRAPPER_PATH: &str = "/installed-agent/nemo_agent_run_wrapper.py";
const CONFIG_PATH: &str = "/app/nemo-agent-config.yml";
const PATH_SETUP: &str = "export PATH=\"$HOME/.local/bin:$PATH\"";
const LLM_TYPES: [&str; 7] = ["nim", "openai", "azure_openai", "aws_bedrock", "litellm", "huggingface_inference", "dynamo"];
const LOG_LEVELS: [&str; 4] = ["debug", "info", "warning", "error"];

/// Per-provider settings.
struct ProviderSpec {
    api_key_env: Option<&'static str>,
    base_url_env: Option<&'static str>,
    model_field: &'static str,
    base_url_field: &'static str,
    extra: &'static [(&'static str, f64)],
    env_to_config: &'static [(&'static str, &'static str)],
    forwarded: &'static [&'static str],
}

fn provider_spec(llm_type: &str) -> ProviderSpec {
    let base = ProviderSpec {
        api_key_env: None,
        base_url_env: None,
        model_field: "model_name",
        base_url_field: "base_url",
        extra: &[],
        env_to_config: &[],
        forwarded: &[],
    };
    match llm_type {
        "nim" => ProviderSpec {
            api_key_env: Some("NVIDIA_API_KEY"),
            base_url_env: Some("NVIDIA_BASE_URL"),
            extra: &[("max_tokens", 2048.0)],
            ..base
        },
        "openai" => ProviderSpec { api_key_env: Some("OPENAI_API_KEY"), base_url_env: Some("OPENAI_BASE_URL"), ..base },
        "azure_openai" => ProviderSpec {
            api_key_env: Some("AZURE_OPENAI_API_KEY"),
            base_url_env: Some("AZURE_OPENAI_ENDPOINT"),
            model_field: "azure_deployment",
            base_url_field: "azure_endpoint",
            env_to_config: &[("AZURE_OPENAI_API_VERSION", "api_version")],
            ..base
        },
        "aws_bedrock" => ProviderSpec {
            extra: &[("max_tokens", 2048.0)],
            env_to_config: &[("AWS_DEFAULT_REGION", "region_name")],
            forwarded: &["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_DEFAULT_REGION", "AWS_REGION"],
            ..base
        },
        "litellm" => ProviderSpec { api_key_env: Some("LITELLM_API_KEY"), base_url_env: Some("LITELLM_BASE_URL"), ..base },
        "huggingface_inference" => ProviderSpec {
            api_key_env: Some("HF_TOKEN"),
            base_url_field: "endpoint_url",
            env_to_config: &[("HF_ENDPOINT_URL", "endpoint_url")],
            ..base
        },
        _ => ProviderSpec {
            api_key_env: Some("DYNAMO_API_KEY"),
            base_url_env: Some("DYNAMO_BASE_URL"),
            extra: &[("request_timeout", 600.0)],
            ..base
        },
    }
}

pub struct NemoAgent {
    args: AgentArgs,
    version: Option<String>,
    llm_type: String,
    config_file: Option<String>,
    workflow_package: Option<String>,
    nat_repo: Option<String>,
    log_level: String,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(NemoAgent::new(args)?))
}

impl NemoAgent {
    fn new(args: AgentArgs) -> Result<NemoAgent> {
        // Options with env fallbacks.
        let opt = |k: &str, env: &str| if args.kwargs.contains_key(k) { args.kw(k) } else { args.get_env(env) };
        let llm_type = opt("llm_type", "NVIDIA_NAT_LLM_TYPE").unwrap_or_else(|| "nim".into());
        if !LLM_TYPES.contains(&llm_type.as_str()) {
            bail!("Invalid kwargs for agent 'nemo-agent':\n  llm_type: Input should be {}", LLM_TYPES.map(|t| format!("'{t}'")).join(", "));
        }
        let log_level = opt("verbose", "NVIDIA_NAT_LOG_LEVEL").unwrap_or_else(|| "warning".into());
        if !LOG_LEVELS.contains(&log_level.as_str()) {
            bail!("Invalid kwargs for agent 'nemo-agent':\n  verbose: Input should be 'debug', 'info', 'warning' or 'error'");
        }
        Ok(NemoAgent {
            config_file: opt("config_file", "NVIDIA_NAT_CONFIG_FILE"),
            workflow_package: opt("workflow_package", "NVIDIA_NAT_WORKFLOW_PACKAGE"),
            nat_repo: opt("nat_repo", "NVIDIA_NAT_REPO"),
            version: args.version_kw(),
            args,
            llm_type,
            log_level,
        })
    }

    fn env_value(&self, k: &str) -> String {
        self.args.get_env(k).unwrap_or_default()
    }

    fn api_key(&self) -> String {
        provider_spec(&self.llm_type).api_key_env.map(|k| self.env_value(k)).unwrap_or_default()
    }

    fn model(&self) -> String {
        match &self.args.model_name {
            None => "meta/llama-3.3-70b-instruct".into(),
            Some(m) => m.split_once('/').map(|(_, n)| n.to_string()).unwrap_or(m.clone()),
        }
    }

    /// `_generate_config_yaml` (insertion-ordered like `yaml.dump(sort_keys=False)`).
    fn config_yaml(&self) -> String {
        let spec = provider_spec(&self.llm_type);
        let s = |v: &str| Yaml::String(v.into());
        let mut llm = Mapping::new();
        llm.insert(s("_type"), s(&self.llm_type));
        llm.insert(s(spec.model_field), s(&self.model()));
        let key = self.api_key();
        if !key.is_empty() {
            llm.insert(s("api_key"), s(&key));
        }
        if let Some(e) = spec.base_url_env {
            let url = self.env_value(e);
            if !url.is_empty() {
                llm.insert(s(spec.base_url_field), s(&url));
            }
        }
        for (env, field) in spec.env_to_config {
            let v = self.env_value(env);
            if !v.is_empty() {
                llm.insert(s(field), s(&v));
            }
        }
        for (k, v) in spec.extra {
            let n = if v.fract() == 0.0 && *k == "max_tokens" { Yaml::Number((*v as i64).into()) } else { Yaml::Number((*v).into()) };
            llm.insert(s(k), n);
        }
        llm.insert(s("temperature"), Yaml::Number(0.0.into()));
        let name = format!("{}_llm", self.llm_type);
        let mut llms = Mapping::new();
        llms.insert(s(&name), Yaml::Mapping(llm));
        let mut workflow = Mapping::new();
        workflow.insert(s("_type"), s("chat_completion"));
        workflow.insert(s("llm_name"), s(&name));
        workflow.insert(
            s("system_prompt"),
            s("You are a helpful AI assistant. Read the user's request carefully and follow the output format instructions exactly. Be concise and precise."),
        );
        let mut root = Mapping::new();
        root.insert(s("llms"), Yaml::Mapping(llms));
        root.insert(s("workflow"), Yaml::Mapping(workflow));
        let out = serde_yaml_ng::to_string(&Yaml::Mapping(root)).unwrap();
        if out.ends_with('\n') { out } else { out + "\n" }
    }

    fn build_env(&self) -> BTreeMap<String, String> {
        let spec = provider_spec(&self.llm_type);
        let mut env = BTreeMap::new();
        if let Some(k) = spec.api_key_env {
            env.insert(k.to_string(), self.api_key());
        }
        if let Some(e) = spec.base_url_env {
            env.insert(e.to_string(), self.env_value(e));
        }
        for k in spec.forwarded {
            env.insert(k.to_string(), self.env_value(k));
        }
        env.insert("NVIDIA_NAT_LOG_LEVEL".into(), self.log_level.clone());
        env.retain(|_, v| !v.is_empty());
        env
    }

    fn run_command(instruction: &str) -> String {
        let sentinel_writes = |var: &str| {
            [
                "/app/answer.txt",
                "/app/result.json",
                "/workspace/answer.txt",
                "/logs/agent/nemo-agent-output.txt",
                "/workspace/solution.txt",
                "/app/response.txt",
            ]
            .map(|p| format!("echo \"${var}\" > {p}; "))
            .concat()
        };
        format!(
            "export PATH=\"/opt/nvidia-nat-venv/bin:$HOME/.local/bin:$PATH\"; mkdir -p /app /logs/agent /workspace && \
             python3 {WRAPPER_PATH} {CONFIG_PATH} {} --trajectory-output /logs/agent/trajectory.json \
             > /app/answer.txt 2>/logs/agent/nemo-agent-stderr.txt; NEMO_EXIT=$?; if [ $NEMO_EXIT -ne 0 ]; then \
             STDERR_SUMMARY=$(head -3 /logs/agent/nemo-agent-stderr.txt 2>/dev/null | tr '\\n' ' '); \
             SENTINEL=\"[ERROR] NAT workflow failed (code $NEMO_EXIT): $STDERR_SUMMARY\"; {}else if [ ! -s /app/answer.txt ]; then \
             EMPTY_SENTINEL=\"[EMPTY] NAT workflow produced no output (exit code 0)\"; {}else cp /app/answer.txt /app/result.json; \
             cp /app/answer.txt /workspace/answer.txt; cp /app/answer.txt /logs/agent/nemo-agent-output.txt; \
             cp /workspace/answer.txt /workspace/solution.txt; cp /app/answer.txt /app/response.txt; fi; fi; cat /app/answer.txt",
            quote(instruction),
            sentinel_writes("SENTINEL"),
            sentinel_writes("EMPTY_SENTINEL"),
        )
    }
}

/// Final output line + usage from the wrapper's ATIF `trajectory.json`.
pub fn parse_outputs(output: Option<&str>, trajectory: Option<&str>, ctx: &mut AgentContext) {
    if let Some(last) = output.and_then(|o| o.trim().split('\n').map(str::trim).rfind(|l| !l.is_empty())) {
        let mut m = ctx.metadata.take().filter(Value::is_object).unwrap_or_else(|| json!({}));
        m["nemo_agent_final_output"] = json!(last);
        ctx.metadata = Some(m);
    }
    let Some(fm) = trajectory.and_then(|t| serde_json::from_str::<Value>(t).ok()).map(|t| t["final_metrics"].clone()) else { return };
    if fm.as_object().is_some_and(|o| !o.is_empty()) {
        ctx.n_input_tokens = Some(fm["total_prompt_tokens"].as_i64().unwrap_or(0));
        ctx.n_output_tokens = Some(fm["total_completion_tokens"].as_i64().unwrap_or(0));
        ctx.n_cache_tokens = Some(fm["total_cached_tokens"].as_i64().unwrap_or(0));
        ctx.cost_usd = fm["total_cost_usd"].as_f64();
    }
}

#[async_trait::async_trait]
impl Agent for NemoAgent {
    fn name(&self) -> &str {
        "nemo-agent"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        let none = BTreeMap::new();
        let install = async {
            ensure_system_dependencies(cx, if self.nat_repo.is_some() { &["curl", "git"] } else { &["curl"] }).await?;
            exec_agent(cx, &format!("{PATH_SETUP}; command -v uv > /dev/null 2>&1 || {uv_install}", uv_install = uv_install(None)), &none)
                .await?;
            let user = cx.user.clone().unwrap_or_else(|| "root".into());
            exec_root(cx, &format!("mkdir -p /opt/nvidia-nat-venv && chown {user}:{user} /opt/nvidia-nat-venv"), &none).await?;
            exec_agent(cx, &format!("{PATH_SETUP}; uv venv /opt/nvidia-nat-venv --python 3.12 --clear"), &none).await?;
            let spec = match (&self.nat_repo, &self.version) {
                (Some(r), _) => quote(&format!("{r}[eval,langchain]")),
                (None, v) => quote(&format!("nvidia-nat[eval,langchain]{}", pins::NEMO_AGENT.pip_constraint(v.as_deref()))),
            };
            let cmd = format!("{PATH_SETUP}; uv pip install --python /opt/nvidia-nat-venv/bin/python {spec}");
            let r = cx.exec(&format!("set -o pipefail; {cmd}"), &none, Some(Duration::from_secs(300))).await?;
            if r.code != 0 {
                return Err(exec_error(&cmd, &r, None));
            }
            exec_agent(cx, "echo 'export PATH=\"/opt/nvidia-nat-venv/bin:$HOME/.local/bin:$PATH\"' >> ~/.bashrc", &none).await?;
            exec_root(cx, "mkdir -p /app", &none).await?;
            exec_agent(cx, "/opt/nvidia-nat-venv/bin/nat --version", &none).await?;
            anyhow::Ok(())
        };
        install.await?;
        if self.version.is_none()
            && let Ok(r) = cx.exec("export PATH=\"/opt/nvidia-nat-venv/bin:$PATH\"; nat --version", &none, None).await
            && r.code == 0
            && !r.stdout.is_empty()
        {
            let t = r.stdout.trim();
            self.version = Some(t.rsplit("version").next().unwrap_or(t).trim().to_string());
        }

        upload_asset(cx, WRAPPER, WRAPPER_PATH).await?;
        if let Some(pkg) = &self.workflow_package {
            let env = BTreeMap::from([("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())]);
            let local = std::path::Path::new(pkg);
            let cmd = if local.exists() {
                cx.env.upload_dir(local, "/installed-agent/workflow-package").await?;
                "export PATH=\"/opt/nvidia-nat-venv/bin:$HOME/.local/bin:$PATH\"; SETUPTOOLS_SCM_PRETEND_VERSION=0.0.0 pip install --no-deps /installed-agent/workflow-package".to_string()
            } else {
                format!(
                    "export PATH=\"/opt/nvidia-nat-venv/bin:$HOME/.local/bin:$PATH\"; uv pip install --python /opt/nvidia-nat-venv/bin/python {}",
                    quote(pkg)
                )
            };
            let r = cx.exec(&cmd, &env, None).await?;
            if r.code != 0 {
                bail!("Failed to install workflow package '{pkg}': {}", if r.stderr.is_empty() { &r.stdout } else { &r.stderr });
            }
        }
        if let Some(cf) = &self.config_file {
            let p = std::path::Path::new(cf);
            if !p.exists() {
                bail!("NeMo Agent Toolkit config file not found: {cf}");
            }
            cx.env.upload_file(p, CONFIG_PATH).await?;
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let env = self.build_env();
        if self.config_file.is_none() {
            let cmd = format!("cat > {CONFIG_PATH} << 'NEMOEOF'\n{}NEMOEOF", self.config_yaml());
            let r = cx.exec(&format!("set -o pipefail; {cmd}"), &env, Some(Duration::from_secs(5))).await?;
            if r.code != 0 {
                return Err(exec_error(&cmd, &r, None));
            }
        }
        exec_agent(cx, &Self::run_command(&instruction), &env).await?;
        Ok(())
    }

    fn collect_usage(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        let out = std::fs::read_to_string(logs_dir.join("nemo-agent-output.txt")).ok();
        let traj = std::fs::read_to_string(logs_dir.join("trajectory.json")).ok();
        parse_outputs(out.as_deref(), traj.as_deref(), ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common::test_args as args;

    #[test]
    fn options_env_and_config() {
        assert!(NemoAgent::new(args(json!({"llm_type": "bogus"}), &[], None)).is_err());
        assert!(NemoAgent::new(args(json!({"verbose": "loud"}), &[], None)).is_err());
        let a = NemoAgent::new(args(
            json!({}),
            &[
                ("NVIDIA_NAT_LLM_TYPE", "azure_openai"),
                ("AZURE_OPENAI_API_KEY", "k+="),
                ("AZURE_OPENAI_ENDPOINT", "https://r/"),
                ("AZURE_OPENAI_API_VERSION", "2024"),
            ],
            Some("azure/dep"),
        ))
        .unwrap();
        assert_eq!(a.llm_type, "azure_openai");
        let env = a.build_env();
        assert_eq!(
            (env["AZURE_OPENAI_API_KEY"].as_str(), env["AZURE_OPENAI_ENDPOINT"].as_str(), env["NVIDIA_NAT_LOG_LEVEL"].as_str()),
            ("k+=", "https://r/", "warning")
        );
        let y: serde_yaml_ng::Value = serde_yaml_ng::from_str(&a.config_yaml()).unwrap();
        let llm = &y["llms"]["azure_openai_llm"];
        assert_eq!(llm["azure_deployment"].as_str(), Some("dep"));
        assert_eq!(llm["api_key"].as_str(), Some("k+="));
        assert_eq!(llm["azure_endpoint"].as_str(), Some("https://r/"));
        assert_eq!(llm["api_version"].as_str(), Some("2024"));
        assert_eq!(y["workflow"]["llm_name"].as_str(), Some("azure_openai_llm"));
        let nim = NemoAgent::new(args(json!({}), &[], None)).unwrap();
        let yaml = nim.config_yaml();
        assert!(yaml.starts_with("llms:\n  nim_llm:\n    _type: nim\n    model_name: meta/llama-3.3-70b-instruct\n"), "{yaml}");
        assert!(yaml.contains("max_tokens: 2048\n"));
        assert!(
            NemoAgent::run_command("q a")
                .contains("python3 /installed-agent/nemo_agent_run_wrapper.py /app/nemo-agent-config.yml 'q a' --trajectory-output")
        );
        assert!(WRAPPER.contains("IntermediateStepToATIFConverter"));
    }

    #[test]
    fn outputs() {
        let mut ctx = AgentContext::default();
        parse_outputs(
            Some("x\n  final answer \n\n"),
            Some(r#"{"final_metrics":{"total_prompt_tokens":10,"total_completion_tokens":3,"total_cost_usd":0.1}}"#),
            &mut ctx,
        );
        assert_eq!(ctx.metadata.unwrap()["nemo_agent_final_output"], "final answer");
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens, ctx.n_cache_tokens, ctx.cost_usd), (Some(10), Some(3), Some(0), Some(0.1)));
    }
}
