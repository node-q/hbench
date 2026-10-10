//! Agents: oracle (reference solution), nop, and claude-code (installed CLI agent).

use crate::env::{DockerEnv, kind_err, tail};
use crate::task::{Task, resolve_env_vars};
use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Default, Clone, Serialize, serde::Deserialize)]
pub struct AgentContext {
    pub n_input_tokens: Option<i64>,
    pub n_cache_tokens: Option<i64>,
    pub n_output_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    Oracle,
    Nop,
    ClaudeCode,
}

/// Host env vars forwarded to claude-code (overridable with --ae).
const CLAUDE_PASSTHROUGH: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "MAX_THINKING_TOKENS",
    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
];

pub struct Agent {
    pub kind: AgentKind,
    pub model: Option<String>,
    pub extra_env: BTreeMap<String, String>,
    pub version: Option<String>,
}

impl Agent {
    pub fn name(&self) -> &'static str {
        match self.kind {
            AgentKind::Oracle => "oracle",
            AgentKind::Nop => "nop",
            AgentKind::ClaudeCode => "claude-code",
        }
    }

    pub fn setup_timeout(&self) -> Duration {
        Duration::from_secs(360)
    }

    /// Checked once per job, before any container starts.
    pub fn preflight(&self) -> Result<()> {
        if self.kind == AgentKind::ClaudeCode && !self.claude_env().keys().any(|k| {
            matches!(k.as_str(), "ANTHROPIC_API_KEY" | "ANTHROPIC_AUTH_TOKEN" | "CLAUDE_CODE_OAUTH_TOKEN")
        }) {
            bail!("claude-code needs ANTHROPIC_API_KEY, ANTHROPIC_AUTH_TOKEN or CLAUDE_CODE_OAUTH_TOKEN (host env or --ae)");
        }
        Ok(())
    }

    fn claude_env(&self) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> = CLAUDE_PASSTHROUGH
            .iter()
            .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| (k.to_string(), v)))
            .collect();
        env.extend(self.extra_env.clone());
        env
    }

    pub async fn setup(&mut self, env: &DockerEnv, user: Option<&str>) -> Result<()> {
        if self.kind != AgentKind::ClaudeCode {
            return Ok(());
        }
        let deps = "set -e; \
            if command -v apk >/dev/null 2>&1; then apk add --no-cache curl bash nodejs npm procps; \
            elif ! (command -v curl && command -v ps) >/dev/null 2>&1; then \
              if command -v apt-get >/dev/null 2>&1; then apt-get update && apt-get install -y curl ca-certificates procps; \
              elif command -v dnf >/dev/null 2>&1; then dnf install -y curl procps-ng; \
              elif command -v yum >/dev/null 2>&1; then yum install -y curl procps-ng; fi; fi";
        let noninteractive: BTreeMap<String, String> = [("DEBIAN_FRONTEND".into(), "noninteractive".into())].into();
        let r = env.exec(deps, Some("root"), &noninteractive, None).await?;
        if r.code != 0 {
            bail!("Agent install failed (system deps): {}", tail(&(r.stdout + &r.stderr), 2000));
        }
        let ver = self.version.clone();
        let install = format!(
            "set -euo pipefail; if command -v apk &> /dev/null; then npm install -g @anthropic-ai/claude-code{npm_ver}; \
             else curl -fsSL https://downloads.claude.ai/claude-code-releases/bootstrap.sh | bash -s --{sh_ver}; fi && \
             echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.bashrc && export PATH=\"$HOME/.local/bin:$PATH\" && claude --version",
            npm_ver = ver.as_ref().map(|v| format!("@{v}")).unwrap_or_default(),
            sh_ver = ver.as_ref().map(|v| format!(" {v}")).unwrap_or_default(),
        );
        let r = env.exec(&install, user, &BTreeMap::new(), None).await?;
        if r.code != 0 {
            bail!("Agent install failed: {}", tail(&(r.stdout + &r.stderr), 2000));
        }
        if self.version.is_none() {
            // `claude --version` prints e.g. "2.1.3 (Claude Code)" as its last line.
            self.version = r.stdout.lines().last().and_then(|l| l.split_whitespace().next()).map(String::from);
        }
        Ok(())
    }

    /// Run the agent. Errors with kind NonZeroAgentExitCodeError are recorded but don't stop verification.
    pub async fn run(&self, task: &Task, env: &DockerEnv, user: Option<&str>, agent_dir: &Path) -> Result<()> {
        match self.kind {
            AgentKind::Nop => Ok(()),
            AgentKind::Oracle => {
                let sol = task.solution_dir();
                if !sol.join("solve.sh").exists() {
                    bail!("Solution script not found: {}", sol.join("solve.sh").display());
                }
                env.upload_dir(&sol, "/solution").await?;
                env.exec("chmod +x /solution/solve.sh", Some("root"), &BTreeMap::new(), None).await?;
                let mut e = self.extra_env.clone();
                e.insert("DEBIAN_FRONTEND".into(), "noninteractive".into());
                e.extend(resolve_env_vars(&task.config.solution.env)?);
                let r = env.exec("(/solution/solve.sh) > /logs/agent/oracle.txt 2>&1", user, &e, None).await?;
                if r.code != 0 {
                    std::fs::write(agent_dir.join("exit-code.txt"), r.code.to_string())?;
                }
                Ok(())
            }
            AgentKind::ClaudeCode => {
                let mut e = self.claude_env();
                if let Some(m) = &self.model {
                    let m = m.strip_prefix("anthropic/").unwrap_or(m).to_string();
                    if e.contains_key("ANTHROPIC_BASE_URL") {
                        for k in ["ANTHROPIC_DEFAULT_SONNET_MODEL", "ANTHROPIC_DEFAULT_OPUS_MODEL", "ANTHROPIC_DEFAULT_HAIKU_MODEL", "CLAUDE_CODE_SUBAGENT_MODEL"] {
                            e.insert(k.into(), m.clone());
                        }
                    }
                    e.insert("ANTHROPIC_MODEL".into(), m);
                }
                e.insert("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(), "1".into());
                e.insert("IS_SANDBOX".into(), "1".into()); // allows bypassPermissions as root
                e.insert("CLAUDE_CONFIG_DIR".into(), "/logs/agent/sessions".into());
                let r = env.exec("mkdir -p $CLAUDE_CONFIG_DIR/projects $CLAUDE_CONFIG_DIR/todos", user, &e, None).await?;
                if r.code != 0 {
                    bail!("claude-code session setup failed: {}", r.stderr);
                }
                e.insert("HBENCH_INSTRUCTION".into(), task.instruction.clone());
                let cmd = "set -o pipefail; export PATH=\"$HOME/.local/bin:$PATH\"; \
                    hbench_instruction=\"$HBENCH_INSTRUCTION\"; unset HBENCH_INSTRUCTION; \
                    printf '%s' \"$hbench_instruction\" | claude --verbose --output-format=stream-json \
                    --permission-mode=bypassPermissions --print 2>&1 | tee /logs/agent/claude-code.txt";
                let r = env.exec(cmd, user, &e, None).await?;
                if r.code != 0 {
                    return Err(kind_err(
                        "NonZeroAgentExitCodeError",
                        format!("claude-code exited with code {}: {}", r.code, tail(&r.stdout, 1000)),
                    ));
                }
                Ok(())
            }
        }
    }

    /// Fill token/cost usage from downloaded agent logs.
    pub fn populate_context(&self, agent_dir: &Path, ctx: &mut AgentContext) {
        if self.kind != AgentKind::ClaudeCode {
            return;
        }
        let Ok(text) = std::fs::read_to_string(agent_dir.join("claude-code.txt")) else { return };
        if let Some(r) = parse_claude_result(&text) {
            *ctx = r;
        }
    }
}

/// Parse the final `{"type":"result"}` event of claude's stream-json output.
pub fn parse_claude_result(text: &str) -> Option<AgentContext> {
    let v: Value = text
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find(|v| v["type"] == "result")?;
    let u = &v["usage"];
    let n = |k: &str| u[k].as_i64().unwrap_or(0);
    let cache_read = n("cache_read_input_tokens");
    Some(AgentContext {
        // upstream semantics: input tokens include cached tokens.
        n_input_tokens: Some(n("input_tokens") + n("cache_creation_input_tokens") + cache_read),
        n_cache_tokens: Some(cache_read),
        n_output_tokens: Some(n("output_tokens")),
        cost_usd: v["total_cost_usd"].as_f64(),
        metadata: Some(serde_json::json!({
            "num_turns": v["num_turns"], "is_error": v["is_error"],
            "duration_ms": v["duration_ms"], "subtype": v["subtype"],
        })),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_claude_result_line() {
        let text = "{\"type\":\"system\"}\nnot json\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\
            \"num_turns\":3,\"total_cost_usd\":0.25,\"usage\":{\"input_tokens\":10,\"cache_creation_input_tokens\":5,\
            \"cache_read_input_tokens\":100,\"output_tokens\":7}}\n";
        let c = super::parse_claude_result(text).unwrap();
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens), (Some(115), Some(100), Some(7)));
        assert_eq!(c.cost_usd, Some(0.25));
        assert!(super::parse_claude_result("{\"type\":\"assistant\"}").is_none());
    }
}
