//! aider.

use super::common::{detect_version, installed_dir, opts, uv_install};
use super::pins;
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
    let a = Aider { version: args.version_kw(), args };
    a.cli_flags()?;
    Ok(Box::new(a))
}

pub struct Aider {
    args: AgentArgs,
    version: Option<String>,
}

impl Aider {
    /// The aider options as CLI flags, in schema order.
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

    /// (env, model) for the run; errors are raised at run time.
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
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.strip_prefix("aider").unwrap_or(l).trim().to_string())
        .unwrap_or_else(|| text.to_string())
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
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "git", "coreutils"]).await?;
        exec_agent(
            cx,
            &format!(
                "set -euo pipefail; {} && \
                 if [ -f \"$HOME/.local/bin/env\" ]; then source \"$HOME/.local/bin/env\"; else export PATH=\"$HOME/.local/bin:$PATH\"; fi && \
                 uv tool install --force --python python3.12 --with pip aider-chat{} && aider --version",
                uv_install(None),
                pins::AIDER.pip_constraint(self.version.as_deref())
            ),
            &BTreeMap::new(),
        )
        .await?;
        if self.version.is_none() {
            self.version = detect_version(cx, ". $HOME/.local/bin/env; aider --version", parse_aider_version).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;
    use serde_json::json;

    fn agent(kwargs: serde_json::Value, env: &[(&str, &str)], model: Option<&str>) -> Result<Aider> {
        let args = common::test_args(kwargs, env, model);
        crate::agents::check_kwargs("aider", &args.kwargs)?;
        let a = Aider { version: None, args };
        a.cli_flags()?;
        Ok(a)
    }

    #[test]
    fn flags_env_and_validation() {
        let a = agent(json!({"reasoning_effort": "high", "thinking_tokens": "2048", "cache_prompts": true, "auto_lint": false, "test_cmd": "pytest -q", "map_tokens": 1024}), &[("OPENAI_API_KEY", "sk-o")], Some("openai/gpt-4o")).unwrap();
        assert_eq!(
            a.cli_flags().unwrap(),
            "--reasoning-effort high --thinking-tokens 2048 --cache-prompts --test-cmd 'pytest -q' --map-tokens 1024"
        );
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
        assert_eq!(crate::fmt::float_with_point(1.0), "1.0");
        assert_eq!(crate::fmt::float_with_point(0.25), "0.25");
    }
}
