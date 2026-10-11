//! antigravity-cli (port of upstream's agents/installed/antigravity_cli.py + antigravity_base_url.py).

use super::{ensure_system_dependencies, exec_error};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::{ExecResult, kind_err, quote};
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

const EFFORTS: &[&str] = &["high", "low", "medium", "minimal"]; // sorted, as upstream prints them
const FLASH_ONLY_EFFORTS: &[&str] = &["minimal", "medium"];
const KNOWN_OPTIONS: &[&str] = &["version", "prompt_template_path", "config", "sandbox", "reasoning_effort", "binary_path", "override_setup_timeout_sec"];
const PRINT_TIMEOUT: &str = "24h";
const MODEL_LIST_TIMEOUT: Duration = Duration::from_secs(15);
const STDERR_REPLAY_BYTES: usize = 256 * 1024;
const RUN_MARKER: &str = "/tmp/.hbench-antigravity-run-start";
const STREAM_LOG: &str = "antigravity-stream.jsonl";
const STDERR_LOG: &str = "antigravity-cli.err";
const AUTH_NAMES: &[&str] = &["GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_GEMINI_BASE_URL", "GOOGLE_APPLICATION_CREDENTIALS"];

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(AntigravityCli::new(args)?))
}

/// upstream's parse_bool_env_value.
fn parse_bool(v: &str, name: &str) -> Result<bool> {
    match v.trim().to_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => bail!("Invalid value for '{name}': cannot parse '{v}' as bool (expected true/false/1/0/yes/no)"),
    }
}

/// Transport policy for a custom Gemini endpoint (antigravity_base_url.validate_base_url).
/// `get_env` is upstream's `_get_env` (empty values are returned, not skipped). Errors never echo the URL.
pub fn validate_base_url(url: &str, get_env: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    if url.chars().any(char::is_whitespace) {
        bail!("GOOGLE_GEMINI_BASE_URL contains whitespace; set a single absolute http(s) URL");
    }
    let invalid = || anyhow!("GOOGLE_GEMINI_BASE_URL must be an absolute http(s) URL with a valid host and port and no fragment");
    let (scheme, rest) = url.split_once("://").ok_or_else(invalid)?;
    let scheme = scheme.to_lowercase();
    let netloc = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let has_fragment = url.find('#').is_some_and(|i| i + 1 < url.len());
    let (userinfo, hostport) = match netloc.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, netloc),
    };
    let (host, port) = if let Some(r) = hostport.strip_prefix('[') {
        let (h, after) = r.split_once(']').unwrap_or((r, ""));
        (h, after.strip_prefix(':').unwrap_or(""))
    } else {
        hostport.rsplit_once(':').unwrap_or((hostport, ""))
    };
    let port_ok = port.is_empty() || (port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u32>().is_ok_and(|p| (1..=65535).contains(&p)));
    if !["http", "https"].contains(&scheme.as_str()) || host.is_empty() || !port_ok || has_fragment {
        return Err(invalid());
    }
    if let Some(u) = userinfo {
        let (user, pass) = u.split_once(':').unwrap_or((u, ""));
        if !user.is_empty() || !pass.is_empty() {
            bail!("GOOGLE_GEMINI_BASE_URL must not embed credentials (userinfo)");
        }
    }
    if scheme == "http" {
        let allow = get_env("HBENCH_ALLOW_INSECURE_MODEL_BASE_URL");
        let ok = match allow.as_deref() {
            Some(a) if !a.trim().is_empty() => parse_bool(a, "HBENCH_ALLOW_INSECURE_MODEL_BASE_URL")?,
            _ => false,
        };
        if !ok {
            bail!(
                "GOOGLE_GEMINI_BASE_URL uses plain HTTP and would send GEMINI_API_KEY without transport encryption. \
                 Use an https:// endpoint, or explicitly set HBENCH_ALLOW_INSECURE_MODEL_BASE_URL=true for a trusted local/test endpoint."
            );
        }
    }
    Ok(())
}

/// agy's `AGY_ERROR: {...}` stderr marker patterns (appended after the base ERROR_PATTERNS).
fn agy_patterns() -> &'static [(Regex, &'static str)] {
    static P: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    P.get_or_init(|| {
        [
            (r#"AGY_ERROR:.*"status":"RESOURCE_EXHAUSTED".*"retryable":\s*true.*"#, "ApiRateLimitError"),
            (r#"AGY_ERROR:.*"status":"RESOURCE_EXHAUSTED".*"retryable":\s*false.*"#, "ApiUsageLimitError"),
            (r#"AGY_ERROR:.*"status":"UNAVAILABLE".*"retryable":\s*true.*"#, "ApiOverloadedError"),
            (r#"AGY_ERROR:.*"status":"INTERNAL".*"retryable":\s*true.*"#, "ApiInternalServerError"),
            (r#"AGY_ERROR:.*"status":"DEADLINE_EXCEEDED".*"retryable":\s*true.*"#, "ApiResponseStalledError"),
            (r#"AGY_ERROR:.*"status":"UNAUTHENTICATED".*"#, "AgentAuthenticationError"),
            (r#"AGY_ERROR:.*"retryable":\s*true.*"#, "UnknownApiError"),
            (r#"AGY_ERROR:.*"retryable":\s*false.*"#, "NonZeroAgentExitCodeError"),
        ]
        .iter()
        .map(|(p, k)| (Regex::new(&format!("(?i){p}")).unwrap(), *k))
        .collect()
    })
}

/// Base ERROR_PATTERNS + agy patterns, last match wins (base wins ties: it comes first in the list).
pub fn classify_agy(output: &str) -> &'static str {
    super::classify_with(output, agy_patterns())
}

fn agy_error(cmd: &str, r: &ExecResult) -> anyhow::Error {
    let e = exec_error(cmd, r, None);
    kind_err(classify_agy(&format!("{}\n{}", r.stdout, r.stderr)), e.to_string())
}

pub struct AntigravityCli {
    args: AgentArgs,
    version: Option<String>,
    sandbox: Option<bool>,
    reasoning_effort: Option<String>,
    binary_path: Option<String>,
    adc_disabled_via_extra_env: bool,
}

impl AntigravityCli {
    pub fn new(mut args: AgentArgs) -> Result<AntigravityCli> {
        let unknown: Vec<String> = args.kwargs.keys().filter(|k| !KNOWN_OPTIONS.contains(&k.as_str())).map(|k| format!("  Unknown option '{k}'.")).collect();
        if !unknown.is_empty() {
            bail!("Invalid kwargs for agent 'antigravity-cli':\n{}\nSee `hbench agents schema antigravity-cli` for available options.", unknown.join("\n"));
        }
        if args.kwargs.get("config").is_some_and(|v| !v.is_null()) {
            bail!("Agent 'antigravity-cli' does not support config. You can implement it.");
        }
        let sandbox = match args.kwargs.get("sandbox") {
            None | Some(Value::Null) => None,
            Some(_) => Some(args.kw_bool("sandbox").ok_or_else(|| anyhow!("Invalid kwargs for agent 'antigravity-cli':\n  sandbox: Input should be a valid boolean"))?),
        };
        let reasoning_effort = args.kw("reasoning_effort");
        if let Some(e) = &reasoning_effort {
            if !EFFORTS.contains(&e.as_str()) {
                bail!("Invalid value for 'reasoning_effort': '{e}'. Valid values: {}", EFFORTS.join(", "));
            }
            if let Some(m) = &args.model_name {
                if m.contains("2.5") {
                    bail!("Gemini 2.5 models do not support reasoning_effort. Use a Gemini 3 model, or add explicit thinking_budget support.");
                }
                if FLASH_ONLY_EFFORTS.contains(&e.as_str()) && !m.contains("flash") {
                    bail!("Gemini model '{m}' does not support reasoning_effort='{e}'. Use 'low' or 'high', or choose a Gemini 3 Flash model.");
                }
            }
        }

        // _sanitize_auth_extra_env: empty auth values mean unset; GOOGLE_API_KEY renamed to GEMINI_API_KEY.
        let env = &mut args.extra_env;
        env.retain(|k, v| !v.is_empty() || !(AUTH_NAMES.contains(&k.as_str()) || k.starts_with("AGY_CLI_")));
        if let Some(alias) = env.remove("GOOGLE_API_KEY") {
            env.entry("GEMINI_API_KEY".into()).or_insert(alias);
        }
        let mut adc_disabled_via_extra_env = false;
        if let Some(raw) = env.get("AGY_ADC_AUTH").cloned() {
            if raw.trim().is_empty() {
                env.remove("AGY_ADC_AUTH");
            } else if parse_bool(&raw, "AGY_ADC_AUTH")? {
                env.insert("AGY_ADC_AUTH".into(), "true".into());
            } else {
                env.remove("AGY_ADC_AUTH");
                adc_disabled_via_extra_env = true;
            }
        }
        let mut a = AntigravityCli { version: args.version_kw(), binary_path: args.kw("binary_path"), args, sandbox, reasoning_effort, adc_disabled_via_extra_env };
        if a.use_adc_auth()? {
            for k in ["GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_GEMINI_BASE_URL"] {
                a.args.extra_env.remove(k);
            }
        }
        Ok(a)
    }

    /// upstream's `_get_env`: agent env > host env, empty values returned as-is.
    fn raw_env(&self, k: &str) -> Option<String> {
        self.args.extra_env.get(k).cloned().or_else(|| std::env::var(k).ok())
    }

    fn use_adc_auth(&self) -> Result<bool> {
        if self.adc_disabled_via_extra_env {
            return Ok(false);
        }
        match self.raw_env("AGY_ADC_AUTH") {
            Some(v) if !v.trim().is_empty() => parse_bool(&v, "AGY_ADC_AUTH"),
            _ => Ok(false),
        }
    }

    /// First non-empty GEMINI_API_KEY / GOOGLE_API_KEY, per source in precedence order.
    fn find_api_key(&self) -> Option<String> {
        let names = ["GEMINI_API_KEY", "GOOGLE_API_KEY"];
        names
            .iter()
            .find_map(|n| self.args.extra_env.get(*n).filter(|v| !v.is_empty()).cloned())
            .or_else(|| names.iter().find_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty())))
    }

    /// Non-empty AGY_CLI_* variables; agent env wins over host env.
    fn forwarded_cli_env(&self) -> BTreeMap<String, String> {
        let mut out: BTreeMap<String, String> = std::env::vars().filter(|(k, v)| k.starts_with("AGY_CLI_") && !v.is_empty()).collect();
        out.extend(self.args.extra_env.iter().filter(|(k, v)| k.starts_with("AGY_CLI_") && !v.is_empty()).map(|(k, v)| (k.clone(), v.clone())));
        out
    }

    /// Auth/run env built by upstream's run() before any exec.
    fn run_env(&self, model: &str) -> Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::from([("GEMINI_CLI_TRUST_WORKSPACE".to_string(), "true".to_string())]);
        if self.use_adc_auth()? {
            if model.contains("2.5") {
                bail!(
                    "ADC auth (AGY_ADC_AUTH) supports Gemini 3 Flash and newer models only; '{model}' is not supported. \
                     Use a Gemini 3 model or authenticate with GEMINI_API_KEY instead."
                );
            }
            env.insert("AGY_ADC_AUTH".into(), "true".into());
            if let Some(p) = self.raw_env("GOOGLE_APPLICATION_CREDENTIALS").filter(|p| !p.is_empty()) {
                env.insert("GOOGLE_APPLICATION_CREDENTIALS".into(), p);
            }
        } else {
            let key = self.find_api_key().ok_or_else(|| {
                anyhow!(
                    "antigravity-cli requires a Gemini API key: set GEMINI_API_KEY (or GOOGLE_API_KEY) in the host environment, \
                     or pass `--ae GEMINI_API_KEY=<key>`. Create a key in Google AI Studio: https://aistudio.google.com/app/api-keys. \
                     For enterprise Application Default Credentials auth, set AGY_ADC_AUTH=true instead."
                )
            })?;
            env.insert("GEMINI_API_KEY".into(), key);
            if let Some(url) = self.raw_env("GOOGLE_GEMINI_BASE_URL").filter(|u| !u.is_empty()) {
                validate_base_url(&url, &|k| self.raw_env(k))?;
                env.insert("GOOGLE_GEMINI_BASE_URL".into(), url);
            }
        }
        env.extend(self.forwarded_cli_env());
        Ok(env)
    }

    fn skills_dir(&self) -> Option<String> {
        self.args.task.config.environment.skills_dir.clone().or_else(|| (!self.args.skills.is_empty()).then(|| "/hbench/skills".into()))
    }

    fn settings_config(&self, model: &str, register_model: bool) -> Result<Value> {
        let mut c = Map::new();
        if !self.args.mcp_servers.is_empty() {
            let mut servers = Map::new();
            for s in &self.args.mcp_servers {
                let v = match s.transport() {
                    "stdio" => json!({"command": s.command, "args": s.args}),
                    "streamable-http" => json!({"httpUrl": s.url}),
                    _ => json!({"url": s.url}),
                };
                servers.insert(s.name.clone(), v);
            }
            c.insert("mcpServers".into(), Value::Object(servers));
        }
        if !model.is_empty() && register_model {
            c.insert("customModelsConfig".into(), json!({"customModels": {model: {"modelName": model}}}));
        }
        if !self.use_adc_auth()? && self.find_api_key().is_some() {
            c.insert("modelProvider".into(), json!("gemini"));
        }
        c.insert("allowNonWorkspaceAccess".into(), json!(true));
        c.insert("experimental".into(), json!({"skills": true}));
        Ok(Value::Object(c))
    }

    /// agy >= 1.1.27 reads MCP servers from ~/.gemini/config/mcp_config.json.
    fn mcp_config(&self) -> Value {
        let servers: Map<String, Value> = self
            .args
            .mcp_servers
            .iter()
            .map(|s| {
                let v = if s.transport() == "stdio" { json!({"command": s.command, "args": s.args, "disabled": false}) } else { json!({"serverUrl": s.url, "disabled": false}) };
                (s.name.clone(), v)
            })
            .collect();
        json!({"mcpServers": servers})
    }

    fn settings_command(&self, model: &str, register_model: bool, activate_mcp: bool) -> Result<String> {
        let mut config = self.settings_config(model, register_model)?;
        if !activate_mcp {
            config.as_object_mut().unwrap().remove("mcpServers");
        }
        let mut cmd = format!(
            "mkdir -p ~/.gemini/antigravity-cli && printf %s {} > ~/.gemini/antigravity-cli/settings.json",
            quote(&serde_json::to_string_pretty(&config)?)
        );
        if !self.args.mcp_servers.is_empty() || !activate_mcp {
            let mcp = if activate_mcp { self.mcp_config() } else { json!({"mcpServers": {}}) };
            cmd += &format!(" && mkdir -p ~/.gemini/config && printf %s {} > ~/.gemini/config/mcp_config.json", quote(&serde_json::to_string_pretty(&mcp)?));
        }
        Ok(cmd)
    }

    fn agy_command(&self, model: &str, instruction: &str) -> String {
        let model_flag = if model.is_empty() { String::new() } else { format!("--model {} ", quote(model)) };
        let effort_flag = self.reasoning_effort.as_ref().map(|e| format!("--effort {} ", quote(e))).unwrap_or_default();
        let extra = if self.sandbox == Some(true) { "--sandbox " } else { "" };
        format!(
            "$HOME/.local/bin/agy --dangerously-skip-permissions --print-timeout {PRINT_TIMEOUT} --output-format stream-json --add-dir \"$PWD\" \
             {model_flag}{effort_flag}{extra}--prompt={} 2>/logs/agent/{STDERR_LOG} </dev/null | stdbuf -oL tee /logs/agent/{STREAM_LOG}; \
             rc=$?; if [ $rc -ne 0 ]; then tail -c {STDERR_REPLAY_BYTES} /logs/agent/{STDERR_LOG} >&2; fi; exit $rc",
            quote(instruction)
        )
    }

    async fn exec(&self, cx: &AgentCtx<'_>, cmd: &str, user: Option<&str>, env: &BTreeMap<String, String>, timeout: Option<Duration>) -> Result<ExecResult> {
        let user = user.or(cx.user.as_deref());
        let r = cx.exec_as(&format!("set -o pipefail; {cmd}"), user, env, timeout).await?;
        if r.code != 0 {
            return Err(agy_error(cmd, &r));
        }
        Ok(r)
    }
}

/// Parse `agy models` output (`slug<TAB>display name` rows) into slugs.
pub fn parse_known_models(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|l| {
            let parts: Vec<&str> = l.split('\t').map(str::trim).collect();
            (parts.len() >= 2 && !parts[0].is_empty() && !parts[0].contains(' ')).then(|| parts[0].to_string())
        })
        .collect()
}

/// agy's gate accepts listed slugs and bare bases of effort-suffixed slugs.
pub fn model_is_known(model: &str, known: &[String]) -> bool {
    known.iter().any(|k| k == model) || EFFORTS.iter().any(|e| known.contains(&format!("{model}-{e}")))
}

fn collect_trajectory_command(scoped: bool) -> String {
    let newer = if scoped { format!("-newer {RUN_MARKER} ") } else { String::new() };
    format!(
        "src=$(find \"$HOME/.gemini/antigravity-cli/brain\" -type f -name 'transcript_full.jsonl' {newer}-printf '%T@\\t%p\\n' 2>/dev/null | sort -nr | head -n1 | cut -f2-); \
         if [ -n \"$src\" ]; then cp -- \"$src\" /logs/agent/antigravity-cli.trajectory.jsonl; else \
         src=$(find \"$HOME/.agy/antigravity-cli/tmp\" -type f \\( -name 'session-*.jsonl' -o -name 'session-*.json' \\) {newer}-printf '%T@\\t%p\\n' 2>/dev/null | sort -nr | head -n1 | cut -f2-); \
         if [ -n \"$src\" ]; then cp -- \"$src\" \"/logs/agent/antigravity-cli.trajectory.${{src##*.}}\"; fi; fi; \
         db=$(find \"$HOME/.gemini/antigravity-cli/conversations\" -type f -name '*.db' {newer}-printf '%T@\\t%p\\n' 2>/dev/null | sort -nr | head -n1 | cut -f2-); \
         if [ -n \"$db\" ]; then cp -- \"$db\" /logs/agent/antigravity-conversation.db; for ext in -wal -shm; do [ -f \"${{db}}${{ext}}\" ] && \
         cp -- \"${{db}}${{ext}}\" \"/logs/agent/antigravity-conversation.db${{ext}}\"; done; fi; true"
    )
}

type Totals = (i64, i64, i64);

fn as_int(v: &Value) -> i64 {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).unwrap_or(0)
}

/// (prompt incl. cache reads, completion, cached) from agy's stream-json log: the `result`
/// aggregate if present, else the per-step maxima summed. None when no usage was seen.
pub fn stream_usage_totals(text: &str) -> Option<Totals> {
    let totals = |u: &Value| -> Totals {
        let cached = as_int(&u["cache_read_tokens"]);
        (as_int(&u["input_tokens"]) + cached, as_int(&u["output_tokens"]), cached)
    };
    let largest = |a: Option<Totals>, b: Totals| a.map_or(b, |a| (a.0.max(b.0), a.1.max(b.1), a.2.max(b.2)));
    let mut result: Option<Totals> = None;
    let mut steps: HashMap<i64, Totals> = HashMap::new();
    let mut unkeyed: Vec<Totals> = vec![];
    for line in text.lines() {
        let Ok(ev) = serde_json::from_str::<Value>(line.trim()) else { continue };
        match ev["event"].as_str() {
            Some("result") if ev["result"]["usage"].is_object() => result = Some(largest(result, totals(&ev["result"]["usage"]))),
            Some("step_update") if ev["step_update"]["usage"].is_object() => {
                let p = &ev["step_update"];
                let t = totals(&p["usage"]);
                match p["step_index"].as_i64() {
                    Some(i) => {
                        steps.insert(i, largest(steps.get(&i).copied(), t));
                    }
                    None => unkeyed.push(t),
                }
            }
            _ => {}
        }
    }
    if result.is_some() {
        return result;
    }
    if steps.is_empty() && unkeyed.is_empty() {
        return None;
    }
    Some(steps.values().chain(&unkeyed).fold((0, 0, 0), |a, t| (a.0 + t.0, a.1 + t.1, a.2 + t.2)))
}

/// agy 1.1.x step transcript: any record with an int step_index of type USER_INPUT / PLANNER_RESPONSE.
fn is_step_transcript(text: &str) -> bool {
    text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).any(|r| {
        (r["step_index"].is_i64() || r["step_index"].is_u64()) && r["type"].as_str().is_some_and(|t| ["USER_INPUT", "PLANNER_RESPONSE"].contains(&t.to_uppercase().as_str()))
    })
}

fn merge_update(msg: &mut Value, update: &Value) {
    for (k, v) in update.as_object().into_iter().flatten() {
        if k == "type" || k == "id" {
            continue;
        }
        match (msg.get_mut(k), v) {
            (Some(Value::Object(cur)), Value::Object(new)) => cur.extend(new.clone()),
            _ => {
                msg[k] = v.clone();
            }
        }
    }
}

/// Gemini-style session (legacy JSON with `messages`, or v0.40+ JSONL records) -> messages.
/// None when nothing parseable (upstream's `_load_gemini_session`).
pub fn load_gemini_messages(text: &str) -> Option<Vec<Value>> {
    if text.trim().is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(text)
        && v.get("messages").is_some()
    {
        return Some(v["messages"].as_array().cloned().unwrap_or_default());
    }
    let mut has_metadata = false;
    let mut ids: Vec<String> = vec![];
    let mut msgs: HashMap<String, Value> = HashMap::new();
    let mut pending: HashMap<String, Vec<Value>> = HashMap::new();
    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<Value>(line.trim()) else { continue };
        let Some(obj) = rec.as_object() else { continue };
        let mid = rec["id"].as_str().map(String::from);
        if let Some(rw) = obj.get("$rewindTo") {
            match rw.as_str().and_then(|r| ids.iter().position(|i| i == r)) {
                Some(idx) => {
                    for removed in ids.drain(idx..) {
                        msgs.remove(&removed);
                        pending.remove(&removed);
                    }
                }
                None => {
                    ids.clear();
                    msgs.clear();
                    pending.clear();
                }
            }
        } else if obj.get("$set").is_some_and(Value::is_object) {
            has_metadata |= !obj["$set"].as_object().unwrap().is_empty();
        } else if matches!(rec["type"].as_str(), Some("user" | "gemini")) {
            let Some(mid) = mid else { continue }; // ponytail: id-less messages are dropped, as in upstream
            match msgs.get_mut(&mid) {
                Some(m) => m.as_object_mut().unwrap().extend(obj.clone()),
                None => {
                    ids.push(mid.clone());
                    msgs.insert(mid.clone(), rec.clone());
                }
            }
            for u in pending.remove(&mid).unwrap_or_default() {
                merge_update(msgs.get_mut(&mid).unwrap(), &u);
            }
        } else if rec["type"] == "message_update" {
            if let Some(mid) = mid {
                match msgs.get_mut(&mid) {
                    Some(m) => merge_update(m, &rec),
                    None => pending.entry(mid).or_default().push(rec.clone()),
                }
            }
        } else if obj.contains_key("sessionId") {
            has_metadata = true;
        }
    }
    if ids.is_empty() && !has_metadata {
        return None;
    }
    Some(ids.iter().map(|i| msgs[i].clone()).collect())
}

/// upstream's populate_context_post_run token totals (cost needs LiteLLM pricing, unavailable here).
pub fn usage_from_logs(logs_dir: &Path) -> Option<Totals> {
    let stream = || std::fs::read_to_string(logs_dir.join(STREAM_LOG)).ok().and_then(|t| stream_usage_totals(&t));
    let Some(text) = ["antigravity-cli.trajectory.jsonl", "antigravity-cli.trajectory.json"].iter().find_map(|f| std::fs::read_to_string(logs_dir.join(f)).ok()) else {
        return stream();
    };
    if is_step_transcript(&text) {
        return stream(); // step transcripts carry no usage
    }
    let Some(messages) = load_gemini_messages(&text) else { return stream() };
    let mut t = (0, 0, 0);
    for m in messages.iter().filter(|m| m["type"] == "gemini") {
        let k = &m["tokens"];
        t.0 += as_int(&k["input"]);
        t.1 += as_int(&k["output"]) + as_int(&k["tool"]) + as_int(&k["thoughts"]);
        t.2 += as_int(&k["cached"]);
    }
    Some(t)
}

#[async_trait::async_trait]
impl Agent for AntigravityCli {
    /// upstream's sanitized `_extra_env` (empty auth values dropped, alias renamed, keys stripped under ADC).
    fn extra_env(&self) -> Option<BTreeMap<String, String>> {
        Some(self.args.extra_env.clone())
    }
    fn name(&self) -> &str {
        "antigravity-cli"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        cx.env.exec_root("[ -d /installed-agent ] || mkdir -p /installed-agent", None).await?;
        let none = BTreeMap::new();
        let install = async {
            if let Some(bp) = &self.binary_path {
                let p = if let Some(rest) = bp.strip_prefix("~/") { std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest) } else { bp.into() };
                if !p.is_file() {
                    bail!("binary_path does not point at a file: {bp}");
                }
                ensure_system_dependencies(cx, &["ca_certificates"]).await?;
                cx.env.upload_file(&p, "/tmp/agy-custom").await?;
                self.exec(cx, "chmod 755 /tmp/agy-custom", Some("root"), &none, None).await?;
                self.exec(cx, "install -D -m 755 /tmp/agy-custom $HOME/.local/bin/agy && rm -f /tmp/agy-custom", None, &none, None).await?;
            } else {
                ensure_system_dependencies(cx, &["curl"]).await?;
                self.exec(cx, "curl -fsSL https://antigravity.google/cli/install.sh | bash", None, &none, None).await?;
            }
            self.exec(cx, "$HOME/.local/bin/agy --version", None, &none, None).await
        };
        install.await.map_err(|e| anyhow!("Agent install failed: {e:#}"))?;
        if self.version.is_none() {
            let r = cx.exec("$HOME/.local/bin/agy --version", &none, None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(r.stdout.trim().to_string());
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let model = match &self.args.model_name {
            Some(m) if m.contains('/') => m.rsplit('/').next().unwrap().to_string(),
            _ => bail!("Model name must be in the format provider/model_name"),
        };
        let env = self.run_env(&model)?;
        let none = BTreeMap::new();
        if let Some(d) = self.skills_dir() {
            let cmd = format!("mkdir -p ~/.gemini/antigravity-cli/skills && cp -r {}/* ~/.gemini/antigravity-cli/skills/ 2>/dev/null || true", quote(&d));
            self.exec(cx, &cmd, None, &none, None).await?;
        }
        // Settings (auth opt-in, no MCP yet) must exist before `agy models` can list.
        self.exec(cx, &self.settings_command(&model, false, false)?, None, &none, None).await?;
        let mut register = !model.is_empty();
        if register && let Ok(r) = self.exec(cx, "$HOME/.local/bin/agy models </dev/null", None, &env, Some(MODEL_LIST_TIMEOUT)).await {
            let known = parse_known_models(&r.stdout);
            if !known.is_empty() {
                register = !model_is_known(&model, &known);
            }
        }
        self.exec(cx, &self.settings_command(&model, register, true)?, None, &none, None).await?;

        let scoped = self.exec(cx, &format!("touch {RUN_MARKER}"), None, &none, None).await.is_ok();
        let r = self.exec(cx, &self.agy_command(&model, &instruction), None, &env, None).await;
        let _ = self.exec(cx, &collect_trajectory_command(scoped), None, &none, None).await;
        r.map(|_| ())
    }

    fn populate_context_post_run(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        if let Some((i, o, c)) = usage_from_logs(logs_dir) {
            ctx.n_input_tokens = Some(i);
            ctx.n_output_tokens = Some(o);
            ctx.n_cache_tokens = Some(c);
            ctx.cost_usd = self.args.model_name.as_deref().and_then(|m| cost_from_pricing(m, i, o, c));
        }
    }
}

/// upstream's `_compute_cost_from_pricing`: LiteLLM rates for the model (then provider-stripped),
/// cache reads at `cache_read_input_token_cost or input rate`. None when the model isn't priced.
pub fn cost_from_pricing(model: &str, prompt: i64, completion: i64, cached: i64) -> Option<f64> {
    let p = crate::agents::pricing::model_cost(model).or_else(|| crate::agents::pricing::model_cost(model.split_once('/').map_or(model, |(_, m)| m)))?;
    let rate = |k: &str| p.get(k).and_then(Value::as_f64).filter(|r| *r != 0.0);
    let input = rate("input_cost_per_token").unwrap_or(0.0);
    let read = rate("cache_read_input_token_cost").unwrap_or(input);
    Some((prompt - cached).max(0) as f64 * input + cached as f64 * read + completion as f64 * rate("output_cost_per_token").unwrap_or(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn args(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> AgentArgs {
        let dir = std::env::temp_dir().join(format!("hbench-agy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("environment")).unwrap();
        std::fs::write(dir.join("task.toml"), "[environment]\ndocker_image='x'\n").unwrap();
        std::fs::write(dir.join("instruction.md"), "x").unwrap();
        let task = Arc::new(crate::task::Task::load(&dir).unwrap());
        AgentArgs { load_trajectory: None,
            model_name: model.map(String::from),
            kwargs: serde_json::from_value(kwargs).unwrap(),
            extra_env: env.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            mcp_servers: vec![],
            skills: vec![],
            logs_dir: dir,
            task,
        }
    }
    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<AntigravityCli> {
        AntigravityCli::new(args(kwargs, env, model))
    }

    #[test]
    fn option_validation() {
        assert!(agent(json!({"reasoning_effort": "huge"}), &[], None).is_err());
        assert!(agent(json!({"reasoning_effort": "high"}), &[], Some("google/gemini-2.5-pro")).is_err());
        assert!(agent(json!({"reasoning_effort": "medium"}), &[], Some("google/gemini-3-pro")).is_err());
        assert!(agent(json!({"reasoning_effort": "medium"}), &[], Some("google/gemini-3-flash")).is_ok());
        assert!(agent(json!({"bogus": 1}), &[], None).is_err());
        assert!(agent(json!({"config": {"a": 1}}), &[], None).is_err());
        assert!(agent(json!({"sandbox": "maybe"}), &[], None).is_err());
        assert!(agent(json!({}), &[("AGY_ADC_AUTH", "perhaps")], None).is_err());
    }

    #[test]
    fn auth_env_and_flags() {
        let a = agent(json!({"sandbox": true, "reasoning_effort": "low"}), &[("GOOGLE_API_KEY", "gk"), ("AGY_CLI_FOO", "1"), ("AGY_CLI_BAR", "")], Some("google/gemini-3-pro")).unwrap();
        let e = a.run_env("gemini-3-pro").unwrap();
        assert_eq!((e["GEMINI_API_KEY"].as_str(), e["GEMINI_CLI_TRUST_WORKSPACE"].as_str(), e["AGY_CLI_FOO"].as_str()), ("gk", "true", "1"));
        assert!(!e.contains_key("AGY_CLI_BAR") && !e.contains_key("GOOGLE_API_KEY"));
        let cmd = a.agy_command("gemini-3-pro", "do it");
        assert!(cmd.contains("--add-dir \"$PWD\" --model gemini-3-pro --effort low --sandbox --prompt='do it' 2>/logs/agent/antigravity-cli.err </dev/null | stdbuf -oL tee /logs/agent/antigravity-stream.jsonl"));
        assert!(cmd.contains("tail -c 262144"));

        // ADC: key material dropped, modelProvider not written, 2.5 rejected.
        let adc = agent(json!({}), &[("AGY_ADC_AUTH", "1"), ("GEMINI_API_KEY", "k"), ("GOOGLE_APPLICATION_CREDENTIALS", "/c.json")], None).unwrap();
        let e = adc.run_env("gemini-3-flash").unwrap();
        assert_eq!((e["AGY_ADC_AUTH"].as_str(), e["GOOGLE_APPLICATION_CREDENTIALS"].as_str()), ("true", "/c.json"));
        assert!(!e.contains_key("GEMINI_API_KEY"));
        assert!(adc.run_env("gemini-2.5-pro").is_err());
        assert!(adc.settings_config("m", false).unwrap().get("modelProvider").is_none());
        let off = agent(json!({}), &[("AGY_ADC_AUTH", "false")], None).unwrap();
        assert!(off.adc_disabled_via_extra_env && !off.use_adc_auth().unwrap());

        // Insecure base URL needs an explicit opt-in.
        let b = agent(json!({}), &[("GEMINI_API_KEY", "k"), ("GOOGLE_GEMINI_BASE_URL", "http://host.docker.internal:8080")], None).unwrap();
        assert!(b.run_env("m").is_err());
        let b = agent(json!({}), &[("GEMINI_API_KEY", "k"), ("GOOGLE_GEMINI_BASE_URL", "http://h:8080"), ("HBENCH_ALLOW_INSECURE_MODEL_BASE_URL", "true")], None).unwrap();
        assert_eq!(b.run_env("m").unwrap()["GOOGLE_GEMINI_BASE_URL"], "http://h:8080");
    }

    #[test]
    fn base_url_policy() {
        let no = |_: &str| None;
        assert!(validate_base_url("https://example.com/v1", &no).is_ok());
        assert!(validate_base_url("https://[::1]:443", &no).is_ok());
        for bad in ["ftp://x", "https://", "https://h:0", "https://h:99999", "https://h:ab", "https://h/#frag", "https://u:p@h", "https://h /x", "example.com"] {
            assert!(validate_base_url(bad, &no).is_err(), "{bad}");
        }
    }

    #[test]
    fn settings_and_models() {
        let mut a = args(json!({}), &[("GEMINI_API_KEY", "k")], Some("google/custom-x"));
        a.mcp_servers = vec![serde_json::from_value(json!({"name": "s", "transport": "streamable-http", "url": "http://m"})).unwrap()];
        let a = AntigravityCli::new(a).unwrap();
        let c = a.settings_config("custom-x", true).unwrap();
        assert_eq!(c["modelProvider"], "gemini");
        assert_eq!(c["customModelsConfig"]["customModels"]["custom-x"]["modelName"], "custom-x");
        assert_eq!(c["mcpServers"]["s"]["httpUrl"], "http://m");
        assert_eq!(c["allowNonWorkspaceAccess"], true);
        assert_eq!(a.mcp_config()["mcpServers"]["s"], json!({"serverUrl": "http://m", "disabled": false}));
        let pre = a.settings_command("custom-x", false, false).unwrap();
        assert!(!pre.contains("mcpServers\": {\n    \"s") && pre.contains("~/.gemini/config/mcp_config.json") && !pre.contains("customModels"));

        let known = parse_known_models("Fetching available models...\n  gemini-3.7-flash \tGemini 3.7 Flash\ngemini-3.5-flash-low\tX\nsome error text");
        assert_eq!(known, vec!["gemini-3.7-flash", "gemini-3.5-flash-low"]);
        assert!(model_is_known("gemini-3.7-flash", &known) && model_is_known("gemini-3.5-flash", &known) && !model_is_known("custom-x", &known));
    }

    #[test]
    fn agy_error_classification() {
        let m = |status: &str, retry: bool| format!("error: something went wrong\nAGY_ERROR: {{\"short_error\":\"backend failed\",\"status\":\"{status}\",\"error_code\":1,\"retryable\":{retry},\"error_id\":\"t\"}}");
        assert_eq!(classify_agy(&m("RESOURCE_EXHAUSTED", true)), "ApiRateLimitError");
        assert_eq!(classify_agy(&m("RESOURCE_EXHAUSTED", false)), "ApiUsageLimitError");
        assert_eq!(classify_agy(&m("UNAVAILABLE", true)), "ApiOverloadedError");
        assert_eq!(classify_agy(&m("UNAUTHENTICATED", false)), "AgentAuthenticationError");
        assert_eq!(classify_agy(&m("WEIRD", true)), "UnknownApiError");
        let nr = "AGY_ERROR: {\"short_error\":\"API Error: 500 Internal server error\",\"status\":\"INTERNAL\",\"retryable\":false}";
        assert_eq!(classify_agy(nr), "NonZeroAgentExitCodeError");
        assert_eq!(classify_agy(&format!("{nr}\nCould not resolve host: x")), "NetworkConnectionError");
        assert_eq!(classify_agy("429 Too Many Requests"), "ApiRateLimitError");
    }

    #[test]
    fn stream_usage() {
        let res = |i: i64, o: i64, c: i64| json!({"event": "result", "result": {"usage": {"input_tokens": i, "output_tokens": o, "cache_read_tokens": c}}}).to_string();
        let step = |s: i64, i: i64, o: i64, c: i64| json!({"event": "step_update", "step_update": {"step_index": s, "usage": {"input_tokens": i, "output_tokens": o, "cache_read_tokens": c}}}).to_string();
        assert_eq!(stream_usage_totals(&res(100, 40, 900)), Some((1000, 40, 900)));
        assert_eq!(stream_usage_totals(&[res(100, 40, 0), res(100, 40, 0)].join("\n")), Some((100, 40, 0)));
        assert_eq!(stream_usage_totals(&[step(1, 10, 3, 5), step(2, 20, 4, 7)].join("\n")), Some((42, 7, 12)));
        assert_eq!(stream_usage_totals(&[step(1, 10, 3, 4), step(1, 0, 0, 0)].join("\n")), Some((14, 3, 4)));
        assert_eq!(stream_usage_totals(&[step(1, 10, 3, 0), step(1, 25, 9, 0)].join("\n")), Some((25, 9, 0)));
        let torn = ["Authentication required. Please visit the URL to log in:".to_string(), res(5, 2, 0), "{\"event\":\"result\",\"result\":{\"usa".into()].join("\n");
        assert_eq!(stream_usage_totals(&torn), Some((5, 2, 0)));
        assert_eq!(stream_usage_totals("{\"event\":\"init\",\"init\":{}}"), None);
    }

    #[test]
    fn context_from_logs() {
        let dir = std::env::temp_dir().join(format!("hbench-agy-logs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // Gemini JSONL session with message_update merged (incl. one buffered before its message).
        let session = [
            json!({"type": "session_metadata", "sessionId": "session-1"}),
            json!({"type": "message_update", "id": "g0", "tokens": {"input": 7, "output": 4, "cached": 1}}),
            json!({"type": "user", "id": "user-1", "content": [{"text": "Solve"}]}),
            json!({"type": "gemini", "id": "gemini-1", "content": [{"text": "Done"}]}),
            json!({"type": "message_update", "id": "gemini-1", "tokens": {"input": 10, "output": 5, "cached": 3, "thoughts": 2, "tool": 1}}),
            json!({"type": "gemini", "id": "g0", "content": []}),
        ];
        std::fs::write(dir.join("antigravity-cli.trajectory.jsonl"), session.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        assert_eq!(usage_from_logs(&dir), Some((17, 12, 4)));
        // Step transcript: usage comes from the stream log.
        let steps = json!({"step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT", "status": "DONE", "content": "x"}).to_string();
        std::fs::write(dir.join("antigravity-cli.trajectory.jsonl"), steps).unwrap();
        assert_eq!(usage_from_logs(&dir), None);
        std::fs::write(dir.join(STREAM_LOG), json!({"event": "result", "result": {"usage": {"input_tokens": 70, "output_tokens": 8}}}).to_string()).unwrap();
        let mut ctx = AgentContext::default();
        agent(json!({}), &[], Some("google/gemini-3.5-flash")).unwrap().populate_context_post_run(&dir, &mut ctx);
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens, ctx.n_cache_tokens), (Some(70), Some(8), Some(0)));
        // gemini-3.5-flash: in 1.5e-6, out 9e-6, cache read 1.5e-7.
        assert!((ctx.cost_usd.unwrap() - (70.0 * 1.5e-6 + 8.0 * 9e-6)).abs() < 1e-12);
        let c = cost_from_pricing("google/gemini-3.5-flash", 100, 10, 40).unwrap();
        assert!((c - (60.0 * 1.5e-6 + 40.0 * 1.5e-7 + 10.0 * 9e-6)).abs() < 1e-12);
        assert_eq!(cost_from_pricing("google/not-a-model", 1, 1, 0), None);
    }
}
