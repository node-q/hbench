//! Hermes.

use super::common::parse_jsonl;
use super::common::{installed_dir, skills_dir};
use super::pins;
use super::{ensure_system_dependencies, exec_agent, exec_error};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::atif;
use crate::env::quote;
use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

const PATH_EXPORT: &str = "export PATH=\"$HOME/.local/bin:$PATH\"";

/// Model provider prefix -> (hermes --provider flag, key env names). Others route via OpenRouter.
fn native_provider(p: &str) -> Option<(&'static str, &'static [&'static str])> {
    Some(match p {
        "anthropic" => ("anthropic", &["ANTHROPIC_API_KEY", "ANTHROPIC_TOKEN"]),
        "openai" => ("openai-api", &["OPENAI_API_KEY"]),
        "zai" => ("zai", &["GLM_API_KEY", "ZAI_API_KEY", "Z_AI_API_KEY"]),
        "kimi" => ("kimi-coding", &["KIMI_API_KEY"]),
        "minimax" => ("minimax", &["MINIMAX_API_KEY"]),
        "minimax-cn" => ("minimax-cn", &["MINIMAX_CN_API_KEY"]),
        _ => return None,
    })
}

pub struct Hermes {
    args: AgentArgs,
    version: Option<String>,
    version_was_pinned: bool,
    toolsets: Option<String>,
    max_turns: Option<i64>,
    native_session_id: Option<String>,
    last_run_was_resume: bool,
    /// (native session id, prompt tokens, completion tokens) last reported.
    reported: Mutex<Option<(Option<String>, i64, i64)>>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(Hermes::new(args)?))
}

impl Hermes {
    fn new(args: AgentArgs) -> Result<Hermes> {
        let max_turns = args
            .kw("max_turns")
            .map(|v| v.trim().parse::<i64>().map_err(|_| anyhow!("Invalid kwargs for agent 'hermes':\n  max_turns: must be an integer")))
            .transpose()?;
        Ok(Hermes {
            version: args.version_kw(),
            version_was_pinned: args.version_kw().is_some(),
            toolsets: args.kw("toolsets"),
            max_turns,
            native_session_id: None,
            last_run_was_resume: false,
            reported: Mutex::new(None),
            args,
        })
    }

    /// (env, --provider flag, cli model) for the configured model.
    fn auth(&self) -> Result<(BTreeMap<String, String>, Option<&'static str>, String)> {
        let (provider, model) = self.args.require_provider_model()?;
        let mut env = BTreeMap::from([("HERMES_HOME".to_string(), "/tmp/hermes".to_string()), ("TERMINAL_ENV".into(), "local".into())]);
        let mut flag = None;
        let native = native_provider(provider);
        if let Some((native_flag, keys)) = native {
            // Agent env first, then host env, first name wins per source.
            let found = keys
                .iter()
                .find_map(|k| self.args.extra_env.get(*k).map(|v| (k, v.clone())))
                .or_else(|| keys.iter().find_map(|k| std::env::var(k).ok().map(|v| (k, v))));
            if let Some((k, v)) = found {
                env.insert(k.to_string(), v);
                flag = Some(native_flag);
                if provider == "openai"
                    && let Some(u) = self.args.get_env("OPENAI_BASE_URL")
                {
                    env.insert("OPENAI_BASE_URL".into(), u);
                }
            }
        }
        if flag.is_none() {
            let Some(k) = self.args.get_env("OPENROUTER_API_KEY") else {
                match native {
                    Some((_, keys)) => bail!("No API key found. Set {} or OPENROUTER_API_KEY.", keys.join(" or ")),
                    None => bail!("No API key found. Set OPENROUTER_API_KEY."),
                }
            };
            env.insert("OPENROUTER_API_KEY".into(), k);
        }
        let cli_model = if flag.is_some() { model.to_string() } else { self.args.model_name.clone().unwrap() };
        Ok((env, flag, cli_model))
    }

    fn config_yaml(model: &str, max_turns: Option<i64>) -> String {
        let mut c = json!({
            "model": model,
            "provider": "auto",
            "toolsets": ["hermes-cli"],
            "memory": {"memory_enabled": false, "user_profile_enabled": false},
            "terminal": {"backend": "local", "timeout": 180},
            "delegation": {"max_iterations": 50},
            "checkpoints": {"enabled": false},
        });
        if let Some(n) = max_turns {
            c["agent"] = json!({"max_turns": n});
        }
        serde_yaml_ng::to_string(&c).unwrap()
    }

    fn mcp_command(&self) -> Option<String> {
        if self.args.mcp_servers.is_empty() {
            return None;
        }
        let mut m = Map::new();
        for s in &self.args.mcp_servers {
            let v = if s.transport() == "stdio" { json!({"command": s.command, "args": s.args}) } else { json!({"url": s.url}) };
            m.insert(s.name.clone(), v);
        }
        let yaml = serde_yaml_ng::to_string(&json!({"mcp_servers": m})).unwrap();
        Some(format!("cat >> /tmp/hermes/config.yaml << 'MCPEOF'\n{yaml}MCPEOF"))
    }

    fn run_command(&self, flag: Option<&str>, cli_model: &str, resume: bool) -> Result<String> {
        let mut parts = vec!["hermes --yolo chat".to_string()];
        if resume {
            if self.native_session_id.is_none() && self.version_was_pinned {
                bail!("Cannot resume the pinned Hermes version because the previous run did not export a native session ID.");
            }
            parts.push(format!("--resume {}", quote(self.native_session_id.as_deref().unwrap_or("latest"))));
        }
        parts.extend(["-q \"$HBENCH_INSTRUCTION\"".into(), "-Q".into(), format!("--model {}", quote(cli_model))]);
        if let Some(f) = flag {
            parts.push(format!("--provider {}", quote(f)));
        }
        if let Some(t) = &self.toolsets {
            parts.push(format!("--toolsets {}", quote(t)));
        }
        Ok(format!("{PATH_EXPORT} && {} 2>&1 | stdbuf -oL tee /logs/agent/hermes.txt", parts.join(" ")))
    }
}

/// Exec as the agent with a timeout; non-zero exit -> classified error.
async fn exec_timeout(cx: &AgentCtx<'_>, cmd: &str, env: &BTreeMap<String, String>, secs: u64) -> Result<crate::env::ExecResult> {
    let r = cx.exec(&format!("set -o pipefail; {cmd}"), env, Some(Duration::from_secs(secs))).await?;
    if r.code != 0 {
        return Err(exec_error(cmd, &r, None));
    }
    Ok(r)
}

fn native_session_id(text: &str) -> Option<String> {
    parse_jsonl(text).iter().find_map(|v| v["id"].as_str().map(String::from))
}

fn text_of(c: &Value) -> String {
    match c {
        Value::Array(parts) => {
            parts.iter().filter(|p| p["type"] == "text").map(|p| p["text"].as_str().unwrap_or("")).collect::<Vec<_>>().join(" ")
        }
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// Token totals the ATIF converter yields: Some((prompt, completion)) iff the session has steps.
pub fn session_totals(text: &str) -> Option<(i64, i64)> {
    let mut msgs: Vec<Value> = vec![];
    for l in text.trim().split('\n').map(str::trim).filter(|l| !l.is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        match v.get("messages").and_then(Value::as_array) {
            Some(m) if v.is_object() => msgs.extend(m.iter().cloned()),
            _ => msgs.push(v),
        }
    }
    let (mut steps, mut prompt, mut completion) = (0, 0, 0);
    for m in &msgs {
        match m["role"].as_str() {
            Some("user") if !text_of(&m["content"]).is_empty() => steps += 1,
            Some("assistant") => {
                if m["tool_calls"].as_array().is_some_and(|t| !t.is_empty()) || !text_of(&m["content"]).is_empty() {
                    steps += 1;
                }
                if let Some(u) = m["usage"].as_object().filter(|u| !u.is_empty()) {
                    prompt += u.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(0);
                    completion += u.get("completion_tokens").and_then(Value::as_i64).unwrap_or(0);
                }
            }
            _ => {}
        }
    }
    (steps > 0).then_some((prompt, completion))
}

fn step_time(m: &Value) -> Option<String> {
    atif::timestamp_of_millis((m["timestamp"].as_f64()? * 1000.0) as i64)
}

/// An assistant message as an agent step; `tool_calls` are OpenAI-shaped, as a JSON list or its text.
fn agent_step(m: &Value) -> atif::Step {
    let parsed = m["tool_calls"].as_str().and_then(|t| serde_json::from_str::<Value>(t).ok());
    let calls = parsed.as_ref().unwrap_or(&m["tool_calls"]).as_array().cloned().unwrap_or_default();
    let tool_calls: Vec<atif::ToolCall> = calls
        .iter()
        .map(|c| {
            let raw = c["function"]["arguments"].as_str().unwrap_or_default();
            atif::ToolCall {
                tool_call_id: c["id"].as_str().unwrap_or_default().into(),
                function_name: c["function"]["name"].as_str().unwrap_or_default().into(),
                arguments: serde_json::from_str(raw).unwrap_or_else(|_| json!(raw)),
                extra: None,
            }
        })
        .collect();
    atif::Step {
        source: "agent".into(),
        timestamp: step_time(m),
        message: json!(text_of(&m["content"])),
        reasoning_content: m["reasoning_content"].as_str().or(m["reasoning"].as_str()).filter(|r| !r.is_empty()).map(String::from),
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        ..Default::default()
    }
}

/// `hermes sessions export` -> ATIF. Tokens are kept on the session, not on messages, and `input_tokens` leaves out
/// cache reads and writes.
pub fn atif_trajectory(text: &str, version: Option<&str>, model: Option<&str>) -> Option<atif::Trajectory> {
    let session = text.lines().find_map(|l| serde_json::from_str::<Value>(l).ok().filter(|v| v["messages"].is_array()))?;
    let mut steps = vec![];
    let system = session["system_prompt"].as_str().filter(|p| !p.is_empty());
    steps.extend(system.map(|p| atif::Step { source: "system".into(), message: json!(p), ..Default::default() }));
    for m in session["messages"].as_array()? {
        match m["role"].as_str() {
            Some("user") if !text_of(&m["content"]).is_empty() => steps.push(atif::Step {
                source: "user".into(),
                timestamp: step_time(m),
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
    let agent = atif::Agent {
        name: "hermes".into(),
        version: version.unwrap_or("unknown").into(),
        model_name: model.or(session["model"].as_str()).map(String::from),
        ..Default::default()
    };
    let mut trajectory = atif::assemble(agent, session["id"].as_str().map(String::from), steps)?;
    let n = |k: &str| session[k].as_i64();
    let cached = n("cache_read_tokens");
    let prompt = n("input_tokens").map(|i| i + cached.unwrap_or(0) + n("cache_write_tokens").unwrap_or(0));
    atif::set_run_totals(&mut trajectory, prompt, n("output_tokens"), cached, session["estimated_cost_usd"].as_f64());
    Some(trajectory)
}

#[async_trait::async_trait]
impl Agent for Hermes {
    fn name(&self) -> &str {
        "hermes"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }
    fn setup_timeout_sec(&self) -> f64 {
        1200.0
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "git", "ripgrep", "xz"]).await?;
        // The `version` kwarg names a branch; the default pins a commit on main (release v0.22.2).
        let (script_ref, pin) = match &self.version {
            Some(v) => ("main", format!("--branch {v}")),
            None => (pins::HERMES.version, format!("--commit {}", pins::HERMES.version)),
        };
        let none = BTreeMap::new();
        exec_agent(
            cx,
            &format!(
                "set -euo pipefail; curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/{script_ref}/scripts/install.sh | bash -s -- --skip-setup {pin} && \
                 {PATH_EXPORT} && export HERMES_HOME=\"${{HERMES_HOME:-/tmp/hermes}}\" && \
                 mkdir -p \"$HERMES_HOME\" \"$HERMES_HOME/sessions\" \"$HERMES_HOME/skills\" \"$HERMES_HOME/memories\" && hermes version"
            ),
            &none,
        )
        .await?;
        if self.version.is_none() {
            let r = cx.exec(&format!("{PATH_EXPORT}; hermes version"), &none, None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(r.stdout.trim().to_string());
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        self.last_run_was_resume = resume;
        let instruction = self.args.render_instruction(instruction)?;
        let (mut env, flag, cli_model) = self.auth()?;
        env.insert("HBENCH_INSTRUCTION".into(), instruction);
        let yaml = Self::config_yaml(&cli_model, self.max_turns);
        exec_timeout(cx, &format!("mkdir -p /tmp/hermes && cat > /tmp/hermes/config.yaml << 'EOF'\n{yaml}EOF"), &env, 10).await?;
        if let Some(c) = self.mcp_command() {
            exec_timeout(cx, &c, &env, 10).await?;
        }
        if let Some(d) = skills_dir(&self.args) {
            exec_timeout(
                cx,
                &format!("mkdir -p /tmp/hermes/skills && cp -r {}/* /tmp/hermes/skills/ 2>/dev/null || true", quote(&d)),
                &env,
                10,
            )
            .await?;
        }
        let cmd = self.run_command(flag, &cli_model, resume)?;
        let r = exec_agent(cx, &cmd, &env).await;
        let export = "export PATH=\"$HOME/.local/bin:$PATH\" && (hermes sessions export /logs/agent/hermes-session.jsonl --source oneshot 2>/dev/null; \
            [ -s /logs/agent/hermes-session.jsonl ] || hermes sessions export /logs/agent/hermes-session.jsonl --source cli 2>/dev/null) && \
            head -n 1 /logs/agent/hermes-session.jsonl || true";
        let hh = BTreeMap::from([("HERMES_HOME".to_string(), "/tmp/hermes".to_string())]);
        if let Ok(e) = exec_timeout(cx, export, &hh, 30).await
            && let Some(id) = native_session_id(&e.stdout)
        {
            self.native_session_id = Some(id);
        }
        r.map(|_| ())
    }

    fn collect_usage(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        let Ok(text) = std::fs::read_to_string(logs_dir.join("hermes-session.jsonl")) else { return };
        if let Some(t) = atif_trajectory(&text, self.version.as_deref(), self.args.model_name.as_deref()) {
            atif::write_trajectory(logs_dir, &t);
        }
        let native = native_session_id(&text);
        let Some((p, c)) = session_totals(&text) else { return };
        let mut reported = self.reported.lock().unwrap();
        match &*reported {
            // Resumed run: report only this step's increment over the same session.
            Some((prev_id, pp, pc))
                if self.last_run_was_resume && (native.is_none() || prev_id.is_none() || native == *prev_id) && p >= *pp && c >= *pc =>
            {
                ctx.n_input_tokens = Some(p - pp);
                ctx.n_output_tokens = Some(c - pc);
            }
            _ => {
                ctx.n_input_tokens = Some(p);
                ctx.n_output_tokens = Some(c);
            }
        }
        *reported = Some((native, p, c));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<Hermes> {
        Hermes::new(common::test_args(kwargs, env, model))
    }

    #[test]
    fn auth_and_command() {
        let a =
            agent(json!({"toolsets": "terminal,file", "max_turns": 30}), &[("ANTHROPIC_TOKEN", "t")], Some("anthropic/claude-sonnet-4-6"))
                .unwrap();
        let (env, flag, model) = a.auth().unwrap();
        assert_eq!((env["ANTHROPIC_TOKEN"].as_str(), flag, model.as_str()), ("t", Some("anthropic"), "claude-sonnet-4-6"));
        let cmd = a.run_command(flag, &model, false).unwrap();
        assert_eq!(
            cmd,
            "export PATH=\"$HOME/.local/bin:$PATH\" && hermes --yolo chat -q \"$HBENCH_INSTRUCTION\" -Q --model claude-sonnet-4-6 --provider anthropic --toolsets terminal,file 2>&1 | stdbuf -oL tee /logs/agent/hermes.txt"
        );
        assert!(a.run_command(flag, &model, true).unwrap().contains("chat --resume latest -q"));
        let y = Hermes::config_yaml(&model, a.max_turns);
        assert!(y.contains("max_turns: 30") && y.contains("model: claude-sonnet-4-6") && y.contains("- hermes-cli"));
        assert!(!Hermes::config_yaml("m", None).contains("agent:"));

        let o = agent(json!({}), &[("OPENROUTER_API_KEY", "or")], Some("meta/llama-4")).unwrap();
        let (env, flag, model) = o.auth().unwrap();
        assert_eq!((env["OPENROUTER_API_KEY"].as_str(), flag, model.as_str()), ("or", None, "meta/llama-4"));
        assert!(agent(json!({}), &[], Some("bare")).unwrap().auth().is_err());
        assert!(agent(json!({"max_turns": "lots"}), &[], None).is_err());
        let p = agent(json!({"version": "v1"}), &[], Some("a/b")).unwrap();
        assert!(p.run_command(None, "b", true).is_err());
    }

    #[test]
    fn session_to_atif() {
        // The columns of the pinned hermes_state_common.py, as `sessions export` writes them.
        let session = json!({"id": "s1", "model": "m", "system_prompt": "be brief", "input_tokens": 100, "output_tokens": 20,
            "cache_read_tokens": 50, "cache_write_tokens": 5, "estimated_cost_usd": 0.5, "messages": [
            {"role": "user", "content": "task", "timestamp": 1793438597.125},
            {"role": "assistant", "content": "look", "reasoning_content": "think", "timestamp": 1793438598.0,
             "tool_calls": [{"id": "t1", "type": "function", "function": {"name": "terminal", "arguments": "{\"command\":\"ls\"}"}}]},
            {"role": "tool", "content": "a.py", "tool_call_id": "t1", "tool_name": "terminal", "timestamp": 1793438599.0},
            {"role": "assistant", "content": "done", "timestamp": 1793438600.0}
        ]});
        let a = atif_trajectory(&session.to_string(), Some("0.22.2"), None).unwrap();
        assert_eq!((a.session_id.as_deref(), a.agent.model_name.as_deref()), (Some("s1"), Some("m")));
        assert_eq!(
            a.steps.iter().map(|s| (s.step_id, s.source.as_str())).collect::<Vec<_>>(),
            [(1, "system"), (2, "user"), (3, "agent"), (4, "agent")]
        );
        assert_eq!(a.steps[1].timestamp.as_deref(), Some("2026-10-31T09:23:17.125Z"));
        let call = &a.steps[2];
        assert_eq!((call.message.as_str(), call.reasoning_content.as_deref()), (Some("look"), Some("think")));
        assert_eq!(call.tool_calls.as_ref().unwrap()[0].arguments, json!({"command": "ls"}));
        let result = &call.observation.as_ref().unwrap().results[0];
        assert_eq!((result.source_call_id.as_deref(), result.content.clone()), (Some("t1"), Some(json!("a.py"))));
        assert!(a.steps.iter().all(|s| s.metrics.is_none()));
        let f = a.final_metrics.unwrap();
        assert_eq!(
            (f.total_prompt_tokens, f.total_completion_tokens, f.total_cached_tokens, f.total_cost_usd),
            (Some(155), Some(20), Some(50), Some(0.5))
        );
        assert!(atif_trajectory("not json", None, None).is_none());
    }

    #[test]
    fn usage_and_resume_increment() {
        let s1 = json!({"id": "s1", "messages": [{"role": "user", "content": "First"}, {"role": "assistant", "content": "A", "usage": {"prompt_tokens": 10, "completion_tokens": 5}}]}).to_string();
        assert_eq!(session_totals(&s1), Some((10, 5)));
        assert_eq!(session_totals(""), None);
        let mut a = agent(json!({}), &[], Some("anthropic/x")).unwrap();
        let dir = a.args.logs_dir.clone();
        std::fs::write(dir.join("hermes-session.jsonl"), &s1).unwrap();
        let mut c = AgentContext::default();
        a.collect_usage(&dir, &mut c);
        assert_eq!((c.n_input_tokens, c.n_output_tokens), (Some(10), Some(5)));
        let s2 = json!({"id": "s1", "messages": [{"role": "assistant", "content": "A", "usage": {"prompt_tokens": 10, "completion_tokens": 5}},
            {"role": "assistant", "content": "", "tool_calls": [{"id": "t", "function": {"name": "terminal", "arguments": "{}"}}], "usage": {"prompt_tokens": 30, "completion_tokens": 7}}]})
        .to_string();
        std::fs::write(dir.join("hermes-session.jsonl"), &s2).unwrap();
        a.last_run_was_resume = true;
        let mut c = AgentContext::default();
        a.collect_usage(&dir, &mut c);
        assert_eq!((c.n_input_tokens, c.n_output_tokens), (Some(30), Some(7)));
    }
}
