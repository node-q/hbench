//! mini-swe-agent installed agent.

use super::common::{installed_dir, uv_install};
use super::pins;
use super::{connection, ensure_system_dependencies, exec_agent};
use crate::agents::model_connection::{Resolved, Spec};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::atif;
use crate::env::quote;
use crate::fmt::type_name;
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde_json::{Value, json};
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
                        s.command.as_deref().unwrap_or("(not set)"),
                        s.args.join(" ")
                    );
                } else {
                    instruction +=
                        &format!("- {}: {} transport, url: {}\n", s.name, s.transport(), s.url.as_deref().unwrap_or("(not set)"));
                }
            }
        }
        self.args.require_provider_model()?;
        let model = self.args.model_name.as_deref().unwrap_or_default();
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

/// A message's chat-completions or Responses-API usage object.
fn usage_of(m: &Value) -> &Value {
    let usage = &m["extra"]["response"]["usage"];
    if !is_nonempty_object(usage) && m["object"] == "response" { &m["usage"] } else { usage }
}

/// Per-message usage, normalized.
fn message_usage(m: &Value) -> (i64, i64, i64) {
    let usage = usage_of(m);
    let first = |a: &str, b: &str| usage[a].as_i64().filter(|n| *n != 0).or(usage[b].as_i64()).unwrap_or(0);
    let details =
        if is_nonempty_object(&usage["prompt_tokens_details"]) { &usage["prompt_tokens_details"] } else { &usage["input_tokens_details"] };
    (first("prompt_tokens", "input_tokens"), first("completion_tokens", "output_tokens"), details["cached_tokens"].as_i64().unwrap_or(0))
}

fn is_nonempty_object(v: &Value) -> bool {
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

/// A message's text: a plain string, or the text of its parts.
fn text_of(content: &Value) -> String {
    match content.as_array() {
        Some(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        None => content.as_str().unwrap_or_default().into(),
    }
}

fn timestamp(m: &Value) -> Option<String> {
    let ms = m["extra"]["timestamp"].as_f64()? * 1000.0;
    chrono::DateTime::from_timestamp_millis(ms as i64).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn tool_call(id: &Value, name: &Value, arguments: &Value) -> atif::ToolCall {
    let raw = arguments.as_str().unwrap_or_default();
    atif::ToolCall {
        tool_call_id: id.as_str().unwrap_or_default().into(),
        function_name: name.as_str().unwrap_or_default().into(),
        arguments: serde_json::from_str(raw).unwrap_or_else(|_| json!(raw)),
        extra: None,
    }
}

/// One model call, chat-completions or Responses-API, as an agent step.
fn agent_step(m: &Value) -> atif::Step {
    let (mut text, mut reasoning, mut calls) = (vec![], vec![], vec![]);
    if m["object"] == "response" {
        for item in m["output"].as_array().into_iter().flatten() {
            match item["type"].as_str() {
                Some("message") => text.push(text_of(&item["content"])),
                Some("reasoning") => reasoning.push(text_of(&item["summary"])),
                Some("function_call") => calls.push(tool_call(&item["call_id"], &item["name"], &item["arguments"])),
                _ => {}
            }
        }
    } else {
        text.push(text_of(&m["content"]));
        reasoning.push(text_of(&m["reasoning_content"]));
        for c in m["tool_calls"].as_array().into_iter().flatten() {
            calls.push(tool_call(&c["id"], &c["function"]["name"], &c["function"]["arguments"]));
        }
    }
    let reasoning = reasoning.join("\n");
    let (prompt, completion, cached) = message_usage(m);
    atif::Step {
        source: "agent".into(),
        timestamp: timestamp(m),
        model_name: m["extra"]["response"]["model"].as_str().or(m["model"].as_str()).map(String::from),
        message: json!(text.join("\n")),
        reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
        tool_calls: (!calls.is_empty()).then_some(calls),
        metrics: is_nonempty_object(usage_of(m)).then(|| atif::Metrics {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            cached_tokens: Some(cached),
            cost_usd: m["extra"]["cost"].as_f64(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// mini-swe-agent's native trajectory -> ATIF.
pub fn atif_trajectory(text: &str, session_id: Option<String>, version: Option<&str>, model: Option<&str>) -> Option<atif::Trajectory> {
    let totals = parse_trajectory(text)?;
    let t: Value = serde_json::from_str(text).ok()?;
    let mut steps: Vec<atif::Step> = vec![];
    for m in t["messages"].as_array()? {
        if m["object"] == "response" {
            steps.push(agent_step(m));
        } else if m["type"] == "function_call_output" {
            atif::attach_result(&mut steps, m["call_id"].as_str().unwrap_or_default(), json!(text_of(&m["output"])));
        } else {
            match m["role"].as_str() {
                Some(role @ ("system" | "user")) => steps.push(atif::Step {
                    source: role.into(),
                    timestamp: timestamp(m),
                    message: json!(text_of(&m["content"])),
                    ..Default::default()
                }),
                Some("assistant") => steps.push(agent_step(m)),
                Some("tool") => {
                    atif::attach_result(&mut steps, m["tool_call_id"].as_str().unwrap_or_default(), json!(text_of(&m["content"])));
                }
                _ => {}
            }
        }
    }
    let agent = atif::Agent {
        name: "mini-swe-agent".into(),
        version: t["info"]["mini_version"].as_str().or(version).unwrap_or("unknown").into(),
        model_name: model.map(String::from),
        ..Default::default()
    };
    let mut trajectory = atif::assemble(agent, session_id, steps)?;
    trajectory.final_metrics.get_or_insert_default().total_cost_usd = totals.cost_usd;
    Some(trajectory)
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
        let Ok(text) = std::fs::read_to_string(logs_dir.join("mini-swe-agent.trajectory.json")) else { return };
        if let Some(c) = parse_trajectory(&text) {
            *ctx = c;
        }
        if let Some(t) = atif_trajectory(&text, session_id(&self.args), self.version.as_deref(), self.args.model_name.as_deref()) {
            atif::write_trajectory(logs_dir, &t);
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

    #[test]
    fn chat_trajectory_to_atif() {
        let t = json!({
            "info": {"mini_version": "2.4.6", "model_stats": {"instance_cost": 0.5}},
            "messages": [
                {"role": "system", "content": "s"},
                {"role": "user", "content": "u"},
                {"role": "assistant", "content": "plan", "reasoning_content": "think",
                 "tool_calls": [
                     {"id": "c1", "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}},
                     {"id": "c2", "function": {"name": "bash", "arguments": "not json"}}],
                 "extra": {"timestamp": 1793434965.5, "cost": 0.5, "response": {"model": "m1", "usage": {"prompt_tokens": 10, "completion_tokens": 2, "prompt_tokens_details": {"cached_tokens": 4}}}}},
                {"role": "tool", "tool_call_id": "c1", "content": "out1", "extra": {"returncode": 0}},
                {"role": "tool", "tool_call_id": "c2", "content": "out2", "extra": {"returncode": 1}},
                {"role": "assistant", "content": "done", "extra": {}},
                {"role": "exit", "content": "", "extra": {"exit_status": "Submitted"}}
            ]
        });
        let a = atif_trajectory(&t.to_string(), Some("trial__agent".into()), Some("9.9"), Some("openai/m1")).unwrap();
        assert_eq!(
            (a.session_id.as_deref(), a.agent.version.as_str(), a.agent.model_name.as_deref()),
            (Some("trial__agent"), "2.4.6", Some("openai/m1"))
        );
        assert_eq!(
            a.steps.iter().map(|s| (s.step_id, s.source.as_str())).collect::<Vec<_>>(),
            [(1, "system"), (2, "user"), (3, "agent"), (4, "agent")]
        );
        let call = &a.steps[2];
        assert_eq!(
            (call.message.as_str(), call.reasoning_content.as_deref(), call.model_name.as_deref()),
            (Some("plan"), Some("think"), Some("m1"))
        );
        assert_eq!(call.timestamp.as_deref(), Some("2026-10-31T08:22:45.500Z"));
        let calls = call.tool_calls.as_ref().unwrap();
        assert_eq!((calls[0].arguments.clone(), calls[1].arguments.clone()), (json!({"command": "ls"}), json!("not json")));
        let results = &call.observation.as_ref().unwrap().results;
        assert_eq!(
            results.iter().map(|r| (r.source_call_id.clone().unwrap(), r.content.clone().unwrap())).collect::<Vec<_>>(),
            [("c1".to_string(), json!("out1")), ("c2".to_string(), json!("out2"))]
        );
        let m = call.metrics.as_ref().unwrap();
        assert_eq!((m.prompt_tokens, m.completion_tokens, m.cached_tokens, m.cost_usd), (Some(10), Some(2), Some(4), Some(0.5)));
        assert!(a.steps[3].metrics.is_none());
        let f = a.final_metrics.unwrap();
        assert_eq!(
            (f.total_prompt_tokens, f.total_completion_tokens, f.total_cached_tokens, f.total_cost_usd, f.total_steps),
            (Some(10), Some(2), Some(4), Some(0.5), Some(4))
        );
    }

    #[test]
    fn responses_trajectory_to_atif() {
        let t = json!({
            "info": {"model_stats": {"instance_cost": 0.0}},
            "messages": [
                {"role": "user", "content": [{"type": "input_text", "text": "u"}]},
                {"object": "response", "model": "m2", "usage": {"input_tokens": 7, "output_tokens": 3, "input_tokens_details": {"cached_tokens": 1}},
                 "output": [
                     {"type": "reasoning", "summary": [{"type": "summary_text", "text": "think"}]},
                     {"type": "message", "content": [{"type": "output_text", "text": "plan"}]},
                     {"type": "function_call", "call_id": "c1", "name": "bash", "arguments": "{}"}]},
                {"type": "function_call_output", "call_id": "c1", "output": "out"}
            ]
        });
        let a = atif_trajectory(&t.to_string(), None, Some("2.4.6"), None).unwrap();
        assert_eq!((a.agent.version.as_str(), a.steps[0].message.as_str()), ("2.4.6", Some("u")));
        let call = &a.steps[1];
        assert_eq!(
            (call.message.as_str(), call.reasoning_content.as_deref(), call.model_name.as_deref()),
            (Some("plan"), Some("think"), Some("m2"))
        );
        assert_eq!(call.tool_calls.as_ref().unwrap()[0].tool_call_id, "c1");
        assert_eq!(call.observation.as_ref().unwrap().results[0].content, Some(json!("out")));
        assert_eq!(call.metrics.as_ref().unwrap().prompt_tokens, Some(7));
    }

    #[test]
    fn no_atif_without_messages() {
        assert!(atif_trajectory("not json", None, None, None).is_none());
        assert!(atif_trajectory(r#"{"messages": []}"#, None, None, None).is_none());
    }

    #[test]
    fn collect_usage_writes_atif() {
        let dir = std::env::temp_dir().join(format!("hbench-mini-swe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let native = json!({"info": {"mini_version": "2.4.6"}, "messages": [{"role": "user", "content": "u"}]});
        std::fs::write(dir.join("mini-swe-agent.trajectory.json"), native.to_string()).unwrap();
        let mut ctx = AgentContext::default();
        agent(json!({}), &[], SONNET).unwrap().collect_usage(&dir, &mut ctx);
        assert_eq!(ctx.n_input_tokens, Some(0));
        let t: atif::Trajectory = serde_json::from_str(&std::fs::read_to_string(dir.join("trajectory.json")).unwrap()).unwrap();
        assert_eq!((t.steps.len(), t.agent.model_name.as_deref()), (1, SONNET));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
