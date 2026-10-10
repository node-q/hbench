//! MiMo (port of upstream's agents/installed/mimo.py).

use super::{connection, ensure_system_dependencies, exec_agent};
use crate::agents::model_connection::Spec;
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::{kind_err, quote};
use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const SPEC: Spec = Spec { default_provider: None, api_key_envs: &[], base_url_envs: &[], passthrough: true };
const WEB_TOOLS: [&str; 2] = ["websearch", "webfetch"];

// ponytail: helpers shared by this batch of ports; candidates for installed/mod.rs.

/// upstream's `self.skills_dir`: task `skills_dir`, else `/hbench/skills` when `--skill`s were given.
pub(crate) fn skills_dir(args: &AgentArgs) -> Option<String> {
    args.task.config.environment.skills_dir.clone().or_else(|| (!args.skills.is_empty()).then(|| "/hbench/skills".into()))
}

/// upstream's `node_install.nvm_node_install_snippet`.
pub(crate) fn nvm_node_install_snippet(node_major: u32) -> String {
    format!(
        "curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh | env -u NODE_VERSION bash && \
         export NVM_DIR=\"$HOME/.nvm\" && \\. \"$NVM_DIR/nvm.sh\" || true && \
         command -v nvm &>/dev/null || {{ echo 'Error: NVM failed to load' >&2; exit 1; }} && \
         nvm install {node_major} && nvm alias default {node_major} && npm -v"
    )
}

/// `BaseInstalledAgent.setup` around `install`: mkdir, install, best-effort version detection.
pub(crate) async fn installed_setup(
    cx: &AgentCtx<'_>,
    version: &mut Option<String>,
    install: impl std::future::Future<Output = Result<()>>,
    version_cmd: &str,
    parse: fn(&str) -> String,
) -> Result<()> {
    cx.env.exec_root("[ -d /installed-agent ] || mkdir -p /installed-agent", None).await?;
    install.await?;
    if version.is_none()
        && let Ok(r) = cx.exec(version_cmd, &BTreeMap::new(), None).await
        && r.code == 0
        && !r.stdout.is_empty()
    {
        *version = Some(parse(&r.stdout));
    }
    Ok(())
}

/// upstream's `environment.upload_file` of a bundled script (root-owned, mode from the temp file).
pub(crate) async fn upload_asset(cx: &AgentCtx<'_>, content: &str, path: &str) -> Result<()> {
    let local = std::env::temp_dir().join(format!("hbench-asset-{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&local, content)?;
    let r = cx.env.upload_file(&local, path).await;
    let _ = std::fs::remove_file(&local);
    r
}

pub(crate) fn strip(s: &str) -> String {
    s.trim().to_string()
}

/// Recursive dict merge (`_deep_merge`): `over` wins, nested objects merge.
pub(crate) fn deep_merge(base: &mut Value, over: &Value) {
    if let (Some(b), Some(o)) = (base.as_object_mut(), over.as_object()) {
        for (k, v) in o {
            match b.get_mut(k) {
                Some(bv) if bv.is_object() && v.is_object() => deep_merge(bv, v),
                _ => {
                    b.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

pub(crate) fn parse_jsonl(text: &str) -> Vec<Value> {
    text.lines().filter_map(|l| serde_json::from_str(l.trim()).ok()).collect()
}

pub struct MiMo {
    args: AgentArgs,
    version: Option<String>,
    mimo_config: Value,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    let mimo_config = match args.kwargs.get("mimo_config") {
        None | Some(Value::Null) => json!({}),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => bail!("Invalid kwargs for agent 'mimo':\n  mimo_config: Input should be a valid dictionary"),
    };
    if args.kwargs.contains_key("disable_web_search") && args.kw_bool("disable_web_search").is_none() {
        bail!("Invalid kwargs for agent 'mimo':\n  disable_web_search: Input should be a valid boolean");
    }
    Ok(Box::new(MiMo { version: args.version_kw(), args, mimo_config }))
}

impl MiMo {
    fn skills_command(&self) -> Option<String> {
        let d = skills_dir(&self.args)?;
        Some(format!("mkdir -p ~/.config/mimocode/skills && cp -r {}/* ~/.config/mimocode/skills/ 2>/dev/null || true", quote(&d)))
    }

    fn config(&self) -> Value {
        let mut config = json!({});
        if !self.args.mcp_servers.is_empty() {
            let mut mcp = Map::new();
            for s in &self.args.mcp_servers {
                let v = if s.transport() == "stdio" {
                    let cmd: Vec<String> = s.command.as_ref().map(|c| std::iter::once(c.clone()).chain(s.args.clone()).collect()).unwrap_or_default();
                    json!({"type": "local", "command": cmd})
                } else {
                    json!({"type": "remote", "url": s.url})
                };
                mcp.insert(s.name.clone(), v);
            }
            config["mcp"] = Value::Object(mcp);
        }
        if let Some((provider, model_id)) = self.args.model_name.as_deref().and_then(|m| m.split_once('/')) {
            let mut pc = json!({"models": {model_id: {}}});
            if let Some(url) = connection(&self.args, SPEC).configured_base_url {
                pc["options"] = json!({"baseURL": url});
                pc["npm"] = json!("@ai-sdk/openai-compatible");
            }
            if provider != "mimo" {
                config["provider"] = json!({provider: pc});
            }
        }
        deep_merge(&mut config, &self.mimo_config);
        if self.args.kw_bool("disable_web_search").unwrap_or(false) {
            let mut perm = match config.get("permission") {
                Some(Value::Object(o)) => o.clone(),
                Some(v) if !v.is_null() && v != &json!("") && v != &json!(false) => Map::from_iter([("*".to_string(), v.clone())]),
                _ => Map::new(),
            };
            for t in WEB_TOOLS {
                perm.insert(t.into(), json!("deny"));
            }
            config["permission"] = Value::Object(perm);
        }
        config
    }

    fn config_command(&self) -> Option<String> {
        let c = self.config();
        if c.as_object().is_some_and(|o| o.is_empty()) {
            return None;
        }
        Some(format!("mkdir -p ~/.config/mimocode && echo {} > ~/.config/mimocode/mimocode.json", quote(&serde_json::to_string_pretty(&c).unwrap())))
    }

    fn cli_flags(&self) -> String {
        self.args.kw("variant").map(|v| format!("--variant {}", quote(&v))).unwrap_or_default()
    }
}

/// Messages from MiMo `error` events.
pub fn error_messages(text: &str) -> Vec<String> {
    parse_jsonl(text)
        .into_iter()
        .filter(|e| e["type"] == "error")
        .map(|e| match &e["error"] {
            Value::Object(o) => {
                let m = o.get("data").and_then(|d| d.get("message")).filter(|m| !m.is_null() && *m != "");
                let v = m.or(o.get("name").filter(|n| !n.is_null() && **n != "")).cloned().unwrap_or(e["error"].clone());
                v.as_str().map(String::from).unwrap_or_else(|| v.to_string())
            }
            Value::String(s) => s.clone(),
            v => v.to_string(),
        })
        .collect()
}

/// Totals the ATIF converter would put in final_metrics (step_start..step_finish turns only).
pub fn parse_usage(text: &str) -> Option<AgentContext> {
    let (mut open, mut turns) = (false, 0);
    let (mut cost, mut input, mut output, mut cache) = (0.0, 0i64, 0i64, 0i64);
    for e in parse_jsonl(text) {
        match e["type"].as_str() {
            Some("step_start") => open = true,
            Some("step_finish") if open => {
                open = false;
                turns += 1;
                let p = &e["part"];
                let t = &p["tokens"];
                let cr = t["cache"]["read"].as_i64().unwrap_or(0);
                cost += p["cost"].as_f64().unwrap_or(0.0);
                input += t["input"].as_i64().unwrap_or(0) + cr;
                output += t["output"].as_i64().unwrap_or(0);
                cache += cr;
            }
            _ => {}
        }
    }
    (turns > 0).then(|| AgentContext {
        n_input_tokens: Some(input),
        n_cache_tokens: Some(cache),
        n_output_tokens: Some(output),
        cost_usd: (cost != 0.0).then_some(cost),
        ..Default::default()
    })
}

#[async_trait::async_trait]
impl Agent for MiMo {
    fn name(&self) -> &str {
        "mimo"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        let install = async {
            ensure_system_dependencies(cx, &["curl"]).await?;
            exec_agent(
                cx,
                "set -euo pipefail; curl -fsSL https://mimo.xiaomi.com/install | bash && export PATH=\"$HOME/.mimocode/bin:$PATH\" && mimo --version",
                &BTreeMap::new(),
            )
            .await
            .map(|_| ())
        };
        installed_setup(cx, &mut self.version, install, "export PATH=\"$HOME/.mimocode/bin:$PATH\" && mimo --version", strip).await
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let Some(model) = self.args.model_name.clone().filter(|m| m.contains('/')) else {
            bail!("Model name must be in the format provider/model_name")
        };
        let mut env = connection(&self.args, SPEC).env;
        env.insert("MIMOCODE_FAKE_VCS".into(), "git".into());
        env.insert("XDG_DATA_HOME".into(), "/logs/agent/mimo/xdg-data".into());
        env.insert("XDG_STATE_HOME".into(), "/logs/agent/mimo/xdg-state".into());
        if let Some(c) = self.skills_command() {
            exec_agent(cx, &c, &env).await?;
        }
        if let Some(c) = self.config_command() {
            exec_agent(cx, &c, &env).await?;
        }
        let flags = self.cli_flags();
        let flags = if flags.is_empty() { flags } else { flags + " " };
        let resume_flag = if resume { "--continue " } else { "" };
        let r = exec_agent(
            cx,
            &format!(
                "export PATH=\"$HOME/.mimocode/bin:$PATH\"; mimo --model={model} run --format=json {resume_flag}{flags}--thinking \
                 --dangerously-skip-permissions -- {} 2>&1 </dev/null | stdbuf -oL tee /logs/agent/mimo.txt",
                quote(&instruction)
            ),
            &env,
        )
        .await?;
        let msgs = error_messages(&r.stdout);
        if !msgs.is_empty() {
            return Err(kind_err("NonZeroAgentExitCodeError", format!("MiMo emitted error event(s): {}", msgs[..msgs.len().min(3)].join("; "))));
        }
        Ok(())
    }

    fn populate_context_post_run(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        if let Some(c) = std::fs::read_to_string(logs_dir.join("mimo.txt")).ok().and_then(|t| parse_usage(&t)) {
            ctx.n_input_tokens = c.n_input_tokens;
            ctx.n_cache_tokens = c.n_cache_tokens;
            ctx.n_output_tokens = c.n_output_tokens;
            ctx.cost_usd = c.cost_usd;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Arc;

    /// AgentArgs for tests (shared by this batch of ports).
    pub(crate) fn args(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> AgentArgs {
        let dir = std::env::temp_dir().join(format!("hbench-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("environment")).unwrap();
        std::fs::write(dir.join("task.toml"), "[environment]\ndocker_image='x'\n").unwrap();
        std::fs::write(dir.join("instruction.md"), "x").unwrap();
        let task = Arc::new(crate::task::Task::load(&dir).unwrap());
        AgentArgs {
            model_name: model.map(String::from),
            kwargs: serde_json::from_value(kwargs).unwrap(),
            extra_env: env.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            mcp_servers: vec![],
            skills: vec![],
            logs_dir: dir,
            task,
        }
    }

    fn mimo(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> MiMo {
        let a = args(kwargs, env, model);
        MiMo { version: None, mimo_config: a.kwargs.get("mimo_config").cloned().unwrap_or(json!({})), args: a }
    }

    #[test]
    fn config_and_flags() {
        let m = mimo(json!({"variant": "high", "disable_web_search": true, "mimo_config": {"permission": "allow", "experimental": {"x": 1}}}), &[("CUSTOM_BASE_URL", "https://custom.example.test/v1")], Some("custom/model"));
        let c = m.config();
        assert_eq!(c["provider"]["custom"]["options"]["baseURL"], "https://custom.example.test/v1");
        assert_eq!(c["provider"]["custom"]["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(c["permission"], json!({"*": "allow", "websearch": "deny", "webfetch": "deny"}));
        assert_eq!(c["experimental"]["x"], 1);
        assert_eq!(m.cli_flags(), "--variant high");
        assert!(mimo(json!({}), &[], Some("mimo/m")).config_command().is_none());
        assert!(create(args(json!({"mimo_config": "x"}), &[], None)).is_err());
    }

    #[test]
    fn usage_and_errors() {
        let text = r#"{"type":"step_start","timestamp":1,"sessionID":"s"}
{"type":"text","part":{"type":"text","text":"hi"}}
{"type":"step_finish","part":{"cost":0.5,"tokens":{"input":100,"output":10,"reasoning":3,"cache":{"read":40,"write":2}}}}
{"type":"step_finish","part":{"cost":9,"tokens":{"input":9}}}
{"type":"step_start"}
{"type":"step_finish","part":{"cost":0.25,"tokens":{"input":5,"output":1}}}
{"type":"error","error":{"name":"APIError","data":{"message":"boom"}}}"#;
        let c = parse_usage(text).unwrap();
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens, c.cost_usd), (Some(145), Some(40), Some(11), Some(0.75)));
        assert_eq!(error_messages(text), vec!["boom"]);
        assert!(parse_usage("{\"type\":\"text\"}").is_none());
    }
}
