//! Claude Code.

use super::common::{installed_dir, skills_dir};
use super::pins;
use super::{
    connection, ensure_acp_node_command, ensure_system_dependencies, exec_agent, exec_error, exec_root_checked, filter_jsonl_events,
    parse_version, pinned_bin_wrapper_command, safe_bin_symlink_command, upload_text,
};
use crate::agents::model_connection::Spec;
use crate::agents::{Agent, AgentArgs, AgentContext, AgentCtx, ModelUsage};
use crate::env::quote;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Zed ACP adapter version (bridges ACP <-> the Claude Agent SDK).
const SETTINGS_DIR: &str = "/tmp/claude-code-settings";
const SETTINGS_PATH: &str = "/tmp/claude-code-settings/settings.json";
const SPEC: Spec = Spec {
    default_provider: Some("anthropic"),
    api_key_envs: &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
    base_url_envs: &["ANTHROPIC_BASE_URL"],
    passthrough: false,
};

pub struct ClaudeCode {
    args: AgentArgs,
    version: Option<String>,
    settings: Option<Value>,
    /// load_trajectory is consumed by the first run only.
    loaded: bool,
    /// Native claude binary the ACP adapter must use (set by acp_install).
    acp_claude_executable: Option<String>,
}

fn env_flag_enabled(v: Option<String>) -> bool {
    matches!(v.as_deref().map(str::trim).map(str::to_lowercase).as_deref(), Some("1" | "true" | "yes"))
}

impl ClaudeCode {
    pub fn new(args: AgentArgs) -> Result<ClaudeCode> {
        let settings = match args.kwargs.get("config") {
            None | Some(Value::Null) => None,
            Some(Value::Object(o)) => Some(Value::Object(o.clone())),
            Some(Value::String(p)) => {
                let raw = std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("Agent config file not found: {p}: {e}"))?;
                let v: Value = serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("Invalid Claude Code settings file {p}: {e}"))?;
                if !v.is_object() {
                    bail!("Invalid Claude Code settings file {p}: expected a JSON object");
                }
                Some(v)
            }
            Some(_) => bail!("Invalid value for 'config': expected a local file path or JSON object"),
        };
        let a = ClaudeCode { version: args.version_kw(), args, settings, loaded: false, acp_claude_executable: None };
        a.cli_flags()?; // validate options eagerly
        Ok(a)
    }

    fn opt(&self, kw: &str, fallback_env: Option<&str>) -> Option<String> {
        self.args.kw(kw).or_else(|| fallback_env.and_then(|e| self.args.get_env(e)))
    }

    /// The claude-code options as CLI flags.
    fn cli_flags(&self) -> Result<String> {
        let mut f = vec![];
        let permission = self.args.kw("permission_mode").unwrap_or_else(|| "bypassPermissions".into());
        if !["default", "acceptEdits", "plan", "auto", "dontAsk", "bypassPermissions"].contains(&permission.as_str()) {
            bail!("Invalid value for 'permission_mode': '{permission}'");
        }
        f.push(format!("--permission-mode={permission}"));
        if let Some(v) = self.opt("max_turns", Some("CLAUDE_CODE_MAX_TURNS")) {
            let n: i64 = v.parse().map_err(|_| anyhow::anyhow!("Invalid value for 'max_turns': cannot parse '{v}' as an integer"))?;
            f.push(format!("--max-turns {n}"));
        }
        if let Some(v) = self.opt("reasoning_effort", Some("CLAUDE_CODE_EFFORT_LEVEL")) {
            if !["low", "medium", "high", "xhigh", "max"].contains(&v.as_str()) {
                bail!("Invalid value for 'reasoning_effort': '{v}'");
            }
            f.push(format!("--effort {v}"));
        }
        if let Some(v) = self.args.kw("thinking_display") {
            if !["summarized", "omitted"].contains(&v.as_str()) {
                bail!("Invalid value for 'thinking_display': '{v}'");
            }
            f.push(format!("--thinking-display {v}"));
        }
        for (kw, flag) in [
            ("max_budget_usd", "--max-budget-usd"),
            ("fallback_model", "--fallback-model"),
            ("append_system_prompt", "--append-system-prompt"),
            ("allowed_tools", "--allowedTools"),
        ] {
            if let Some(v) = self.args.kw(kw) {
                f.push(format!("{flag} {}", quote(&v)));
            }
        }
        let mut disallowed: Vec<String> = self.args.kw("disallowed_tools").into_iter().collect();
        if self.args.kw_bool("disable_web_search").unwrap_or(false) {
            disallowed.extend(["WebSearch".into(), "WebFetch".into()]);
        }
        if !disallowed.is_empty() {
            f.push(format!("--disallowedTools {}", quote(&disallowed.join(","))));
        }
        Ok(f.join(" "))
    }

    fn use_bedrock(&self) -> bool {
        self.args.get_env("AWS_BEARER_TOKEN_BEDROCK").is_some() || env_flag_enabled(self.args.get_env("CLAUDE_CODE_USE_BEDROCK"))
    }

    fn resolved_model(&self) -> Option<String> {
        let m = self.args.model_name.clone()?;
        if self.use_bedrock() {
            return Some(m.split_once('/').map(|(_, n)| n.to_string()).unwrap_or(m));
        }
        if connection(&self.args, SPEC).configured_base_url.is_some() {
            return Some(m);
        }
        Some(m.rsplit('/').next().unwrap().to_string())
    }

    /// Auth env: API key (default) or subscription OAuth token, or Bedrock.
    fn auth_env(&self) -> Result<BTreeMap<String, String>> {
        let bedrock = self.use_bedrock();
        let force_oauth = env_flag_enabled(self.args.get_env("CLAUDE_FORCE_OAUTH"));
        let oauth = self.args.get_env("CLAUDE_CODE_OAUTH_TOKEN").map(|s| s.trim().to_string()).unwrap_or_default();
        if force_oauth && oauth.is_empty() {
            bail!(
                "CLAUDE_FORCE_OAUTH is set but CLAUDE_CODE_OAUTH_TOKEN is not. Run `claude setup-token` to get one, or unset CLAUDE_FORCE_OAUTH."
            );
        }
        let conn = connection(&self.args, SPEC);
        let mut env = BTreeMap::new();
        if !(force_oauth || bedrock) {
            env.insert("ANTHROPIC_API_KEY".into(), conn.api_key.clone().unwrap_or_default());
        }
        if !bedrock {
            env.insert("ANTHROPIC_BASE_URL".into(), conn.configured_base_url.clone().unwrap_or_default());
        }
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".into(), oauth);
        if bedrock {
            env.insert("CLAUDE_CODE_USE_BEDROCK".into(), "1".into());
            for k in [
                "AWS_BEARER_TOKEN_BEDROCK",
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_SESSION_TOKEN",
                "AWS_PROFILE",
                "ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION",
            ] {
                env.insert(k.into(), self.args.get_env(k).unwrap_or_default());
            }
            env.insert("AWS_REGION".into(), self.args.get_env("AWS_REGION").unwrap_or_else(|| "us-east-1".into()));
            if self.args.get_env("DISABLE_PROMPT_CACHING").as_deref().map(str::trim) == Some("1") {
                env.insert("DISABLE_PROMPT_CACHING".into(), "1".into());
            }
        }
        env.retain(|_, v| !v.is_empty());
        Ok(env)
    }

    fn mcp_command(&self) -> Option<String> {
        if self.args.mcp_servers.is_empty() {
            return None;
        }
        let mut servers = serde_json::Map::new();
        for s in &self.args.mcp_servers {
            let v = if s.transport() == "stdio" {
                json!({"type": "stdio", "command": s.command, "args": s.args})
            } else {
                let t = if s.transport() == "streamable-http" { "http" } else { s.transport() };
                json!({"type": t, "url": s.url})
            };
            servers.insert(s.name.clone(), v);
        }
        let body = serde_json::to_string_pretty(&json!({"mcpServers": servers})).unwrap();
        Some(format!("echo {} > $CLAUDE_CONFIG_DIR/.claude.json", quote(&body)))
    }
}

fn session_text(m: &Value) -> String {
    match m {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|p| {
                if p["type"] == "text" && p["text"].as_str().is_some_and(|t| !t.is_empty()) {
                    p["text"].as_str().unwrap().to_string()
                } else {
                    "[image omitted]".into()
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Render an ATIF trajectory as a native Claude Code session (the user/assistant event chain
/// threaded by uuid/parentUuid so `claude --resume <id>` picks it up).
pub fn atif_to_native(traj: &Value, session_id: &str) -> String {
    let version = traj["agent"]["version"].as_str();
    let default_model = traj["agent"]["model_name"].clone();
    let mut parent: Option<String> = None;
    let mut last_ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let mut events: Vec<Value> = vec![];
    let mut base = |kind: &str, ts: &str| {
        let id = uuid::Uuid::new_v4().to_string();
        let mut e = json!({"parentUuid": parent, "isSidechain": false, "type": kind, "uuid": id, "timestamp": ts,
            "sessionId": session_id, "userType": "external", "cwd": "/app", "gitBranch": ""});
        if let Some(v) = version {
            e["version"] = json!(v);
        }
        parent = Some(id);
        e
    };
    for step in traj["steps"].as_array().into_iter().flatten() {
        if step["source"] == "system" {
            continue;
        }
        let ts = step["timestamp"].as_str().map(String::from).unwrap_or_else(|| last_ts.clone());
        last_ts = ts.clone();
        if step["source"] == "user" {
            let mut e = base("user", &ts);
            e["message"] = json!({"role": "user", "content": session_text(&step["message"])});
            events.push(e);
            continue;
        }
        let mut content = vec![];
        let text = session_text(&step["message"]);
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
        let calls: Vec<&Value> = step["tool_calls"].as_array().into_iter().flatten().collect();
        for c in &calls {
            content.push(json!({"type": "tool_use", "id": c["tool_call_id"], "name": c["function_name"], "input": c["arguments"]}));
        }
        if !content.is_empty() {
            let m = &step["metrics"];
            let mut e = base("assistant", &ts);
            e["message"] = json!({
                "model": if step["model_name"].is_null() { default_model.clone() } else { step["model_name"].clone() },
                "id": format!("msg_{}", uuid::Uuid::new_v4().simple()), "type": "message", "role": "assistant", "content": content,
                "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": m["prompt_tokens"].as_i64().unwrap_or(0), "output_tokens": m["completion_tokens"].as_i64().unwrap_or(0)},
            });
            events.push(e);
        }
        let results: Vec<&Value> = step["observation"]["results"].as_array().into_iter().flatten().collect();
        let blocks: Vec<Value> = calls
            .iter()
            .map(|c| {
                let r = results.iter().find(|r| !r["source_call_id"].is_null() && r["source_call_id"] == c["tool_call_id"]);
                let content = match r {
                    Some(r) => {
                        let raw = &r["extra"]["tool_result_metadata"]["raw_tool_result"];
                        if raw.is_object() && raw.get("content").is_some() {
                            raw["content"].clone()
                        } else {
                            json!(session_text(&r["content"]))
                        }
                    }
                    None => json!("[no output recorded]"),
                };
                json!({"tool_use_id": c["tool_call_id"], "type": "tool_result", "content": content})
            })
            .collect();
        if !blocks.is_empty() {
            let mut e = base("user", &ts);
            e["message"] = json!({"role": "user", "content": blocks});
            events.push(e);
        }
    }
    events.iter().map(|e| e.to_string() + "\n").collect()
}

/// Usage from the final stream-json `result` event (cost is authoritative there).
pub fn parse_stream_result(text: &str) -> Option<AgentContext> {
    let v: Value = text.lines().rev().filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok()).find(|v| v["type"] == "result")?;
    let u = &v["usage"];
    let n = |o: &Value, k: &str| o[k].as_i64().unwrap_or(0);
    let cache_read = n(u, "cache_read_input_tokens");
    let mut model_usage = BTreeMap::new();
    if let Some(mu) = v["modelUsage"].as_object() {
        for (model, m) in mu {
            model_usage.insert(
                model.clone(),
                ModelUsage {
                    n_input_tokens: n(m, "inputTokens") + n(m, "cacheReadInputTokens") + n(m, "cacheCreationInputTokens"),
                    n_cache_tokens: n(m, "cacheReadInputTokens"),
                    n_output_tokens: n(m, "outputTokens"),
                    cost_usd: m["costUSD"].as_f64(),
                },
            );
        }
    }
    Some(AgentContext {
        n_input_tokens: Some(n(u, "input_tokens") + n(u, "cache_creation_input_tokens") + cache_read),
        n_cache_tokens: Some(cache_read),
        n_output_tokens: Some(n(u, "output_tokens")),
        cost_usd: v["total_cost_usd"].as_f64(),
        model_usage: (!model_usage.is_empty()).then_some(model_usage),
        rollout_details: None,
        metadata: Some(
            json!({"num_turns": v["num_turns"], "is_error": v["is_error"], "duration_ms": v["duration_ms"], "subtype": v["subtype"], "session_id": v["session_id"]}),
        ),
    })
}

#[async_trait::async_trait]
impl Agent for ClaudeCode {
    fn name(&self) -> &str {
        "claude-code"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }
    fn supports_load(&self, _atif: bool) -> bool {
        true
    }

    fn acp_command(&self) -> Option<Vec<String>> {
        let mut c = vec!["env".to_string(), "-u".into(), "CLAUDECODE".into()];
        if let Some(m) = self.resolved_model() {
            c.push(format!("ANTHROPIC_MODEL={m}"));
            let lower = m.to_lowercase();
            if let Some(tier) = ["opus", "sonnet", "haiku"].iter().find(|t| lower.contains(*t)) {
                c.push(format!("ANTHROPIC_DEFAULT_{}_MODEL={m}", tier.to_uppercase()));
            }
        }
        c.push("CLAUDE_CONFIG_DIR=/logs/agent/sessions".into());
        if let Some(exe) = &self.acp_claude_executable {
            c.push(format!("CLAUDE_CODE_EXECUTABLE={exe}"));
        }
        if let Some(e) = self.opt("reasoning_effort", Some("CLAUDE_CODE_EFFORT_LEVEL")) {
            c.push(format!("CLAUDE_CODE_EFFORT_LEVEL={e}"));
        }
        if let Some(t) = self.opt("max_thinking_tokens", Some("MAX_THINKING_TOKENS")) {
            c.push(format!("MAX_THINKING_TOKENS={t}"));
        }
        c.push("claude-code-acp".into());
        Some(c)
    }

    fn acp_env(&self) -> BTreeMap<String, String> {
        self.auth_env().unwrap_or_default()
    }

    async fn acp_install(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        let sessions = "/logs/agent/sessions";
        // The adapter force-selects a model per session; settings.json is its only pre-session channel.
        let settings = match self.resolved_model() {
            Some(m) => format!("printf '%s' {} > {sessions}/settings.json; ", quote(&json!({"model": m}).to_string())),
            None => String::new(),
        };
        let r = exec_agent(
            cx,
            &format!(
                "set -euo pipefail; mkdir -p {sessions}; {settings}_hb_system_node=\"$(command -v node 2>/dev/null || true)\"; \
                 _hb_claude=\"$(command -v claude 2>/dev/null || true)\"; \
                 if [ -z \"$_hb_claude\" ] && [ -x \"$HOME/.local/bin/claude\" ]; then _hb_claude=\"$HOME/.local/bin/claude\"; fi; \
                 {} && npm install -g {adapter} && \
                 echo \"${{_hb_system_node:-none}}:$(command -v node):$(command -v claude-code-acp):${{_hb_claude:-none}}\"",
                ensure_acp_node_command(),
                adapter = pins::CLAUDE_CODE_ACP.npm_spec(None),
            ),
            &BTreeMap::new(),
        )
        .await?;
        let last = r.stdout.trim().lines().last().unwrap_or_default().to_string();
        let parts: Vec<&str> = last.split(':').collect();
        let [system_node, node, adapter, claude] = parts.as_slice() else {
            bail!("Could not locate node/claude-code-acp binaries for ACP mode (got {last:?})")
        };
        if node.is_empty() || adapter.is_empty() {
            bail!("Could not locate node/claude-code-acp binaries for ACP mode (got {last:?})");
        }
        if claude.is_empty() || *claude == "none" {
            bail!("Could not locate the native claude binary for ACP mode (got {last:?})");
        }
        self.acp_claude_executable = Some(claude.to_string());
        let mut link = pinned_bin_wrapper_command(node, adapter, "/usr/local/bin/claude-code-acp");
        if *system_node == "none" {
            link += &format!(" && {}", safe_bin_symlink_command(node, "/usr/local/bin/node"));
        }
        exec_root_checked(cx, &link, &BTreeMap::new()).await.map(|_| ())
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        let check = match &self.version {
            None => {
                cx.exec("export PATH=\"$HOME/.local/bin:$PATH\"; command -v claude >/dev/null 2>&1", &BTreeMap::new(), None).await?.code
                    == 0
            }
            Some(v) => {
                let r = cx.exec("export PATH=\"$HOME/.local/bin:$PATH\"; claude --version", &BTreeMap::new(), None).await?;
                r.code == 0 && parse_version(&r.stdout) == *v
            }
        };
        if !check {
            ensure_system_dependencies(cx, &["curl", "bash", "nodejs", "npm", "procps"]).await?;
            let npm = pins::CLAUDE_CODE.npm_spec(self.version.as_deref());
            let v = pins::CLAUDE_CODE.or(self.version.as_deref());
            exec_agent(
                cx,
                &format!(
                    "set -euo pipefail; if command -v apk &> /dev/null; then  npm install -g {npm}; \
                     else  curl -fsSL https://downloads.claude.ai/claude-code-releases/bootstrap.sh | bash -s -- {v}; fi && \
                     echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.bashrc && export PATH=\"$HOME/.local/bin:$PATH\" && claude --version"
                ),
                &BTreeMap::new(),
            )
            .await
            .map_err(|e| e.context("Agent install failed"))?;
        }
        if self.version.is_none() {
            let r = cx.exec("export PATH=\"$HOME/.local/bin:$PATH\"; claude --version", &BTreeMap::new(), None).await?;
            if r.code == 0 && !r.stdout.trim().is_empty() {
                self.version = Some(parse_version(&r.stdout));
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let mut env = self.auth_env()?;
        if let Some(v) = std::env::var("CLAUDE_CODE_MAX_OUTPUT_TOKENS").ok().filter(|v| !v.is_empty()) {
            env.insert("CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(), v);
        }
        if let Some(m) = self.resolved_model().or_else(|| self.args.get_env("ANTHROPIC_MODEL")) {
            env.insert("ANTHROPIC_MODEL".into(), m);
        }
        if env.contains_key("ANTHROPIC_BASE_URL")
            && let Some(m) = env.get("ANTHROPIC_MODEL").cloned()
        {
            for k in [
                "ANTHROPIC_DEFAULT_SONNET_MODEL",
                "ANTHROPIC_DEFAULT_OPUS_MODEL",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL",
                "CLAUDE_CODE_SUBAGENT_MODEL",
            ] {
                env.insert(k.into(), m.clone());
            }
        }
        if std::env::var("CLAUDE_CODE_DISABLE_ADAPTIVE_THINKING").is_ok_and(|v| v.trim() == "1") {
            env.insert("CLAUDE_CODE_DISABLE_ADAPTIVE_THINKING".into(), "1".into());
        }
        env.insert("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(), "1".into());
        env.insert("IS_SANDBOX".into(), "1".into()); // allows bypassPermissions as root
        if let Some(v) = self.opt("max_thinking_tokens", Some("MAX_THINKING_TOKENS")) {
            env.insert("MAX_THINKING_TOKENS".into(), v);
        }
        env.insert("CLAUDE_CONFIG_DIR".into(), "/logs/agent/sessions".into());

        let mut setup = "mkdir -p $CLAUDE_CONFIG_DIR/debug $CLAUDE_CONFIG_DIR/projects/-app $CLAUDE_CONFIG_DIR/shell-snapshots \
            $CLAUDE_CONFIG_DIR/statsig $CLAUDE_CONFIG_DIR/todos $CLAUDE_CONFIG_DIR/skills && \
            if [ -d ~/.claude/skills ]; then cp -r ~/.claude/skills/. $CLAUDE_CONFIG_DIR/skills/ 2>/dev/null || true; fi"
            .to_string();
        if self.settings.is_some() {
            setup += &format!(" && mkdir -p {SETTINGS_DIR}");
        }
        if let Some(d) = skills_dir(&self.args) {
            setup += &format!(" && (cp -r {}/* $CLAUDE_CONFIG_DIR/skills/ 2>/dev/null || true)", quote(&d));
        }
        if let Some(m) = self.args.kw("memory_dir") {
            setup += &format!(
                " && (mkdir -p $CLAUDE_CONFIG_DIR/projects/-app/memory && cp -r {}/* $CLAUDE_CONFIG_DIR/projects/-app/memory/ 2>/dev/null || true)",
                quote(&m)
            );
        }
        if let Some(m) = self.mcp_command() {
            setup += &format!(" && {m}");
        }
        exec_agent(cx, &setup, &env).await?;

        if let Some(s) = &self.settings {
            upload_text(cx, SETTINGS_PATH, &serde_json::to_string_pretty(s)?).await?;
        }
        let flags = self.cli_flags()?;
        let settings_flag = if self.settings.is_some() { format!("--settings {SETTINGS_PATH} ") } else { String::new() };
        let mut resume_flag = if resume { "--continue ".to_string() } else { String::new() };
        if let Some(src) = self.args.load_trajectory.clone().filter(|_| !self.loaded) {
            self.loaded = true;
            let (name, content) = if src.extension().is_some_and(|e| e == "json") {
                let traj: Value = serde_json::from_str(&std::fs::read_to_string(&src)?)?;
                let sid = traj["session_id"]
                    .as_str()
                    .filter(|s| uuid::Uuid::parse_str(s).is_ok())
                    .map(String::from)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                (format!("{sid}.jsonl"), atif_to_native(&traj, &sid))
            } else {
                let stem = src.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                if src.extension().is_none_or(|e| e != "jsonl") || uuid::Uuid::parse_str(&stem).is_err() {
                    bail!(
                        "Claude Code load_trajectory must be a native session file named <session-id>.jsonl or an ATIF trajectory .json file; got {:?}",
                        src.file_name().unwrap_or_default()
                    );
                }
                (format!("{stem}.jsonl"), std::fs::read_to_string(&src)?)
            };
            let target = format!("/logs/agent/sessions/projects/-app/{name}");
            upload_text(cx, &target, &content).await?;
            resume_flag = format!("--resume {} ", name.trim_end_matches(".jsonl"));
        }
        let var = format!("hbench_claude_code_instruction_{}", uuid::Uuid::new_v4().simple());
        let upper = var.to_uppercase();
        let mut run_env = env.clone();
        run_env.insert(upper.clone(), instruction);
        let cmd = format!(
            "export PATH=\"$HOME/.local/bin:$PATH\"; {var}=\"${upper}\"; unset {upper}; printf \"%s\" \"${var}\" | \
             claude --verbose --output-format=stream-json {settings_flag}{flags} {resume_flag}--print 2>&1 | tee /logs/agent/claude-code.txt"
        );
        let r = cx.exec(&format!("set -o pipefail; {cmd}"), &run_env, None).await?;
        if self.settings.is_some() {
            let _ = cx.exec(&format!("rm -rf {SETTINGS_DIR}"), &env, None).await;
        }
        if r.code != 0 {
            // Classify only harness-emitted events: model text and tool output can echo error phrases.
            let evidence = filter_jsonl_events(&r.stdout, |e| {
                e["type"] == "error"
                    || (e["type"] == "result" && (e["is_error"] == true || e["subtype"].as_str().is_some_and(|s| s.starts_with("error"))))
            });
            return Err(exec_error(&cmd, &r, Some(&format!("{evidence}\n{}", r.stderr))));
        }
        Ok(())
    }

    fn collect_usage(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        if let Some(c) = std::fs::read_to_string(logs_dir.join("claude-code.txt")).ok().and_then(|t| parse_stream_result(&t)) {
            *ctx = c;
        }
        // Write the ATIF trajectory and take totals from its final metrics.
        if let Some(t) = crate::atif::claude_code_trajectory(logs_dir) {
            let _ = std::fs::write(logs_dir.join("trajectory.json"), serde_json::to_string_pretty(&t).unwrap_or_default());
            let fm = &t["final_metrics"];
            if fm.is_object() {
                ctx.cost_usd = fm["total_cost_usd"].as_f64().or(ctx.cost_usd);
                ctx.n_input_tokens = Some(fm["total_prompt_tokens"].as_i64().unwrap_or(0));
                ctx.n_cache_tokens = Some(fm["total_cached_tokens"].as_i64().unwrap_or(0));
                ctx.n_output_tokens = Some(fm["total_completion_tokens"].as_i64().unwrap_or(0));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<ClaudeCode> {
        ClaudeCode::new(common::test_args(kwargs, env, model))
    }

    #[test]
    fn flags_and_auth() {
        let a = agent(
            json!({"max_turns": 5, "reasoning_effort": "high", "disable_web_search": true, "allowed_tools": "Bash,Edit"}),
            &[("ANTHROPIC_API_KEY", "sk")],
            Some("anthropic/claude-opus-5-5"),
        )
        .unwrap();
        let f = a.cli_flags().unwrap();
        assert!(f.contains("--permission-mode=bypassPermissions") && f.contains("--max-turns 5") && f.contains("--effort high"));
        assert!(f.contains("--disallowedTools WebSearch,WebFetch") && f.contains("--allowedTools Bash,Edit"));
        assert_eq!(a.resolved_model().unwrap(), "claude-opus-5-5");
        assert_eq!(a.auth_env().unwrap()["ANTHROPIC_API_KEY"], "sk");
        assert!(agent(json!({"reasoning_effort": "huge"}), &[], None).is_err());
        let o = agent(json!({}), &[("CLAUDE_CODE_OAUTH_TOKEN", "oat"), ("CLAUDE_FORCE_OAUTH", "1"), ("ANTHROPIC_API_KEY", "sk")], None)
            .unwrap();
        let e = o.auth_env().unwrap();
        assert_eq!(e.get("CLAUDE_CODE_OAUTH_TOKEN").map(String::as_str), Some("oat"));
        assert!(!e.contains_key("ANTHROPIC_API_KEY"));
        let b = agent(json!({}), &[("CLAUDE_CODE_USE_BEDROCK", "1")], Some("bedrock/us.anthropic.claude")).unwrap();
        assert_eq!(b.auth_env().unwrap()["AWS_REGION"], "us-east-1");
        assert_eq!(b.resolved_model().unwrap(), "us.anthropic.claude");
    }

    #[test]
    fn atif_round_trip_shape() {
        let traj = json!({"schema_version": "ATIF-v1.4", "session_id": "not-a-uuid", "agent": {"name": "x", "version": "2.0.0", "model_name": "m"},
        "steps": [
            {"step_id": 1, "source": "system", "message": "sys"},
            {"step_id": 2, "source": "user", "message": "create a file"},
            {"step_id": 3, "source": "agent", "message": "ok", "tool_calls": [{"tool_call_id": "t1", "function_name": "Bash", "arguments": {"command": "touch x"}}],
             "observation": {"results": [{"source_call_id": "t1", "content": "done"}]}, "metrics": {"prompt_tokens": 5, "completion_tokens": 2}}
        ]});
        let out = atif_to_native(&traj, "sid");
        let ev: Vec<Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(ev.len(), 3);
        assert_eq!(
            (ev[0]["type"].as_str(), ev[1]["type"].as_str(), ev[2]["type"].as_str()),
            (Some("user"), Some("assistant"), Some("user"))
        );
        assert!(ev[0]["parentUuid"].is_null() && ev[1]["parentUuid"] == ev[0]["uuid"] && ev[2]["parentUuid"] == ev[1]["uuid"]);
        assert_eq!(ev[1]["message"]["content"][1]["name"], "Bash");
        assert_eq!(ev[1]["message"]["usage"]["input_tokens"], 5);
        assert_eq!(ev[2]["message"]["content"][0]["content"], "done");
        assert_eq!(ev[0]["version"], "2.0.0");
    }

    #[test]
    fn stream_result_usage() {
        let text = "{\"type\":\"system\"}\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":3,\"total_cost_usd\":0.25,\
            \"usage\":{\"input_tokens\":10,\"cache_creation_input_tokens\":5,\"cache_read_input_tokens\":100,\"output_tokens\":7},\
            \"modelUsage\":{\"claude-x\":{\"inputTokens\":10,\"outputTokens\":7,\"cacheReadInputTokens\":100,\"cacheCreationInputTokens\":5,\"costUSD\":0.25}}}\n";
        let c = parse_stream_result(text).unwrap();
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens, c.cost_usd), (Some(115), Some(100), Some(7), Some(0.25)));
        assert_eq!(c.model_usage.unwrap()["claude-x"].n_input_tokens, 115);
    }
}
