//! Goose.

use super::common::{detect_version, installed_dir, opts, skills_dir};
use super::{connection, ensure_system_dependencies, exec_agent, exec_error};
use crate::agents::model_connection::{Resolved, Spec, with_canonical_provider_envs};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

const SPEC: Spec = Spec { default_provider: None, api_key_envs: &[], base_url_envs: &[], passthrough: true };
const INSTRUCTIONS: &str = "You are given a task and you need to complete it. You are currently executing in a docker container where you are \
    being evaluated on a benchmark for LLM agents. Act autonomously. You will not receive any feedback on your progress, so you must \
    use your own tools to complete the task without any intervention.";

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    let a = Goose { version: args.version_kw(), args };
    a.cli_flags()?;
    a.option_env()?;
    Ok(Box::new(a))
}

pub struct Goose {
    args: AgentArgs,
    version: Option<String>,
}

impl Goose {
    fn cli_flags(&self) -> Result<String> {
        Ok(opts::int(&self.args, "max_turns", None)?.map(|n| format!("--max-turns {n}")).unwrap_or_default())
    }

    /// GooseOptions `Env(...)` fields.
    fn option_env(&self) -> Result<BTreeMap<String, String>> {
        let a = &self.args;
        let mut env = BTreeMap::new();
        if let Some(v) = opts::choice(a, "reasoning_effort", None, &["off", "low", "medium", "high", "max"])? {
            env.insert("GOOSE_THINKING_EFFORT".into(), v);
        }
        if let Some(t) = opts::float(a, "temperature", None)? {
            if !(0.0..=1.0).contains(&t) {
                bail!("Invalid value for 'temperature': must be between 0.0 and 1.0");
            }
            env.insert("GOOSE_TEMPERATURE".into(), opts::py_float(t));
        }
        if let Some(n) = opts::int(a, "max_tokens", None)? {
            if !(1..=2_147_483_647).contains(&n) {
                bail!("Invalid value for 'max_tokens': must be > 0 and <= 2147483647");
            }
            env.insert("GOOSE_MAX_TOKENS".into(), n.to_string());
        }
        if let Some(n) = opts::int(a, "context_limit", None)? {
            if n <= 0 {
                bail!("Invalid value for 'context_limit': must be > 0");
            }
            env.insert("GOOSE_CONTEXT_LIMIT".into(), n.to_string());
        }
        Ok(env)
    }

    /// Canonical provider env names; goose reads ANTHROPIC_HOST / GOOGLE_HOST for base URLs.
    fn connection(&self) -> Resolved {
        let mut c = with_canonical_provider_envs(connection(&self.args, SPEC));
        let host = match c.provider.as_deref() {
            Some("anthropic") => Some(("ANTHROPIC_BASE_URL", "ANTHROPIC_HOST")),
            Some("google") => Some(("GOOGLE_BASE_URL", "GOOGLE_HOST")),
            _ => None,
        };
        if let Some((src, dst)) = host
            && let Some(v) = c.env.remove(src)
        {
            c.env.insert(dst.into(), v);
        }
        c
    }

    fn run_env(&self) -> Result<BTreeMap<String, String>> {
        let Some((prov, model)) = self.args.model_name.as_deref().and_then(|m| m.split_once('/')) else {
            bail!("Model name must be in the format provider/model_name")
        };
        let c = self.connection();
        let prov = c.provider.clone().unwrap_or_else(|| prov.to_string());
        if !["anthropic", "databricks", "google", "openai", "tetrate"].contains(&prov.as_str()) {
            bail!("Unsupported provider: {prov}");
        }
        if c.api_key.as_deref().is_none_or(str::is_empty) {
            bail!("No API key found for provider: {prov}");
        }
        if prov == "databricks" && c.configured_base_url.is_none() {
            bail!("DATABRICKS_HOST environment variable not set");
        }
        let mut env = c.env;
        env.insert("GOOSE_MODEL".into(), model.into());
        env.insert("GOOSE_PROVIDER".into(), prov);
        env.insert("XDG_DATA_HOME".into(), "/logs/agent/goose/xdg-data".into());
        env.insert("XDG_STATE_HOME".into(), "/logs/agent/goose/xdg-state".into());
        env.extend(self.option_env()?);
        Ok(env)
    }

    /// An explicitly pinned Goose release below 1.19.0 still supports SSE MCP servers.
    fn supports_sse(&self) -> bool {
        let Some(v) = self.version.as_deref() else { return false };
        let parts: Option<Vec<u64>> = v.split('.').map(|p| p.parse().ok()).collect();
        parts.is_some_and(|p| !p.is_empty() && p < vec![1, 19, 0])
    }

    fn recipe_yaml(&self, instruction: &str) -> Result<String> {
        let mut ext = vec![json!({"type": "builtin", "name": "developer"}), json!({"type": "platform", "name": "todo"})];
        for s in &self.args.mcp_servers {
            ext.push(match s.transport() {
                "stdio" => json!({"type": "stdio", "name": s.name, "cmd": s.command, "args": s.args}),
                "sse" if self.supports_sse() => json!({"type": "sse", "name": s.name, "uri": s.url}),
                "sse" => bail!(
                    "Goose 1.19.0 and later do not support MCP transport 'sse' (server '{}'). Set transport = 'streamable-http' or 'stdio' explicitly \
                     in the MCP server config, or pin Goose below 1.19.0 ('sse' is the default when omitted).",
                    s.name
                ),
                _ => json!({"type": "streamable_http", "name": s.name, "uri": s.url}),
            });
        }
        let recipe = json!({"version": "1.0.0", "title": "hbench-task", "description": "hbench task recipe", "instructions": INSTRUCTIONS, "prompt": instruction, "extensions": ext});
        Ok(serde_yaml_ng::to_string(&recipe)?)
    }
}

/// Usage as the goose.txt -> ATIF conversion reports it: the stream-json `complete` event,
/// else (old plain-text goose, no usage) zeros when the log has any content.
pub fn parse_goose_usage(text: &str) -> Option<AgentContext> {
    #[derive(Default)]
    struct Msg {
        role: String,
        text: String,
        reasoning: String,
        tool_calls: bool,
        tool_responses: bool,
    }
    let mut order: Vec<String> = vec![];
    let mut msgs: BTreeMap<String, Msg> = BTreeMap::new();
    let mut complete: Option<Value> = None;
    for (i, e) in text.trim().lines().filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok()).enumerate() {
        match e["type"].as_str() {
            Some("message") => {
                let m = &e["message"];
                let id = m["id"].as_str().map(String::from).unwrap_or_else(|| format!("\0anon-{i}"));
                let entry = msgs.entry(id.clone()).or_insert_with(|| {
                    order.push(id.clone());
                    Msg { role: m["role"].as_str().unwrap_or("").into(), ..Default::default() }
                });
                for item in m["content"].as_array().into_iter().flatten() {
                    match item["type"].as_str() {
                        Some("text") => entry.text += item["text"].as_str().unwrap_or(""),
                        Some("thinking") => {
                            entry.reasoning += item["thinking"].as_str().filter(|s| !s.is_empty()).or(item["text"].as_str()).unwrap_or("")
                        }
                        Some("toolRequest") => entry.tool_calls = true,
                        Some("toolResponse") => entry.tool_responses = true,
                        _ => {}
                    }
                }
            }
            Some("complete") => complete = Some(e),
            Some("error") => {
                let id = format!("\0error-{i}");
                order.push(id.clone());
                msgs.insert(id, Msg { role: "error".into(), text: "[error]".into(), ..Default::default() });
            }
            _ => {}
        }
    }
    let mut steps: Vec<&str> = vec![]; // step sources
    for id in &order {
        let m = &msgs[id];
        let text = m.text.trim();
        match m.role.as_str() {
            "user" if m.tool_responses && steps.last() == Some(&"agent") => {}
            "user" if !text.is_empty() => steps.push("user"),
            "assistant" if !text.is_empty() || !m.reasoning.trim().is_empty() || m.tool_calls => steps.push("agent"),
            "error" => steps.push("agent"),
            _ => {}
        }
    }
    if steps.is_empty() {
        // Plain-text fallback: any content yields steps but no usage.
        return (!text.trim().is_empty()).then(|| AgentContext { n_input_tokens: Some(0), n_output_tokens: Some(0), ..Default::default() });
    }
    let c = complete.unwrap_or(Value::Null);
    Some(AgentContext {
        // goose's input_tokens already include cache reads/writes; older goose reports only total_tokens.
        n_input_tokens: Some(c["input_tokens"].as_i64().or(c["total_tokens"].as_i64()).unwrap_or(0)),
        n_output_tokens: Some(c["output_tokens"].as_i64().unwrap_or(0)),
        n_cache_tokens: c["cache_read_input_tokens"].as_i64(),
        cost_usd: c["cost_usd"].as_f64(),
        ..Default::default()
    })
}

/// `exec_as_agent(..., timeout_sec=10)`.
async fn exec_agent_10s(cx: &AgentCtx<'_>, cmd: &str, env: &BTreeMap<String, String>) -> Result<()> {
    let r = cx.exec(&format!("set -o pipefail; {cmd}"), env, Some(Duration::from_secs(10))).await?;
    if r.code != 0 {
        return Err(exec_error(cmd, &r, None));
    }
    Ok(())
}

#[async_trait::async_trait]
impl Agent for Goose {
    fn name(&self) -> &str {
        "goose"
    }
    fn version(&self) -> Option<String> {
        Some(self.version.clone().unwrap_or_else(|| "stable".into()))
    }
    fn supports_resume(&self) -> bool {
        true
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "bzip2", "libxcb", "libgomp"]).await?;
        let env =
            BTreeMap::from([("GOOSE_DISABLE_KEYRING".to_string(), "true".to_string()), ("CONFIGURE".to_string(), "false".to_string())]);
        exec_agent(
            cx,
            "mkdir -p ~/.config/goose && cat > ~/.config/goose/config.yaml << 'INNEREOF'\nGOOSE_MODEL: ${GOOSE_MODEL}\nGOOSE_PROVIDER: ${GOOSE_PROVIDER}\nextensions:\n  \
             developer:\n    bundled: true\n    display_name: Developer\n    enabled: true\n    name: developer\n    timeout: 300\n    type: builtin\n  \
             todo:\n    bundled: true\n    display_name: Todo\n    enabled: true\n    name: todo\n    type: platform\n  \
             summon:\n    bundled: true\n    display_name: Summon\n    enabled: true\n    name: summon\n    type: platform\nINNEREOF",
            &env,
        )
        .await?;
        let url_v = self.version.as_deref().unwrap_or("stable");
        exec_agent(
            cx,
            &format!(
                "set -euo pipefail; curl -fsSL https://github.com/block/goose/releases/download/{url_v}/download_cli.sh | bash && \
                 export PATH=\"$HOME/.local/bin:$PATH\" && goose --version"
            ),
            &env,
        )
        .await?;
        if self.version.is_none() {
            self.version = detect_version(cx, "export PATH=\"$HOME/.local/bin:$PATH\"; goose --version", super::parse_version).await;
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let env = self.run_env()?;
        let recipe = self.recipe_yaml(&instruction)?;
        if let Some(d) = skills_dir(&self.args) {
            exec_agent_10s(
                cx,
                &format!("mkdir -p ~/.config/goose/skills && cp -r {}/* ~/.config/goose/skills/ 2>/dev/null || true", quote(&d)),
                &env,
            )
            .await?;
        }
        exec_agent_10s(cx, &format!("cat > ~/hbench-recipe.yaml << 'EOF'\n{recipe}EOF"), &env).await?;
        let flags = self.cli_flags()?;
        let extra = if flags.is_empty() { String::new() } else { format!("{flags} ") };
        let cmd = format!(
            "export PATH=\"$HOME/.local/bin:$PATH\" && goose run --recipe ~/hbench-recipe.yaml {}--output-format stream-json {extra}2>&1 | stdbuf -oL tee /logs/agent/goose.txt",
            if resume { "--resume " } else { "" }
        );
        exec_agent(cx, &cmd, &env).await?;
        Ok(())
    }

    fn populate_context_post_run(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        let Some(c) = std::fs::read_to_string(logs_dir.join("goose.txt")).ok().and_then(|t| parse_goose_usage(&t)) else { return };
        ctx.n_input_tokens = c.n_input_tokens;
        ctx.n_output_tokens = c.n_output_tokens;
        if c.n_cache_tokens.is_some() {
            ctx.n_cache_tokens = c.n_cache_tokens; // unreported stays unset
        }
        if c.cost_usd.is_some() {
            ctx.cost_usd = c.cost_usd;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;

    fn goose(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Goose {
        Goose { version: None, args: common::test_args(kwargs, env, model) }
    }

    #[test]
    fn options_env_and_validation() {
        let g = goose(
            json!({"max_turns": 30, "reasoning_effort": "high", "temperature": 1, "max_tokens": "4096"}),
            &[("ANTHROPIC_API_KEY", "sk"), ("ANTHROPIC_BASE_URL", "http://p")],
            Some("anthropic/claude-x"),
        );
        assert_eq!(g.cli_flags().unwrap(), "--max-turns 30");
        let env = g.run_env().unwrap();
        assert_eq!((env["GOOSE_PROVIDER"].as_str(), env["GOOSE_MODEL"].as_str()), ("anthropic", "claude-x"));
        assert_eq!(
            (env["GOOSE_THINKING_EFFORT"].as_str(), env["GOOSE_TEMPERATURE"].as_str(), env["GOOSE_MAX_TOKENS"].as_str()),
            ("high", "1.0", "4096")
        );
        assert_eq!(env["ANTHROPIC_HOST"], "http://p");
        assert!(!env.contains_key("ANTHROPIC_BASE_URL"));
        assert!(create(common::test_args(json!({"temperature": 1.5}), &[], None)).is_err());
        assert!(create(common::test_args(json!({"max_tokens": 0}), &[], None)).is_err());
        assert!(create(common::test_args(json!({"reasoning_effort": "extreme"}), &[], None)).is_err());
        assert!(goose(json!({}), &[], Some("mistral/m")).run_env().unwrap_err().to_string().contains("Unsupported provider"));
    }

    #[test]
    fn recipe_and_sse() {
        let mut g = goose(json!({}), &[], None);
        g.args.mcp_servers = vec![serde_json::from_value(json!({"name": "s", "url": "http://m"})).unwrap()];
        assert!(g.recipe_yaml("x").is_err());
        g.version = Some("1.18.2".into());
        let y: Value = serde_yaml_ng::from_str(&g.recipe_yaml("do it").unwrap()).unwrap();
        assert_eq!(y["prompt"], "do it");
        assert_eq!(y["extensions"][2], json!({"type": "sse", "name": "s", "uri": "http://m"}));
        g.version = Some("1.19.0".into());
        assert!(g.recipe_yaml("x").is_err());
    }

    #[test]
    fn usage_parsing() {
        let log = "{\"type\":\"message\",\"message\":{\"id\":\"a\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Hel\"}]}}\n\
            {\"type\":\"message\",\"message\":{\"id\":\"a\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"lo\"}]}}\n\
            {\"type\":\"complete\",\"total_tokens\":1500,\"input_tokens\":1200,\"output_tokens\":300,\"cache_read_input_tokens\":800,\"cost_usd\":0.01}\n";
        let c = parse_goose_usage(log).unwrap();
        assert_eq!((c.n_input_tokens, c.n_output_tokens, c.n_cache_tokens, c.cost_usd), (Some(1200), Some(300), Some(800), Some(0.01)));
        let old = "{\"type\":\"message\",\"message\":{\"id\":\"a\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"x\"}]}}\n{\"type\":\"complete\",\"total_tokens\":77}\n";
        let c = parse_goose_usage(old).unwrap();
        assert_eq!((c.n_input_tokens, c.n_output_tokens, c.n_cache_tokens, c.cost_usd), (Some(77), Some(0), None, None));
        let text = parse_goose_usage("starting session\n─── shell | developer ──────\ncommand: ls\n\nfile.txt\n").unwrap();
        assert_eq!((text.n_input_tokens, text.n_output_tokens), (Some(0), Some(0)));
        assert!(parse_goose_usage("  \n").is_none());
    }
}
