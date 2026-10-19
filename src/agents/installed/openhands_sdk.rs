//! openhands-sdk: the OpenHands SDK in its own venv, driven by assets/agents/openhands-sdk/openhands_sdk_runner.py.

use super::common::{installed_dir, skills_dir, uv_install};
use super::openhands::{opt_bool, opt_f64, opt_int, opt_str, raw_env};
use super::pins;
use super::{ensure_system_dependencies, exec_agent, exec_root};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use crate::fmt::float_with_point;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

const RUNNER: &str = include_str!("../../../assets/agents/openhands-sdk/openhands_sdk_runner.py");
const N: &str = "openhands-sdk";
const INSTRUCTION_PATH: &str = "/installed-agent/instruction.txt";
const VERSION_CMD: &str = "/opt/openhands-sdk-venv/bin/python -c 'import openhands.sdk; print(openhands.sdk.__version__)' 2>/dev/null";
const DEFAULT_SKILL_PATHS: &[&str] = &[
    "~/.openhands-sdk/skills",
    "~/.claude/skills",
    "~/.codex/skills",
    "~/.agents/skills",
    "~/.goose/skills",
    "~/.gemini/skills",
    "~/.factory/skills",
    "~/.opencode/skill",
];
/// One line; the instruction is passed by file.
const RUN_CMD: &str = "/opt/openhands-sdk-venv/bin/python /installed-agent/run_agent.py     --instruction-file=/installed-agent/instruction.txt     \
                       --logs-dir=\"$AGENT_LOGS_DIR\"     --trajectory-path=\"$TRAJECTORY_PATH\"     2>&1 | stdbuf -oL tee /logs/agent/openhands_sdk.txt";

pub struct OpenHandsSdk {
    args: AgentArgs,
    version: Option<String>,
    reasoning_effort: Option<String>,
    load_skills: bool,
    skill_paths: Option<Vec<String>>,
    collect_token_ids: bool,
    max_iterations: Option<i64>,
    temperature: Option<f64>,
    python_version: String,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(OpenHandsSdk::new(args)?))
}

impl OpenHandsSdk {
    pub fn new(args: AgentArgs) -> Result<OpenHandsSdk> {
        let skill_paths = match args.kwargs.get("skill_paths") {
            None | Some(Value::Null) => None,
            Some(Value::Array(a)) => Some(
                a.iter()
                    .map(|v| match v {
                        Value::String(s) => Ok(s.clone()),
                        Value::Number(n) => Ok(n.to_string()),
                        _ => bail!("Invalid kwargs for agent '{N}':\n  skill_paths: must be a string"),
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            Some(_) => bail!("Invalid kwargs for agent '{N}':\n  skill_paths: must be a list"),
        };
        Ok(OpenHandsSdk {
            version: opt_str(&args, N, "version")?,
            reasoning_effort: opt_str(&args, N, "reasoning_effort")?,
            load_skills: opt_bool(&args, N, "load_skills")?.unwrap_or(true),
            skill_paths,
            collect_token_ids: opt_bool(&args, N, "collect_token_ids")?.unwrap_or(false),
            max_iterations: opt_int(&args, N, "max_iterations")?,
            temperature: opt_f64(&args, N, "temperature")?,
            python_version: opt_str(&args, N, "python_version")?.unwrap_or_else(|| "3.12".into()),
            args,
        })
    }

    fn install_cmd(&self) -> String {
        let v = pins::OPENHANDS_SDK.pip_constraint(self.version.as_deref());
        let py = &self.python_version;
        format!(
            "set -euo pipefail; {uv_install} && \
             if [ -f \"$HOME/.local/bin/env\" ]; then source \"$HOME/.local/bin/env\"; fi && \
             uv python install {py} && uv venv /opt/openhands-sdk-venv --python {py} --clear && \
             source /opt/openhands-sdk-venv/bin/activate && uv pip install openhands-sdk{v} openhands-tools{v} {fastapi}",
            uv_install = uv_install(None),
            fastapi = pins::FASTAPI.requirement()
        )
    }

    fn run_env(&self) -> Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::new();
        let Some(key) = raw_env(&self.args, "LLM_API_KEY") else { bail!("LLM_API_KEY environment variable must be set") };
        env.insert("LLM_API_KEY".into(), key);
        if let Some(u) = raw_env(&self.args, "LLM_BASE_URL") {
            env.insert("LLM_BASE_URL".into(), u);
        }
        let Some(model) = self.args.model_name.clone().filter(|m| !m.is_empty()).or_else(|| raw_env(&self.args, "LLM_MODEL")) else {
            bail!("No LLM model specified")
        };
        env.insert("LLM_MODEL".into(), model);
        env.insert("AGENT_LOGS_DIR".into(), "/logs/agent".into());
        env.insert("TRAJECTORY_PATH".into(), "/logs/agent/trajectory.json".into());
        env.insert("LOAD_SKILLS".into(), (if self.load_skills { "1" } else { "0" }).into());
        // `skill_paths or DEFAULT`: an empty list falls back to the defaults too.
        let mut paths: Vec<String> = match &self.skill_paths {
            Some(p) if !p.is_empty() => p.clone(),
            _ => DEFAULT_SKILL_PATHS.iter().map(|s| s.to_string()).collect(),
        };
        if let Some(d) = skills_dir(&self.args) {
            let mut seen = vec![];
            for p in std::iter::once(d).chain(paths) {
                if !seen.contains(&p) {
                    seen.push(p);
                }
            }
            paths = seen;
        }
        env.insert("SKILL_PATHS".into(), paths.join(":"));
        if !self.args.mcp_servers.is_empty() {
            // Compact JSON; the runner only json.loads it.
            let list: Vec<Value> = self
                .args
                .mcp_servers
                .iter()
                .map(|s| {
                    let mut e = json!({"name": s.name, "transport": s.transport()});
                    if s.transport() == "stdio" {
                        if let Some(c) = &s.command {
                            e["command"] = json!(c);
                        }
                        if !s.args.is_empty() {
                            e["args"] = json!(s.args);
                        }
                    } else if let Some(u) = &s.url {
                        e["url"] = json!(u);
                    }
                    e
                })
                .collect();
            env.insert("MCP_SERVERS_JSON".into(), serde_json::to_string(&list)?);
        }
        if let Some(r) = &self.reasoning_effort {
            env.insert("LLM_REASONING_EFFORT".into(), r.clone());
        }
        if self.collect_token_ids {
            env.insert("LITELLM_EXTRA_BODY".into(), "{\"return_token_ids\": true}".into());
        }
        if let Some(m) = self.max_iterations {
            env.insert("MAX_ITERATIONS".into(), m.to_string());
        }
        if let Some(t) = self.temperature {
            env.insert("LLM_TEMPERATURE".into(), float_with_point(t));
        }
        Ok(env)
    }
}

/// Totals from the runner-written ATIF `trajectory.json` `final_metrics`.
pub fn parse_trajectory_metrics(text: &str) -> Option<AgentContext> {
    let v: Value = serde_json::from_str(text).ok()?;
    let fm = v.get("final_metrics").unwrap_or(&Value::Null);
    if !fm.is_object() && !fm.is_null() {
        return None;
    }
    // `.get(k, 0)`: missing -> 0, explicit null -> None.
    let n = |k: &str| match fm.get(k) {
        None => Some(0),
        Some(x) => x.as_i64(),
    };
    Some(AgentContext {
        n_input_tokens: n("total_prompt_tokens"),
        n_cache_tokens: n("total_cached_tokens"),
        n_output_tokens: n("total_completion_tokens"),
        cost_usd: fm.get("total_cost_usd").and_then(Value::as_f64),
        ..Default::default()
    })
}

#[async_trait::async_trait]
impl Agent for OpenHandsSdk {
    fn name(&self) -> &str {
        "openhands-sdk"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        let installed = cx
            .exec(
                "[ -f /opt/openhands-sdk-venv/bin/python ] && /opt/openhands-sdk-venv/bin/python -c \"import openhands.sdk\" 2>/dev/null",
                &BTreeMap::new(),
                None,
            )
            .await?
            .code
            == 0;
        ensure_system_dependencies(cx, &["curl", "coreutils"]).await?;
        if !installed {
            let u = cx.user.clone().unwrap_or_else(|| "root".into());
            exec_root(cx, &format!("mkdir -p /opt/openhands-sdk-venv && chown {u}:{u} /opt/openhands-sdk-venv"), &BTreeMap::new()).await?;
            exec_agent(cx, &self.install_cmd(), &BTreeMap::new()).await?;
        }
        let local = cx.logs_dir.join("run_agent.py");
        std::fs::write(&local, RUNNER)?;
        cx.env.upload_file(&local, "/installed-agent/run_agent.py").await?;
        cx.env.exec_root("chmod +x /installed-agent/run_agent.py", None).await?;
        if self.version.is_none() {
            let r = cx.exec(VERSION_CMD, &BTreeMap::new(), None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(r.stdout.trim().to_string());
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        // A single argv string is capped at 128 KiB on Linux, so the instruction goes in a file.
        let local = cx.logs_dir.join("instruction.txt");
        std::fs::write(&local, &instruction)?;
        cx.env.upload_file(&local, INSTRUCTION_PATH).await?;
        if let Some(u) = &cx.user {
            exec_root(cx, &format!("chown {} {}", quote(u), quote(INSTRUCTION_PATH)), &BTreeMap::new()).await?;
        }
        let env = self.run_env()?;
        exec_agent(cx, RUN_CMD, &env).await?;
        Ok(())
    }

    fn collect_usage(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        if let Some(c) = std::fs::read_to_string(logs_dir.join("trajectory.json")).ok().and_then(|t| parse_trajectory_metrics(&t)) {
            ctx.n_input_tokens = c.n_input_tokens;
            ctx.n_cache_tokens = c.n_cache_tokens;
            ctx.n_output_tokens = c.n_output_tokens;
            ctx.cost_usd = c.cost_usd;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::openhands::tests::args;
    use crate::task::McpServer;

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<OpenHandsSdk> {
        OpenHandsSdk::new(args(kwargs, env, model, vec![]))
    }

    #[test]
    fn options_and_install() {
        let a = agent(json!({}), &[], Some("test/model")).unwrap();
        assert!(a.load_skills && !a.collect_token_ids && a.python_version == "3.12");
        assert!(
            a.install_cmd().contains("--python 3.12 --clear")
                && a.install_cmd().ends_with("uv pip install openhands-sdk==1.55.1 openhands-tools==1.55.1 fastapi==0.143.2")
        );
        let v = agent(json!({"version": "1.2.3", "python_version": "3.13"}), &[], None).unwrap();
        assert!(
            v.install_cmd().contains("openhands-sdk==1.2.3 openhands-tools==1.2.3") && v.install_cmd().contains("uv python install 3.13")
        );
        assert!(agent(json!({"max_iterations": "x"}), &[], None).is_err());
        assert!(agent(json!({"skill_paths": "/a"}), &[], None).is_err());
        assert!(RUNNER.contains("--instruction-file"));
        assert!(!RUN_CMD.contains('\n') && RUN_CMD.contains("--instruction-file=/installed-agent/instruction.txt"));
    }

    #[test]
    fn run_env() {
        let a = agent(
            json!({"reasoning_effort": "max", "max_iterations": 15, "temperature": 0.7, "collect_token_ids": true, "load_skills": false}),
            &[("LLM_API_KEY", "k"), ("LLM_BASE_URL", "https://custom.api")],
            Some("anthropic/claude-sonnet-4-5"),
        )
        .unwrap();
        let e = a.run_env().unwrap();
        assert_eq!(
            (e["LLM_API_KEY"].as_str(), e["LLM_BASE_URL"].as_str(), e["LLM_MODEL"].as_str()),
            ("k", "https://custom.api", "anthropic/claude-sonnet-4-5")
        );
        assert_eq!((e["LLM_REASONING_EFFORT"].as_str(), e["MAX_ITERATIONS"].as_str(), e["LLM_TEMPERATURE"].as_str()), ("max", "15", "0.7"));
        assert_eq!(e["LITELLM_EXTRA_BODY"], "{\"return_token_ids\": true}");
        assert_eq!(e["LOAD_SKILLS"], "0");
        assert_eq!(e["SKILL_PATHS"], DEFAULT_SKILL_PATHS.join(":"));
        assert!(!e.contains_key("MCP_SERVERS_JSON"));
        assert_eq!(
            agent(json!({"temperature": 1}), &[("LLM_API_KEY", "k")], Some("m")).unwrap().run_env().unwrap()["LLM_TEMPERATURE"],
            "1.0"
        );

        let mut s = args(
            json!({"skill_paths": ["/custom/skills", "/hbench/skills", "/hbench/skills"]}),
            &[("LLM_API_KEY", "k")],
            Some("m"),
            vec![],
        );
        s.skills = vec!["x".into()];
        assert_eq!(OpenHandsSdk::new(s).unwrap().run_env().unwrap()["SKILL_PATHS"], "/hbench/skills:/custom/skills");

        let m = OpenHandsSdk::new(args(
            json!({}),
            &[("LLM_API_KEY", "k")],
            Some("m"),
            vec![
                McpServer {
                    name: "test-server".into(),
                    transport: "stdio".into(),
                    url: None,
                    command: Some("node".into()),
                    args: vec!["server.js".into(), "--port=3000".into()],
                },
                McpServer {
                    name: "sse-server".into(),
                    transport: "sse".into(),
                    url: Some("http://localhost:8080/sse".into()),
                    command: None,
                    args: vec![],
                },
            ],
        ))
        .unwrap();
        let p: Value = serde_json::from_str(&m.run_env().unwrap()["MCP_SERVERS_JSON"]).unwrap();
        assert_eq!(
            p,
            json!([{"name": "test-server", "transport": "stdio", "command": "node", "args": ["server.js", "--port=3000"]}, {"name": "sse-server", "transport": "sse", "url": "http://localhost:8080/sse"}])
        );

        if std::env::var("LLM_API_KEY").is_err() {
            assert!(agent(json!({}), &[], Some("m")).unwrap().run_env().unwrap_err().to_string().contains("LLM_API_KEY"));
        }
        if std::env::var("LLM_MODEL").is_err() {
            assert!(agent(json!({}), &[("LLM_API_KEY", "k")], None).unwrap().run_env().unwrap_err().to_string().contains("model"));
        }
    }

    #[test]
    fn trajectory_metrics() {
        let t = json!({"schema_version": "ATIF-v1.5", "session_id": "s", "agent": {"name": "openhands-sdk", "version": "1.0.0"}, "steps": [],
            "final_metrics": {"total_prompt_tokens": 1000, "total_completion_tokens": 500, "total_cached_tokens": 200, "total_cost_usd": 0.05}});
        let c = parse_trajectory_metrics(&t.to_string()).unwrap();
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens, c.cost_usd), (Some(1000), Some(200), Some(500), Some(0.05)));
        let c = parse_trajectory_metrics("{\"final_metrics\": {}}").unwrap();
        assert_eq!((c.n_input_tokens, c.cost_usd), (Some(0), None));
        assert!(parse_trajectory_metrics("not json").is_none());
    }
}
