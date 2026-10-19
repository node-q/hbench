//! mini-swe-agent installed agent.

use super::common::{installed_dir, uv_install};
use super::pins;
use super::{connection, ensure_system_dependencies, exec_agent};
use crate::agents::model_connection::{Resolved, Spec};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use crate::fmt::type_name;
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeMap;

const SPEC: Spec = Spec {
    default_provider: None,
    api_key_envs: &["MSWEA_API_KEY"],
    base_url_envs: &["OPENAI_BASE_URL", "OPENAI_API_BASE"],
    passthrough: true,
};
const DEFAULT_LITELLM_TIMEOUT_SEC: u32 = 3600;
const PYTHON_VERSION: &str = "3.12";
const TRAJECTORY_PATH: &str = "/logs/agent/mini-swe-agent.trajectory.json";
const ENV_PRELUDE: &str =
    "if [ -f \"$HOME/.local/bin/env\" ]; then . \"$HOME/.local/bin/env\"; else export PATH=\"$HOME/.local/bin:$PATH\"; fi; ";

/// Optional string kwarg; numbers are accepted as their text. Explicit null = opt-out (None).
pub(crate) fn opt_str(args: &AgentArgs, agent: &str, k: &str) -> Result<Option<String>> {
    match args.kwargs.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(_) => bail!("Invalid kwargs for agent '{agent}':\n  {k}: must be a string"),
    }
}

/// Trial session id (`{trial_name}__agent`); logs_dir is `<trial>/agent`.
pub(crate) fn session_id(args: &AgentArgs) -> Option<String> {
    args.logs_dir.parent()?.file_name().map(|n| format!("{}__agent", n.to_string_lossy()))
}

pub struct MiniSweAgent {
    args: AgentArgs,
    version: Option<String>,
    cost_limit: Option<String>,
    reasoning_effort: Option<String>,
    max_tokens: Option<i64>,
    config_yaml: Option<String>,
    registry_json: Option<String>,
    session_id_headers: Option<Vec<String>>,
    session_id: Option<String>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(MiniSweAgent::new(args)?))
}

impl MiniSweAgent {
    fn install_command(&self) -> String {
        let py = quote(PYTHON_VERSION);
        let pkg = quote(&format!("mini-swe-agent{}", pins::MINI_SWE_AGENT.pip_constraint(self.version.as_deref())));
        let deps: String =
            [pins::LITELLM, pins::ORJSON, pins::FASTAPI].iter().map(|d| format!(" --with {}", quote(&d.requirement()))).collect();
        format!(
            "set -euo pipefail; if ! command -v uv >/dev/null 2>&1; then  {uv_install}; fi && \
             if ! grep -q 'export PATH=\"$HOME/.local/bin:$PATH\"' \"$HOME/.bashrc\" 2>/dev/null; then  echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> \"$HOME/.bashrc\"; fi && \
             if [ -f \"$HOME/.local/bin/env\" ]; then source \"$HOME/.local/bin/env\"; fi && export PATH=\"$HOME/.local/bin:$PATH\" && \
             uv python install {py} && uv tool install --python {py} {pkg}{deps} && mini-swe-agent --help",
            uv_install = uv_install(None)
        )
    }

    fn new(args: AgentArgs) -> Result<Self> {
        const N: &str = "mini-swe-agent";
        let max_tokens = match args.kwargs.get("max_tokens") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_i64().filter(|_| v.is_i64() || v.is_u64()) {
                Some(n) if n <= 0 => bail!("Invalid value for 'max_tokens': must be greater than 0"),
                Some(n) => Some(n),
                None => bail!("Invalid value for 'max_tokens': expected an integer"),
            },
        };
        let config = args.kwargs.get("config").filter(|v| !v.is_null());
        if let Some(c) = config
            && !c.is_object()
        {
            bail!("Invalid value for 'config': expected an object, got {}", type_name(c));
        }
        let config_file = opt_str(&args, N, "config_file")?;
        if config.is_some() && config_file.is_some() {
            bail!("'config' and 'config_file' are mutually exclusive");
        }
        let config_yaml = match (config, &config_file) {
            (Some(c), _) => Some(serde_yaml_ng::to_string(c)?),
            (None, Some(p)) if !p.is_empty() => Some(std::fs::read_to_string(p).map_err(|e| anyhow!("{p}: {e}"))?),
            _ => None,
        };
        let registry = args.kwargs.get("litellm_model_registry").filter(|v| !v.is_null());
        let registry_file = opt_str(&args, N, "litellm_model_registry_file")?.filter(|s| !s.is_empty());
        if registry.is_some() && registry_file.is_some() {
            bail!("'litellm_model_registry' and 'litellm_model_registry_file' are mutually exclusive");
        }
        let registry_json = match (registry, &registry_file) {
            (None, None) => None,
            (r, f) => {
                let (source, v) = match f {
                    Some(p) => (
                        "litellm_model_registry_file",
                        serde_json::from_str::<Value>(&std::fs::read_to_string(p).map_err(|e| anyhow!("{p}: {e}"))?)?,
                    ),
                    None => ("litellm_model_registry", r.unwrap().clone()),
                };
                if !v.is_object() {
                    bail!("Invalid value for '{source}': expected an object, got {}", type_name(&v));
                }
                Some(serde_json::to_string_pretty(&v)?)
            }
        };
        let cost_limit = if args.kwargs.contains_key("cost_limit") { opt_str(&args, N, "cost_limit")? } else { Some("0".into()) };
        let reasoning_effort = opt_str(&args, N, "reasoning_effort")?;
        let session_id_headers = validate_session_id_headers(args.kwargs.get("session_id_headers"))?;
        Ok(MiniSweAgent {
            version: args.version_kw(),
            session_id: session_id(&args),
            args,
            cost_limit,
            reasoning_effort,
            max_tokens,
            config_yaml,
            registry_json,
            session_id_headers,
        })
    }

    /// mini-swe accepts either OpenAI base-URL alias; export both when set.
    fn model_connection(&self) -> Resolved {
        let mut access = connection(&self.args, SPEC);
        if let Some(url) = access.configured_base_url.clone()
            && ["OPENAI_BASE_URL", "OPENAI_API_BASE"].iter().any(|k| access.env.contains_key(*k))
        {
            access.env.insert("OPENAI_BASE_URL".into(), url.clone());
            access.env.insert("OPENAI_API_BASE".into(), url);
        }
        access
    }

    /// (pre-run commands writing config files, run command, env).
    fn commands(&self, instruction: &str) -> Result<(Vec<String>, String, BTreeMap<String, String>)> {
        let mut instruction = instruction.to_string();
        if !self.args.mcp_servers.is_empty() {
            instruction += "\n\nMCP Servers:\nThe following MCP servers are available for this task.\n";
            for s in &self.args.mcp_servers {
                if s.transport() == "stdio" {
                    instruction += &format!(
                        "- {}: stdio transport, command: {} {}\n",
                        s.name,
                        s.command.as_deref().unwrap_or("None"),
                        s.args.join(" ")
                    );
                } else {
                    instruction += &format!("- {}: {} transport, url: {}\n", s.name, s.transport(), s.url.as_deref().unwrap_or("None"));
                }
            }
        }
        let model = match self.args.model_name.as_deref() {
            Some(m) if m.contains('/') => m,
            _ => bail!("Model name must be in the format provider/model_name"),
        };
        let access = self.model_connection();
        let mut env = access.env.clone();
        env.insert("MSWEA_CONFIGURED".into(), "true".into());
        env.insert("MSWEA_COST_TRACKING".into(), "ignore_errors".into());
        if access.api_key.is_none() {
            bail!("No API key found for model '{model}'. Please set MSWEA_API_KEY or the provider's API key variable.");
        }
        let extra_flags = self.cost_limit.as_ref().map(|v| format!("--cost-limit {} ", quote(v))).unwrap_or_default();
        let mut config_flags = format!("-c model.model_kwargs.timeout={DEFAULT_LITELLM_TIMEOUT_SEC} ");
        let mut pre = vec![];
        let heredoc = |path: &str, marker: &str, body: &str| {
            let m = format!("{marker}_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
            format!("mkdir -p /tmp/mswea-config\ncat > '{path}' << '{m}'\n{body}\n{m}\n")
        };
        if let Some(y) = self.config_yaml.as_deref().filter(|y| !y.is_empty()) {
            pre.push(heredoc("/tmp/mswea-config/custom.yaml", "MSWEA_CONFIG_EOF", y));
            config_flags += "-c /tmp/mswea-config/custom.yaml ";
        }
        if let Some(r) = &self.registry_json {
            pre.push(heredoc("/tmp/mswea-config/registry.json", "MSWEA_REGISTRY_EOF", r));
            config_flags += "-c model.litellm_model_registry=/tmp/mswea-config/registry.json ";
        }
        if let Some(sid) = &self.session_id {
            let default = vec!["X-Session-ID".to_string()];
            for h in self.session_id_headers.as_ref().unwrap_or(&default) {
                config_flags += &format!("-c {} ", quote(&format!("model.model_kwargs.extra_headers.{h}={sid}")));
            }
        }
        let effort = self.reasoning_effort.as_deref().filter(|e| !e.is_empty());
        if let Some(e) = effort {
            let e = quote(e);
            if model.starts_with("openai/") {
                // gpt-5.x rejects tools+reasoning_effort on chat completions: use the Responses API model class.
                config_flags += &format!("-c model.model_class=litellm_response -c model.model_kwargs.reasoning.effort={e} ");
            } else {
                config_flags += &format!("-c model.model_kwargs.reasoning_effort={e} ");
            }
        }
        if let Some(n) = self.max_tokens {
            let key = if model.starts_with("openai/") && effort.is_some() { "max_output_tokens" } else { "max_tokens" };
            config_flags += &format!("-c model.model_kwargs.{key}={n} ");
        }
        // Any -c suppresses the packaged mini.yaml; prepend the builtin so overrides layer on top.
        let config_flags = format!("-c mini {config_flags}");
        let cmd = format!(
            "{ENV_PRELUDE}mini-swe-agent --yolo --model={model} --task={} --output={TRAJECTORY_PATH} {extra_flags}{config_flags}--exit-immediately 2>&1 </dev/null | tee /logs/agent/mini-swe-agent.txt",
            quote(&instruction)
        );
        Ok((pre, cmd, env))
    }
}

pub(crate) fn validate_session_id_headers(v: Option<&Value>) -> Result<Option<Vec<String>>> {
    let re = Regex::new(r"^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$").unwrap();
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => a
            .iter()
            .map(|h| match h.as_str() {
                Some(s) if re.is_match(s) => Ok(s.to_string()),
                _ => bail!("Invalid HTTP header name in session_id_headers: {h}"),
            })
            .collect::<Result<Vec<_>>>()
            .map(Some),
        Some(_) => bail!("session_id_headers must be a list of valid HTTP header names"),
    }
}

/// Per-message usage: chat-completions or Responses-API usage, normalized.
fn message_usage(m: &Value) -> (i64, i64, i64) {
    let mut usage = &m["extra"]["response"]["usage"];
    if !truthy(usage) && m["object"] == "response" {
        usage = &m["usage"];
    }
    let first = |a: &str, b: &str| usage[a].as_i64().filter(|n| *n != 0).or(usage[b].as_i64()).unwrap_or(0);
    let details = if truthy(&usage["prompt_tokens_details"]) { &usage["prompt_tokens_details"] } else { &usage["input_tokens_details"] };
    (first("prompt_tokens", "input_tokens"), first("completion_tokens", "output_tokens"), details["cached_tokens"].as_i64().unwrap_or(0))
}

fn truthy(v: &Value) -> bool {
    v.as_object().is_some_and(|o| !o.is_empty())
}

/// Totals from mini-swe-agent's native trajectory.
pub fn parse_trajectory(text: &str) -> Option<AgentContext> {
    let t: Value = serde_json::from_str(text).ok()?;
    let mut ctx = AgentContext::default();
    let (mut i, mut o, mut c) = (0, 0, 0);
    for m in t["messages"].as_array().into_iter().flatten() {
        let (pi, po, pc) = message_usage(m);
        (i, o, c) = (i + pi, o + po, c + pc);
    }
    ctx.n_input_tokens = Some(i);
    ctx.n_output_tokens = Some(o);
    ctx.n_cache_tokens = Some(c);
    ctx.cost_usd = Some(t["info"]["model_stats"]["instance_cost"].as_f64().unwrap_or(0.0));
    Some(ctx)
}

#[async_trait::async_trait]
impl Agent for MiniSweAgent {
    fn name(&self) -> &str {
        "mini-swe-agent"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "bash", "build_tools", "git"]).await?;
        exec_agent(cx, &self.install_command(), &BTreeMap::new()).await?;
        if self.version.is_none() {
            self.version = Some(pins::MINI_SWE_AGENT.version.into());
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let (pre, cmd, env) = self.commands(&instruction)?;
        for p in pre {
            exec_agent(cx, &p, &env).await?;
        }
        exec_agent(cx, &cmd, &env).await.map(|_| ())
    }

    fn collect_usage(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        if let Some(c) = std::fs::read_to_string(logs_dir.join("mini-swe-agent.trajectory.json")).ok().and_then(|t| parse_trajectory(&t)) {
            *ctx = c;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agents::installed::common;
    use serde_json::json;

    pub(crate) fn args(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> AgentArgs {
        let mut args = common::test_args(kwargs, env, model);
        args.logs_dir = args.logs_dir.join("my-trial").join("agent");
        args
    }

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<MiniSweAgent> {
        MiniSweAgent::new(args(kwargs, env, model))
    }

    const SONNET: Option<&str> = Some("anthropic/claude-sonnet-4-5-20250929");

    #[test]
    fn run_command_and_env() {
        let a = agent(json!({}), &[("MSWEA_API_KEY", "test-key")], SONNET).unwrap();
        let (pre, cmd, env) = a.commands("Fix the bug; rm -rf /").unwrap();
        assert!(pre.is_empty());
        assert!(cmd.contains("mini-swe-agent --yolo --model=anthropic/claude-sonnet-4-5-20250929 --task='Fix the bug; rm -rf /'"));
        assert!(cmd.contains("--cost-limit 0 -c mini -c model.model_kwargs.timeout=3600 "));
        assert!(cmd.contains("-c model.model_kwargs.extra_headers.X-Session-ID=my-trial__agent "));
        assert!(cmd.ends_with("--exit-immediately 2>&1 </dev/null | tee /logs/agent/mini-swe-agent.txt"));
        assert_eq!((env["MSWEA_CONFIGURED"].as_str(), env["MSWEA_API_KEY"].as_str()), ("true", "test-key"));
        assert_eq!(env["MSWEA_COST_TRACKING"], "ignore_errors");
        assert!(agent(json!({}), &[], Some("no-slash")).unwrap().commands("t").unwrap_err().to_string().contains("provider/model_name"));
        assert!(
            !agent(json!({"cost_limit": null}), &[("MSWEA_API_KEY", "k")], SONNET)
                .unwrap()
                .commands("t")
                .unwrap()
                .1
                .contains("--cost-limit")
        );
    }

    #[test]
    fn base_url_and_keyless() {
        let a = agent(json!({}), &[("MSWEA_API_KEY", ""), ("OPENAI_BASE_URL", "http://localhost:8000/v1")], Some("openai/local-model"))
            .unwrap();
        let env = a.commands("t").unwrap().2;
        assert_eq!(env["MSWEA_API_KEY"], "");
        let a = agent(json!({}), &[("OPENAI_API_KEY", "sk-extra"), ("OPENAI_API_BASE", "https://b/v1")], Some("openai/Qwen/Qwen3.5-27B"))
            .unwrap();
        let env = a.commands("t").unwrap().2;
        assert_eq!(
            (env["OPENAI_API_KEY"].as_str(), env["OPENAI_BASE_URL"].as_str(), env["OPENAI_API_BASE"].as_str()),
            ("sk-extra", "https://b/v1", "https://b/v1")
        );
    }

    #[test]
    fn options_to_config_flags() {
        let k = &[("MSWEA_API_KEY", "k")];
        let cmd = agent(json!({"reasoning_effort": "max", "max_tokens": 32000}), k, Some("anthropic/claude-opus-4-8"))
            .unwrap()
            .commands("t")
            .unwrap()
            .1;
        assert!(cmd.contains("-c model.model_kwargs.reasoning_effort=max ") && cmd.contains("-c model.model_kwargs.max_tokens=32000 "));
        let cmd = agent(json!({"reasoning_effort": "low", "max_tokens": 4096}), k, Some("openai/gpt-5")).unwrap().commands("t").unwrap().1;
        assert!(cmd.contains("-c model.model_class=litellm_response -c model.model_kwargs.reasoning.effort=low "));
        assert!(cmd.contains("-c model.model_kwargs.max_output_tokens=4096") && !cmd.contains("max_tokens=4096"));
        let cmd = agent(json!({"session_id_headers": ["Modal-Session-ID"]}), k, SONNET).unwrap().commands("t").unwrap().1;
        assert!(cmd.contains("extra_headers.Modal-Session-ID=my-trial__agent") && !cmd.contains("X-Session-ID"));

        let (pre, cmd, _) = agent(
            json!({"config": {"agent": {"step_limit": 20}}, "litellm_model_registry": {"m": {"supports_reasoning": true}}}),
            k,
            SONNET,
        )
        .unwrap()
        .commands("t")
        .unwrap();
        assert!(
            pre[0].contains("cat > '/tmp/mswea-config/custom.yaml' << 'MSWEA_CONFIG_EOF_") && pre[0].contains("agent:\n  step_limit: 20")
        );
        assert!(pre[1].contains("cat > '/tmp/mswea-config/registry.json'") && pre[1].contains("\"supports_reasoning\": true"));
        assert!(cmd.contains("-c mini -c model.model_kwargs.timeout=3600 -c /tmp/mswea-config/custom.yaml -c model.litellm_model_registry=/tmp/mswea-config/registry.json "));
    }

    #[test]
    fn validation() {
        let err = |kw: Value| agent(kw, &[], None).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err(json!({"max_tokens": 0})).contains("greater than 0"));
        assert!(err(json!({"max_tokens": "5"})).contains("expected an integer"));
        assert!(err(json!({"config": "agent: {}"})).contains("expected an object, got string"));
        assert!(err(json!({"config": {}, "config_file": "/x.yaml"})).contains("mutually exclusive"));
        assert!(err(json!({"litellm_model_registry": "{}"})).contains("expected an object, got string"));
        assert!(err(json!({"litellm_model_registry": {}, "litellm_model_registry_file": "/r.json"})).contains("mutually exclusive"));
        assert!(err(json!({"session_id_headers": ["bad header"]})).contains("Invalid HTTP header name"));
        let f = std::env::temp_dir().join(format!("reg-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&f, "[1, 2, 3]").unwrap();
        assert!(
            err(json!({"litellm_model_registry_file": f.to_str().unwrap()}))
                .contains("'litellm_model_registry_file': expected an object, got array")
        );
    }

    #[test]
    fn install_pins_every_package() {
        let cmd = agent(json!({}), &[], None).unwrap().install_command();
        assert!(cmd.contains(
            "uv tool install --python 3.12 mini-swe-agent==2.4.6 --with litellm==1.105.1 --with orjson==3.13.0 --with fastapi==0.143.2 &&"
        ));
        let cmd = agent(json!({"version": "2.1.0"}), &[], None).unwrap().install_command();
        assert!(cmd.contains("mini-swe-agent==2.1.0 --with litellm==1.105.1"));
    }

    #[test]
    fn usage_from_trajectory() {
        let usage = |p: i64, c: i64, cached: i64| json!({"response": {"usage": {"prompt_tokens": p, "completion_tokens": c, "prompt_tokens_details": {"cached_tokens": cached}, "completion_tokens_details": {"reasoning_tokens": 5}}}});
        let t = json!({
            "trajectory_format": "mini-swe-agent-1.1",
            "info": {"mini_version": "2.1.0", "model_stats": {"instance_cost": 0.25}},
            "messages": [
                {"role": "system", "content": "s", "extra": {}},
                {"role": "user", "content": "u", "extra": {}},
                {"role": "assistant", "content": "a", "extra": usage(600, 120, 100)},
                {"role": "tool", "content": "", "tool_call_id": "c", "extra": {}},
                {"role": "assistant", "content": "a", "extra": usage(900, 80, 300)},
                {"role": "assistant", "content": "a", "extra": usage(1100, 50, 500)},
                {"object": "response", "output": [], "usage": {"input_tokens": 1200, "output_tokens": 150, "input_tokens_details": {"cached_tokens": 100}}},
                {"type": "function_call_output", "call_id": "x", "output": "o"}
            ]
        });
        let c = parse_trajectory(&t.to_string()).unwrap();
        assert_eq!((c.n_input_tokens, c.n_output_tokens, c.n_cache_tokens, c.cost_usd), (Some(3800), Some(400), Some(1000), Some(0.25)));
        assert!(parse_trajectory("not json").is_none());
    }
}
