//! Devin CLI (port of upstream's agents/installed/devin.py).
//!
//! upstream's populate_context_post_run only writes an ATIF trajectory from sessions.db (it never sets
//! token/cost on the context), so there is no usage parsing to port; sessions.db is still copied to logs.

use super::{ensure_system_dependencies, exec_agent, upload_text};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub struct Devin {
    args: AgentArgs,
    version: Option<String>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(Devin { version: args.version_kw(), args }))
}

impl Devin {
    fn mcp_config(&self) -> Value {
        let mut servers = Map::new();
        for s in &self.args.mcp_servers {
            let v = if s.transport() == "stdio" {
                json!({"command": s.command, "args": s.args})
            } else {
                json!({"url": s.url, "transport": if s.transport() == "streamable-http" { "http" } else { s.transport() }})
            };
            servers.insert(s.name.clone(), v);
        }
        json!({"mcpServers": servers})
    }

    fn credentials(&self) -> Result<String> {
        let Some(key) = self.args.get_env("DEVIN_API_KEY") else { bail!("DEVIN_API_KEY must be set") };
        let mut c = format!("windsurf_api_key = \"{key}\"\n");
        if let Some(u) = self.args.get_env("DEVIN_API_SERVER_URL") {
            c += &format!("api_server_url = \"{u}\"\n");
        }
        Ok(c)
    }

    fn run_command(&self, instruction: &str) -> String {
        let model = self.args.model_name.as_deref().filter(|m| !m.is_empty()).map(|m| format!("--model {} ", quote(m.rsplit('/').next().unwrap()))).unwrap_or_default();
        format!(
            "export PATH=\"$HOME/.local/bin:$PATH\"; devin {model}--permission-mode yolo --print --respect-workspace-trust false -- {} 2>&1 | tee /logs/agent/rollout.log",
            quote(instruction)
        )
    }
}

#[async_trait::async_trait]
impl Agent for Devin {
    fn name(&self) -> &str {
        "devin"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        cx.env.exec_root("[ -d /installed-agent ] || mkdir -p /installed-agent", None).await?;
        ensure_system_dependencies(cx, &["curl", "bash"]).await?;
        let none = BTreeMap::new();
        exec_agent(cx, "CONFIG_DIR=\"${XDG_CONFIG_HOME:-$HOME/.config}/devin\"; mkdir -p \"$CONFIG_DIR\"; echo '{\"shell\":{\"setup_complete\":true}}' > \"$CONFIG_DIR/config.json\"", &none).await?;
        // The API key goes into credentials.toml, never the agent's run env.
        // ponytail: upstream also pops DEVIN_API_KEY from the agent env overlay; AgentCtx.overlay is framework-owned.
        let creds = self.credentials()?;
        let data_dir = exec_agent(cx, "DATA_DIR=\"${XDG_DATA_HOME:-$HOME/.local/share}/devin\"; mkdir -p \"$DATA_DIR\"; echo \"$DATA_DIR\"", &none).await?;
        upload_text(cx, &format!("{}/credentials.toml", data_dir.stdout.trim()), &creds).await?;
        let v = self.version.as_ref().map(|v| format!(" {v}")).unwrap_or_default();
        exec_agent(cx, &format!("curl -fsSL https://cli.devin.ai/install.sh | bash -s --{v}; export PATH=\"$HOME/.local/bin:$PATH\"; devin version"), &none).await?;
        if !self.args.mcp_servers.is_empty() {
            let dir = exec_agent(cx, "CONFIG_DIR=\"${XDG_CONFIG_HOME:-$HOME/.config}/devin\"; mkdir -p \"$CONFIG_DIR\"; echo \"$CONFIG_DIR\"", &none).await?;
            upload_text(cx, &format!("{}/mcp_config.json", dir.stdout.trim()), &(serde_json::to_string_pretty(&self.mcp_config())? + "\n")).await?;
        }
        if self.version.is_none() {
            let r = cx.exec("export PATH=\"$HOME/.local/bin:$PATH\"; devin version", &none, None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(r.stdout.trim().to_string());
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let none = BTreeMap::new();
        exec_agent(cx, "mkdir -p /logs/agent", &none).await?;
        let env = BTreeMap::from([("RUST_LOG".to_string(), "debug".to_string())]);
        let r = exec_agent(cx, &self.run_command(&instruction), &env).await;
        // Always copy the session DB (conversations live there).
        let copy = exec_agent(
            cx,
            "DB=\"${XDG_DATA_HOME:-$HOME/.local/share}/devin/cli/sessions.db\"; if command -v sqlite3 >/dev/null; then sqlite3 \"$DB\" \".backup /logs/agent/sessions.db\"; \
             else cp \"$DB\" \"$DB-wal\" \"$DB-shm\" /logs/agent/ 2>/dev/null; fi || true",
            &none,
        )
        .await;
        r?;
        copy.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn agent(env: &[(&str, &str)], model: Option<&str>, mcp: Vec<crate::task::McpServer>) -> Devin {
        let dir = std::env::temp_dir().join(format!("hbench-devin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("environment")).unwrap();
        std::fs::write(dir.join("task.toml"), "[environment]\ndocker_image='x'\n").unwrap();
        std::fs::write(dir.join("instruction.md"), "x").unwrap();
        let args = AgentArgs {
            model_name: model.map(String::from),
            kwargs: Default::default(),
            extra_env: env.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            mcp_servers: mcp,
            skills: vec![],
            logs_dir: dir.clone(),
            task: Arc::new(crate::task::Task::load(&dir).unwrap()),
        };
        Devin { version: None, args }
    }

    #[test]
    fn command_credentials_mcp() {
        let srv = |name: &str, transport: &str, url: Option<&str>, cmd: Option<&str>| crate::task::McpServer {
            name: name.into(),
            transport: transport.into(),
            url: url.map(String::from),
            command: cmd.map(String::from),
            args: vec!["-y".into()],
        };
        let a = agent(&[("DEVIN_API_KEY", "dk"), ("DEVIN_API_SERVER_URL", "https://x")], Some("devin/claude-opus-4.7"), vec![srv("s", "stdio", None, Some("npx")), srv("h", "http", Some("http://h"), None)]);
        assert_eq!(
            a.run_command("do it"),
            "export PATH=\"$HOME/.local/bin:$PATH\"; devin --model claude-opus-4.7 --permission-mode yolo --print --respect-workspace-trust false -- 'do it' 2>&1 | tee /logs/agent/rollout.log"
        );
        assert_eq!(a.credentials().unwrap(), "windsurf_api_key = \"dk\"\napi_server_url = \"https://x\"\n");
        assert_eq!(a.mcp_config(), json!({"mcpServers": {"s": {"command": "npx", "args": ["-y"]}, "h": {"url": "http://h", "transport": "http"}}}));
        let b = agent(&[], None, vec![]);
        assert!(!b.run_command("x").contains("--model"));
    }
}
