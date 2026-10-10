//! aider (port of upstream's agents/installed/aider.py).

use super::{connection, ensure_system_dependencies, exec_agent};
use crate::agents::model_connection::{Spec, provider};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use std::collections::BTreeMap;

const SPEC: Spec = Spec { default_provider: None, api_key_envs: &[], base_url_envs: &[], passthrough: true };

/// Commit task files first so aider's git-based edits don't sweep them into its own commits.
const SNAPSHOT_UNTRACKED_WORKSPACE: &str = "if command -v git >/dev/null 2>&1 \
    && ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then \
    { git init -q && timeout 120 git add -A \
    && git -c user.name=hbench -c user.email=hbench@localhost \
    commit -q --no-verify --allow-empty -m 'Task files before agent run'; } \
    || echo 'hbench: could not snapshot the workspace for aider' >&2; \
    fi; ";

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    opts::check_known(&args, "aider", &["reasoning_effort", "thinking_tokens", "cache_prompts", "auto_lint", "auto_test", "test_cmd", "stream", "map_tokens"], false)?;
    let a = Aider { version: args.version_kw(), args };
    a.cli_flags()?;
    Ok(Box::new(a))
}

pub struct Aider {
    args: AgentArgs,
    version: Option<String>,
}

impl Aider {
    /// AiderOptions -> CLI flags (field order).
    fn cli_flags(&self) -> Result<String> {
        let a = &self.args;
        let mut f = vec![];
        if let Some(v) = opts::string(a, "reasoning_effort", None)? {
            f.push(format!("--reasoning-effort {}", quote(&v)));
        }
        if let Some(n) = opts::int(a, "thinking_tokens", None)? {
            f.push(format!("--thinking-tokens {n}"));
        }
        for (k, flag) in [("cache_prompts", "--cache-prompts"), ("auto_lint", "--auto-lint"), ("auto_test", "--auto-test")] {
            if opts::boolean(a, k, None)? == Some(true) {
                f.push(flag.into());
            }
        }
        if let Some(v) = opts::string(a, "test_cmd", None)? {
            f.push(format!("--test-cmd {}", quote(&v)));
        }
        if opts::boolean(a, "stream", None)? == Some(true) {
            f.push("--stream".into());
        }
        if let Some(n) = opts::int(a, "map_tokens", None)? {
            f.push(format!("--map-tokens {n}"));
        }
        Ok(f.join(" "))
    }

    /// (env, model) for the run; upstream raises these at run time.
    fn run_env(&self) -> Result<(BTreeMap<String, String>, String)> {
        let Some((prov, model)) = self.args.model_name.as_deref().and_then(|m| m.split_once('/')) else {
            bail!("Model name must be in the format provider/model_name")
        };
        let conn = connection(&self.args, SPEC);
        let prov = conn.provider.clone().unwrap_or_else(|| prov.to_string());
        if !["anthropic", "openai"].contains(&prov.as_str()) {
            bail!("Unsupported provider: {prov}");
        }
        let Some(key) = conn.api_key.clone().filter(|k| !k.is_empty()) else { bail!("No API key found for provider: {prov}") };
        // Aider injects AIDER_API_KEY itself; keep URL/extras only.
        let skip = provider(&prov).map(|p| p.api_key_envs).unwrap_or(&[]);
        let mut env: BTreeMap<String, String> = conn.env.into_iter().filter(|(k, _)| !skip.contains(&k.as_str())).collect();
        env.insert("AIDER_API_KEY".into(), format!("{prov}={key}"));
        Ok((env, model.to_string()))
    }
}

fn parse_aider_version(stdout: &str) -> String {
    let text = stdout.trim();
    text.lines().map(str::trim).find(|l| !l.is_empty()).map(|l| l.strip_prefix("aider").unwrap_or(l).trim().to_string()).unwrap_or_else(|| text.to_string())
}

#[async_trait::async_trait]
impl Agent for Aider {
    fn name(&self) -> &str {
        "aider"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        opts::installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "git", "coreutils"]).await?;
        exec_agent(
            cx,
            "set -euo pipefail; curl -LsSf https://aider.chat/install.sh | sh && \
             if [ -f \"$HOME/.local/bin/env\" ]; then source \"$HOME/.local/bin/env\"; fi && aider --version",
            &BTreeMap::new(),
        )
        .await?;
        if self.version.is_none() {
            self.version = opts::detect_version(cx, ". $HOME/.local/bin/env; aider --version", parse_aider_version).await;
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let (env, model) = self.run_env()?;
        let flags = self.cli_flags()?;
        let extra = if flags.is_empty() { String::new() } else { format!("{flags} ") };
        let restore = if resume { "--restore-chat-history " } else { "" };
        let cmd = format!(
            ". $HOME/.local/bin/env; {SNAPSHOT_UNTRACKED_WORKSPACE}aider --yes --chat-history-file=/logs/agent/aider.chat.history.md \
             {restore}{extra}--model={model} --message={} 2>&1 | stdbuf -oL tee /logs/agent/aider.txt",
            quote(&instruction)
        );
        exec_agent(cx, &cmd, &env).await?;
        Ok(())
    }
}

/// Option parsing shared by the installed-agent ports in this crate
/// (pydantic lax-mode coercion of upstream's `options_model` fields).
pub(super) mod opts {
    use crate::agents::{AgentArgs, AgentCtx};
    use anyhow::{Result, anyhow, bail};
    use serde_json::Value;
    use std::collections::BTreeMap;

    /// kwargs value (explicit null = opt-out), else the `fallback` env var.
    fn raw(a: &AgentArgs, k: &str, fallback: Option<&str>) -> Option<Value> {
        match a.kwargs.get(k) {
            Some(Value::Null) => None,
            Some(v) => Some(v.clone()),
            None => fallback.and_then(|e| a.get_env(e)).map(Value::String),
        }
    }

    pub fn string(a: &AgentArgs, k: &str, fallback: Option<&str>) -> Result<Option<String>> {
        Ok(match raw(a, k, fallback) {
            None => None,
            Some(Value::String(s)) => Some(s),
            Some(Value::Number(n)) => Some(n.to_string()),
            Some(v) => bail!("Invalid value for '{k}': expected a string, got {v}"),
        })
    }

    pub fn int(a: &AgentArgs, k: &str, fallback: Option<&str>) -> Result<Option<i64>> {
        let err = |v: &Value| anyhow!("Invalid value for '{k}': cannot parse '{}' as int", v.as_str().map(String::from).unwrap_or_else(|| v.to_string()));
        Ok(match raw(a, k, fallback) {
            None => None,
            Some(v @ Value::Number(_)) => Some(v.as_i64().or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)).ok_or_else(|| err(&v))?),
            Some(v @ Value::String(_)) => Some(v.as_str().unwrap().trim().parse().map_err(|_| err(&v))?),
            Some(v) => return Err(err(&v)),
        })
    }

    pub fn float(a: &AgentArgs, k: &str, fallback: Option<&str>) -> Result<Option<f64>> {
        Ok(match raw(a, k, fallback) {
            None => None,
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::String(s)) => Some(s.trim().parse().map_err(|_| anyhow!("Invalid value for '{k}': cannot parse '{s}' as float"))?),
            Some(v) => bail!("Invalid value for '{k}': expected a number, got {v}"),
        })
    }

    pub fn boolean(a: &AgentArgs, k: &str, fallback: Option<&str>) -> Result<Option<bool>> {
        let v = raw(a, k, fallback);
        let parsed = match &v {
            None => return Ok(None),
            Some(Value::Bool(b)) => Some(*b),
            Some(Value::Number(n)) => match n.as_f64() {
                Some(0.0) => Some(false),
                Some(1.0) => Some(true),
                _ => None,
            },
            Some(Value::String(s)) => match s.trim().to_lowercase().as_str() {
                "1" | "on" | "t" | "true" | "y" | "yes" => Some(true),
                "0" | "off" | "f" | "false" | "n" | "no" => Some(false),
                _ => None,
            },
            _ => None,
        };
        parsed.map(Some).ok_or_else(|| anyhow!("Invalid value for '{k}': {} is not a valid boolean", v.unwrap()))
    }

    /// `Literal[...]` field: exact string match.
    pub fn choice(a: &AgentArgs, k: &str, fallback: Option<&str>, choices: &[&str]) -> Result<Option<String>> {
        match raw(a, k, fallback) {
            None => Ok(None),
            Some(Value::String(s)) if choices.contains(&s.as_str()) => Ok(Some(s)),
            Some(v) => {
                let mut c = choices.to_vec();
                c.sort();
                bail!("Invalid value for '{k}': '{}'. Valid values: {}", v.as_str().map(String::from).unwrap_or_else(|| v.to_string()), c.join(", "))
            }
        }
    }

    /// upstream's options models forbid unknown kwargs; `config` needs `native_config`.
    pub fn check_known(a: &AgentArgs, agent: &str, known: &[&str], native_config: bool) -> Result<()> {
        for k in a.kwargs.keys() {
            if !known.contains(&k.as_str()) && !["version", "prompt_template_path", "config"].contains(&k.as_str()) {
                bail!("Invalid kwargs for agent '{agent}':\n  Unknown option '{k}'.\nSee hbench agent schema {agent} for available options.");
            }
        }
        if !native_config && a.kw("config").is_some() {
            bail!("Agent '{agent}' does not support config. You can implement it.");
        }
        Ok(())
    }

    /// Skills directory inside the environment (task `skills_dir`, or uploaded `--skill`s).
    pub fn skills_dir(a: &AgentArgs) -> Option<String> {
        a.task.config.environment.skills_dir.clone().or_else(|| (!a.skills.is_empty()).then(|| "/hbench/skills".into()))
    }

    /// Python `str(float)`.
    pub fn py_float(f: f64) -> String {
        if f.is_finite() && f.fract() == 0.0 { format!("{f:.1}") } else { f.to_string() }
    }

    /// BaseInstalledAgent.setup prelude.
    pub async fn installed_dir(cx: &AgentCtx<'_>) -> Result<()> {
        cx.env.exec_root("[ -d /installed-agent ] || mkdir -p /installed-agent", None).await?;
        Ok(())
    }

    /// Best-effort version detection after install (`get_version_command` + `parse_version`).
    pub async fn detect_version(cx: &AgentCtx<'_>, cmd: &str, parse: impl Fn(&str) -> String) -> Option<String> {
        let r = cx.exec(cmd, &BTreeMap::new(), None).await.ok()?;
        (r.code == 0 && !r.stdout.is_empty()).then(|| parse(&r.stdout))
    }

    #[cfg(test)]
    pub fn test_args(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> AgentArgs {
        let dir = std::env::temp_dir().join(format!("hbench-agent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("environment")).unwrap();
        std::fs::write(dir.join("task.toml"), "[environment]\ndocker_image='x'\n").unwrap();
        std::fs::write(dir.join("instruction.md"), "x").unwrap();
        let task = std::sync::Arc::new(crate::task::Task::load(&dir).unwrap());
        AgentArgs { load_trajectory: None,
            model_name: model.map(String::from),
            kwargs: serde_json::from_value(kwargs).unwrap(),
            extra_env: env.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            mcp_servers: vec![],
            skills: vec![],
            logs_dir: dir,
            task,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent(kwargs: serde_json::Value, env: &[(&str, &str)], model: Option<&str>) -> Result<Aider> {
        let args = opts::test_args(kwargs, env, model);
        opts::check_known(&args, "aider", &["reasoning_effort", "thinking_tokens", "cache_prompts", "auto_lint", "auto_test", "test_cmd", "stream", "map_tokens"], false)?;
        let a = Aider { version: None, args };
        a.cli_flags()?;
        Ok(a)
    }

    #[test]
    fn flags_env_and_validation() {
        let a = agent(json!({"reasoning_effort": "high", "thinking_tokens": "2048", "cache_prompts": true, "auto_lint": false, "test_cmd": "pytest -q", "map_tokens": 1024}), &[("OPENAI_API_KEY", "sk-o")], Some("openai/gpt-4o")).unwrap();
        assert_eq!(a.cli_flags().unwrap(), "--reasoning-effort high --thinking-tokens 2048 --cache-prompts --test-cmd 'pytest -q' --map-tokens 1024");
        let (env, model) = a.run_env().unwrap();
        assert_eq!(model, "gpt-4o");
        assert_eq!(env["AIDER_API_KEY"], "openai=sk-o");
        assert!(!env.contains_key("OPENAI_API_KEY"));
        assert!(agent(json!({"thinking_tokens": "lots"}), &[], None).is_err());
        assert!(agent(json!({"bogus": 1}), &[], None).is_err());
        assert!(agent(json!({"config": {"a": 1}}), &[], None).is_err());
        let g = agent(json!({}), &[], Some("google/gemini-x")).unwrap();
        assert!(g.run_env().unwrap_err().to_string().contains("Unsupported provider: google"));
        assert!(agent(json!({}), &[], Some("gpt-4o")).unwrap().run_env().is_err());
    }

    #[test]
    fn version_parsing() {
        assert_eq!(parse_aider_version("\naider 0.86.1\n"), "0.86.1");
        assert_eq!(opts::py_float(1.0), "1.0");
        assert_eq!(opts::py_float(0.25), "0.25");
    }
}
