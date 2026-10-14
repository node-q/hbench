//! fx (port of upstream's agents/installed/fx.py). `fx-dev` reuses this struct with `dev = true`.

use super::common::opts::check_options;
use super::common::{installed_dir, skills_dir};
use super::{connection, ensure_system_dependencies, exec_agent, upload_text};
use crate::agents::model_connection::Spec;
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const PATH_EXPORT: &str = "export PATH=\"$HOME/.local/bin:$PATH\"";
const INSTALL_SCRIPT_URL: &str = "https://fx.sh/setup.sh";
const PERMISSION_MODES: &[&str] = &["ask", "auto", "yolo"];
const REASONING_EFFORTS: &[&str] = &["auto", "none", "minimal", "low", "medium", "high", "xhigh", "max"];
const SPEC: Spec = Spec {
    default_provider: Some("vercel_ai_gateway"),
    api_key_envs: &["AI_GATEWAY_API_KEY", "VERCEL_AI_GATEWAY_API_KEY"],
    base_url_envs: &[],
    passthrough: false,
};
const DEV_TRACE_LOG: &str = "/logs/agent/fx-trace.log";
const DEV_TRACE_SCOPES: &str = "agent,core,gateway,stream,tool,session,permission,worker,command_output";

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(Fx::new(args, false)?))
}

pub struct Fx {
    args: AgentArgs,
    dev: bool,
    version: Option<String>,
    reasoning_effort: Option<String>,
    permission_mode: String,
    /// FxOptions/FxDevOptions `Env(...)` fields, compiled.
    option_env: BTreeMap<String, String>,
}

impl Fx {
    pub fn new(args: AgentArgs, dev: bool) -> Result<Fx> {
        let name = if dev { "fx-dev" } else { "fx" };
        let mut fields = vec!["max_agent_steps", "reasoning_effort", "permission_mode"];
        if dev {
            fields.extend(["trace_log", "trace_scopes"]);
        }
        check_options(&args, name, &fields)?;
        let reasoning_effort = args.kw("reasoning_effort");
        if let Some(r) = &reasoning_effort
            && !REASONING_EFFORTS.contains(&r.as_str())
        {
            let mut v = REASONING_EFFORTS.to_vec();
            v.sort();
            bail!("Invalid value for 'reasoning_effort': '{r}'. Valid values: {}", v.join(", "));
        }
        let permission_mode = args.kw("permission_mode").unwrap_or_else(|| "yolo".into());
        if !PERMISSION_MODES.contains(&permission_mode.as_str()) {
            bail!("Invalid value for 'permission_mode': '{permission_mode}'. Valid values: ask, auto, yolo");
        }
        let mut option_env = BTreeMap::new();
        // max_agent_steps: Env("FX_MAX_AGENT_STEPS", fallback="FX_MAX_AGENT_STEPS"), an int.
        let steps = match args.kwargs.get("max_agent_steps") {
            Some(Value::Null) => None,
            Some(_) => args.kw("max_agent_steps"),
            None => args.extra_env.get("FX_MAX_AGENT_STEPS").cloned().or_else(|| std::env::var("FX_MAX_AGENT_STEPS").ok()),
        };
        if let Some(s) = steps {
            let n: i64 = s
                .trim()
                .parse()
                .map_err(|_| anyhow!("Invalid kwargs for agent '{name}':\n  max_agent_steps: Input should be a valid integer"))?;
            option_env.insert("FX_MAX_AGENT_STEPS".into(), n.to_string());
        }
        if dev {
            for (kw, env, default) in [("trace_log", "FX_TRACE_LOG", DEV_TRACE_LOG), ("trace_scopes", "FX_TRACE_SCOPES", DEV_TRACE_SCOPES)]
            {
                let v = match args.kwargs.get(kw) {
                    Some(Value::Null) => None,
                    Some(_) => args.kw(kw),
                    None => Some(default.to_string()),
                };
                if let Some(v) = v {
                    option_env.insert(env.into(), v);
                }
            }
        }
        Ok(Fx { version: args.version_kw(), args, dev, reasoning_effort, permission_mode, option_env })
    }

    fn mcp_config(&self) -> Result<String> {
        let mut servers = serde_json::Map::new();
        for s in &self.args.mcp_servers {
            let v = if s.transport() == "stdio" {
                let Some(cmd) = &s.command else { bail!("MCP server '{}' requires a command", s.name) };
                let mut command = vec![cmd.clone()];
                command.extend(s.args.iter().cloned());
                json!({"type": "local", "command": command, "enabled": true})
            } else {
                let Some(url) = &s.url else { bail!("MCP server '{}' requires a URL", s.name) };
                json!({"type": if s.transport() == "streamable-http" { "http" } else { "sse" }, "url": url, "enabled": true})
            };
            servers.insert(s.name.clone(), v);
        }
        Ok(serde_json::to_string_pretty(&json!({"mcp": servers}))?)
    }

    fn execution_model(&self) -> Result<String> {
        let Some(m) = self.args.model_name.as_deref().filter(|m| !m.is_empty()) else { bail!("Model name is required") };
        match m.strip_prefix("vercel_ai_gateway/") {
            None => Ok(m.to_string()),
            Some("") => bail!("Vercel AI Gateway model name is required after prefix"),
            Some(rest) => Ok(rest.to_string()),
        }
    }

    fn run_env(&self) -> Result<BTreeMap<String, String>> {
        if self.args.model_name.as_deref().is_none_or(str::is_empty) {
            bail!("Model name is required");
        }
        let access = connection(&self.args, SPEC);
        let oidc = self.args.get_env("VERCEL_OIDC_TOKEN");
        if access.api_key.as_deref().is_none_or(str::is_empty) && oidc.is_none() {
            bail!("AI_GATEWAY_API_KEY or VERCEL_OIDC_TOKEN is required. Set one on the host or pass it with --ae.");
        }
        let mut env = self.option_env.clone();
        env.extend(access.env);
        env.insert("FX_AUTO_UPGRADE".into(), "0".into());
        env.insert("FX_MODEL".into(), self.execution_model()?);
        env.insert("FX_SOUND".into(), "0".into());
        if let Some(t) = oidc {
            env.insert("VERCEL_OIDC_TOKEN".into(), t);
        }
        Ok(env)
    }

    fn run_command(&self, instruction: &str) -> String {
        let perm = match self.permission_mode.as_str() {
            "ask" => "",
            "auto" => "--auto ",
            _ => "--yolo ",
        };
        format!("{PATH_EXPORT}; fx ask {perm}--json -- {} < /dev/null | stdbuf -oL tee /logs/agent/fx.json", quote(instruction))
    }

    fn install_command(&self) -> String {
        let version = self.version.as_ref().map(|v| if v.starts_with('v') { v.clone() } else { format!("v{v}") });
        let version_arg = version.map(|v| format!(" -s -- {}", quote(&v))).unwrap_or_default();
        format!("set -euo pipefail; curl -fsSL {INSTALL_SCRIPT_URL} | bash{version_arg} && {PATH_EXPORT} && fx --version")
    }

    /// Upload a private profile file the way upstream does: /tmp staging, then mv into ~/.fx.
    async fn install_profile_file(cx: &AgentCtx<'_>, staging: &str, name: &str, content: &str) -> Result<()> {
        upload_text(cx, staging, content).await?;
        exec_agent(
            cx,
            &format!("mkdir -p \"$HOME/.fx\" && mv {staging} \"$HOME/.fx/{name}\" && chmod 600 \"$HOME/.fx/{name}\""),
            &BTreeMap::new(),
        )
        .await?;
        Ok(())
    }
}

/// fx-dev's post-install identity check on `fx status --json`.
fn check_dev_status(stdout: &str) -> Result<()> {
    let ok = serde_json::from_str::<Value>(stdout)
        .ok()
        .is_some_and(|s| s.is_object() && s["build_channel"] == "dev" && s["build_revision"].as_str().is_some_and(|r| !r.is_empty()));
    if !ok {
        bail!("fx-dev did not report a valid dev build identity");
    }
    Ok(())
}

#[async_trait::async_trait]
impl Agent for Fx {
    fn name(&self) -> &str {
        if self.dev { "fx-dev" } else { "fx" }
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "bash", "ca_certificates", "tar", "coreutils", "tmux"]).await?;
        exec_agent(cx, &self.install_command(), &BTreeMap::new()).await?;
        if let Some(r) = &self.reasoning_effort {
            let body = format!("{{\"effort\": {}}}", Value::String(r.clone()));
            Self::install_profile_file(cx, "/tmp/hbench-fx-settings.json", "settings.json", &body).await?;
        }
        if let Some(d) = skills_dir(&self.args) {
            let src = quote(&d);
            exec_agent(
                cx,
                &format!("if [ -d {src} ]; then mkdir -p \"$HOME/.fx/skills\" && cp -R {src}/. \"$HOME/.fx/skills/\"; fi"),
                &BTreeMap::new(),
            )
            .await?;
        }
        if !self.args.mcp_servers.is_empty() {
            let cfg = self.mcp_config().map_err(|e| anyhow!("Agent install failed: {e}"))?;
            Self::install_profile_file(cx, "/tmp/hbench-fx-mcp.json", "mcp.json", &cfg).await?;
        }
        if self.dev {
            exec_agent(cx, &format!("{PATH_EXPORT}; fx upgrade --channel dev --json"), &BTreeMap::new()).await?;
            let r = exec_agent(cx, &format!("{PATH_EXPORT}; fx status --json | tee /logs/agent/fx-dev.json"), &BTreeMap::new()).await?;
            check_dev_status(&r.stdout)?;
        }
        if self.version.is_none() {
            let r = cx.exec(&format!("{PATH_EXPORT}; fx --version"), &BTreeMap::new(), None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(r.stdout.trim().to_string());
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let env = self.run_env()?;
        exec_agent(cx, &self.run_command(&instruction), &env).await?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agents::installed::common;

    pub(crate) fn args(kwargs: Value, env: &[(&str, &str)], model: Option<&str>, mcp: Vec<crate::task::McpServer>) -> AgentArgs {
        AgentArgs { mcp_servers: mcp, ..common::test_args(kwargs, env, model) }
    }

    fn mcp(name: &str, transport: &str, url: Option<&str>, command: Option<&str>, a: &[&str]) -> crate::task::McpServer {
        crate::task::McpServer {
            name: name.into(),
            transport: transport.into(),
            url: url.map(String::from),
            command: command.map(String::from),
            args: a.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn options_validation() {
        assert!(
            Fx::new(args(json!({"reasoning_effort": "ultra"}), &[], None, vec![]), false)
                .err()
                .unwrap()
                .to_string()
                .contains("reasoning_effort")
        );
        assert!(
            Fx::new(args(json!({"permission_mode": "dangerous"}), &[], None, vec![]), false)
                .err()
                .unwrap()
                .to_string()
                .contains("permission_mode")
        );
        assert!(Fx::new(args(json!({"bogus": 1}), &[], None, vec![]), false).err().unwrap().to_string().contains("Unknown option 'bogus'"));
        assert!(Fx::new(args(json!({"trace_log": "x"}), &[], None, vec![]), false).is_err());
        assert!(Fx::new(args(json!({"max_agent_steps": "abc"}), &[], None, vec![]), false).is_err());
        assert!(
            Fx::new(args(json!({"config": {}}), &[], None, vec![]), false).err().unwrap().to_string().contains("does not support config")
        );
    }

    #[test]
    fn install_pins_version() {
        for v in ["0.3.72", "v0.3.72"] {
            let a = Fx::new(args(json!({"version": v}), &[], None, vec![]), false).unwrap();
            assert!(a.install_command().contains(
                "curl -fsSL https://fx.sh/setup.sh | bash -s -- v0.3.72 && export PATH=\"$HOME/.local/bin:$PATH\" && fx --version"
            ));
        }
    }

    #[test]
    fn run_env_and_command() {
        let a = Fx::new(
            args(json!({"max_agent_steps": 32}), &[("AI_GATEWAY_API_KEY", "test-key")], Some("anthropic/claude-opus-4-6"), vec![]),
            false,
        )
        .unwrap();
        let env = a.run_env().unwrap();
        let expect: BTreeMap<String, String> = [
            ("AI_GATEWAY_API_KEY", "test-key"),
            ("FX_AUTO_UPGRADE", "0"),
            ("FX_MAX_AGENT_STEPS", "32"),
            ("FX_MODEL", "anthropic/claude-opus-4-6"),
            ("FX_SOUND", "0"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(env, expect);
        assert_eq!(
            a.run_command("do the task"),
            "export PATH=\"$HOME/.local/bin:$PATH\"; fx ask --yolo --json -- 'do the task' < /dev/null | stdbuf -oL tee /logs/agent/fx.json"
        );

        let g =
            Fx::new(args(json!({}), &[("AI_GATEWAY_API_KEY", "k")], Some("vercel_ai_gateway/openai/gpt-5.6-luna"), vec![]), false).unwrap();
        assert_eq!(g.run_env().unwrap()["FX_MODEL"], "openai/gpt-5.6-luna");
        let e = Fx::new(args(json!({}), &[("AI_GATEWAY_API_KEY", "k")], Some("vercel_ai_gateway/"), vec![]), false).unwrap();
        assert!(e.run_env().unwrap_err().to_string().contains("model name is required after prefix"));
        let nm = Fx::new(args(json!({}), &[("AI_GATEWAY_API_KEY", "k")], None, vec![]), false).unwrap();
        assert!(nm.run_env().unwrap_err().to_string().contains("Model name is required"));

        for (mode, expected) in [("ask", "fx ask --json"), ("auto", "fx ask --auto --json"), ("yolo", "fx ask --yolo --json")] {
            let a = Fx::new(args(json!({"permission_mode": mode}), &[("VERCEL_OIDC_TOKEN", "tok")], Some("anthropic/x"), vec![]), false)
                .unwrap();
            assert!(a.run_command("t").contains(expected));
            assert_eq!(a.run_env().unwrap()["VERCEL_OIDC_TOKEN"], "tok");
        }
    }

    #[test]
    fn mcp_config_native_schema() {
        let a = Fx::new(
            args(
                json!({}),
                &[],
                None,
                vec![
                    mcp("local-tools", "stdio", None, Some("npx"), &["-y", "@example/mcp"]),
                    mcp("events", "sse", Some("https://example.test/sse"), None, &[]),
                    mcp("api", "streamable-http", Some("https://example.test/mcp"), None, &[]),
                ],
            ),
            false,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&a.mcp_config().unwrap()).unwrap();
        assert_eq!(
            v,
            json!({"mcp": {
                "local-tools": {"type": "local", "command": ["npx", "-y", "@example/mcp"], "enabled": true},
                "events": {"type": "sse", "url": "https://example.test/sse", "enabled": true},
                "api": {"type": "http", "url": "https://example.test/mcp", "enabled": true},
            }})
        );
        let bad = Fx::new(args(json!({}), &[], None, vec![mcp("x", "stdio", None, None, &[])]), false).unwrap();
        assert!(bad.mcp_config().is_err());
    }

    #[test]
    fn dev_trace_defaults_and_status() {
        let d = Fx::new(args(json!({}), &[], None, vec![]), true).unwrap();
        assert_eq!(d.name(), "fx-dev");
        assert_eq!(d.option_env["FX_TRACE_LOG"], DEV_TRACE_LOG);
        assert_eq!(d.option_env["FX_TRACE_SCOPES"], DEV_TRACE_SCOPES);
        assert_eq!(d.option_env.len(), 2);
        assert!(check_dev_status(r#"{"kind":"status","build_channel":"dev","build_revision":"0123456789ab"}"#).is_ok());
        assert!(check_dev_status(r#"{"kind":"status","build_channel":"stable","build_revision":"0123456789ab"}"#).is_err());
        assert!(check_dev_status("[]").is_err());
        assert!(check_dev_status("").is_err());
    }
}
