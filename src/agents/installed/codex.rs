//! Codex CLI, incl. ACP target mode and load_trajectory seeding.

use super::common::{detect_version, installed_dir, opts, parse_bool_env, skills_dir};
use super::{
    connection, ensure_acp_node_command, ensure_system_dependencies, exec_agent, exec_error, exec_root, filter_jsonl_events,
    pinned_bin_wrapper_command, safe_bin_symlink_command, upload_text,
};
use crate::agents::model_connection::Spec;
use crate::agents::pricing;
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SPEC: Spec = Spec { default_provider: Some("openai"), api_key_envs: &[], base_url_envs: &[], passthrough: false };
const CODEX_HOME: &str = "/installed-agent/codex-home";
const SECRETS_DIR: &str = "/tmp/codex-secrets";
const AUTH_PATH: &str = "/tmp/codex-secrets/auth.json";
const CONFIG_PATH: &str = "/installed-agent/codex-home/config.toml";
const ACP_CODEX_HOME: &str = "/installed-agent/codex-acp-home";
/// `@agentclientprotocol/codex-acp` adapter version.
const CODEX_ACP_VERSION: &str = "1.12.0";
const ROLLOUT_FILENAME_RE: &str = r"^rollout-(\d{4})-(\d{2})-(\d{2})T\S+\.jsonl$";
const VERSION_CMD: &str = "if [ -s ~/.nvm/nvm.sh ]; then . ~/.nvm/nvm.sh; fi; codex --version";

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(Codex::new(args)?))
}

pub struct Codex {
    args: AgentArgs,
    version: Option<String>,
    base_config: serde_json::Map<String, Value>,
    /// Parsed ATIF load_trajectory (None for native rollout files).
    atif_load: Option<Value>,
    /// load_trajectory is consumed by the first run only.
    loaded: bool,
    /// Codex CLI the ACP adapter must use (set by acp_install).
    acp_codex_executable: Option<String>,
}

impl Codex {
    fn new(args: AgentArgs) -> Result<Codex> {
        let base_config = load_base_config(args.kwargs.get("config"))?;
        let atif_load = validate_load_trajectory(args.load_trajectory.as_deref())?;
        let a = Codex { version: args.version_kw(), args, base_config, atif_load, loaded: false, acp_codex_executable: None };
        a.cli_flags()?;
        Ok(a)
    }

    /// `web_search`, forced to "disabled" by `disable_web_search`.
    fn web_search(&self) -> Result<Option<String>> {
        let web = opts::choice(&self.args, "web_search", None, &["disabled", "cached", "live"])?;
        Ok(if opts::boolean(&self.args, "disable_web_search", None)?.unwrap_or(false) { Some("disabled".into()) } else { web })
    }

    /// `_seed_load_trajectory` + `_upload_load_trajectory`: place the rollout under
    /// /logs/agent/sessions/<YYYY>/<MM>/<DD>/ (the resume path copies it into CODEX_HOME).
    async fn seed_load_trajectory(&self, cx: &AgentCtx<'_>) -> Result<()> {
        let Some(src) = self.args.load_trajectory.clone() else { bail!("load_trajectory is not set") };
        let tmp = std::env::temp_dir().join(format!("hbench-codex-load-{}", uuid::Uuid::new_v4().simple()));
        let source = match &self.atif_load {
            Some(traj) => {
                let (name, content) = atif_to_native_trajectory(traj, &atif_session_id(traj));
                std::fs::create_dir_all(&tmp)?;
                let p = tmp.join(name);
                std::fs::write(&p, content)?;
                p
            }
            None => src,
        };
        let r = async {
            let name = source.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let re = regex::Regex::new(ROLLOUT_FILENAME_RE).unwrap();
            let Some(m) = re.captures(&name) else { bail!("Invalid Codex rollout filename: {name:?}") };
            let dir = format!("/logs/agent/sessions/{}/{}/{}", &m[1], &m[2], &m[3]);
            exec_agent(cx, &format!("mkdir -p {}", quote(&dir)), &BTreeMap::new()).await?;
            let target = format!("{dir}/{name}");
            cx.env.upload_file(&source, &target).await?;
            if let Some(u) = &cx.user {
                exec_root(cx, &format!("chown {} {}", quote(u), quote(&target)), &BTreeMap::new()).await?;
            }
            Ok(())
        }
        .await;
        let _ = std::fs::remove_dir_all(&tmp);
        r
    }

    /// `acp_command`'s per-session `CODEX_CONFIG` (Python `json.dumps` formatting).
    fn acp_session_config(&self) -> Option<String> {
        let mut items: Vec<(&str, Value)> = vec![];
        if let Some(m) = &self.args.model_name {
            items.push(("model", json!(m.rsplit('/').next().unwrap())));
        }
        let a = &self.args;
        let effort =
            opts::choice(a, "reasoning_effort", None, &["none", "minimal", "low", "medium", "high", "xhigh", "max"]).ok().flatten();
        let summary = opts::choice(a, "reasoning_summary", None, &["auto", "concise", "detailed", "none"]).ok().flatten();
        for (k, v) in
            [("model_reasoning_effort", effort), ("model_reasoning_summary", summary), ("web_search", self.web_search().ok().flatten())]
        {
            if let Some(v) = v {
                items.push((k, json!(v)));
            }
        }
        if opts::boolean(a, "dangerously_bypass_hook_trust", None).ok().flatten().unwrap_or(false) {
            items.push(("bypass_hook_trust", json!(true)));
        }
        (!items.is_empty())
            .then(|| format!("{{{}}}", items.iter().map(|(k, v)| format!("{}: {v}", json!(k))).collect::<Vec<_>>().join(", ")))
    }

    /// CodexOptions -> CLI flags.
    fn cli_flags(&self) -> Result<String> {
        let a = &self.args;
        let mut f = vec![];
        if let Some(v) = opts::choice(a, "reasoning_effort", None, &["none", "minimal", "low", "medium", "high", "xhigh", "max"])? {
            f.push(format!("-c model_reasoning_effort={v}"));
        }
        if let Some(v) = opts::choice(a, "reasoning_summary", None, &["auto", "concise", "detailed", "none"])? {
            f.push(format!("-c model_reasoning_summary={v}"));
        }
        if let Some(v) = self.web_search()? {
            f.push(format!("-c web_search={v}"));
        }
        if opts::boolean(a, "dangerously_bypass_hook_trust", None)?.unwrap_or(false) {
            f.push("--dangerously-bypass-hook-trust".into());
        }
        Ok(f.join(" "))
    }

    /// Effective config: base config + runtime base URL + task MCP servers.
    fn effective_config(&self, base_url: Option<&str>) -> Result<serde_json::Map<String, Value>> {
        let mut c = self.base_config.clone();
        if let Some(u) = base_url {
            c.insert("openai_base_url".into(), json!(u));
        }
        if self.args.mcp_servers.is_empty() {
            return Ok(c);
        }
        let Some(servers) = c.entry("mcp_servers").or_insert_with(|| json!({})).as_object_mut() else {
            bail!("Invalid Codex config: mcp_servers must be a TOML table when task MCP servers are configured")
        };
        for s in &self.args.mcp_servers {
            let v = if s.transport() == "stdio" { json!({"command": s.command, "args": s.args}) } else { json!({"url": s.url}) };
            servers.insert(s.name.clone(), v);
        }
        Ok(c)
    }

    /// CODEX_AUTH_JSON_PATH=<path> or CODEX_FORCE_AUTH_JSON=<truthy> (~/.codex/auth.json); default OPENAI_API_KEY.
    fn auth_json_path(&self) -> Result<Option<PathBuf>> {
        if let Some(p) = self.args.get_env("CODEX_AUTH_JSON_PATH") {
            if !Path::new(&p).is_file() {
                bail!("CODEX_AUTH_JSON_PATH points to non-existent file: {p}");
            }
            return Ok(Some(p.into()));
        }
        if parse_bool_env(self.args.get_env("CODEX_FORCE_AUTH_JSON"), "CODEX_FORCE_AUTH_JSON")? {
            let d = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex/auth.json");
            if !d.is_file() {
                bail!("CODEX_FORCE_AUTH_JSON is set but {} does not exist", d.display());
            }
            return Ok(Some(d));
        }
        Ok(None)
    }

    /// `_ensure_agent_owned_dir`: private dir under root-owned /installed-agent.
    async fn ensure_agent_owned_dir(cx: &AgentCtx<'_>, path: &str) -> Result<()> {
        let r = exec_agent(cx, "printf \"\\nHBENCH_CODEX_ID %s %s\\n\" \"$(id -u)\" \"$(id -g)\"", &BTreeMap::new()).await?;
        let re = regex::Regex::new(r"(?m)^HBENCH_CODEX_ID ([0-9]+) ([0-9]+)$").unwrap();
        let c = re.captures(&r.stdout).ok_or_else(|| anyhow!("Could not resolve the agent uid/gid to create {path}"))?;
        let q = quote(path);
        exec_root(cx, &format!("mkdir -p {q} && chown -R {}:{} {q} && chmod 700 {q}", &c[1], &c[2]), &BTreeMap::new()).await?;
        Ok(())
    }
}

/// `config`: a TOML file path, or an inline JSON object that must round-trip through TOML.
fn load_base_config(config: Option<&Value>) -> Result<serde_json::Map<String, Value>> {
    match config {
        None | Some(Value::Null) => Ok(Default::default()),
        Some(Value::Object(o)) => {
            let back = toml::Value::try_from(o)
                .and_then(|t| toml::to_string(&t))
                .map_err(|e| anyhow!("Invalid inline Codex config for TOML conversion: {e}"))
                .and_then(|s| toml::from_str::<Value>(&s).map_err(|e| anyhow!("Invalid inline Codex config for TOML conversion: {e}")))?;
            if back != Value::Object(o.clone()) {
                bail!("Invalid inline Codex config: JSON object cannot be represented losslessly as TOML");
            }
            Ok(o.clone())
        }
        Some(Value::String(p)) => {
            if !Path::new(p).is_file() {
                bail!("Agent config file not found: {p}");
            }
            let text = std::fs::read_to_string(p).map_err(|e| anyhow!("Invalid Codex config file {p}: {e}"))?;
            match toml::from_str::<Value>(&text).map_err(|e| anyhow!("Invalid Codex config file {p}: {e}"))? {
                Value::Object(o) => Ok(o),
                _ => bail!("Invalid Codex config file {p}: expected a TOML table"),
            }
        }
        Some(_) => bail!("Invalid value for 'config': expected a local file path or JSON object"),
    }
}

fn parse_codex_version(stdout: &str) -> String {
    let text = stdout.trim();
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.strip_prefix("codex-cli").unwrap_or(l).trim().to_string())
        .unwrap_or_else(|| text.to_string())
}

/// `load_trajectory` validation: `.json` must be an ATIF trajectory (returned parsed);
/// anything else must be a native rollout file name.
fn validate_load_trajectory(path: Option<&Path>) -> Result<Option<Value>> {
    let Some(path) = path else { return Ok(None) };
    if path.extension().is_some_and(|e| e == "json") {
        let text = std::fs::read_to_string(path).map_err(|e| anyhow!("load_trajectory file not found: {}: {e}", path.display()))?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("load_trajectory is not a valid ATIF trajectory file ({}): {e}", path.display()))?;
        if !v["steps"].is_array() {
            bail!("load_trajectory is not a valid ATIF trajectory file ({}): missing steps", path.display());
        }
        return Ok(Some(v));
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if !regex::Regex::new(ROLLOUT_FILENAME_RE).unwrap().is_match(&name) {
        bail!(
            "Codex load_trajectory must be a native rollout file named rollout-<timestamp>-<session-id>.jsonl or an ATIF trajectory .json file; got {name:?}"
        );
    }
    Ok(None)
}

/// Reuse the trajectory's session id when it is a UUID; else mint one.
fn atif_session_id(traj: &Value) -> String {
    traj["session_id"]
        .as_str()
        .filter(|s| uuid::Uuid::parse_str(s).is_ok())
        .map(String::from)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

fn rollout_text(m: &Value) -> String {
    match m {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|p| match p["text"].as_str() {
                Some(t) if p["type"] == "text" && !t.is_empty() => t.to_string(),
                _ => "[image omitted]".into(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The `custom_tool_call` input when the recorded call was free-form; None = structured `function_call`.
fn rollout_custom_tool_input(step: &Value, call: &Value) -> Option<String> {
    let entry = &step["extra"]["tool_call_details"][call["tool_call_id"].as_str().unwrap_or_default()];
    let raw = entry["raw_arguments"].as_str()?;
    if !entry["item_type"].is_null() {
        return (entry["item_type"] == "custom_tool_call").then(|| raw.to_string());
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v) if v == call["arguments"] => None,
        _ => Some(raw.to_string()),
    }
}

/// Session start: first step timestamp (parse failure or none -> now).
fn rollout_timestamp(traj: &Value) -> chrono::DateTime<chrono::FixedOffset> {
    let now = || chrono::Utc::now().fixed_offset();
    let Some(ts) = traj["steps"].as_array().into_iter().flatten().find_map(|s| s["timestamp"].as_str().filter(|t| !t.is_empty())) else {
        return now();
    };
    chrono::DateTime::parse_from_rfc3339(ts)
        // ponytail: naive timestamps taken as UTC (Python would use local time).
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.f").map(|n| n.and_utc().fixed_offset()))
        .unwrap_or_else(|_| now())
}

/// Render an ATIF trajectory as a native Codex rollout (`session_meta`, `response_item`,
/// `event_msg` lines) so `codex exec resume` picks it up. Returns (filename, content).
pub fn atif_to_native_trajectory(traj: &Value, session_id: &str) -> (String, String) {
    let ts = rollout_timestamp(traj);
    let filename = format!("rollout-{}-{session_id}.jsonl", ts.format("%Y-%m-%dT%H-%M-%S"));
    let iso = ts.with_timezone(&chrono::Utc).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let mut lines = vec![json!({"timestamp": iso, "type": "session_meta", "payload": {
        "session_id": session_id, "id": session_id, "timestamp": iso, "cwd": "/app", "originator": "codex_exec",
        "cli_version": traj["agent"]["version"].as_str().filter(|v| !v.is_empty()).unwrap_or("unknown"), "source": "exec", "thread_source": "user"}})];
    let item = |lines: &mut Vec<Value>, payload: Value, ts: &str| {
        lines.push(json!({"timestamp": ts, "type": "response_item", "payload": payload}))
    };
    let event =
        |lines: &mut Vec<Value>, payload: Value, ts: &str| lines.push(json!({"timestamp": ts, "type": "event_msg", "payload": payload}));
    let message = |lines: &mut Vec<Value>, role: &str, text: &str, ts: &str| {
        let ct = if role == "assistant" { "output_text" } else { "input_text" };
        item(lines, json!({"type": "message", "role": role, "content": [{"type": ct, "text": text}]}), ts)
    };
    let mut last = iso.clone();
    for step in traj["steps"].as_array().into_iter().flatten() {
        let ts = step["timestamp"].as_str().filter(|t| !t.is_empty()).map(String::from).unwrap_or_else(|| last.clone());
        last = ts.clone();
        let text = rollout_text(&step["message"]);
        if step["source"] == "system" {
            message(&mut lines, "developer", &text, &ts);
            continue;
        }
        if step["source"] == "user" {
            message(&mut lines, "user", &text, &ts);
            // `codex exec resume` only lists threads carrying a user_message event.
            event(&mut lines, json!({"type": "user_message", "message": text}), &ts);
            continue;
        }
        if !text.is_empty() {
            message(&mut lines, "assistant", &text, &ts);
            event(&mut lines, json!({"type": "agent_message", "message": text}), &ts);
        }
        let calls: Vec<&Value> = step["tool_calls"].as_array().into_iter().flatten().collect();
        let mut custom = vec![];
        for c in &calls {
            let payload = match rollout_custom_tool_input(step, c) {
                Some(input) => {
                    custom.push(c["tool_call_id"].clone());
                    json!({"type": "custom_tool_call", "status": "completed", "call_id": c["tool_call_id"], "name": c["function_name"], "input": input})
                }
                None => {
                    json!({"type": "function_call", "call_id": c["tool_call_id"], "name": c["function_name"], "arguments": c["arguments"].to_string()})
                }
            };
            item(&mut lines, payload, &ts);
        }
        let results: Vec<&Value> = step["observation"]["results"].as_array().into_iter().flatten().collect();
        for c in &calls {
            let id = &c["tool_call_id"];
            // Last result per call id wins.
            let output = results
                .iter()
                .rev()
                .find(|r| !r["source_call_id"].is_null() && r["source_call_id"] == *id)
                .map_or("[no output recorded]".to_string(), |r| rollout_text(&r["content"]));
            let kind = if custom.contains(id) { "custom_tool_call_output" } else { "function_call_output" };
            item(&mut lines, json!({"type": kind, "call_id": id, "output": output}), &ts);
        }
    }
    (filename, lines.iter().map(|l| l.to_string() + "\n").collect())
}

/// Deepest, lexicographically last session date dir under `logs_dir/sessions`, then its last rollout file.
fn latest_session_file(logs_dir: &Path) -> Option<PathBuf> {
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.push(p.clone());
                walk(&p, out);
            }
        }
    }
    let mut dirs = vec![];
    walk(&logs_dir.join("sessions"), &mut dirs);
    let depth = dirs.iter().map(|d| d.components().count()).max()?;
    let dir = dirs.into_iter().filter(|d| d.components().count() == depth).max()?;
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).max()
}

/// Totals from the last `token_count` event (the rollout's ATIF final_metrics).
/// Cost: Codex's own `total_cost`/`cost_usd` when present, else the sum of per-API-call LiteLLM
/// estimates (None if any call is unpriced). `model` is the fallback when no `turn_context` names one.
pub fn parse_rollout_usage(text: &str, model: Option<&str>) -> Option<AgentContext> {
    let events: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l.trim()).ok()).collect();
    let model = events.iter().find_map(|e| (e["type"] == "turn_context").then(|| e["payload"]["model"].as_str()).flatten()).or(model);
    // A token_count event closes one API call if the model produced output since the last one.
    let (mut saw_output, mut costs) = (false, vec![]);
    for e in &events {
        let p = &e["payload"];
        if e["type"] == "event_msg" && p["type"] == "token_count" {
            if !std::mem::take(&mut saw_output) {
                continue;
            }
            let u = &p["info"]["last_token_usage"];
            if p["info"].is_object() && u.is_object() {
                let n = |k: &str| u[k].as_i64().unwrap_or(0);
                costs.push(model.and_then(|m| {
                    pricing::total_cost(m, n("input_tokens"), n("output_tokens"), n("cache_write_input_tokens"), n("cached_input_tokens"))
                }));
            }
        } else if e["type"] == "response_item"
            && match p["type"].as_str() {
                Some("message") => p["role"] == "assistant",
                Some("web_search_call") => true,
                Some("function_call" | "custom_tool_call") => p["call_id"].as_str().is_some_and(|c| !c.is_empty()),
                _ => false,
            }
        {
            saw_output = true;
        }
    }
    let estimate: Option<f64> = if costs.is_empty() { None } else { costs.into_iter().sum() };
    let step_types = ["message", "web_search_call", "function_call", "custom_tool_call", "function_call_output", "custom_tool_call_output"];
    if !events.iter().any(|e| e["type"] == "response_item" && e["payload"]["type"].as_str().is_some_and(|t| step_types.contains(&t))) {
        return None; // no ATIF steps -> no trajectory
    }
    let info = events
        .iter()
        .rev()
        .filter(|e| e["type"] == "event_msg" && e["payload"]["type"] == "token_count")
        .map(|e| &e["payload"]["info"])
        .find(|i| i["total_token_usage"].is_object())?;
    let t = &info["total_token_usage"];
    let n = |k: &str| t[k].as_i64().unwrap_or(0);
    let cost = info["total_cost"].as_f64().or_else(|| info["cost_usd"].as_f64()).or(estimate);
    Some(AgentContext {
        n_input_tokens: Some(n("input_tokens")),
        n_cache_tokens: Some(n("cached_input_tokens")),
        n_output_tokens: Some(n("output_tokens")),
        cost_usd: cost,
        ..Default::default()
    })
}

#[async_trait::async_trait]
impl Agent for Codex {
    fn name(&self) -> &str {
        "codex"
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
        let mut c: Vec<String> =
            ["env", "NO_BROWSER=1", "INITIAL_AGENT_MODE=agent-full-access", "DEFAULT_AUTH_REQUEST={\"methodId\":\"api-key\"}"]
                .map(String::from)
                .into();
        c.push(format!("CODEX_HOME={ACP_CODEX_HOME}"));
        if let Some(cfg) = self.acp_session_config() {
            c.push(format!("CODEX_CONFIG={cfg}"));
        }
        if let Some(exe) = &self.acp_codex_executable {
            c.push(format!("CODEX_PATH={exe}"));
        }
        c.push("codex-acp".into());
        Some(c)
    }

    /// Keep the adapter's credential separate from an OpenAI user agent's key.
    fn acp_env(&self) -> BTreeMap<String, String> {
        connection(&self.args, SPEC)
            .api_key
            .filter(|k| !k.is_empty())
            .map(|k| BTreeMap::from([("CODEX_API_KEY".to_string(), k)]))
            .unwrap_or_default()
    }

    async fn acp_install(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        if self.args.model_name.is_none() {
            bail!("Model name is required");
        }
        if self.auth_json_path()?.is_some() {
            bail!("Codex auth.json authentication is not supported in ACP mode");
        }
        Self::ensure_agent_owned_dir(cx, ACP_CODEX_HOME).await?;
        let r = exec_agent(
            cx,
            &format!(
                "set -euo pipefail; _hb_system_node=\"$(command -v node 2>/dev/null || true)\"; \
                 if [ -s \"$HOME/.nvm/nvm.sh\" ]; then . \"$HOME/.nvm/nvm.sh\"; fi; \
                 _hb_codex=\"$(command -v codex 2>/dev/null || true)\"; \
                 if [ -z \"$_hb_codex\" ]; then echo \"Could not locate the installed Codex binary for ACP mode\" >&2; exit 1; fi; \
                 {} && npm install -g @agentclientprotocol/codex-acp@{CODEX_ACP_VERSION} && \
                 echo \"${{_hb_system_node:-none}}:$(command -v node):$(command -v codex-acp):${{_hb_codex}}\"",
                ensure_acp_node_command()
            ),
            &BTreeMap::new(),
        )
        .await?;
        let last = r.stdout.trim().lines().last().unwrap_or_default().to_string();
        let fields: Vec<&str> = last.split(':').collect();
        let [system_node, node, adapter, codex] = fields.as_slice() else {
            bail!("Could not locate node/codex-acp/codex binaries for ACP mode")
        };
        if node.is_empty() || adapter.is_empty() || codex.is_empty() {
            bail!("Could not locate node/codex-acp/codex binaries for ACP mode");
        }
        // Otherwise the adapter silently uses its bundled Codex version.
        self.acp_codex_executable = Some(codex.to_string());
        let mut cmd = pinned_bin_wrapper_command(node, adapter, "/usr/local/bin/codex-acp");
        if *system_node == "none" {
            cmd += &format!(" && {}", safe_bin_symlink_command(node, "/usr/local/bin/node"));
        }
        exec_root(cx, &cmd, &BTreeMap::new()).await?;

        let mut config = self.effective_config(connection(&self.args, SPEC).configured_base_url.as_deref())?;
        // API-key login can persist credentials; keep them in the temporary home, not a keyring.
        config.insert("cli_auth_credentials_store".into(), json!("file"));
        upload_text(cx, &format!("{ACP_CODEX_HOME}/config.toml"), &toml::to_string(&toml::Value::try_from(&config)?)?).await?;
        if let Some(d) = skills_dir(&self.args) {
            exec_agent(
                cx,
                &format!("mkdir -p $HOME/.agents/skills && cp -r {}/* $HOME/.agents/skills/ 2>/dev/null || true", quote(&d)),
                &BTreeMap::new(),
            )
            .await?;
        }
        Ok(())
    }

    async fn acp_teardown(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        let home = quote(ACP_CODEX_HOME);
        let r = exec_agent(
            cx,
            &format!("if [ -d {home}/sessions ]; then mkdir -p /logs/agent && rm -rf /logs/agent/sessions && cp -R {home}/sessions /logs/agent/sessions; fi"),
            &BTreeMap::new(),
        )
        .await;
        // Bridge runs skip native cleanup; root owns the home's parent.
        exec_root(cx, &format!("rm -rf {home}"), &BTreeMap::new()).await?;
        r.map(|_| ())
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        let none = BTreeMap::new();
        let satisfied = match &self.version {
            None => {
                cx.exec("if [ -s ~/.nvm/nvm.sh ]; then . ~/.nvm/nvm.sh; fi; command -v codex >/dev/null 2>&1", &none, None).await?.code == 0
            }
            Some(v) => {
                let r = cx.exec(VERSION_CMD, &none, None).await?;
                r.code == 0 && parse_codex_version(&r.stdout) == *v
            }
        };
        if !satisfied {
            ensure_system_dependencies(cx, &["curl", "bash", "nodejs", "npm", "ripgrep"]).await?;
            let spec = format!("@{}", self.version.as_deref().unwrap_or("latest"));
            exec_agent(
                cx,
                &format!(
                    "set -euo pipefail; if ldd --version 2>&1 | grep -qi musl || [ -f /etc/alpine-release ]; then  npm install -g @openai/codex{spec}; \
                     else  curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh | bash &&  export NVM_DIR=\"$HOME/.nvm\" &&  \
                     \\. \"$NVM_DIR/nvm.sh\" || true &&  command -v nvm &>/dev/null || {{ echo 'Error: NVM failed to load' >&2; exit 1; }} &&  \
                     nvm install 22 && nvm alias default 22 && npm -v &&  npm install -g @openai/codex{spec}; fi && codex --version"
                ),
                &BTreeMap::from([("NVM_NODEJS_ORG_MIRROR".to_string(), "https://nodejs.org/dist".to_string())]),
            )
            .await?;
            exec_root(
                cx,
                "for bin in node codex; do  BIN_PATH=\"$(which \"$bin\" 2>/dev/null || true)\";  \
                 if [ -n \"$BIN_PATH\" ] && [ \"$BIN_PATH\" != \"/usr/local/bin/$bin\" ]; then    ln -sf \"$BIN_PATH\" \"/usr/local/bin/$bin\";  fi; done",
                &none,
            )
            .await?;
        }
        if self.version.is_none() {
            self.version = detect_version(cx, VERSION_CMD, parse_codex_version).await;
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let Some(model_name) = &self.args.model_name else { bail!("Model name is required") };
        let model = model_name.rsplit('/').next().unwrap().to_string();
        let flags = self.cli_flags()?;
        let flags_arg = if flags.is_empty() { String::new() } else { format!("{flags} ") };
        let auth_json = self.auth_json_path()?;
        let conn = connection(&self.args, SPEC);

        let mut env = BTreeMap::from([("CODEX_HOME".to_string(), CODEX_HOME.to_string())]);
        Self::ensure_agent_owned_dir(cx, CODEX_HOME).await?;
        exec_agent(cx, &format!("mkdir -p \"$CODEX_HOME\" {} /logs/agent", quote(SECRETS_DIR)), &env).await?;
        let load = self.args.load_trajectory.is_some() && !std::mem::replace(&mut self.loaded, true);
        if load {
            self.seed_load_trajectory(cx).await?;
        }
        let resume = resume || load;

        let mut setup = if let Some(p) = &auth_json {
            cx.env.upload_file(p, AUTH_PATH).await?;
            if let Some(u) = &cx.user {
                exec_root(cx, &format!("chown {u} {AUTH_PATH}"), &BTreeMap::new()).await?;
            }
            format!("ln -sf {} \"$CODEX_HOME/auth.json\"\n", quote(AUTH_PATH))
        } else {
            env.insert("OPENAI_API_KEY".into(), conn.api_key.clone().unwrap_or_default());
            format!(
                "cat >{q} <<EOF\n{{\n  \"OPENAI_API_KEY\": \"${{OPENAI_API_KEY}}\"\n}}\nEOF\nln -sf {q} \"$CODEX_HOME/auth.json\"\n",
                q = quote(AUTH_PATH)
            )
        };
        if let Some(u) = &conn.configured_base_url {
            env.insert("OPENAI_BASE_URL".into(), u.clone());
        }
        // codex only honors openai_base_url from config.toml, not the env var.
        let config = self.effective_config(conn.configured_base_url.as_deref())?;
        if !config.is_empty() {
            upload_text(cx, CONFIG_PATH, &toml::to_string(&toml::Value::try_from(&config)?)?).await?;
        }
        if let Some(d) = skills_dir(&self.args) {
            setup += &format!("\nmkdir -p $HOME/.agents/skills && cp -r {}/* $HOME/.agents/skills/ 2>/dev/null || true", quote(&d));
        }
        if resume {
            setup += "\nif [ ! -d /logs/agent/sessions ]; then\n  echo \"Cannot resume Codex: no previous session logs found\" >&2\n  exit 1\nfi\n\
                      rm -rf \"$CODEX_HOME/sessions\"\ncp -R /logs/agent/sessions \"$CODEX_HOME/sessions\"";
        }
        exec_agent(cx, &setup, &env).await?;

        let cmd = format!(
            "if [ -s ~/.nvm/nvm.sh ]; then . ~/.nvm/nvm.sh; fi; codex exec {}--dangerously-bypass-approvals-and-sandbox --skip-git-repo-check \
             --model {model} --json --enable unified_exec {flags_arg}-- {} 2>&1 </dev/null | tee /logs/agent/codex.txt",
            if resume { "resume --last " } else { "" },
            quote(&instruction)
        );
        let r = cx.exec(&format!("set -o pipefail; {cmd}"), &env, None).await;
        // Always persist sessions and drop secrets, even on failure.
        let _ = cx
            .exec(
                "set -o pipefail; mkdir -p /logs/agent\nif [ -d \"$CODEX_HOME/sessions\" ]; then\n  rm -rf /logs/agent/sessions\n  cp -R \"$CODEX_HOME/sessions\" /logs/agent/sessions\nfi",
                &env,
                None,
            )
            .await;
        let _ = cx.exec_root(&format!("set -o pipefail; rm -rf {} {}", quote(SECRETS_DIR), quote(CODEX_HOME)), &BTreeMap::new()).await;
        let r = r?;
        if r.code != 0 {
            // Classify from harness-emitted events only; item.* events carry tool output and model text.
            let stdout = filter_jsonl_events(&r.stdout, |e| e["type"] == "turn.failed" || e["type"] == "error");
            return Err(exec_error(&cmd, &r, Some(&format!("{stdout}\n{}", r.stderr))));
        }
        Ok(())
    }

    fn populate_context_post_run(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        if let Some(c) = latest_session_file(logs_dir)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| parse_rollout_usage(&t, self.args.model_name.as_deref()))
        {
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
    use crate::agents::installed::common;

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<Codex> {
        Codex::new(common::test_args(kwargs, env, model))
    }

    #[test]
    fn flags_config_and_validation() {
        let a = agent(json!({"reasoning_effort": "high", "reasoning_summary": "auto", "web_search": "live", "disable_web_search": true, "dangerously_bypass_hook_trust": true}), &[], Some("openai/gpt-5")).unwrap();
        assert_eq!(
            a.cli_flags().unwrap(),
            "-c model_reasoning_effort=high -c model_reasoning_summary=auto -c web_search=disabled --dangerously-bypass-hook-trust"
        );
        assert!(agent(json!({"reasoning_effort": "huge"}), &[], None).is_err());

        let mut args = common::test_args(
            json!({"config": {"model_verbosity": "low", "mcp_servers": {"keep": {"url": "http://k"}}}}),
            &[],
            Some("openai/gpt-5"),
        );
        args.mcp_servers =
            vec![serde_json::from_value(json!({"name": "fs", "transport": "stdio", "command": "npx", "args": ["fs"]})).unwrap()];
        let a = Codex::new(args).unwrap();
        let c = a.effective_config(Some("http://proxy/v1")).unwrap();
        assert_eq!(c["openai_base_url"], "http://proxy/v1");
        assert_eq!(c["mcp_servers"]["fs"], json!({"command": "npx", "args": ["fs"]}));
        assert_eq!(c["mcp_servers"]["keep"]["url"], "http://k");
        assert!(toml::to_string(&toml::Value::try_from(&c).unwrap()).unwrap().contains("[mcp_servers.fs]"));
        assert!(agent(json!({"config": {"a": null}}), &[], None).is_err());
        assert!(parse_bool_env(Some("maybe".into()), "X").is_err());
        assert_eq!(parse_codex_version("codex-cli 0.118.0\n"), "0.118.0");
    }

    #[test]
    fn rollout_usage() {
        let text = r#"{"timestamp":"t","type":"session_meta","payload":{"id":"s1","cli_version":"0.118.0"}}
{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}
{"timestamp":"t","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":7,"reasoning_output_tokens":3,"total_tokens":107},"last_token_usage":{"input_tokens":100}}}}
{"timestamp":"t","type":"event_msg","payload":{"type":"token_count","info":null}}
not json
"#;
        let c = parse_rollout_usage(text, None).unwrap();
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens, c.cost_usd), (Some(100), Some(40), Some(7), None));
        assert!(
            parse_rollout_usage(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{}}}}",
                None
            )
            .is_none()
        );

        let dir = std::env::temp_dir().join(format!("hbench-codex-{}", uuid::Uuid::new_v4()));
        let day = dir.join("sessions/2026/10/10");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::create_dir_all(dir.join("sessions/2026/09/01")).unwrap();
        std::fs::write(day.join("rollout-2026-10-10T00-00-00-a.jsonl"), text).unwrap();
        assert_eq!(latest_session_file(&dir).unwrap(), day.join("rollout-2026-10-10T00-00-00-a.jsonl"));
    }

    #[test]
    fn litellm_cost_estimate() {
        // gpt-5: in 1.25e-6, cache read 1.25e-7, out 1e-5 (pinned LiteLLM table).
        let text = r#"{"type":"turn_context","payload":{"model":"gpt-5"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[]}}
{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":999999,"output_tokens":999}}}}
{"type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"shell","arguments":"{}"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100}}}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":3000,"cached_input_tokens":400,"output_tokens":150},"last_token_usage":{"input_tokens":2000,"output_tokens":50}}}}
"#;
        let c = parse_rollout_usage(text, Some("openai/not-priced")).unwrap();
        let want = (600.0 * 1.25e-6 + 400.0 * 1.25e-7 + 100.0 * 1e-5) + (2000.0 * 1.25e-6 + 50.0 * 1e-5);
        assert!((c.cost_usd.unwrap() - want).abs() < 1e-12 && (want - 0.0048).abs() < 1e-12);
        // No turn_context: fall back to the configured model (provider prefix stripped for lookup).
        let no_ctx = text.lines().skip(1).collect::<Vec<_>>().join("\n");
        assert!((parse_rollout_usage(&no_ctx, Some("openai/gpt-5")).unwrap().cost_usd.unwrap() - want).abs() < 1e-12);
        assert_eq!(parse_rollout_usage(&no_ctx, Some("openai/not-priced")).unwrap().cost_usd, None);
        // Codex-reported cost wins.
        let reported = text.replace("\"total_token_usage\"", "\"total_cost\":0.5,\"total_token_usage\"");
        assert_eq!(parse_rollout_usage(&reported, None).unwrap().cost_usd, Some(0.5));
    }

    #[test]
    fn acp_command_contents() {
        let mut a = agent(
            json!({"reasoning_effort": "high", "disable_web_search": true, "dangerously_bypass_hook_trust": true}),
            &[("OPENAI_API_KEY", "sk")],
            Some("openai/gpt-5"),
        )
        .unwrap();
        a.acp_codex_executable = Some("/usr/bin/codex".into());
        assert_eq!(
            a.acp_command().unwrap(),
            [
                "env",
                "NO_BROWSER=1",
                "INITIAL_AGENT_MODE=agent-full-access",
                "DEFAULT_AUTH_REQUEST={\"methodId\":\"api-key\"}",
                "CODEX_HOME=/installed-agent/codex-acp-home",
                "CODEX_CONFIG={\"model\": \"gpt-5\", \"model_reasoning_effort\": \"high\", \"web_search\": \"disabled\", \"bypass_hook_trust\": true}",
                "CODEX_PATH=/usr/bin/codex",
                "codex-acp"
            ]
        );
        assert_eq!(a.acp_env()["CODEX_API_KEY"], "sk");
        let bare = agent(json!({}), &[], None).unwrap().acp_command().unwrap();
        assert!(!bare.iter().any(|s| s.starts_with("CODEX_CONFIG") || s.starts_with("CODEX_PATH")));
    }

    #[test]
    fn atif_to_native_shape() {
        let traj = json!({"session_id": "x", "agent": {"name": "codex", "version": "0.118.0"}, "steps": [
            {"step_id": 1, "source": "system", "message": "sys", "timestamp": "2026-03-04T05:06:07Z"},
            {"step_id": 2, "source": "user", "message": "do it"},
            {"step_id": 3, "source": "agent", "message": "ok",
             "tool_calls": [{"tool_call_id": "c1", "function_name": "shell", "arguments": {"cmd": "ls"}},
                            {"tool_call_id": "c2", "function_name": "apply_patch", "arguments": {"input": "*** patch"}}],
             "extra": {"tool_call_details": {"c2": {"raw_arguments": "*** patch", "item_type": "custom_tool_call"}}},
             "observation": {"results": [{"source_call_id": "c1", "content": [{"type": "text", "text": "a.txt"}]}]}}
        ]});
        let (name, content) = atif_to_native_trajectory(&traj, "sid");
        assert_eq!(name, "rollout-2026-03-04T05-06-07-sid.jsonl");
        assert!(regex::Regex::new(ROLLOUT_FILENAME_RE).unwrap().is_match(&name));
        let ev: Vec<Value> = content.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let kinds: Vec<String> =
            ev.iter().map(|e| format!("{}:{}", e["type"].as_str().unwrap(), e["payload"]["type"].as_str().unwrap_or(""))).collect();
        assert_eq!(
            kinds,
            [
                "session_meta:",
                "response_item:message",
                "response_item:message",
                "event_msg:user_message",
                "response_item:message",
                "event_msg:agent_message",
                "response_item:function_call",
                "response_item:custom_tool_call",
                "response_item:function_call_output",
                "response_item:custom_tool_call_output"
            ]
        );
        assert_eq!(ev[0]["payload"]["timestamp"], "2026-03-04T05:06:07.000Z");
        assert_eq!(ev[0]["payload"]["cli_version"], "0.118.0");
        assert_eq!(ev[1]["payload"]["role"], "developer");
        assert_eq!(ev[2]["timestamp"], "2026-03-04T05:06:07Z");
        assert_eq!(ev[4]["payload"]["content"][0]["type"], "output_text");
        assert_eq!(serde_json::from_str::<Value>(ev[6]["payload"]["arguments"].as_str().unwrap()).unwrap(), json!({"cmd": "ls"}));
        assert_eq!(ev[7]["payload"]["input"], "*** patch");
        assert_eq!(ev[8]["payload"]["output"], "a.txt");
        assert_eq!(ev[9]["payload"]["output"], "[no output recorded]");
    }

    #[test]
    fn load_trajectory_validation() {
        let dir = std::env::temp_dir().join(format!("hbench-codex-load-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut args = common::test_args(json!({}), &[], Some("openai/gpt-5"));
        args.load_trajectory = Some(dir.join("session.jsonl"));
        assert!(Codex::new(args.clone()).is_err());
        args.load_trajectory = Some(dir.join("rollout-2026-01-02T03-04-05-abc.jsonl"));
        let a = Codex::new(args.clone()).unwrap();
        assert!(a.atif_load.is_none() && a.supports_load(false) && a.supports_load(true));
        let p = dir.join("t.json");
        std::fs::write(&p, r#"{"steps": []}"#).unwrap();
        args.load_trajectory = Some(p.clone());
        assert!(Codex::new(args.clone()).unwrap().atif_load.is_some());
        std::fs::write(&p, "not json").unwrap();
        assert!(Codex::new(args).is_err());
    }
}
