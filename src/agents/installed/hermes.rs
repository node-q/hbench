//! Hermes (port of upstream's agents/installed/hermes.py).

use super::{ensure_system_dependencies, exec_agent, exec_error};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

const PATH_EXPORT: &str = "export PATH=\"$HOME/.local/bin:$PATH\"";

/// upstream provider prefix -> (hermes --provider flag, key env names). Others route via OpenRouter.
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
            .map(|v| {
                v.trim()
                    .parse::<i64>()
                    .map_err(|_| anyhow!("Invalid kwargs for agent 'hermes':\n  max_turns: Input should be a valid integer"))
            })
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

    fn skills_dir(&self) -> Option<String> {
        self.args.task.config.environment.skills_dir.clone().or_else(|| (!self.args.skills.is_empty()).then(|| "/hbench/skills".into()))
    }

    /// (env, --provider flag, cli model) for the configured model.
    fn auth(&self) -> Result<(BTreeMap<String, String>, Option<&'static str>, String)> {
        let Some((provider, model)) = self.args.model_name.as_deref().and_then(|m| m.split_once('/')) else {
            bail!("Model name must be in the format provider/model_name");
        };
        let mut env = BTreeMap::from([("HERMES_HOME".to_string(), "/tmp/hermes".to_string()), ("TERMINAL_ENV".into(), "local".into())]);
        let mut flag = None;
        let native = native_provider(provider);
        if let Some((native_flag, keys)) = native {
            // upstream's _resolve_env: agent env first, then host env, first name wins per source.
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
    text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).find_map(|v| v["id"].as_str().map(String::from))
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

/// Token totals upstream's ATIF converter yields: Some((prompt, completion)) iff the session has steps.
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
        cx.env.exec_root("[ -d /installed-agent ] || mkdir -p /installed-agent", None).await?;
        ensure_system_dependencies(cx, &["curl", "git", "ripgrep", "xz"]).await?;
        let branch = self.version.as_ref().map(|v| format!(" --branch {v}")).unwrap_or_default();
        let none = BTreeMap::new();
        exec_agent(
            cx,
            &format!(
                "set -euo pipefail; curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh | bash -s -- --skip-setup{branch} && \
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
        if let Some(d) = self.skills_dir() {
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

    fn populate_context_post_run(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        let Ok(text) = std::fs::read_to_string(logs_dir.join("hermes-session.jsonl")) else { return };
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
    use std::sync::Arc;

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<Hermes> {
        let dir = std::env::temp_dir().join(format!("hbench-hermes-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("environment")).unwrap();
        std::fs::write(dir.join("task.toml"), "[environment]\ndocker_image='x'\n").unwrap();
        std::fs::write(dir.join("instruction.md"), "x").unwrap();
        let task = Arc::new(crate::task::Task::load(&dir).unwrap());
        Hermes::new(AgentArgs {
            load_trajectory: None,
            model_name: model.map(String::from),
            kwargs: serde_json::from_value(kwargs).unwrap(),
            extra_env: env.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            mcp_servers: vec![],
            skills: vec![],
            logs_dir: dir,
            task,
        })
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
    fn usage_and_resume_increment() {
        let s1 = json!({"id": "s1", "messages": [{"role": "user", "content": "First"}, {"role": "assistant", "content": "A", "usage": {"prompt_tokens": 10, "completion_tokens": 5}}]}).to_string();
        assert_eq!(session_totals(&s1), Some((10, 5)));
        assert_eq!(session_totals(""), None);
        let mut a = agent(json!({}), &[], Some("anthropic/x")).unwrap();
        let dir = a.args.logs_dir.clone();
        std::fs::write(dir.join("hermes-session.jsonl"), &s1).unwrap();
        let mut c = AgentContext::default();
        a.populate_context_post_run(&dir, &mut c);
        assert_eq!((c.n_input_tokens, c.n_output_tokens), (Some(10), Some(5)));
        let s2 = json!({"id": "s1", "messages": [{"role": "assistant", "content": "A", "usage": {"prompt_tokens": 10, "completion_tokens": 5}},
            {"role": "assistant", "content": "", "tool_calls": [{"id": "t", "function": {"name": "terminal", "arguments": "{}"}}], "usage": {"prompt_tokens": 30, "completion_tokens": 7}}]})
        .to_string();
        std::fs::write(dir.join("hermes-session.jsonl"), &s2).unwrap();
        a.last_run_was_resume = true;
        let mut c = AgentContext::default();
        a.populate_context_post_run(&dir, &mut c);
        assert_eq!((c.n_input_tokens, c.n_output_tokens), (Some(30), Some(7)));
    }
}
