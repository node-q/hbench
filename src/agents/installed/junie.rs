//! Junie (port of upstream's agents/installed/junie.py).

use super::{ensure_system_dependencies, exec_agent};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const PATH_EXPORT: &str = "export PATH=\"$HOME/.local/bin:$PATH\"; ";
const ARTIFACTS: &str = "/logs/agent/junie";

/// upstream model prefix -> Junie BYOK provider.
fn provider_alias(p: &str) -> Option<&'static str> {
    Some(match p {
        "anthropic" | "claude" => "anthropic",
        "openai" | "gpt" => "openai",
        "google" | "gemini" => "google",
        "xai" | "grok" => "xai",
        "openrouter" => "openrouter",
        _ => return None,
    })
}

/// Junie provider -> (env Junie reads, host fallbacks).
fn provider_key_env(p: &str) -> (&'static str, &'static [&'static str]) {
    match p {
        "anthropic" => ("JUNIE_ANTHROPIC_API_KEY", &["ANTHROPIC_API_KEY"]),
        "openai" => ("JUNIE_OPENAI_API_KEY", &["OPENAI_API_KEY"]),
        "google" => ("JUNIE_GOOGLE_API_KEY", &["GOOGLE_API_KEY", "GEMINI_API_KEY"]),
        "xai" => ("JUNIE_GROK_API_KEY", &["XAI_API_KEY", "GROK_API_KEY"]),
        _ => ("JUNIE_OPENROUTER_API_KEY", &["OPENROUTER_API_KEY"]),
    }
}

enum ConfigSource {
    Path(PathBuf),
    Inline(Map<String, Value>),
}

pub struct Junie {
    args: AgentArgs,
    version: Option<String>,
    flags: String,
    channel: &'static str,
    brave: bool,
    share_anonymous_statistics: bool,
    json_output: bool,
    config: Option<ConfigSource>,
    session_id: Option<String>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(Junie::new(args)?))
}

/// Pydantic lax bool.
fn opt_bool(args: &AgentArgs, k: &str, default: bool) -> Result<bool> {
    match args.kwargs.get(k) {
        None => Ok(default),
        Some(v) => match v {
            Value::Bool(b) => Some(*b),
            Value::Number(n) => match n.as_f64() {
                Some(0.0) => Some(false),
                Some(1.0) => Some(true),
                _ => None,
            },
            Value::String(s) => match s.trim().to_lowercase().as_str() {
                "true" | "1" | "yes" | "on" | "t" | "y" => Some(true),
                "false" | "0" | "no" | "off" | "f" | "n" => Some(false),
                _ => None,
            },
            _ => None,
        }
        .ok_or_else(|| anyhow!("Invalid kwargs for agent 'junie':\n  {k}: Input should be a valid boolean")),
    }
}

impl Junie {
    fn new(args: AgentArgs) -> Result<Junie> {
        let channel = match args.kw("channel").as_deref() {
            None | Some("stable") => "stable",
            Some("eap") => "eap",
            Some(c) => bail!("Invalid value for 'channel': '{c}'. Valid values: eap, stable"),
        };
        let config = match args.kwargs.get("config") {
            None | Some(Value::Null) => None,
            Some(Value::Object(o)) => Some(ConfigSource::Inline(o.clone())),
            Some(Value::String(p)) => {
                let p = PathBuf::from(p);
                if !p.is_file() {
                    bail!("Agent config file not found: {}", p.display());
                }
                Some(ConfigSource::Path(p))
            }
            Some(_) => bail!("Invalid value for 'config': expected a local file path or JSON object"),
        };
        let mut f = vec![];
        if let Some(v) = args.kw("timeout_ms") {
            let n: i64 = v.trim().parse().map_err(|_| anyhow!("Invalid kwargs for agent 'junie':\n  timeout_ms: Input should be a valid integer"))?;
            f.push(format!("--timeout {n}"));
        }
        if let Some(v) = args.kw("guidelines_filename") {
            f.push(format!("--guidelines-filename {}", quote(&v)));
        }
        if let Some(v) = args.kw("output_format") {
            if !["text", "json"].contains(&v.as_str()) {
                bail!("Invalid kwargs for agent 'junie':\n  output_format: Input should be 'text' or 'json'");
            }
            f.push(format!("--output-format {v}"));
        }
        Ok(Junie {
            version: args.version_kw(),
            flags: f.join(" "),
            channel,
            brave: opt_bool(&args, "brave", true)?,
            share_anonymous_statistics: opt_bool(&args, "share_anonymous_statistics", false)?,
            json_output: opt_bool(&args, "json_output", false)?,
            config,
            session_id: None,
            args,
        })
    }

    fn skills_dir(&self) -> Option<String> {
        self.args.task.config.environment.skills_dir.clone().or_else(|| (!self.args.skills.is_empty()).then(|| "/hbench/skills".into()))
    }

    fn build_config(&self) -> Result<Value> {
        let mut c = Map::new();
        c.insert("brave".into(), json!(self.brave));
        c.insert("auto-update".into(), json!(false));
        match &self.config {
            Some(ConfigSource::Path(p)) => {
                let raw = std::fs::read_to_string(p).map_err(|e| anyhow!("Invalid Junie config file {}: {e}", p.display()))?;
                let v: Value = serde_json::from_str(&raw).map_err(|e| anyhow!("Invalid Junie config file {}: {e}", p.display()))?;
                let Value::Object(o) = v else { bail!("Junie config file must contain a JSON object: {}", p.display()) };
                c.extend(o);
            }
            Some(ConfigSource::Inline(o)) => c.extend(o.clone()),
            None => {}
        }
        Ok(Value::Object(c))
    }

    fn resolve_provider(&self) -> Result<Option<&'static str>> {
        let Some((p, _)) = self.args.model_name.as_deref().and_then(|m| m.split_once('/')) else { return Ok(None) };
        let p = p.to_lowercase();
        match provider_alias(&p) {
            Some(j) => Ok(Some(j)),
            None => bail!(
                "Unsupported BYOK provider '{p}' for Junie. Supported: anthropic, claude, gemini, google, gpt, grok, openai, openrouter, xai. \
                 Pass a bare model name (no 'provider/' prefix) to use JetBrains-hosted models."
            ),
        }
    }

    fn auth_env(&self, provider: Option<&str>) -> Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::new();
        if let Some(k) = self.args.get_env("JUNIE_API_KEY") {
            env.insert("JUNIE_API_KEY".to_string(), k);
        }
        if let Some(p) = provider {
            let (name, fallbacks) = provider_key_env(p);
            let all: Vec<&str> = std::iter::once(name).chain(fallbacks.iter().copied()).collect();
            let Some(v) = all.iter().find_map(|n| self.args.get_env(n)) else {
                bail!("Junie needs a {p} API key for model '{}'. Set one of: {}.", self.args.model_name.as_deref().unwrap_or(""), all.join(", "));
            };
            env.insert(name.into(), v);
            env.insert("JUNIE_LLM_PROVIDER".into(), p.into());
        }
        if env.is_empty() {
            bail!(
                "Junie requires authentication. Set JUNIE_API_KEY (generate one at https://junie.jetbrains.com/cli), or select a BYOK model such as \
                 'anthropic/sonnet' and set the matching provider API key."
            );
        }
        Ok(env)
    }

    fn run_command(&self, instruction: &str, resume: bool) -> String {
        let mut parts = vec!["junie".to_string(), "--skip-update-check".into()];
        if let Some(m) = self.args.model_name.as_deref().filter(|m| !m.is_empty()) {
            parts.extend(["--model".into(), quote(m.split_once('/').map(|(_, r)| r).unwrap_or(m))]);
        }
        if self.json_output {
            parts.extend(["--json-output-file".into(), "/logs/agent/junie-output.json".into()]);
        }
        if resume && let Some(s) = &self.session_id {
            parts.extend(["--session-id".into(), quote(s)]);
        }
        if !self.flags.is_empty() {
            parts.push(self.flags.clone());
        }
        parts.extend(["--task".into(), quote(instruction)]);
        format!("{PATH_EXPORT}{} 2>&1 | stdbuf -oL tee /logs/agent/junie.txt", parts.join(" "))
    }
}

fn write_json_cmd(remote_path: &str, payload: &Value) -> String {
    format!(
        "mkdir -p \"$(dirname \"{remote_path}\")\" && printf '%s' {} > \"{remote_path}\"",
        quote(&serde_json::to_string_pretty(payload).unwrap())
    )
}

fn num(v: &Value) -> i64 {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).unwrap_or(0)
}

/// Whole-session usage over `LlmResponseMetadataEvent` records; None when no LLM step exists.
pub fn usage_from_events(text: &str) -> Option<AgentContext> {
    let (mut steps, mut prompt, mut completion, mut cached, mut cost) = (0, 0, 0, 0, None::<f64>);
    for e in text.lines().filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok()) {
        let a = &e["event"]["agentEvent"];
        if a["kind"] != "LlmResponseMetadataEvent" {
            continue;
        }
        let usages: Vec<&Value> = a["modelUsage"].as_array().map(|u| u.iter().filter(|x| x.is_object()).collect()).unwrap_or_default();
        if usages.is_empty() {
            continue;
        }
        steps += 1;
        for u in usages {
            prompt += num(&u["inputTokens"]);
            completion += num(&u["outputTokens"]);
            cached += num(&u["cacheInputTokens"]) + num(&u["cacheCreateTokens"]);
            if let Some(c) = u["cost"].as_f64() {
                cost = Some(cost.unwrap_or(0.0) + c);
            }
        }
    }
    (steps > 0).then(|| AgentContext { n_input_tokens: Some(prompt), n_output_tokens: Some(completion), n_cache_tokens: Some(cached), cost_usd: cost, ..Default::default() })
}

fn find_events(dir: &Path) -> Option<PathBuf> {
    let direct = dir.join("junie/session/events.jsonl");
    if direct.is_file() {
        return Some(direct);
    }
    fn walk(d: &Path) -> Option<PathBuf> {
        for e in std::fs::read_dir(d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(f) = walk(&p) {
                    return Some(f);
                }
            } else if p.file_name().is_some_and(|n| n == "events.jsonl") {
                return Some(p);
            }
        }
        None
    }
    walk(&dir.join("junie"))
}

#[async_trait::async_trait]
impl Agent for Junie {
    fn name(&self) -> &str {
        "junie"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        cx.env.exec_root("[ -d /installed-agent ] || mkdir -p /installed-agent", None).await?;
        ensure_system_dependencies(cx, &["curl", "bash", "unzip"]).await?;
        let url = if self.channel == "eap" { "https://junie.jetbrains.com/install-eap.sh" } else { "https://junie.jetbrains.com/install.sh" };
        let none = BTreeMap::new();
        exec_agent(
            cx,
            &format!(
                "set -euo pipefail; curl -fsSL {url} | bash && {PATH_EXPORT}if [ ! -x \"$HOME/.local/bin/junie\" ]; then \
                 echo \"Junie launcher not found at $HOME/.local/bin/junie\" >&2; exit 1; fi && junie --version"
            ),
            &none,
        )
        .await?;
        exec_agent(cx, &write_json_cmd("$HOME/.junie/settings.json", &json!({"shareAnonymousStatistics": self.share_anonymous_statistics})), &none).await?;
        let config = self.build_config().map_err(|e| anyhow!("Agent install failed: {e}"))?;
        exec_agent(cx, &write_json_cmd("$HOME/.junie/config.json", &config), &none).await?;
        if self.version.is_none() {
            let r = cx.exec(&format!("{PATH_EXPORT}junie --version"), &none, None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = r.stdout.lines().map(str::trim).filter(|l| !l.is_empty()).last().map(String::from).or_else(|| Some(r.stdout.trim().into()));
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let env = self.auth_env(self.resolve_provider()?)?;
        let none = BTreeMap::new();
        if !self.args.mcp_servers.is_empty() {
            let mut servers = Map::new();
            for s in &self.args.mcp_servers {
                let v = if s.transport() == "stdio" { json!({"command": s.command, "args": s.args}) } else { json!({"url": s.url}) };
                servers.insert(s.name.clone(), v);
            }
            exec_agent(cx, &write_json_cmd("$HOME/.junie/mcp/mcp.json", &json!({"mcpServers": servers})), &none).await?;
        }
        if let Some(d) = self.skills_dir() {
            let d = quote(&d);
            exec_agent(cx, &format!("if [ -d {d} ]; then mkdir -p \"$HOME/.junie/skills\" && cp -r {d}/* \"$HOME/.junie/skills/\" 2>/dev/null || true; fi"), &none).await?;
        }
        let r = exec_agent(cx, &self.run_command(&instruction, resume), &env).await;
        // Collect the newest session + logs even when Junie failed.
        let collect = format!(
            "mkdir -p {ARTIFACTS}; junie_home=\"$HOME/.junie\"; if [ -d \"$junie_home/logs\" ]; then rm -rf {ARTIFACTS}/logs; \
             cp -r \"$junie_home/logs\" {ARTIFACTS}/logs 2>/dev/null || true; fi; \
             session_dir=\"$(ls -1dt \"$junie_home\"/sessions/*/ 2>/dev/null | head -n 1)\"; if [ -n \"$session_dir\" ]; then \
             rm -rf {ARTIFACTS}/session; cp -r \"$session_dir\" {ARTIFACTS}/session 2>/dev/null || true; basename \"$session_dir\"; fi"
        );
        if let Ok(c) = cx.exec(&collect, &none, None).await
            && c.code == 0
            && let Some(id) = c.stdout.trim().lines().last()
        {
            self.session_id = Some(id.to_string());
        }
        r.map(|_| ())
    }

    fn populate_context_post_run(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        if let Some(c) = find_events(logs_dir).and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| usage_from_events(&t)) {
            *ctx = c;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<Junie> {
        let dir = std::env::temp_dir().join(format!("hbench-junie-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("environment")).unwrap();
        std::fs::write(dir.join("task.toml"), "[environment]\ndocker_image='x'\n").unwrap();
        std::fs::write(dir.join("instruction.md"), "x").unwrap();
        let task = Arc::new(crate::task::Task::load(&dir).unwrap());
        Junie::new(AgentArgs { load_trajectory: None,
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
    fn options_auth_and_command() {
        let mut a = agent(json!({"timeout_ms": 5000, "output_format": "json", "json_output": true, "config": {"brave": false, "x": 1}}), &[("ANTHROPIC_API_KEY", "ak")], Some("claude/sonnet")).unwrap();
        let p = a.resolve_provider().unwrap();
        let env = a.auth_env(p).unwrap();
        assert_eq!((env["JUNIE_ANTHROPIC_API_KEY"].as_str(), env["JUNIE_LLM_PROVIDER"].as_str()), ("ak", "anthropic"));
        a.session_id = Some("s-1".into());
        assert_eq!(
            a.run_command("fix it", true),
            "export PATH=\"$HOME/.local/bin:$PATH\"; junie --skip-update-check --model sonnet --json-output-file /logs/agent/junie-output.json \
             --session-id s-1 --timeout 5000 --output-format json --task 'fix it' 2>&1 | stdbuf -oL tee /logs/agent/junie.txt"
        );
        assert!(!a.run_command("x", false).contains("--session-id"));
        assert_eq!(a.build_config().unwrap(), json!({"brave": false, "auto-update": false, "x": 1}));

        assert!(agent(json!({"channel": "beta"}), &[], None).is_err());
        assert!(agent(json!({"output_format": "xml"}), &[], None).is_err());
        assert!(agent(json!({"config": "/nonexistent/junie.json"}), &[], None).is_err());
        let b = agent(json!({}), &[], Some("mistral/large")).unwrap();
        assert!(b.resolve_provider().is_err());
        let c = agent(json!({}), &[], Some("sonnet")).unwrap();
        assert!(c.auth_env(c.resolve_provider().unwrap()).unwrap_err().to_string().contains("requires authentication"));
        let d = agent(json!({}), &[("JUNIE_API_KEY", "jk")], Some("sonnet")).unwrap();
        assert_eq!(d.auth_env(None).unwrap()["JUNIE_API_KEY"], "jk");
    }

    #[test]
    fn usage_from_session_events() {
        let llm = |model: &str, cost: Option<f64>| {
            let mut u = json!({"model": model, "inputTokens": 100, "outputTokens": 20, "cacheInputTokens": 5, "cacheCreateTokens": 3});
            if let Some(c) = cost {
                u["cost"] = json!(c);
            }
            json!({"kind": "SessionEvent", "timestampMs": 1, "event": {"agentEvent": {"kind": "LlmResponseMetadataEvent", "modelUsage": [u]}}}).to_string()
        };
        let status = json!({"kind": "SessionEvent", "event": {"agentEvent": {"kind": "AgentCurrentStatusUpdatedEvent"}}}).to_string();
        let text = [status.clone(), llm("a", Some(0.5)), llm("b", None)].join("\n");
        let c = usage_from_events(&text).unwrap();
        assert_eq!((c.n_input_tokens, c.n_output_tokens, c.n_cache_tokens, c.cost_usd), (Some(200), Some(40), Some(16), Some(0.5)));
        assert_eq!(usage_from_events(&llm("a", None)).unwrap().cost_usd, None);
        assert!(usage_from_events(&status).is_none());

        let a = agent(json!({}), &[], None).unwrap();
        let dir = a.args.logs_dir.join("junie/session");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("events.jsonl"), &text).unwrap();
        let mut ctx = AgentContext::default();
        a.populate_context_post_run(&a.args.logs_dir, &mut ctx);
        assert_eq!(ctx.n_input_tokens, Some(200));
    }
}
