//! OpenClaw (port of upstream's agents/installed/openclaw.py).

use super::mcode::kw_int;
use super::mimo::{deep_merge, skills_dir};
use super::{ensure_system_dependencies, exec_agent, exec_error, upload_text};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::{ExecResult, quote};
use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

/// upstream's `_openclaw_container_copy_session_transcript` source + call (sent via `python3 -c`).
const COPY_SESSION_SCRIPT: &str = include_str!("../../../assets/agents/openclaw/copy_session.py");
const SUPPORTED_PROVIDERS: [&str; 3] = ["anthropic", "nvidia", "openai"];
const INSTALL_TIMEOUT: Duration = Duration::from_secs(1200);

fn nvm22(cmd: &str) -> String {
    format!(". ~/.nvm/nvm.sh && nvm use 22 && {cmd}")
}

pub struct OpenClaw {
    args: AgentArgs,
    version: Option<String>,
    config_overlay: Value,
    failover_retries: Option<i64>,
    timeout: Option<i64>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(OpenClaw::new(args)?))
}

impl OpenClaw {
    fn new(args: AgentArgs) -> Result<OpenClaw> {
        let config_overlay = match args.kwargs.get("openclaw_config") {
            None | Some(Value::Null) => json!({}),
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => bail!("Invalid kwargs for agent 'openclaw':\n  openclaw_config: Input should be a valid dictionary"),
        };
        for k in ["disable_web_search", "session_to_trajectory"] {
            if args.kwargs.get(k).is_some_and(|v| !v.is_null()) && args.kw_bool(k).is_none() {
                bail!("Invalid kwargs for agent 'openclaw':\n  {k}: Input should be a valid boolean");
            }
        }
        let failover_retries = kw_int(&args, "failover_retries")?;
        if failover_retries.is_some_and(|n| n < 0) {
            bail!("failover_retries must be non-negative");
        }
        Ok(OpenClaw { timeout: kw_int(&args, "timeout")?, version: args.version_kw(), args, config_overlay, failover_retries })
    }

    fn provider(&self) -> Option<&str> {
        self.args.model_name.as_deref()?.split_once('/').map(|(p, _)| p)
    }

    fn cli_flags(&self) -> String {
        let mut f = vec![format!("--agent {}", quote(&self.args.kw("openclaw_agent_id").unwrap_or_else(|| "main".into())))];
        if let Some(t) = self.args.kw("thinking").or_else(|| self.args.kw("reasoning_effort")) {
            f.push(format!("--thinking {}", quote(&t)));
        }
        if let Some(n) = self.timeout {
            f.push(format!("--timeout {n}"));
        }
        f.join(" ")
    }

    /// `_build_full_openclaw_config`: baseline + overlay + MCP + provider + denies.
    fn full_config(&self) -> Value {
        let mut cfg = json!({"agents": {"defaults": {"workspace": "."}}, "gateway": {"mode": "local"}});
        deep_merge(&mut cfg, &self.config_overlay);
        if !self.args.mcp_servers.is_empty() {
            let mut servers = Map::new();
            for s in &self.args.mcp_servers {
                let v = match s.transport() {
                    "stdio" => {
                        let mut e = Map::new();
                        if let Some(c) = s.command.as_ref().filter(|c| !c.is_empty()) {
                            e.insert("command".into(), json!(c));
                        }
                        if !s.args.is_empty() {
                            e.insert("args".into(), json!(s.args));
                        }
                        Value::Object(e)
                    }
                    "sse" => json!({"url": s.url, "transport": "sse"}),
                    _ => json!({"url": s.url, "transport": "streamable-http"}),
                };
                servers.insert(s.name.clone(), v);
            }
            let mcp = cfg.as_object_mut().unwrap().entry("mcp").or_insert_with(|| json!({}));
            if !mcp.is_object() {
                *mcp = json!({});
            }
            let mut merged = mcp.get("servers").and_then(Value::as_object).cloned().unwrap_or_default();
            merged.extend(servers);
            mcp["servers"] = Value::Object(merged);
        }
        if let Some(provider) = self.provider() {
            let key = format!("{}_BASE_URL", provider.to_uppercase().replace('-', "_"));
            let base = self.args.get_env(&key).map(|s| s.trim().to_string()).unwrap_or_default();
            if !base.is_empty() {
                let o = cfg.as_object_mut().unwrap();
                let models = o.entry("models").or_insert_with(|| json!({}));
                let providers = models.as_object_mut().map(|m| m.entry("providers").or_insert_with(|| json!({})));
                if let Some(prov) = providers.and_then(|p| p.as_object_mut()).map(|p| p.entry(provider).or_insert_with(|| json!({})))
                    && let Some(prov) = prov.as_object_mut()
                    && !prov.contains_key("baseUrl")
                {
                    prov.insert("baseUrl".into(), json!(base));
                }
            }
            // `_normalize_provider_models_schema`: ensure a non-empty `models` array.
            if let Some(prov) = cfg.get_mut("models").and_then(|m| m.get_mut("providers")).and_then(|p| p.get_mut(provider)).and_then(Value::as_object_mut)
                && prov.get("models").and_then(Value::as_array).is_none_or(|a| a.is_empty())
            {
                prov.insert("models".into(), json!([{"id": self.args.model_name, "name": self.args.model_name}]));
            }
        }
        // Headless denies: keep user string entries (deduped), append "message".
        let tools = cfg.as_object_mut().unwrap().entry("tools").or_insert_with(|| json!({}));
        if !tools.is_object() {
            *tools = json!({});
        }
        let mut deny: Vec<String> = vec![];
        for d in tools["deny"].as_array().into_iter().flatten().filter_map(Value::as_str).chain(["message"]) {
            if !deny.iter().any(|x| x == d) {
                deny.push(d.into());
            }
        }
        if self.args.kw_bool("disable_web_search").unwrap_or(false) {
            deny.push("group:web".into());
        }
        tools["deny"] = json!(deny);
        if let Some(n) = self.failover_retries {
            let o = cfg.as_object_mut().unwrap();
            let auth = o.entry("auth").or_insert_with(|| json!({}));
            if !auth.is_object() {
                *auth = json!({});
            }
            if !auth["cooldowns"].is_object() {
                auth["cooldowns"] = json!({});
            }
            auth["cooldowns"]["rateLimitedProfileRotations"] = json!(n);
        }
        cfg
    }
}

/// `_load_json_object`: whole text as an object, else the last `{...}` that consumes the suffix.
pub fn load_envelope(raw: &str) -> Option<Map<String, Value>> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return v.as_object().cloned();
    }
    text.match_indices('{').rev().find_map(|(i, _)| serde_json::from_str::<Value>(&text[i..]).ok().and_then(|v| v.as_object().cloned()))
}

/// Final metrics from the CLI envelope's `meta.agentMeta.usage` (both upstream trajectory paths use it).
pub fn parse_usage(raw: &str) -> Option<AgentContext> {
    let env = load_envelope(raw).filter(|e| !e.is_empty())?;
    let usage = &env.get("meta").cloned().unwrap_or_default()["agentMeta"]["usage"];
    let n = |k: &str| match &usage[k] {
        Value::Number(x) => x.as_f64().unwrap_or(0.0) as i64,
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    };
    let (input, output, cache) = (n("input"), n("output"), n("cacheRead"));
    Some(AgentContext { n_input_tokens: Some(input + cache), n_output_tokens: Some(output), n_cache_tokens: Some(cache), cost_usd: None, ..Default::default() })
}

async fn exec_timeout(cx: &AgentCtx<'_>, cmd: &str) -> Result<ExecResult> {
    let r = cx.exec(&format!("set -o pipefail; {cmd}"), &BTreeMap::new(), Some(INSTALL_TIMEOUT)).await?;
    if r.code != 0 {
        return Err(exec_error(cmd, &r, None));
    }
    Ok(r)
}

#[async_trait::async_trait]
impl Agent for OpenClaw {
    fn name(&self) -> &str {
        "openclaw"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        cx.env.exec_root("[ -d /installed-agent ] || mkdir -p /installed-agent", None).await?;
        ensure_system_dependencies(cx, &["curl", "ca_certificates"]).await?;
        let pkg = quote(&format!("openclaw@{}", self.version.as_deref().unwrap_or("latest")));
        for cmd in [
            "set -o pipefail; retry_all=$(curl --help all 2>/dev/null | grep -q -- '--retry-all-errors' && echo '--retry-all-errors'); \
             curl -fsSL --retry 5 --retry-delay 2 $retry_all https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh | bash"
                .to_string(),
            "export NVM_DIR=\"${NVM_DIR:-$HOME/.nvm}\" && . \"$NVM_DIR/nvm.sh\" && nvm install 22".to_string(),
            nvm22("node -v && npm -v"),
            nvm22(&format!("npm install -g {pkg} --fetch-retries=5 --fetch-retry-mintimeout=20000 --fetch-retry-maxtimeout=120000")),
            nvm22("openclaw --version"),
        ] {
            exec_timeout(cx, &cmd).await.map_err(|e| anyhow::anyhow!("Agent install failed: {e:#}"))?;
        }
        if self.version.is_none()
            && let Ok(r) = cx.exec(&nvm22("openclaw --version"), &BTreeMap::new(), None).await
            && r.code == 0
            && !r.stdout.is_empty()
        {
            self.version = Some(r.stdout.trim().to_string());
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let Some(provider) = self.provider().map(String::from) else { bail!("Model name must be in the format provider/model_name") };
        if !SUPPORTED_PROVIDERS.contains(&provider.as_str()) {
            bail!(
                "Unsupported provider '{provider}'. Supported providers: ['anthropic', 'nvidia', 'openai']. Subclass OpenClaw and extend `_SUPPORTED_PROVIDERS` to add more."
            );
        }
        let prefix = provider.to_uppercase().replace('-', "_");
        let mut env = BTreeMap::new();
        for k in [format!("{prefix}_API_KEY"), format!("{prefix}_BASE_URL")] {
            if let Some(v) = self.args.get_env(&k) {
                env.insert(k, v);
            }
        }
        // upstream writes these into the mounted logs dir; hbench has no live mount, so upload them.
        let config = serde_json::to_string_pretty(&self.full_config())? + "\n";
        let _ = std::fs::write(cx.logs_dir.join("openclaw.upload.json"), &config);
        upload_text(cx, "/logs/agent/openclaw.upload.json", &config).await?;
        let _ = std::fs::write(cx.logs_dir.join("instruction.txt"), &instruction);
        let _ = upload_text(cx, "/logs/agent/instruction.txt", &instruction).await;

        exec_agent(cx, &nvm22("openclaw setup --baseline --workspace ."), &env).await?;
        exec_agent(cx, "mkdir -p ~/.openclaw && cp /logs/agent/openclaw.upload.json ~/.openclaw/openclaw.json", &env).await?;
        if let Some(d) = skills_dir(&self.args) {
            exec_agent(cx, &format!("mkdir -p ~/.openclaw/skills && cp -r {}/* ~/.openclaw/skills/ 2>/dev/null || true", quote(&d)), &env).await?;
        }
        let cmd = format!(
            ". ~/.nvm/nvm.sh && nvm use 22 && openclaw agent --local --json {} --model {} --message {} 2>&1 </dev/null | stdbuf -oL tee /logs/agent/openclaw.txt",
            self.cli_flags(),
            quote(self.args.model_name.as_deref().unwrap_or_default()),
            quote(&instruction)
        );
        exec_agent(cx, &cmd, &env).await?;
        // Best-effort copy of the native session transcript into /logs/agent.
        let _ = exec_agent(cx, &format!("python3 -c {}", quote(COPY_SESSION_SCRIPT)), &env).await;
        Ok(())
    }

    fn populate_context_post_run(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        if let Some(c) = std::fs::read_to_string(logs_dir.join("openclaw.txt")).ok().and_then(|t| parse_usage(&t)) {
            ctx.n_input_tokens = c.n_input_tokens;
            ctx.n_cache_tokens = c.n_cache_tokens;
            ctx.n_output_tokens = c.n_output_tokens;
            ctx.cost_usd = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::mimo::tests::args;

    fn oc(kwargs: Value, env: &[(&str, &str)]) -> OpenClaw {
        OpenClaw::new(args(kwargs, env, Some("openai/gpt-4.1"))).unwrap()
    }

    #[test]
    fn config_and_flags() {
        let a = oc(json!({"openclaw_config": {"agents": {"defaults": {"verboseDefault": "off"}}, "tools": {"deny": ["x", "message", 3]}}, "disable_web_search": true, "failover_retries": 2}), &[("OPENAI_BASE_URL", "https://proxy.example.com/v1")]);
        let c = a.full_config();
        assert_eq!(c["agents"]["defaults"], json!({"workspace": ".", "verboseDefault": "off"}));
        assert_eq!(c["models"]["providers"]["openai"], json!({"baseUrl": "https://proxy.example.com/v1", "models": [{"id": "openai/gpt-4.1", "name": "openai/gpt-4.1"}]}));
        assert_eq!(c["tools"]["deny"], json!(["x", "message", "group:web"]));
        assert_eq!(c["auth"]["cooldowns"]["rateLimitedProfileRotations"], 2);
        let user = oc(json!({"openclaw_config": {"models": {"providers": {"openai": {"baseUrl": "https://example.com/v1"}}}}}), &[("OPENAI_BASE_URL", "https://proxy")]);
        assert_eq!(user.full_config()["models"]["providers"]["openai"]["baseUrl"], "https://example.com/v1");
        assert_eq!(oc(json!({"reasoning_effort": "high", "timeout": 60}), &[]).cli_flags(), "--agent main --thinking high --timeout 60");
        assert!(OpenClaw::new(args(json!({"failover_retries": -1}), &[], None)).is_err());
        assert!(COPY_SESSION_SCRIPT.ends_with("shutil.copy2(src, dst)\n\n_openclaw_container_copy_session_transcript()\n"));
    }

    #[test]
    fn envelope_usage() {
        let raw = "[tools] raw_params={\"path\": \"/x\"}\n{\"payloads\": [{\"text\": \"ok\"}], \"meta\": {\"agentMeta\": {\"sessionId\": \"s\", \"usage\": {\"input\": 10, \"output\": 5, \"cacheRead\": 2}}}}\n";
        assert_eq!(load_envelope(raw).unwrap()["meta"]["agentMeta"]["sessionId"], "s");
        let c = parse_usage(raw).unwrap();
        assert_eq!((c.n_input_tokens, c.n_output_tokens, c.n_cache_tokens, c.cost_usd), (Some(12), Some(5), Some(2), None));
        assert_eq!(parse_usage("{\"payloads\": [], \"meta\": {}}").unwrap().n_input_tokens, Some(0));
        assert!(parse_usage("no json").is_none());
    }
}
