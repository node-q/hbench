//! pi installed agent.

use super::common::opts::kw_int;
use super::common::{installed_setup, nvm_node_install_snippet, skills_dir};
use super::pins;
use super::{connection, ensure_system_dependencies, exec_agent, upload_text};
use crate::agents::model_connection::{Resolved, Spec};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

const SPEC: Spec = Spec { default_provider: None, api_key_envs: &[], base_url_envs: &[], passthrough: true };
const CONFIG_DIR: &str = "/tmp/hbench-pi-agent";
const CUSTOM_PROVIDER: &str = "hbench-endpoint";
const MCP_ADAPTER_DEFAULT_PI_VERSION: &str = "0.85.1";
const MCP_ADAPTER_DIR: &str = "/tmp/hbench-pi-mcp-adapter";
const MCP_ADAPTER_EXTENSION: &str = "/tmp/hbench-pi-mcp-adapter/node_modules/pi-mcp-adapter/index.ts";
const THINKING: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
const SESSION_DIR: &str = "/logs/agent/pi/sessions";

/// Dotted numeric release (`packaging.Version` subset), zero-padded for comparison.
fn version(s: &str) -> Option<[u64; 4]> {
    let mut out = [0u64; 4];
    let parts: Vec<&str> = s.trim().trim_start_matches('v').split('.').collect();
    if parts.len() > 4 {
        return None;
    }
    for (i, p) in parts.iter().enumerate() {
        out[i] = p.parse().ok()?;
    }
    Some(out)
}

pub struct Pi {
    args: AgentArgs,
    version: Option<String>,
    thinking: Option<String>,
    model_api: Option<String>,
    max_turns: Option<i64>,
    session_usage_start: HashMap<String, usize>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(Pi::new(args)?))
}

impl Pi {
    fn new(args: AgentArgs) -> Result<Pi> {
        let thinking = args.kw("thinking").or_else(|| args.kw("reasoning_effort"));
        if let Some(t) = &thinking
            && !THINKING.contains(&t.as_str())
        {
            bail!("Invalid kwargs for agent 'pi':\n  thinking: Input should be {}", THINKING.map(|t| format!("'{t}'")).join(", "));
        }
        let model_api = match args.kwargs.get("model_api") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => bail!("model_api must be a string"),
        };
        let max_turns = kw_int(&args, "max_turns")?;
        if max_turns.is_some_and(|n| n < 1) {
            bail!("Invalid kwargs for agent 'pi':\n  max_turns: Input should be greater than or equal to 1");
        }
        Ok(Pi { version: args.version_kw(), args, thinking, model_api, max_turns, session_usage_start: HashMap::new() })
    }

    fn package_name(&self) -> &'static str {
        match self.version.as_deref().and_then(version) {
            Some(v) if v < [0, 74, 0, 0] => "@mariozechner/pi-coding-agent",
            _ => pins::PI.name,
        }
    }

    /// Install command (with MCP adapter / max_turns version gates).
    fn install_command(&self) -> Result<String> {
        let mut install_version = self.version.clone();
        if !self.args.mcp_servers.is_empty() {
            let v = install_version.get_or_insert_with(|| MCP_ADAPTER_DEFAULT_PI_VERSION.into()).clone();
            let Some(p) = version(&v) else { bail!("Pi MCP requires a valid Pi version in the supported range <0.86.0,>=0.84.1") };
            if !([0, 84, 1, 0]..[0, 86, 0, 0]).contains(&p) {
                bail!("Pi MCP adapter {} requires Pi <0.86.0,>=0.84.1; got {v}", pins::PI_MCP_ADAPTER.npm_spec(None));
            }
        }
        if self.max_turns.is_some()
            && let Some(v) = install_version.as_deref().filter(|v| *v != "latest")
        {
            let Some(p) = version(v) else { bail!("Pi max_turns requires a valid Pi version at or above 0.35.0") };
            if p < [0, 35, 0, 0] {
                bail!("Pi max_turns requires the extension turn_end and abort APIs, available in Pi >=0.35.0; got {v}");
            }
        }
        let spec = format!("@{}", pins::PI.or(install_version.as_deref()));
        let mcp = if self.args.mcp_servers.is_empty() {
            String::new()
        } else {
            format!(" && npm install --prefix {MCP_ADAPTER_DIR} --ignore-scripts {}", pins::PI_MCP_ADAPTER.npm_spec(None))
        };
        Ok(format!(
            "set -euo pipefail; {} && npm install -g --ignore-scripts {}{spec}{mcp} && pi --version",
            nvm_node_install_snippet(22),
            self.package_name()
        ))
    }

    fn custom_models_json(&self, access: &Resolved, model_id: &str) -> Result<Option<Value>> {
        let model_api = self.model_api.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let Some(endpoint) = &access.configured_base_url else {
            if model_api.is_some() {
                bail!("model_api requires an explicitly configured base URL");
            }
            return Ok(None);
        };
        let Some(model_api) = model_api else { bail!("Pi custom endpoints require the model_api agent argument") };
        // BTreeMap iterates sorted.
        let Some(key_env) = access.api_key.as_ref().and_then(|k| access.env.iter().find(|(_, v)| *v == k).map(|(n, _)| n.clone())) else {
            bail!("Pi custom endpoints require an API-key environment-variable reference")
        };
        let mut model = json!({"id": model_id});
        if let Some(t) = self.thinking.as_deref().filter(|t| *t != "off") {
            model["reasoning"] = json!(true);
            if t == "xhigh" || t == "max" {
                model["thinkingLevelMap"] = json!({t: t});
            }
        }
        Ok(Some(
            json!({"providers": {CUSTOM_PROVIDER: {"baseUrl": endpoint, "apiKey": format!("${key_env}"), "api": model_api, "models": [model]}}}),
        ))
    }

    fn mcp_config(&self) -> Value {
        let mut servers = Map::new();
        for s in &self.args.mcp_servers {
            let v = if s.transport() == "stdio" { json!({"command": s.command, "args": s.args}) } else { json!({"url": s.url}) };
            servers.insert(s.name.clone(), v);
        }
        json!({"settings": {"hostConfigDiscovery": "off", "notifyOnStartupConnect": false, "scriptMode": false}, "mcpServers": servers})
    }

    fn max_turns_extension(max_turns: i64) -> String {
        format!(
            r#"export default function (pi: any) {{
  const maxTurns = {max_turns};
  let completedTurns = 0;

  pi.on("before_agent_start", (event: any) => ({{
    systemPrompt: `${{event.systemPrompt}}\n\nYou have a hard budget of ${{maxTurns}} model turns. Complete the task and provide your final answer within that budget.`,
  }}));

  pi.on("turn_end", (_event: any, ctx: any) => {{
    completedTurns += 1;
    if (completedTurns >= maxTurns) {{
      ctx.abort();
    }}
  }});
}}
"#
        )
    }

    fn run_command(
        &self,
        instruction: &str,
        provider: &str,
        model_id: &str,
        isolated: bool,
        extensions: &[String],
        resume: bool,
    ) -> String {
        let prefix = if isolated { format!("PI_CODING_AGENT_DIR={CONFIG_DIR} ") } else { String::new() };
        let ext: String = extensions.iter().map(|p| format!("--extension {} ", quote(p))).collect();
        let flags = self.thinking.as_ref().map(|t| format!("--thinking {} ", quote(t))).unwrap_or_default();
        let resume_flag = if resume { "--continue " } else { "" };
        format!(
            ". ~/.nvm/nvm.sh; mkdir -p {SESSION_DIR} && {prefix}pi --print --mode json --session-dir {SESSION_DIR} {resume_flag}\
             --provider {provider} --model {model_id} {ext}{flags}{} 2>&1 </dev/null | grep -v '\"type\":\"message_update\"' \
             | stdbuf -oL tee /logs/agent/pi.txt",
            quote(instruction)
        )
    }
}

fn int(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

/// `_metrics_from_usage` -> (prompt, completion, cached, cost).
fn metrics(usage: &Value) -> Option<(i64, i64, i64, Option<f64>)> {
    let u = usage.as_object()?;
    if !["input", "output", "cacheRead", "cacheWrite", "reasoning", "totalTokens", "cost"].iter().any(|k| u.contains_key(*k)) {
        return None;
    }
    let cost = usage["cost"]["total"].as_f64().filter(|c| *c > 0.0);
    Some((
        int(&usage["input"]) + int(&usage["cacheRead"]) + int(&usage["cacheWrite"]),
        int(&usage["output"]),
        int(&usage["cacheRead"]),
        cost,
    ))
}

/// Root-to-leaf branch of Pi's append-only session tree (headers first).
fn active_events(events: &[(usize, Value)]) -> Vec<&(usize, Value)> {
    let mut by_id: HashMap<&str, &(usize, Value)> = HashMap::new();
    let mut headers = vec![];
    let mut leaf: Option<&str> = None;
    for pe in events {
        if pe.1["type"] == "session" {
            headers.push(pe);
            continue;
        }
        if let Some(id) = pe.1["id"].as_str().filter(|s| !s.is_empty()) {
            by_id.insert(id, pe);
            leaf = Some(id);
        }
    }
    if leaf.is_none() {
        return events.iter().collect();
    }
    let mut branch = vec![];
    let mut seen = HashSet::new();
    while let Some(id) = leaf {
        if !seen.insert(id) {
            break;
        }
        let Some(pe) = by_id.get(id) else { break };
        branch.push(*pe);
        leaf = pe.1["parentId"].as_str().filter(|s| !s.is_empty());
    }
    branch.reverse();
    headers.extend(branch);
    headers
}

/// Whether the ATIF converter would produce any step for these events.
fn has_steps(events: &[&(usize, Value)]) -> bool {
    events.iter().any(|(_, e)| match e["type"].as_str() {
        Some("custom_message") => true,
        Some("compaction" | "branch_summary") => e["summary"].as_str().is_some_and(|s| !s.trim().is_empty()),
        Some("message") => match e["message"]["role"].as_str() {
            Some("user") => true,
            Some("assistant") => {
                let m = &e["message"];
                let aborted =
                    m["stopReason"] == "aborted" || (m["stopReason"] == "error" && m["errorMessage"] == "This operation was aborted");
                let has_content = m["content"].as_array().is_some_and(|c| {
                    c.iter().any(|b| match b["type"].as_str() {
                        Some("text") => b["text"].is_string(),
                        Some("thinking") => b["thinking"].is_string(),
                        Some("toolCall") => b["id"].is_string() && b["name"].is_string(),
                        _ => false,
                    })
                });
                !aborted || has_content
            }
            _ => false,
        },
        _ => false,
    })
}

/// Usage from the newest Pi session file (events after `copied_until` only).
pub fn session_usage(text: &str, copied_until: usize) -> Option<AgentContext> {
    let events: Vec<(usize, Value)> = text
        .lines()
        .enumerate()
        .filter_map(|(i, l)| serde_json::from_str::<Value>(l).ok().filter(Value::is_object).map(|v| (i + 1, v)))
        .collect();
    if !has_steps(&active_events(&events)) {
        return None;
    }
    let mut ctx = AgentContext { n_input_tokens: Some(0), n_cache_tokens: Some(0), n_output_tokens: Some(0), ..Default::default() };
    for (pos, e) in &events {
        if *pos <= copied_until {
            continue;
        }
        let usage = match e["type"].as_str() {
            Some("compaction" | "branch_summary") => &e["usage"],
            Some("message") if matches!(e["message"]["role"].as_str(), Some("assistant" | "toolResult")) => &e["message"]["usage"],
            _ => continue,
        };
        if let Some((p, c, cached, cost)) = metrics(usage) {
            ctx.add_usage(p, cached, c, cost);
        }
    }
    Some(ctx)
}

/// Fallback: `message_end` usage from pi.txt.
pub fn output_usage(text: &str) -> AgentContext {
    let (mut input, mut output, mut cr, mut cw, mut cost) = (0, 0, 0, 0, 0.0);
    for l in text.lines() {
        let Ok(e) = serde_json::from_str::<Value>(l.trim()) else { continue };
        let m = &e["message"];
        if e["type"] != "message_end" || !matches!(m["role"].as_str(), Some("assistant" | "toolResult")) {
            continue;
        }
        let u = &m["usage"];
        input += int(&u["input"]);
        output += int(&u["output"]);
        cr += int(&u["cacheRead"]);
        cw += int(&u["cacheWrite"]);
        cost += u["cost"]["total"].as_f64().unwrap_or(0.0);
    }
    AgentContext {
        n_input_tokens: Some(input + cr + cw),
        n_output_tokens: Some(output),
        n_cache_tokens: Some(cr),
        cost_usd: (cost > 0.0).then_some(cost),
        ..Default::default()
    }
}

fn newest_session(dir: &Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .max_by_key(|p| (std::fs::metadata(p).and_then(|m| m.modified()).ok(), p.file_name().map(|n| n.to_os_string())))
}

#[async_trait::async_trait]
impl Agent for Pi {
    fn name(&self) -> &str {
        "pi"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        let cmd = self.install_command();
        let install = async {
            let cmd = cmd?;
            ensure_system_dependencies(cx, &["curl"]).await?;
            exec_agent(cx, &cmd, &BTreeMap::new()).await.map(|_| ())
        };
        installed_setup(cx, &mut self.version, install, ". ~/.nvm/nvm.sh; pi --version", |s| {
            s.trim().lines().last().unwrap_or("").trim().to_string()
        })
        .await
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let Some((provider, model_id)) =
            self.args.model_name.clone().and_then(|m| m.split_once('/').map(|(a, b)| (a.to_string(), b.to_string())))
        else {
            bail!("Model name must be in the format provider/model_name")
        };
        let access = connection(&self.args, SPEC);
        let mut provider = access.provider.clone().unwrap_or(provider);
        let mut env = access.env.clone();
        if provider == "anthropic"
            && let Some(t) = self.args.get_env("ANTHROPIC_OAUTH_TOKEN")
        {
            env.insert("ANTHROPIC_OAUTH_TOKEN".into(), t);
        }
        let models_json = self.custom_models_json(&access, &model_id)?;
        let has_mcp = !self.args.mcp_servers.is_empty();
        let ensure_dir = format!("mkdir -p {CONFIG_DIR} && chmod 700 {CONFIG_DIR}");
        if let Some(m) = &models_json {
            exec_agent(cx, &ensure_dir, &BTreeMap::new()).await?;
            let path = format!("{CONFIG_DIR}/models.json");
            upload_text(cx, &path, &(serde_json::to_string_pretty(m)? + "\n")).await?;
            exec_agent(cx, &format!("chmod 600 {path}"), &BTreeMap::new()).await?;
            provider = CUSTOM_PROVIDER.into();
        }
        if has_mcp {
            exec_agent(cx, &ensure_dir, &BTreeMap::new()).await?;
            upload_text(cx, &format!("{CONFIG_DIR}/mcp.json"), &(serde_json::to_string_pretty(&self.mcp_config())? + "\n")).await?;
        }
        let mut extensions = vec![];
        if has_mcp {
            extensions.push(MCP_ADAPTER_EXTENSION.to_string());
        }
        if let Some(n) = self.max_turns {
            exec_agent(cx, &ensure_dir, &BTreeMap::new()).await?;
            let path = format!("{CONFIG_DIR}/max-turns.ts");
            upload_text(cx, &path, &Self::max_turns_extension(n)).await?;
            extensions.push(path);
        }
        if let Some(d) = skills_dir(&self.args) {
            exec_agent(
                cx,
                &format!("mkdir -p $HOME/.agents/skills && cp -r {}/* $HOME/.agents/skills/ 2>/dev/null || true", quote(&d)),
                &BTreeMap::new(),
            )
            .await?;
        }
        self.session_usage_start.clear();
        if resume {
            let r = exec_agent(cx, &format!("find {SESSION_DIR} -maxdepth 1 -type f -name '*.jsonl' -exec wc -l {{}} +"), &BTreeMap::new())
                .await?;
            for line in r.stdout.lines() {
                if let Some((n, f)) = line.trim().split_once(' ')
                    && let Ok(n) = n.parse()
                {
                    let name = f.trim().rsplit('/').next().unwrap_or_default().to_string();
                    self.session_usage_start.insert(name, n);
                }
            }
        }
        let cmd = self.run_command(&instruction, &provider, &model_id, models_json.is_some() || has_mcp, &extensions, resume);
        exec_agent(cx, &cmd, &env).await?;
        Ok(())
    }

    fn collect_usage(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        let session = newest_session(&logs_dir.join("pi/sessions")).and_then(|p| {
            let copied = p.file_name().and_then(|n| self.session_usage_start.get(n.to_str()?)).copied().unwrap_or(0);
            session_usage(&std::fs::read_to_string(&p).ok()?, copied)
        });
        let c = match session {
            Some(c) => c,
            None => match std::fs::read_to_string(logs_dir.join("pi.txt")) {
                Ok(t) => output_usage(&t),
                Err(_) => return,
            },
        };
        ctx.n_input_tokens = c.n_input_tokens;
        ctx.n_cache_tokens = c.n_cache_tokens;
        ctx.n_output_tokens = c.n_output_tokens;
        ctx.cost_usd = c.cost_usd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common::test_args as args;

    fn pi(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<Pi> {
        Pi::new(args(kwargs, env, model))
    }

    #[test]
    fn options_and_install() {
        assert!(pi(json!({"thinking": "huge"}), &[], None).is_err());
        assert!(pi(json!({"max_turns": 0}), &[], None).is_err());
        assert!(pi(json!({"model_api": 3}), &[], None).is_err());
        assert_eq!(pi(json!({"reasoning_effort": "high"}), &[], None).unwrap().thinking.as_deref(), Some("high"));
        let legacy = pi(json!({"version": "0.73.9"}), &[], None).unwrap();
        assert!(
            legacy
                .install_command()
                .unwrap()
                .contains("npm install -g --ignore-scripts @mariozechner/pi-coding-agent@0.73.9 && pi --version")
        );
        assert!(pi(json!({"version": "0.30.0", "max_turns": 3}), &[], None).unwrap().install_command().is_err());
        assert!(pi(json!({}), &[], None).unwrap().install_command().unwrap().contains("@earendil-works/pi-coding-agent@1.3.0"));
        let mut a = args(json!({}), &[], None);
        a.mcp_servers = vec![serde_json::from_value(json!({"name": "s", "transport": "stdio", "command": "npx", "args": ["x"]})).unwrap()];
        let m = Pi::new(a).unwrap();
        assert!(
            m.install_command()
                .unwrap()
                .contains("@0.85.1 && npm install --prefix /tmp/hbench-pi-mcp-adapter --ignore-scripts pi-mcp-adapter@2.34.0")
        );
        assert_eq!(m.mcp_config()["mcpServers"]["s"], json!({"command": "npx", "args": ["x"]}));
    }

    #[test]
    fn custom_endpoint_and_command() {
        let a = pi(
            json!({"model_api": "openai-completions", "thinking": "xhigh"}),
            &[("OPENAI_API_KEY", "secret"), ("OPENAI_BASE_URL", "http://e/v1")],
            Some("openai/m"),
        )
        .unwrap();
        let access = connection(&a.args, SPEC);
        let mj = a.custom_models_json(&access, "m").unwrap().unwrap();
        assert_eq!(
            mj["providers"]["hbench-endpoint"],
            json!({"baseUrl": "http://e/v1", "apiKey": "$OPENAI_API_KEY", "api": "openai-completions", "models": [{"id": "m", "reasoning": true, "thinkingLevelMap": {"xhigh": "xhigh"}}]})
        );
        let no_api = pi(json!({}), &[("OPENAI_API_KEY", "s"), ("OPENAI_BASE_URL", "http://e")], Some("openai/m")).unwrap();
        assert!(no_api.custom_models_json(&connection(&no_api.args, SPEC), "m").is_err());
        let cmd = a.run_command("hi there", "hbench-endpoint", "m", true, &["/x.ts".into()], true);
        assert!(cmd.contains("PI_CODING_AGENT_DIR=/tmp/hbench-pi-agent pi --print --mode json --session-dir /logs/agent/pi/sessions --continue --provider hbench-endpoint --model m --extension /x.ts --thinking xhigh 'hi there' 2>&1"));
    }

    #[test]
    fn usage() {
        let out = r#"{"type":"message_end","message":{"role":"assistant","usage":{"input":100,"output":50,"cacheRead":20,"cacheWrite":10,"cost":{"total":0.005}}}}
{"type":"message_end","message":{"role":"toolResult","usage":{"input":200,"output":80,"cacheRead":30,"cacheWrite":5,"cost":{"total":0.008}}}}
{"type":"message_end","message":{"role":"user","usage":{"input":999}}}"#;
        let c = output_usage(out);
        assert_eq!((c.n_input_tokens, c.n_output_tokens, c.n_cache_tokens), (Some(365), Some(130), Some(50)));
        assert!((c.cost_usd.unwrap() - 0.013).abs() < 1e-9);

        let session = r#"{"type":"session","id":"pi-session-1"}
{"type":"message","message":{"role":"user","content":[{"type":"text","text":"First step"}]}}
{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"First answer"}],"usage":{"input":100,"output":50,"cacheRead":20,"cost":{"total":0.005}}}}
{"type":"message","message":{"role":"user","content":[{"type":"text","text":"Second step"}]}}
{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"Second answer"}],"usage":{"input":40,"output":20,"cacheRead":10,"cost":{"total":0.002}}}}"#;
        let c = session_usage(session, 3).unwrap();
        assert_eq!((c.n_input_tokens, c.n_output_tokens, c.n_cache_tokens, c.cost_usd), (Some(50), Some(20), Some(10), Some(0.002)));
        assert_eq!(session_usage(session, 0).unwrap().n_input_tokens, Some(170));
        assert!(session_usage("{\"type\":\"session\",\"id\":\"s\"}", 0).is_none());
    }
}
