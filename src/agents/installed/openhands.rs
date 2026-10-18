//! OpenHands installed agent.

use super::common::installed_dir;
use super::common::opts::lax_bool;
use super::{connection, ensure_system_dependencies, exec_agent, exec_root};
use crate::agents::model_connection::Spec;
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

const SPEC: Spec = Spec {
    default_provider: None,
    api_key_envs: &["LLM_API_KEY"],
    base_url_envs: &["LLM_BASE_URL", "HOSTED_VLLM_API_BASE", "VLLM_API_BASE", "OPENAI_API_BASE", "AZURE_API_BASE"],
    passthrough: false,
};
const VERSION_CMD: &str = "/opt/openhands-venv/bin/python -m openhands.core.main --version";
const CONFIG_PATH: &str = "$HOME/.openhands/config.toml";

// ---- Pydantic-style option coercion shared with openhands_sdk ----

fn invalid(agent: &str, k: &str, msg: &str) -> anyhow::Error {
    anyhow::anyhow!("Invalid kwargs for agent '{agent}':\n  {k}: {msg}\nSee `hbench agents schema {agent}` for available options.")
}

fn get<'a>(args: &'a AgentArgs, k: &str) -> Option<&'a Value> {
    args.kwargs.get(k).filter(|v| !v.is_null())
}

/// `str | None` with `coerce_numbers_to_str`.
pub(super) fn opt_str(args: &AgentArgs, agent: &str, k: &str) -> Result<Option<String>> {
    match get(args, k) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(_) => Err(invalid(agent, k, "Input should be a valid string")),
    }
}

pub(super) fn opt_bool(args: &AgentArgs, agent: &str, k: &str) -> Result<Option<bool>> {
    get(args, k).map(|v| lax_bool(v).ok_or_else(|| invalid(agent, k, "Input should be a valid boolean"))).transpose()
}

pub(super) fn opt_int(args: &AgentArgs, agent: &str, k: &str) -> Result<Option<i64>> {
    let err = || invalid(agent, k, "Input should be a valid integer");
    match get(args, k) {
        None => Ok(None),
        Some(Value::Number(n)) => {
            n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)).map(Some).ok_or_else(err)
        }
        Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| err()),
        Some(_) => Err(err()),
    }
}

pub(super) fn opt_f64(args: &AgentArgs, agent: &str, k: &str) -> Result<Option<f64>> {
    let err = || invalid(agent, k, "Input should be a valid number");
    match get(args, k) {
        None => Ok(None),
        Some(Value::Number(n)) => n.as_f64().map(Some).ok_or_else(err),
        Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| err()),
        Some(_) => Err(err()),
    }
}

fn opt_dict(args: &AgentArgs, agent: &str, k: &str) -> Result<Option<Map<String, Value>>> {
    match get(args, k) {
        None => Ok(None),
        Some(Value::Object(o)) => Ok(Some(o.clone())),
        Some(_) => Err(invalid(agent, k, "Input should be a valid dictionary")),
    }
}

/// Python `str(float)` for ordinary values (`1.0`, `0.7`).
pub(super) fn py_float(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e16 { format!("{f:.1}") } else { f.to_string() }
}

/// Agent env > host env, empty values kept.
pub(super) fn raw_env(args: &AgentArgs, k: &str) -> Option<String> {
    args.extra_env.get(k).cloned().or_else(|| std::env::var(k).ok())
}

fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

pub struct OpenHands {
    args: AgentArgs,
    version: Option<String>,
    raw_content: bool,
    /// Compiled `Env(...)` option annotations.
    option_env: BTreeMap<String, String>,
    api_base: Option<String>,
    model_info: Option<Map<String, Value>>,
    python_version: String,
    git_version: Option<String>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(OpenHands::new(args)?))
}

impl OpenHands {
    pub fn new(args: AgentArgs) -> Result<OpenHands> {
        const N: &str = "openhands";
        let disable_tool_calls = opt_bool(&args, N, "disable_tool_calls")?.unwrap_or(false);
        let mut option_env = BTreeMap::new();
        option_env.insert("LLM_NATIVE_TOOL_CALLING".to_string(), (if disable_tool_calls { "false" } else { "true" }).to_string());
        for (k, env) in [
            ("reasoning_effort", "LLM_REASONING_EFFORT"),
            ("temperature", "LLM_TEMPERATURE"),
            ("top_p", "LLM_TOP_P"),
            ("max_budget_per_task", "MAX_BUDGET_PER_TASK"),
        ] {
            if let Some(v) = opt_str(&args, N, k)? {
                option_env.insert(env.to_string(), v);
            }
        }
        for (k, env) in [("max_iterations", "MAX_ITERATIONS"), ("num_retries", "LLM_NUM_RETRIES")] {
            if let Some(v) = opt_int(&args, N, k)? {
                option_env.insert(env.to_string(), v.to_string());
            }
        }
        for (k, env) in
            [("caching_prompt", "LLM_CACHING_PROMPT"), ("drop_params", "LLM_DROP_PARAMS"), ("disable_vision", "LLM_DISABLE_VISION")]
        {
            if let Some(v) = opt_bool(&args, N, k)? {
                option_env.insert(env.to_string(), v.to_string());
            }
        }
        let trajectory_config = opt_dict(&args, N, "trajectory_config")?;
        let raw_content = match trajectory_config.as_ref().and_then(|t| t.get("raw_content")).filter(|v| !v.is_null()) {
            None => false,
            Some(v) => lax_bool(v).ok_or_else(|| invalid(N, "trajectory_config.raw_content", "Input should be a valid boolean"))?,
        };
        if raw_content && !disable_tool_calls {
            bail!(
                "When trajectory_config['raw_content'] is True, disable_tool_calls must also be True. \
                 Raw content mode is designed to preserve text-based tool invocation format (e.g., <execute_bash>) \
                 rather than native function calling format. Native function calling does not appear in LLM response's \
                 message content field, rather, it is structured in the tool_calls field, thus there is no easy and \
                 standard way to persist it in the trajectory in its raw/original format. Note that even OpenHands may not \
                 use native function calling for some models, disable_tool_calls must still be set explicitly in raw_content mode."
            );
        }
        Ok(OpenHands {
            version: opt_str(&args, N, "version")?,
            raw_content,
            option_env,
            api_base: opt_str(&args, N, "api_base")?,
            model_info: opt_dict(&args, N, "model_info")?,
            python_version: opt_str(&args, N, "python_version")?.unwrap_or_else(|| "3.13".into()),
            git_version: opt_str(&args, N, "git_version")?,
            args,
        })
    }

    fn install_cmd(&self) -> String {
        let install = if let Some(g) = &self.git_version {
            format!("uv pip install git+https://github.com/All-Hands-AI/OpenHands.git@{g}")
        } else if let Some(v) = &self.version {
            format!("uv pip install openhands-ai=={v}")
        } else {
            "uv pip install openhands-ai".to_string()
        };
        let py = &self.python_version;
        format!(
            "set -euo pipefail; curl -LsSf https://astral.sh/uv/install.sh | sh && \
             if [ -f \"$HOME/.local/bin/env\" ]; then source \"$HOME/.local/bin/env\"; fi && \
             uv python install {py} && uv venv /opt/openhands-venv --python {py} && \
             source /opt/openhands-venv/bin/activate && export SKIP_VSCODE_BUILD=true && {install} && {VERSION_CMD}"
        )
    }

    /// Run env construction (model connection + options + fixed sandbox settings).
    fn run_env(&self) -> Result<BTreeMap<String, String>> {
        let conn = connection(&self.args, SPEC);
        let mut env = conn.env.clone();
        if let Some(k) = &conn.api_key {
            env.insert("LLM_API_KEY".into(), k.clone());
        }
        // `api_base or configured_base_url`: Python `or` skips an empty api_base.
        let base_url = raw_env(&self.args, "LLM_BASE_URL")
            .or_else(|| self.api_base.clone().filter(|s| !s.is_empty()).or(conn.configured_base_url.clone()));
        if let Some(u) = base_url {
            env.insert("LLM_BASE_URL".into(), u);
        }
        let hosted_vllm = self.args.model_name.as_deref().is_some_and(|m| m.to_lowercase().starts_with("hosted_vllm/"));
        if !env.contains_key("LLM_API_KEY") {
            if !hosted_vllm {
                bail!(
                    "No API key found for model {}. Please set LLM_API_KEY or the provider's API key variable.",
                    self.args.model_name.as_deref().map(|m| format!("'{m}'")).unwrap_or_else(|| "None".into())
                );
            }
            env.insert("LLM_API_KEY".into(), "dummy-key-for-local-vllm".into());
        }
        let model = self
            .args
            .model_name
            .clone()
            .filter(|m| !m.is_empty())
            .or_else(|| raw_env(&self.args, "LLM_MODEL").filter(|m| !m.is_empty()))
            .or_else(|| raw_env(&self.args, "ANTHROPIC_MODEL").filter(|m| !m.is_empty()));
        let Some(model) = model else { bail!("No LLM model found. Please set LLM_MODEL environment variable or specify it in the CLI") };
        env.insert("LLM_MODEL".into(), model);
        let api_version = raw_env(&self.args, "LLM_API_VERSION")
            .or_else(|| (conn.provider.as_deref() == Some("azure")).then(|| raw_env(&self.args, "AZURE_API_VERSION")).flatten());
        if let Some(v) = api_version {
            env.insert("LLM_API_VERSION".into(), v);
        }
        if let Some(mi) = &self.model_info {
            for (k, e) in [("max_input_tokens", "LLM_MAX_INPUT_TOKENS"), ("max_output_tokens", "LLM_MAX_OUTPUT_TOKENS")] {
                if truthy(mi.get(k)) {
                    env.insert(e.into(), py_str(&mi[k]));
                }
            }
        }
        env.extend(self.option_env.clone());
        for (k, v) in [
            ("AGENT_ENABLE_PROMPT_EXTENSIONS", "false"),
            ("AGENT_ENABLE_BROWSING", "false"),
            ("ENABLE_BROWSER", "false"),
            ("SANDBOX_ENABLE_AUTO_LINT", "true"),
            ("SKIP_DEPENDENCY_CHECK", "1"),
            ("RUN_AS_OPENHANDS", "false"),
            ("RUNTIME", "local"),
            ("SU_TO_USER", "false"),
            ("SAVE_TRAJECTORY_PATH", "/logs/agent/openhands.trajectory.json"),
            ("FILE_STORE", "local"),
            ("FILE_STORE_PATH", "/logs/agent/"),
            ("LLM_LOG_COMPLETIONS", "true"),
            ("LLM_LOG_COMPLETIONS_FOLDER", "/logs/agent/completions/"),
        ] {
            env.insert(k.into(), v.into());
        }
        // `_get_env_prefixed("OPENHANDS_")`: host env, then agent env (wins).
        let host = std::env::vars().filter(|(k, _)| k.starts_with("OPENHANDS_"));
        let extra = self.args.extra_env.iter().filter(|(k, _)| k.starts_with("OPENHANDS_")).map(|(k, v)| (k.clone(), v.clone()));
        for (k, v) in host.chain(extra) {
            env.insert(k["OPENHANDS_".len()..].to_string(), v);
        }
        Ok(env)
    }

    fn mcp_config_toml(&self) -> Option<String> {
        if self.args.mcp_servers.is_empty() {
            return None;
        }
        let (mut sse, mut shttp, mut stdio) = (vec![], vec![], vec![]);
        for s in &self.args.mcp_servers {
            let url = s.url.as_deref().unwrap_or("None");
            match s.transport() {
                "sse" => sse.push(format!("{{url = \"{url}\"}}")),
                "streamable-http" => shttp.push(format!("{{url = \"{url}\"}}")),
                "stdio" => {
                    let a = s.args.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(", ");
                    stdio.push(format!(
                        "{{name = \"{}\", command = \"{}\", args = [{a}]}}",
                        s.name,
                        s.command.as_deref().unwrap_or("None")
                    ));
                }
                _ => {}
            }
        }
        let mut lines = vec!["[mcp]".to_string()];
        for (k, e) in [("sse_servers", sse), ("shttp_servers", shttp), ("stdio_servers", stdio)] {
            if !e.is_empty() {
                lines.push(format!("{k} = [{}]", e.join(", ")));
            }
        }
        Some(lines.join("\n") + "\n")
    }

    fn run_cmd(&self, instruction: &str, with_config: bool) -> String {
        let mut c = vec![
            "SANDBOX_VOLUMES=${PWD}:/workspace:rw".to_string(),
            "USER=`id -un`".into(),
            "/opt/openhands-venv/bin/python -m openhands.core.main".into(),
            format!("--task={}", quote(instruction)),
        ];
        if with_config {
            c.push(format!("--config-file={CONFIG_PATH}"));
        }
        c.join(" ") + " 2>&1 </dev/null | stdbuf -oL tee /logs/agent/openhands.txt"
    }
}

/// (prompt, completion, cached, cost) totals the way ATIF final_metrics computes them.
type Totals = (i64, i64, i64, f64);

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn int(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}

/// Events mode: the last event carrying `llm_metrics` holds accumulated usage.
fn events_totals(events_dir: &Path) -> Option<Totals> {
    let mut files: Vec<(i64, std::path::PathBuf)> = std::fs::read_dir(events_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| Some((p.file_stem()?.to_str()?.parse().ok()?, p))) // ponytail: non-int stems are skipped
        .collect();
    files.sort();
    let events: Vec<Value> = files.iter().filter_map(|(_, p)| read_json(p)).collect();
    let m = events.iter().rev().find_map(|e| e.get("llm_metrics"))?;
    let u = m.get("accumulated_token_usage").cloned().unwrap_or(Value::Null);
    Some((
        int(&u, "prompt_tokens"),
        int(&u, "completion_tokens"),
        int(&u, "cache_read_tokens"),
        m.get("accumulated_cost").and_then(Value::as_f64).unwrap_or(0.0),
    ))
}

/// Raw-content mode: sum per-completion usage from `completions/*.json`.
fn completions_totals(dir: &Path) -> Option<Totals> {
    let ts = |p: &Path| {
        p.file_stem().and_then(|s| s.to_str()).and_then(|s| s.rsplit('-').next()).and_then(|t| t.parse::<f64>().ok()).unwrap_or(0.0)
    };
    let mut files: Vec<_> =
        std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    files.sort();
    files.sort_by(|a, b| ts(a).total_cmp(&ts(b)));
    read_json(files.first()?)?; // give up if the first completion is unreadable
    let mut t: Totals = (0, 0, 0, 0.0);
    for c in files.iter().filter_map(|p| read_json(p)) {
        let r = &c["response"];
        if r["choices"].as_array().is_none_or(|a| a.is_empty()) {
            continue;
        }
        let u = &r["usage"];
        let cached = Some(int(u, "cache_read_tokens")).filter(|&n| n != 0).unwrap_or_else(|| int(u, "cache_read_input_tokens"));
        t.0 += int(u, "prompt_tokens");
        t.1 += int(u, "completion_tokens");
        t.2 += cached;
        t.3 += c["cost"].as_f64().unwrap_or(0.0);
    }
    Some(t)
}

pub fn usage_from_logs(logs_dir: &Path, raw_content: bool) -> Option<Totals> {
    let sessions: Vec<_> = std::fs::read_dir(logs_dir.join("sessions")).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    if sessions.len() != 1 {
        return None; // expect exactly one session
    }
    let events_dir = sessions[0].join("events");
    if !events_dir.is_dir() {
        return None;
    }
    if raw_content { completions_totals(&logs_dir.join("completions")) } else { events_totals(&events_dir) }
}

#[async_trait::async_trait]
impl Agent for OpenHands {
    fn name(&self) -> &str {
        "openhands"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "git", "build_tools", "tmux"]).await?;
        let u = cx.user.clone().unwrap_or_else(|| "root".into());
        exec_root(cx, &format!("mkdir -p /opt/openhands-venv && chown {u}:{u} /opt/openhands-venv"), &BTreeMap::new()).await?;
        exec_agent(cx, &self.install_cmd(), &BTreeMap::new()).await?;
        if self.version.is_none() {
            let r = cx.exec(VERSION_CMD, &BTreeMap::new(), None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(r.stdout.trim().to_string());
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let env = self.run_env()?;
        let mcp = self.mcp_config_toml();
        if let Some(c) = &mcp {
            exec_agent(cx, &format!("mkdir -p $HOME/.openhands && echo {} > {CONFIG_PATH}", quote(c)), &env).await?;
        }
        exec_agent(cx, &self.run_cmd(&instruction, mcp.is_some()), &env).await?;
        Ok(())
    }

    fn populate_context_post_run(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        if let Some((p, c, cached, cost)) = usage_from_logs(logs_dir, self.raw_content) {
            ctx.n_input_tokens = Some(p.max(0));
            ctx.n_output_tokens = Some(c.max(0));
            ctx.n_cache_tokens = Some(cached.max(0));
            ctx.cost_usd = (cost > 0.0).then_some(cost);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::agents::installed::common;
    use crate::task::McpServer;
    use serde_json::json;

    pub fn args(kwargs: Value, env: &[(&str, &str)], model: Option<&str>, mcp: Vec<McpServer>) -> AgentArgs {
        AgentArgs { mcp_servers: mcp, ..common::test_args(kwargs, env, model) }
    }

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<OpenHands> {
        OpenHands::new(args(kwargs, env, model, vec![]))
    }

    fn mcp(name: &str, transport: &str, url: Option<&str>, command: Option<&str>, a: &[&str]) -> McpServer {
        McpServer {
            name: name.into(),
            transport: transport.into(),
            url: url.map(String::from),
            command: command.map(String::from),
            args: a.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn options_env_and_validation() {
        let a = agent(json!({"disable_tool_calls": true, "reasoning_effort": "medium", "max_iterations": "30", "caching_prompt": false, "temperature": 0.5}), &[], None).unwrap();
        assert_eq!(a.option_env["LLM_NATIVE_TOOL_CALLING"], "false");
        assert_eq!(a.option_env["LLM_REASONING_EFFORT"], "medium");
        assert_eq!(a.option_env["MAX_ITERATIONS"], "30");
        assert_eq!(a.option_env["LLM_CACHING_PROMPT"], "false");
        assert_eq!(a.option_env["LLM_TEMPERATURE"], "0.5");
        let d = agent(json!({}), &[], None).unwrap();
        assert_eq!(d.option_env.len(), 1);
        assert_eq!(d.option_env["LLM_NATIVE_TOOL_CALLING"], "true");
        assert!(agent(json!({"max_iterations": "x"}), &[], None).is_err());
        assert!(agent(json!({"trajectory_config": {"raw_content": true}}), &[], None).is_err());
        assert!(agent(json!({"trajectory_config": {"raw_content": true}, "disable_tool_calls": true}), &[], None).unwrap().raw_content);
        assert!(
            d.install_cmd().ends_with("uv pip install openhands-ai && /opt/openhands-venv/bin/python -m openhands.core.main --version")
        );
        assert!(agent(json!({"version": "0.59.0"}), &[], None).unwrap().install_cmd().contains("openhands-ai==0.59.0"));
        assert!(
            agent(json!({"git_version": "main", "python_version": "3.12"}), &[], None)
                .unwrap()
                .install_cmd()
                .contains("--python 3.12 && source")
        );
    }

    #[test]
    fn run_env_auth() {
        let a = agent(
            json!({"model_info": {"max_input_tokens": 1000, "max_output_tokens": 0}}),
            &[("LLM_API_KEY", "k"), ("OPENHANDS_FOO", "bar")],
            Some("openai/gpt-5"),
        )
        .unwrap();
        let e = a.run_env().unwrap();
        assert_eq!((e["LLM_API_KEY"].as_str(), e["LLM_MODEL"].as_str(), e["FOO"].as_str()), ("k", "openai/gpt-5", "bar"));
        assert_eq!(e["LLM_MAX_INPUT_TOKENS"], "1000");
        assert!(!e.contains_key("LLM_MAX_OUTPUT_TOKENS"));
        assert_eq!(e["RUNTIME"], "local");

        let az = agent(
            json!({}),
            &[
                ("LLM_API_KEY", ""),
                ("AZURE_API_KEY", "azure-key"),
                ("AZURE_API_BASE", "https://azure.example.test"),
                ("AZURE_API_VERSION", "2026-01-01"),
            ],
            Some("azure/gpt-5"),
        );
        // explicit empty LLM_API_KEY wins over the provider key (keyless endpoint)
        assert_eq!(az.unwrap().run_env().unwrap()["LLM_API_KEY"], "");
        let az = agent(
            json!({}),
            &[("AZURE_API_KEY", "azure-key"), ("AZURE_API_BASE", "https://azure.example.test"), ("AZURE_API_VERSION", "2026-01-01")],
            Some("azure/gpt-5"),
        )
        .unwrap();
        if std::env::var("LLM_API_KEY").is_err() && std::env::var("LLM_BASE_URL").is_err() && std::env::var("LLM_API_VERSION").is_err() {
            let e = az.run_env().unwrap();
            assert_eq!(
                (e["LLM_API_KEY"].as_str(), e["LLM_BASE_URL"].as_str(), e["LLM_API_VERSION"].as_str()),
                ("azure-key", "https://azure.example.test", "2026-01-01")
            );
            let v = agent(json!({}), &[], Some("hosted_vllm/m")).unwrap().run_env().unwrap();
            assert_eq!(v["LLM_API_KEY"], "dummy-key-for-local-vllm");
            assert!(agent(json!({}), &[], Some("nokey-provider/m")).unwrap().run_env().is_err());
        }
        let b = agent(json!({"api_base": "http://h/v1"}), &[("LLM_API_KEY", "k"), ("LLM_MODEL", "x")], None).unwrap();
        if std::env::var("LLM_BASE_URL").is_err() {
            assert_eq!(b.run_env().unwrap()["LLM_BASE_URL"], "http://h/v1");
        }
    }

    #[test]
    fn mcp_toml_and_command() {
        let a = OpenHands::new(args(
            json!({}),
            &[],
            None,
            vec![
                mcp("a", "sse", Some("http://a:8000/sse"), None, &[]),
                mcp("b", "streamable-http", Some("http://b:8000/mcp"), None, &[]),
                mcp("stdio-server", "stdio", None, Some("npx"), &["-y", "my-mcp"]),
            ],
        ))
        .unwrap();
        assert_eq!(
            a.mcp_config_toml().unwrap(),
            "[mcp]\nsse_servers = [{url = \"http://a:8000/sse\"}]\nshttp_servers = [{url = \"http://b:8000/mcp\"}]\n\
             stdio_servers = [{name = \"stdio-server\", command = \"npx\", args = [\"-y\", \"my-mcp\"]}]\n"
        );
        assert!(agent(json!({}), &[], None).unwrap().mcp_config_toml().is_none());
        assert_eq!(
            a.run_cmd("it's", true),
            "SANDBOX_VOLUMES=${PWD}:/workspace:rw USER=`id -un` /opt/openhands-venv/bin/python -m openhands.core.main --task='it'\\''s' \
             --config-file=$HOME/.openhands/config.toml 2>&1 </dev/null | stdbuf -oL tee /logs/agent/openhands.txt"
        );
    }

    #[test]
    fn usage_from_events_and_completions() {
        let d = std::env::temp_dir().join(format!("hbench-oh-logs-{}", uuid::Uuid::new_v4()));
        let ev = d.join("sessions/sess-1/events");
        std::fs::create_dir_all(&ev).unwrap();
        let m = |p: i64, c: i64, k: i64, cost: f64| json!({"accumulated_cost": cost, "accumulated_token_usage": {"prompt_tokens": p, "completion_tokens": c, "cache_read_tokens": k}});
        std::fs::write(
            ev.join("0.json"),
            json!({"id": 0, "source": "user", "action": "message", "args": {"openhands_version": "0.59.0"}}).to_string(),
        )
        .unwrap();
        std::fs::write(ev.join("2.json"), json!({"id": 2, "source": "agent", "llm_metrics": m(100, 10, 50, 0.01)}).to_string()).unwrap();
        std::fs::write(ev.join("10.json"), json!({"id": 10, "source": "agent", "llm_metrics": m(300, 30, 150, 0.03)}).to_string()).unwrap();
        std::fs::write(ev.join("11.json"), json!({"id": 11, "source": "environment", "observation": "x"}).to_string()).unwrap();
        assert_eq!(usage_from_logs(&d, false), Some((300, 30, 150, 0.03)));

        let comp = d.join("completions");
        std::fs::create_dir_all(&comp).unwrap();
        let c = |p: i64, o: i64, cached: Value, cost: f64| json!({"messages": [], "response": {"choices": [{"message": {"content": "x"}}], "usage": {"prompt_tokens": p, "completion_tokens": o, "cache_read_input_tokens": cached}}, "cost": cost, "timestamp": 1.0});
        std::fs::write(comp.join("anthropic__claude-sonnet-4-5-1765823587.968077.json"), c(100, 10, json!(20), 0.5).to_string()).unwrap();
        std::fs::write(comp.join("anthropic__claude-sonnet-4-5-1765823590.1.json"), c(200, 20, json!(30), 0.25).to_string()).unwrap();
        std::fs::write(
            comp.join("anthropic__claude-sonnet-4-5-1765823599.0.json"),
            json!({"response": {"choices": []}, "cost": 9.0}).to_string(),
        )
        .unwrap();
        assert_eq!(usage_from_logs(&d, true), Some((300, 30, 50, 0.75)));

        let a = agent(json!({}), &[], None).unwrap();
        let mut ctx = AgentContext::default();
        a.populate_context_post_run(&d, &mut ctx);
        assert_eq!(
            (ctx.n_input_tokens, ctx.n_cache_tokens, ctx.n_output_tokens, ctx.cost_usd),
            (Some(300), Some(150), Some(30), Some(0.03))
        );
        std::fs::create_dir_all(d.join("sessions/sess-2")).unwrap();
        assert_eq!(usage_from_logs(&d, false), None);
    }
}
