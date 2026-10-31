//! RovoDev CLI installed agent.

use super::common::{installed_dir, pydantic_ai_steps};
use super::{ensure_system_dependencies, exec_agent, exec_root_checked};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::atif;
use crate::env::quote;
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::collections::BTreeMap;

pub struct RovodevCli {
    args: AgentArgs,
    version: Option<String>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    // max_thinking_tokens is accepted (and validated) but unused.
    if let Some(v) = args.kw("max_thinking_tokens") {
        v.trim()
            .parse::<i64>()
            .map_err(|_| anyhow!("Invalid kwargs for agent 'rovodev-cli':\n  max_thinking_tokens: must be an integer"))?;
    }
    Ok(Box::new(RovodevCli { version: args.version_kw(), args }))
}

impl RovodevCli {
    fn auth_env(&self) -> Result<BTreeMap<String, String>> {
        let (Some(email), Some(token)) = (self.args.get_env("ROVODEV_USER_EMAIL"), self.args.get_env("ROVODEV_USER_API_TOKEN")) else {
            bail!("ROVODEV_USER_EMAIL and ROVODEV_USER_API_TOKEN environment variables are required. Please set your RovoDev credentials.");
        };
        Ok(BTreeMap::from([
            ("ROVODEV_USER_EMAIL".to_string(), email),
            ("ROVODEV_USER_API_TOKEN".into(), token),
            (
                "ROVODEV_USER_BILLING_SITE".into(),
                self.args.get_env("ROVODEV_USER_BILLING_SITE").unwrap_or_else(|| "https://hello.atlassian.net".into()),
            ),
        ]))
    }
}

/// Usage from `rovodev_session_context.json` (session-level `usage`), as ATIF final metrics.
pub fn usage_from_session(text: &str) -> Option<AgentContext> {
    let s: Value = serde_json::from_str(text).ok()?;
    if ["id", "message_history", "usage"].iter().any(|k| s.get(*k).is_none()) {
        return None;
    }
    // A trajectory needs at least one step: any response message, or a non-filtered user/system prompt.
    let has_step = s["message_history"].as_array()?.iter().any(|m| match m["kind"].as_str() {
        Some("response") => true,
        Some("request") => m["parts"].as_array().is_some_and(|ps| {
            ps.iter().any(|p| {
                let c = p["content"].as_str().unwrap_or("").trim();
                !c.is_empty() && (p["part_kind"] == "system-prompt" || (p["part_kind"] == "user-prompt" && !is_system_message(c)))
            })
        }),
        _ => false,
    });
    if !has_step {
        return None;
    }
    let u = &s["usage"];
    let n = |k: &str| u[k].as_i64().unwrap_or(0);
    // RovoDev reports no cost.
    Some(AgentContext {
        n_input_tokens: Some(n("input_tokens") + n("cache_read_tokens")),
        n_cache_tokens: Some(n("cache_read_tokens")),
        n_output_tokens: Some(n("output_tokens")),
        ..Default::default()
    })
}

/// `session_context.json`, a pydantic-ai `message_history`, -> ATIF. The session's usage is the only one known to add up,
/// so steps have no metrics.
pub fn atif_trajectory(text: &str, version: Option<&str>, model: Option<&str>) -> Option<atif::Trajectory> {
    let session: Value = serde_json::from_str(text).ok()?;
    let mut steps = pydantic_ai_steps(session["message_history"].as_array()?, &|c| !is_system_message(c));
    steps.iter_mut().for_each(|s| s.metrics = None);
    let agent = atif::Agent {
        name: "rovodev-cli".into(),
        version: version.unwrap_or("unknown").into(),
        model_name: model.map(String::from),
        ..Default::default()
    };
    atif::assemble(agent, session["id"].as_str().map(String::from), steps)
}

/// System message patterns (internal prompts filtered from user steps).
fn is_system_message(c: &str) -> bool {
    use regex::Regex;
    use std::sync::OnceLock;
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| {
        [
            r"(?s)^Based on these messages, generate a 2-4 word title that captures the main task:.*?Respond with ONLY the title, nothing else\. Do not include quotes or formatting\.$",
            r"^You are a helpful assistant that generates short, descriptive titles\.$",
            r"^Before you start working on the next task, please take a look at the workspace\.$",
            r"^You have used \d+ iterations\.$",
            r"(?s)^<system_reminder>.*?</system_reminder>$",
        ]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    })
    .iter()
    .any(|r| r.is_match(c))
}

#[async_trait::async_trait]
impl Agent for RovodevCli {
    fn name(&self) -> &str {
        "rovodev-cli"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["wget", "gnupg", "git"]).await?;
        exec_root_checked(
            cx,
            "set -euo pipefail; mkdir -p -m 755 /etc/apt/keyrings && wget -nv -O- https://acli.atlassian.com/gpg/public-key.asc | \
             gpg --dearmor -o /etc/apt/keyrings/acli-archive-keyring.gpg && chmod go+r /etc/apt/keyrings/acli-archive-keyring.gpg && \
             echo \"deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/acli-archive-keyring.gpg] https://acli.atlassian.com/linux/deb stable main\" | \
             tee /etc/apt/sources.list.d/acli.list > /dev/null && apt update && apt install -y acli",
            &BTreeMap::from([("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())]),
        )
        .await?;
        if self.version.is_none() {
            let r = cx.exec("acli rovodev --version", &BTreeMap::new(), None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(r.stdout.trim().to_string());
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let env = self.auth_env()?;
        exec_agent(
            cx,
            "mkdir -p \"$HOME/.rovodev\" && cat >> \"$HOME/.rovodev/config.yml\" << EOF\n\natlassianBillingSite:\n  siteUrl: \"$ROVODEV_USER_BILLING_SITE\"\nEOF\n\
             echo \"$ROVODEV_USER_API_TOKEN\" | acli rovodev auth login --email \"$ROVODEV_USER_EMAIL\" --token && acli rovodev --version",
            &env,
        )
        .await?;
        let r = exec_agent(
            cx,
            &format!("acli rovodev run --yolo {} 2>&1 </dev/null | tee /logs/agent/rovodev-cli.txt", quote(&instruction)),
            &env,
        )
        .await;
        let _ = exec_agent(
            cx,
            "mkdir -p /logs/agent && LATEST_SESSION=$(ls -t ~/.rovodev/sessions/ | head -1) && \
             if [ -n \"$LATEST_SESSION\" ] && [ -f ~/.rovodev/sessions/$LATEST_SESSION/session_context.json ]; then \
             cp ~/.rovodev/sessions/$LATEST_SESSION/session_context.json /logs/agent/rovodev_session_context.json && \
             echo \"Copied session file from ~/.rovodev/sessions/$LATEST_SESSION/session_context.json to /logs/agent/rovodev_session_context.json\"; \
             else echo \"ERROR: No session file found in ~/.rovodev/sessions/\"; fi",
            &env,
        )
        .await;
        r.map(|_| ())
    }

    fn collect_usage(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        let Ok(text) = std::fs::read_to_string(logs_dir.join("rovodev_session_context.json")) else { return };
        let Some(c) = usage_from_session(&text) else { return };
        if let Some(mut t) = atif_trajectory(&text, self.version.as_deref(), self.args.model_name.as_deref()) {
            atif::set_run_totals(&mut t, c.n_input_tokens, c.n_output_tokens, c.n_cache_tokens, None);
            atif::write_trajectory(logs_dir, &t);
        }
        *ctx = c;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;
    use serde_json::json;

    fn args(kwargs: Value, env: &[(&str, &str)]) -> AgentArgs {
        common::test_args(kwargs, env, None)
    }

    #[test]
    fn session_context_to_atif() {
        let session = serde_json::json!({"id": "s1", "usage": {"input_tokens": 10}, "message_history": [
            {"kind": "request", "parts": [{"part_kind": "system-prompt", "content": "be brief"}, {"part_kind": "user-prompt", "content": "Fix the bug"},
                                          {"part_kind": "user-prompt", "content": "You have used 3 iterations."}]},
            {"kind": "response", "usage": {"input_tokens": 4}, "parts": [{"part_kind": "tool-call", "tool_name": "bash", "args": "{\"command\":\"ls\"}", "tool_call_id": "c1"}]},
            {"kind": "request", "parts": [{"part_kind": "tool-return", "tool_name": "bash", "content": "a.py", "tool_call_id": "c1"}]},
            {"kind": "response", "parts": [{"part_kind": "text", "content": "done"}]}
        ]});
        let a = atif_trajectory(&session.to_string(), Some("1"), None).unwrap();
        assert_eq!(a.session_id.as_deref(), Some("s1"));
        assert_eq!(a.steps.iter().map(|s| s.source.as_str()).collect::<Vec<_>>(), ["system", "user", "agent", "agent"]);
        let result = &a.steps[2].observation.as_ref().unwrap().results[0];
        assert_eq!((result.source_call_id.as_deref(), result.content.clone()), (Some("c1"), Some(serde_json::json!("a.py"))));
        assert!(a.steps[2].metrics.is_none());
    }

    #[test]
    fn auth_and_options() {
        let a = RovodevCli { args: args(json!({}), &[("ROVODEV_USER_EMAIL", "e@x"), ("ROVODEV_USER_API_TOKEN", "t")]), version: None };
        let env = a.auth_env().unwrap();
        assert_eq!(env["ROVODEV_USER_BILLING_SITE"], "https://hello.atlassian.net");
        assert_eq!(env["ROVODEV_USER_EMAIL"], "e@x");
        let b = RovodevCli { args: args(json!({}), &[("ROVODEV_USER_EMAIL", "e@x")]), version: None };
        assert!(b.auth_env().is_err());
        assert!(create(args(json!({"max_thinking_tokens": "lots"}), &[])).is_err());
        assert!(create(args(json!({"max_thinking_tokens": 1000}), &[])).is_ok());
    }

    #[test]
    fn session_usage() {
        let s = json!({
            "id": "sess",
            "message_history": [
                {"kind": "request", "parts": [{"part_kind": "user-prompt", "content": "Fix the bug"}]},
                {"kind": "response", "parts": [{"part_kind": "text", "content": "done"}], "usage": {"input_tokens": 10}}
            ],
            "usage": {"input_tokens": 100, "output_tokens": 30, "cache_read_tokens": 50, "cache_write_tokens": 5}
        });
        let c = usage_from_session(&s.to_string()).unwrap();
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens, c.cost_usd), (Some(150), Some(50), Some(30), None));
        let mut no_usage = s.clone();
        no_usage.as_object_mut().unwrap().remove("usage");
        assert!(usage_from_session(&no_usage.to_string()).is_none());
        let internal = json!({"id": "s", "usage": {}, "message_history": [{"kind": "request", "parts": [{"part_kind": "user-prompt", "content": "You have used 3 iterations."}]}]});
        assert!(usage_from_session(&internal.to_string()).is_none());
    }
}
