//! GitHub Copilot CLI.

use super::common::parse_jsonl;
use super::common::{detect_version, installed_dir, opts, skills_dir};
use super::pins;
use super::{ensure_system_dependencies, exec_agent, exec_error, parse_version};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::Result;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;

const TRAJECTORY_PATH: &str = "/logs/agent/copilot-cli.jsonl";
const OUTPUT_PATH: &str = "/logs/agent/copilot-cli.txt";

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    let a = CopilotCli { version: args.version_kw(), args };
    a.cli_flags()?;
    a.disable_web_search()?;
    Ok(Box::new(a))
}

pub struct CopilotCli {
    args: AgentArgs,
    version: Option<String>,
}

impl CopilotCli {
    fn cli_flags(&self) -> Result<String> {
        Ok(opts::choice(&self.args, "reasoning_effort", Some("COPILOT_CLI_EFFORT"), &["low", "medium", "high", "xhigh", "max"])?
            .map(|v| format!("--effort {}", quote(&v)))
            .unwrap_or_default())
    }

    fn disable_web_search(&self) -> Result<bool> {
        Ok(opts::boolean(&self.args, "disable_web_search", None)?.unwrap_or(false))
    }

    /// `--additional-mcp-config` for task MCP servers.
    fn mcp_flag(&self) -> Option<String> {
        if self.args.mcp_servers.is_empty() {
            return None;
        }
        let mut servers = serde_json::Map::new();
        for s in &self.args.mcp_servers {
            let v = match s.transport() {
                "stdio" => json!({"type": "stdio", "command": s.command, "args": s.args}),
                "streamable-http" => json!({"type": "http", "url": s.url}),
                t => json!({"type": t, "url": s.url}),
            };
            servers.insert(s.name.clone(), v);
        }
        Some(format!("--additional-mcp-config={}", quote(&json!({"mcpServers": servers}).to_string())))
    }

    fn run_command(&self, instruction: &str, resume: bool) -> Result<String> {
        let model_flag =
            self.args.model_name.as_deref().map(|m| format!("--model={}", quote(m.rsplit('/').next().unwrap()))).unwrap_or_default();
        let web = if self.disable_web_search()? { "--deny-tool web_fetch --deny-tool web_search " } else { "" };
        Ok(format!(
            "set -o pipefail; export PATH=\"$HOME/.local/bin:$PATH\"; copilot --prompt={} {}--yolo {web}{model_flag} {} {} --output-format=json \
             2>&1 </dev/null | stdbuf -oL tee {TRAJECTORY_PATH}",
            quote(instruction),
            if resume { "--continue " } else { "" },
            self.cli_flags()?,
            self.mcp_flag().unwrap_or_default()
        ))
    }
}

/// `set -o pipefail; cmd` as the agent user with a 10s timeout; non-zero exit -> classified error.
async fn exec_agent_10s(cx: &AgentCtx<'_>, cmd: &str, env: &BTreeMap<String, String>) -> Result<()> {
    let r = cx.exec(&format!("set -o pipefail; {cmd}"), env, Some(Duration::from_secs(10))).await?;
    if r.code != 0 {
        return Err(exec_error(cmd, &r, None));
    }
    Ok(())
}

/// Token totals as the JSONL -> ATIF conversion computes them, for both
/// the flat (Anthropic) and session-event (GPT) stream shapes.
pub fn parse_copilot_usage(text: &str) -> Option<AgentContext> {
    if text.starts_with("Error: No authentication information found") {
        return None;
    }
    let (mut input, mut output, mut steps) = (0i64, 0i64, 0usize);
    for e in parse_jsonl(text) {
        if !e.is_object() {
            continue;
        }
        let d = &e["data"];
        let other_keys = |skip: &[&str]| d.as_object().is_some_and(|o| o.keys().any(|k| !skip.contains(&k.as_str())));
        match e["type"].as_str() {
            Some("message" | "tool_use" | "tool_result" | "tool.execution_complete") => steps += 1,
            Some("usage") => {
                input += e["input_tokens"].as_i64().unwrap_or(0);
                output += e["output_tokens"].as_i64().unwrap_or(0);
            }
            Some("assistant.message") => {
                output += d["outputTokens"].as_i64().unwrap_or(0);
                let content = !d["content"].is_null() && d["content"] != "" && d["content"] != json!([]);
                let tools = d["toolRequests"].as_array().is_some_and(|a| !a.is_empty());
                if !d.is_object() || content || tools || other_keys(&["content", "toolRequests", "outputTokens", "model"]) {
                    steps += 1;
                }
            }
            Some("user.message") if !d.is_object() || d["content"].as_str().is_some_and(|s| !s.is_empty()) || other_keys(&["content"]) => {
                steps += 1
            }
            _ => {}
        }
    }
    (steps > 0).then(|| AgentContext { n_input_tokens: (input != 0).then_some(input), n_output_tokens: Some(output), ..Default::default() })
}

#[async_trait::async_trait]
impl Agent for CopilotCli {
    fn name(&self) -> &str {
        "copilot-cli"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "bash", "git"]).await?;
        let v = format!(" VERSION={}", quote(pins::COPILOT_CLI.or(self.version.as_deref())));
        exec_agent(
            cx,
            &format!(
                "set -euo pipefail; curl -fsSL https://gh.io/copilot-install |{v} bash && echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.bashrc && \
                 export PATH=\"$HOME/.local/bin:$PATH\" && copilot --version"
            ),
            &BTreeMap::new(),
        )
        .await?;
        if self.version.is_none() {
            self.version = detect_version(cx, "export PATH=\"$HOME/.local/bin:$PATH\"; copilot --version", parse_version).await;
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        // Copilot checks COPILOT_GITHUB_TOKEN > GH_TOKEN > GITHUB_TOKEN; it can also use a cached login.
        let env: BTreeMap<String, String> = ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"]
            .iter()
            .find_map(|k| self.args.get_env(k))
            .map(|t| ("COPILOT_GITHUB_TOKEN".to_string(), t))
            .into_iter()
            .collect();
        exec_agent_10s(
            cx,
            "if [ -d /logs/agent/copilot/session-state ]; then mkdir -p ~/.copilot && cp -R /logs/agent/copilot/session-state ~/.copilot/ && \
             cp /logs/agent/copilot/session-store.db* ~/.copilot/ 2>/dev/null || true; fi",
            &env,
        )
        .await?;
        if let Some(d) = skills_dir(&self.args) {
            exec_agent(cx, &format!("mkdir -p ~/.copilot/skills && cp -r {}/* ~/.copilot/skills/ 2>/dev/null || true", quote(&d)), &env)
                .await?;
        }
        let r = exec_agent(cx, &self.run_command(&instruction, resume)?, &env).await;
        let _ = exec_agent(cx, &format!("cat {TRAJECTORY_PATH} > {OUTPUT_PATH} 2>/dev/null || true"), &BTreeMap::new()).await;
        let _ = exec_agent_10s(
            cx,
            "if [ -d ~/.copilot ]; then rm -rf /logs/agent/copilot && mkdir -p /logs/agent/copilot && \
             cp -R ~/.copilot/session-state /logs/agent/copilot/ 2>/dev/null || true; cp ~/.copilot/session-store.db* /logs/agent/copilot/ 2>/dev/null || true; fi",
            &env,
        )
        .await;
        r.map(|_| ())
    }

    fn collect_usage(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        if let Some(c) = std::fs::read_to_string(logs_dir.join("copilot-cli.jsonl")).ok().and_then(|t| parse_copilot_usage(&t)) {
            ctx.n_input_tokens = c.n_input_tokens;
            ctx.n_output_tokens = c.n_output_tokens;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;

    #[test]
    fn flags_and_command() {
        let mut args =
            common::test_args(json!({"reasoning_effort": "high", "disable_web_search": true}), &[], Some("anthropic/claude-haiku-4-5"));
        args.mcp_servers = vec![serde_json::from_value(json!({"name": "s", "transport": "http", "url": "http://m"})).unwrap()];
        let a = CopilotCli { version: None, args };
        let c = a.run_command("do it", true).unwrap();
        assert!(c.contains(
            "copilot --prompt='do it' --continue --yolo --deny-tool web_fetch --deny-tool web_search --model=claude-haiku-4-5 --effort high"
        ));
        assert!(c.contains(r#"--additional-mcp-config='{"mcpServers":{"s":{"type":"http","url":"http://m"}}}'"#));
        assert!(create(common::test_args(json!({"reasoning_effort": "turbo"}), &[], None)).is_err());
        let fb = CopilotCli { version: None, args: common::test_args(json!({}), &[("COPILOT_CLI_EFFORT", "max")], None) };
        assert_eq!(fb.cli_flags().unwrap(), "--effort max");
    }

    #[test]
    fn usage_both_schemas() {
        let flat = "{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"hi\"}\n{\"type\":\"usage\",\"input_tokens\":120,\"output_tokens\":30}\n{\"type\":\"usage\",\"input_tokens\":5,\"output_tokens\":1}\nstderr noise\n";
        let c = parse_copilot_usage(flat).unwrap();
        assert_eq!((c.n_input_tokens, c.n_output_tokens), (Some(125), Some(31)));
        let session = "{\"type\":\"user.message\",\"data\":{\"content\":\"task\"}}\n\
            {\"type\":\"assistant.message\",\"data\":{\"content\":\"\",\"toolRequests\":[{\"toolCallId\":\"t1\",\"name\":\"bash\"}],\"outputTokens\":40}}\n\
            {\"type\":\"tool.execution_complete\",\"data\":{\"toolCallId\":\"t1\",\"result\":{\"content\":\"ok\"}}}\n\
            {\"type\":\"assistant.message\",\"data\":{\"content\":\"done\",\"outputTokens\":2}}\n{\"type\":\"result\",\"exitCode\":0}\n";
        let c = parse_copilot_usage(session).unwrap();
        assert_eq!((c.n_input_tokens, c.n_output_tokens), (None, Some(42)));
        assert!(parse_copilot_usage("Error: No authentication information found.\n").is_none());
        assert!(parse_copilot_usage("{\"type\":\"usage\",\"input_tokens\":1}\n").is_none());
    }
}
