//! Muse Code installed agent.

use super::common::{installed_dir, skills_dir};
use super::{connection, ensure_system_dependencies, exec_agent, exec_root, upload_text};
use crate::agents::model_connection::{Resolved, Spec};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use crate::fmt::json_spaced;
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

const SPEC: Spec =
    Spec { default_provider: Some("meta"), api_key_envs: &["META_API_KEY"], base_url_envs: &["MUSE_CODE_BASE_URL"], passthrough: false };
const INSTALL_SCRIPT_URL: &str = "https://dev.meta.ai/install.sh";
const PATH_EXPORT: &str = "export PATH=\"$HOME/.local/bin:$PATH\"";
const LOGS: &str = "/logs/agent";
const OUTPUT_FILENAME: &str = "muse-code.jsonl";
const STDERR_FILENAME: &str = "muse-code.stderr.log";
pub const MANIFEST: &str = "muse-session.json";
// /tmp/hbench-muse/{environment_logs_dir.name}
const RUNTIME_DIR: &str = "/tmp/hbench-muse/agent";
const EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "ultra"];

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(MuseCode::new(args)?))
}

pub struct MuseCode {
    args: AgentArgs,
    version: Option<String>,
    base_config: Map<String, Value>,
    /// load_trajectory is consumed by the first run only.
    loaded: bool,
}

impl MuseCode {
    pub fn new(args: AgentArgs) -> Result<MuseCode> {
        let config = match args.kwargs.get("config") {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(Value::String(p)) => {
                let raw = std::fs::read_to_string(p).map_err(|e| anyhow!("Agent config file not found: {p}: {e}"))?;
                serde_json::from_str(&raw)?
            }
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => bail!("Invalid value for 'config': expected a local file path or JSON object"),
        };
        let Value::Object(mut base_config) = config else { bail!("Muse config must be a JSON object") };
        let sv = base_config.entry("schema_version").or_insert(json!(1));
        if !(sv.is_i64() || sv.is_u64()) || sv.as_i64() != Some(1) {
            bail!("Muse settings require schema_version 1");
        }
        let a = MuseCode { version: args.version_kw(), args, base_config, loaded: false };
        a.cli_flags()?; // validate options eagerly
        a.effective_config()?; // validate transport/config conflicts
        if let Some(p) = &a.args.load_trajectory {
            validate_load_trajectory(p)?;
        }
        Ok(a)
    }

    /// Option value: kwarg (explicit null opts out), else env fallback.
    fn opt(&self, kw: &str, fallback_env: &str) -> Option<String> {
        if self.args.kwargs.contains_key(kw) {
            return self.args.kw(kw);
        }
        self.args.get_env(fallback_env)
    }

    fn base_url_opt(&self) -> Option<String> {
        self.opt("base_url", "MUSE_CODE_BASE_URL")
    }

    /// MuseCodeOptions -> CLI flags (field order: reasoning_effort, max_model_steps, base_url).
    fn cli_flags(&self) -> Result<String> {
        let mut f = vec![];
        if let Some(v) = self.opt("reasoning_effort", "MUSE_CODE_REASONING_EFFORT") {
            if !EFFORTS.contains(&v.as_str()) {
                bail!("Invalid value for 'reasoning_effort': '{v}'");
            }
            f.push(format!("--reasoning-effort {}", quote(&v)));
        }
        if let Some(v) = self.opt("max_model_steps", "MUSE_CODE_MAX_MODEL_STEPS") {
            let n: i64 = v.trim().parse().map_err(|_| anyhow!("Invalid value for 'max_model_steps': cannot parse '{v}' as int"))?;
            if n <= 0 {
                bail!("Invalid value for 'max_model_steps': must be greater than 0");
            }
            f.push(format!("--max-model-steps {n}"));
        }
        if let Some(v) = self.base_url_opt() {
            f.push(format!("--base-url {}", quote(&v)));
        }
        Ok(f.join(" "))
    }

    /// Overridden `model_connection`: explicit base_url option wins.
    fn model_connection(&self) -> Resolved {
        let mut c = connection(&self.args, SPEC);
        let base = self.base_url_opt().or(c.configured_base_url.clone());
        c.base_url = Some(base.clone().unwrap_or_else(|| "https://api.meta.ai/v1".into()));
        c.configured_base_url = base;
        c
    }

    fn model_slug(&self) -> Option<String> {
        self.args.model_name.as_ref().map(|m| m.strip_prefix("meta/").unwrap_or(m).to_string())
    }

    fn effective_config(&self) -> Result<Value> {
        let mut config = self.base_config.clone();
        if !self.args.mcp_servers.is_empty() {
            let servers = config.entry("mcp_servers").or_insert_with(|| json!({}));
            let Value::Object(servers) = servers else { bail!("Muse mcp_servers must be a JSON object") };
            for s in &self.args.mcp_servers {
                let entry = match s.transport() {
                    "sse" => bail!("Muse Code does not support SSE MCP servers; use streamable-http or stdio"),
                    "stdio" => json!({"enabled": true, "mode": "required", "transport": "stdio", "command": s.command, "args": s.args}),
                    _ => json!({"enabled": true, "mode": "required", "transport": "streamable_http", "url": s.url}),
                };
                servers.insert(s.name.clone(), entry);
            }
        }
        Ok(Value::Object(config))
    }

    fn run_command(&self, instruction: &str, session_id: &str, load: bool) -> Result<String> {
        let prompt = format!("{RUNTIME_DIR}/prompt.txt");
        let mut parts = vec![
            "muse exec".to_string(),
            "--json".into(),
            "--yolo".into(),
            "--user-input-auto-resolve".into(),
            format!("--session-id {session_id}"),
            format!("--prompt-file {prompt}"),
        ];
        if load {
            parts.push("--allow-workspace-switch".into());
        }
        if let Some(m) = self.model_slug() {
            parts.push(format!("--model {}", quote(&m)));
        }
        let flags = self.cli_flags()?;
        if !flags.is_empty() {
            parts.push(flags);
        }
        if self.base_url_opt().is_none()
            && let Some(u) = self.model_connection().configured_base_url
        {
            parts.push(format!("--base-url {}", quote(&u)));
        }
        Ok(format!(
            "{PATH_EXPORT}; printf '%s' {} > {prompt} && {} 2> >(tee -a {} >&2) < /dev/null | tee -a {}",
            quote(instruction),
            parts.join(" "),
            quote(&format!("{LOGS}/{STDERR_FILENAME}")),
            quote(&format!("{LOGS}/{OUTPUT_FILENAME}"))
        ))
    }
}

/// Validate the load trajectory at construction: ATIF must parse; native must be a valid session.jsonl.
fn validate_load_trajectory(p: &Path) -> Result<()> {
    if p.extension().is_some_and(|e| e == "json") {
        let raw = std::fs::read_to_string(p).map_err(|e| anyhow!("load_trajectory file not found: {}: {e}", p.display()))?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| anyhow!("load_trajectory is not a valid ATIF trajectory file ({}): {e}", p.display()))?;
        if !v["steps"].is_array() {
            bail!("load_trajectory is not a valid ATIF trajectory file ({}): missing steps", p.display());
        }
        return Ok(());
    }
    if p.file_name().is_none_or(|n| n != "session.jsonl") || !p.is_file() {
        bail!("Muse native load_trajectory must be an existing session.jsonl file");
    }
    let records = read_session(&std::fs::read_to_string(p)?);
    native_session_id_strict(&records)?;
    if session_metadata(&records)["workspace_root"].as_str().is_none_or(str::is_empty) {
        bail!("Muse native log is missing session workspace metadata");
    }
    Ok(())
}

fn native_session_id_strict(records: &Records) -> Result<String> {
    let ids: HashSet<String> = records.iter().map(|(_, r)| r["stream"]["id"].to_string()).collect();
    if ids.len() != 1 {
        bail!("Expected exactly one Muse session in the native log");
    }
    native_session_id(records).ok_or_else(|| anyhow!("Muse session log must contain a valid session UUID"))
}

fn session_metadata(records: &Records) -> Value {
    records
        .iter()
        .find(|(_, r)| r["payload"]["kind"] == "metadata")
        .map(|(_, r)| r["payload"]["record"].clone())
        .unwrap_or_else(|| json!({}))
}

/// Base `_atif_session_id`: reuse the trajectory's session id when it is a UUID; else mint one.
fn atif_session_id(traj: &Value) -> String {
    traj["session_id"]
        .as_str()
        .filter(|s| uuid::Uuid::parse_str(s).is_ok())
        .map(String::from)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// Session message text: text content only.
fn session_text(m: &Value) -> Result<String> {
    Ok(match m {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            if parts.iter().any(|p| p["type"] != "text") {
                bail!("Muse ATIF loading currently requires text messages and tool results");
            }
            parts.iter().map(|p| p["text"].as_str().unwrap_or("")).collect::<Vec<_>>().join("\n")
        }
        _ => String::new(),
    })
}

/// ATIF to native trajectory: conversation history as durable
/// session envelopes, without replaying historical usage or effects.
pub fn atif_to_native(traj: &Value, sid: &str) -> Result<String> {
    let mut rows: Vec<Value> = vec![];
    let mut run_id = uuid::Uuid::new_v4().to_string();
    let mut run_seq = 0;
    let now = chrono::Utc::now().timestamp_micros();
    let append = |rows: &mut Vec<Value>, payload: Value, payload_type: &str| {
        let n = rows.len() as i64;
        rows.push(json!({"schema_version": 1, "id": uuid::Uuid::new_v4().to_string(), "stream": {"kind": "session", "id": sid},
            "sequence": n + 1, "recorded_at": now + n, "record_type": "event", "durability": "durable", "causation_id": null,
            "payload_type": payload_type, "payload_schema_version": 1, "payload": payload}));
    };
    let event = |rows: &mut Vec<Value>, run_id: &str, run_seq: &mut i64, value: Value| {
        *run_seq += 1;
        append(
            rows,
            json!({"kind": "run", "run_id": run_id, "event": value, "source_run_record_id": uuid::Uuid::new_v4().to_string(), "source_run_record_sequence": *run_seq}),
            "runtime.session",
        );
    };
    // The uploader replaces this placeholder with the task environment's cwd.
    append(
        &mut rows,
        json!({"kind": "metadata", "record": {"workspace_root": "/", "provider_id": "meta", "web_search_mode": "client", "tool_surface_version": "2"}}),
        "runtime.session.metadata",
    );
    let steps: Vec<&Value> = traj["steps"].as_array().into_iter().flatten().collect();
    let (mut system, mut user_count, mut system_allowed) = (vec![], 0, true);
    for step in &steps {
        match step["source"].as_str() {
            Some("user") => user_count += 1,
            Some("agent") => system_allowed = false,
            Some("system") => {
                if !system_allowed || user_count > 1 {
                    bail!("Muse ATIF loading cannot preserve later system steps; use a native session.jsonl for this history");
                }
                system.push(session_text(&step["message"])?);
            }
            _ => {}
        }
    }
    let mut started = false;
    for step in &steps {
        match step["source"].as_str() {
            Some("system") => continue,
            Some("user") => {
                if started {
                    event(&mut rows, &run_id, &mut run_seq, json!({"kind": "terminal", "terminal": "completed", "reason": null}));
                    (run_id, run_seq) = (uuid::Uuid::new_v4().to_string(), 0);
                }
                event(&mut rows, &run_id, &mut run_seq, json!({"kind": "started", "prompt": session_text(&step["message"])?}));
                if !system.is_empty() {
                    event(
                        &mut rows,
                        &run_id,
                        &mut run_seq,
                        json!({"kind": "model_request_configured", "base_instructions": system.join("\n")}),
                    );
                }
                started = true;
                continue;
            }
            _ => {}
        }
        if !started {
            bail!("Muse ATIF loading requires a user message before agent steps");
        }
        let response_id = uuid::Uuid::new_v4().to_string();
        event(&mut rows, &run_id, &mut run_seq, json!({"kind": "model_response_created", "response_id": response_id}));
        let reasoning = &step["extra"]["muse_reasoning"];
        let reasoning_text = step["reasoning_content"].as_str().unwrap_or("");
        if !reasoning_text.is_empty() || reasoning.as_object().is_some_and(|o| !o.is_empty()) {
            event(
                &mut rows,
                &run_id,
                &mut run_seq,
                json!({"kind": "reasoning_committed", "message_id": uuid::Uuid::new_v4().to_string(), "response_id": response_id,
                "text": reasoning_text, "encrypted_content": reasoning["encrypted_content"]}),
            );
        }
        let message = session_text(&step["message"])?;
        if !message.is_empty() {
            event(
                &mut rows,
                &run_id,
                &mut run_seq,
                json!({"kind": "assistant_message_committed", "message_id": uuid::Uuid::new_v4().to_string(), "response_id": response_id, "text": message}),
            );
        }
        let calls: Vec<&Value> = step["tool_calls"].as_array().into_iter().flatten().collect();
        if !calls.is_empty() {
            let tools: Vec<Value> = calls
                .iter()
                .map(|c| {
                    let raw = &c["extra"]["muse_tool_call"];
                    let id = if raw.get("id").is_some() { raw["id"].clone() } else { json!(uuid::Uuid::new_v4().to_string()) };
                    let args = if raw.get("args").is_some() { raw["args"].clone() } else { json!(json_spaced(&c["arguments"])) };
                    json!({"call_id": c["tool_call_id"], "id": id, "name": c["function_name"], "args": args})
                })
                .collect();
            event(
                &mut rows,
                &run_id,
                &mut run_seq,
                json!({"kind": "assistant_tool_calls_committed", "message_id": uuid::Uuid::new_v4().to_string(), "response_id": response_id, "tool_calls": tools}),
            );
        }
        let results: Vec<&Value> = step["observation"]["results"].as_array().into_iter().flatten().collect();
        if !calls.is_empty() {
            let by_call: BTreeMap<String, &Value> = results.iter().map(|r| (r["source_call_id"].to_string(), *r)).collect();
            let call_ids: HashSet<String> = calls.iter().map(|c| c["tool_call_id"].to_string()).collect();
            if by_call.len() != results.len() || by_call.keys().cloned().collect::<HashSet<_>>() != call_ids {
                bail!("Muse ATIF loading requires exactly one result per tool call");
            }
            let mut out = vec![];
            for (i, c) in calls.iter().enumerate() {
                out.push(json!({"tool_call_index": i, "tool_call_id": c["tool_call_id"], "text": session_text(&by_call[&c["tool_call_id"].to_string()]["content"])?}));
            }
            event(
                &mut rows,
                &run_id,
                &mut run_seq,
                json!({"kind": "tool_result_batch_committed", "batch_id": uuid::Uuid::new_v4().to_string(), "results": out}),
            );
        } else if !results.is_empty() {
            bail!("Muse ATIF loading requires tool results to reference a tool call");
        }
    }
    if !started {
        bail!("Muse ATIF loading requires at least one user message");
    }
    event(&mut rows, &run_id, &mut run_seq, json!({"kind": "terminal", "terminal": "completed", "reason": null}));
    Ok(rows.iter().map(|r| json_spaced(r) + "\n").collect())
}

/// Relocate a session: change only the copied workspace metadata.
pub fn relocate_session(text: &str, workspace: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| match serde_json::from_str::<Value>(line) {
            Ok(mut row) if row["payload"]["kind"] == "metadata" => {
                row["payload"]["record"]["workspace_root"] = json!(workspace);
                json_spaced(&row) + "\n"
            }
            _ => line.to_string(),
        })
        .collect()
}

/// Upload as root, then chown to the agent user.
async fn upload_owned(cx: &AgentCtx<'_>, local: &Path, target: &str) -> Result<()> {
    cx.env.upload_file(local, target).await?;
    if let Some(u) = &cx.user {
        exec_root(cx, &format!("chown {} {}", quote(u), quote(target)), &BTreeMap::new()).await?;
    }
    Ok(())
}

impl MuseCode {
    /// Seed and upload the load trajectory; returns the seeded session id.
    async fn seed_load_trajectory(&self, cx: &AgentCtx<'_>, src: &Path) -> Result<String> {
        let (sid, content, native_dir) = if src.extension().is_some_and(|e| e == "json") {
            let traj: Value = serde_json::from_str(&std::fs::read_to_string(src)?)?;
            let content = atif_to_native(&traj, &atif_session_id(&traj))?;
            (native_session_id_strict(&read_session(&content))?, content, None)
        } else {
            let content = std::fs::read_to_string(src)?;
            let sid = native_session_id_strict(&read_session(&content))?;
            let dir = src.parent().filter(|p| p.file_name().is_some_and(|n| n == sid.as_str()));
            (sid, content, dir)
        };
        let date = chrono::Utc::now().format("%Y/%m/%d");
        let target = format!("{LOGS}/sessions/{date}/{sid}");
        exec_agent(cx, &format!("test ! -e {target} && mkdir -p {target}"), &BTreeMap::new()).await?;
        if let Some(dir) = native_dir {
            cx.env.upload_dir(dir, &target).await?;
            if let Some(u) = &cx.user {
                exec_root(cx, &format!("chown -R {} {target}", quote(u)), &BTreeMap::new()).await?;
            }
        }
        let cwd = exec_agent(cx, "pwd -P", &BTreeMap::new()).await?;
        let local = std::env::temp_dir().join(format!("hbench-muse-{}.jsonl", uuid::Uuid::new_v4().simple()));
        std::fs::write(&local, relocate_session(&content, cwd.stdout.trim()))?;
        let r = upload_owned(cx, &local, &format!("{target}/session.jsonl")).await;
        let _ = std::fs::remove_file(&local);
        r?;
        Ok(sid)
    }
}

fn read_manifest(logs_dir: &Path) -> Result<Value> {
    let p = logs_dir.join(MANIFEST);
    if !p.exists() {
        bail!("Cannot resume Muse Code: no saved session");
    }
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(p)?)?;
    let sid = uuid::Uuid::parse_str(v["session_id"].as_str().unwrap_or_default())?.to_string();
    v["session_id"] = json!(sid);
    Ok(v)
}

/// `find ... -exec wc -l {} +` output -> {path relative to sessions root: line count}.
fn parse_positions(stdout: &str, root: &str) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    for line in stdout.lines() {
        let (count, file) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        if let (Some(rel), Ok(n)) = (file.trim().strip_prefix(&format!("{root}/")), count.parse()) {
            out.insert(rel.to_string(), n);
        }
    }
    out
}

type Records = Vec<(usize, Value)>;

/// Session envelopes v1 with physical line positions, deduped by record id.
fn read_session(text: &str) -> Records {
    let mut seen = HashSet::new();
    let mut out = vec![];
    for (i, line) in text.lines().enumerate() {
        let Ok(row) = serde_json::from_str::<Value>(line) else { continue };
        if !row["payload"].is_object() || row["schema_version"] != json!(1) {
            continue;
        }
        if let Some(id) = row["id"].as_str().filter(|s| !s.is_empty())
            && !seen.insert(id.to_string())
        {
            continue;
        }
        out.push((i + 1, row));
    }
    out
}

/// Run events after `after`, skipping replayed duplicates of the same source record.
fn run_events(records: &Records, after: usize) -> Vec<&Value> {
    let mut seen = HashSet::new();
    let mut out = vec![];
    for (pos, row) in records {
        let p = &row["payload"];
        if p["kind"] != "run" {
            continue;
        }
        if let Some(src) = p["source_run_record_id"].as_str().filter(|s| !s.is_empty())
            && !seen.insert((p["run_id"].as_str().unwrap_or("").to_string(), src.to_string()))
        {
            continue;
        }
        if *pos > after && p["event"].is_object() {
            out.push(&p["event"]);
        }
    }
    out
}

fn native_session_id(records: &Records) -> Option<String> {
    let ids: HashSet<String> = records.iter().map(|(_, r)| r["stream"]["id"].to_string()).collect();
    if ids.len() != 1 {
        return None;
    }
    records.first().and_then(|(_, r)| r["stream"]["id"].as_str()).and_then(|s| uuid::Uuid::parse_str(s).ok()).map(|u| u.to_string())
}

/// Whether the ATIF converter would produce any step (else the session yields no trajectory).
fn has_steps(records: &Records) -> bool {
    run_events(records, 0).iter().any(|e| match e["kind"].as_str().unwrap_or("") {
        "started"
        | "model_response_created"
        | "assistant_message_committed"
        | "assistant_tool_calls_committed"
        | "reasoning_committed"
        | "reasoning_summary_committed"
        | "model_completed"
        | "tool_result"
        | "context_compaction_installed" => true,
        "model_request_configured" => e["base_instructions"].as_str().is_some_and(|s| !s.is_empty()),
        "tool_result_batch_committed" => e["results"].as_array().is_some_and(|a| !a.is_empty()),
        _ => false,
    })
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Usage {
    prompt: Option<i64>,
    completion: Option<i64>,
    cached: Option<i64>,
    cost: Option<f64>,
}

/// Sum known token counts; cost only when every value is priced.
fn sum_usage(values: &[Usage]) -> Usage {
    let s = |f: fn(&Usage) -> Option<i64>| {
        let known: Vec<i64> = values.iter().filter_map(f).collect();
        (!known.is_empty()).then(|| known.iter().sum())
    };
    let cost =
        if values.is_empty() || values.iter().any(|v| v.cost.is_none()) { None } else { Some(values.iter().filter_map(|v| v.cost).sum()) };
    Usage { prompt: s(|v| v.prompt), completion: s(|v| v.completion), cached: s(|v| v.cached), cost }
}

fn session_usage(records: &Records, after: usize) -> Usage {
    let values: Vec<Usage> = run_events(records, after)
        .into_iter()
        .filter(|e| e["kind"] == "model_completed" && e["usage"].is_object())
        .map(|e| {
            let u = &e["usage"];
            Usage {
                prompt: u["input_tokens"].as_i64(),
                completion: u["output_tokens"].as_i64(),
                cached: u["cached_tokens"].as_i64(),
                cost: u["cost_usd"].as_f64(),
            }
        })
        .collect();
    sum_usage(&values)
}

fn find_sessions(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            find_sessions(&p, out);
        } else if p.file_name().is_some_and(|n| n == "session.jsonl") {
            out.push(p);
        }
    }
}

/// Usage totals over the root session (`sessions/*/*/*/<sid>/session.jsonl`) and its nested
/// subagent/observer sessions, counting only records after each file's `usage_start` boundary.
fn usage_from_logs(logs_dir: &Path) -> Option<Usage> {
    let manifest = read_manifest(logs_dir).ok()?;
    let sid = manifest["session_id"].as_str()?.to_string();
    let root = logs_dir.join("sessions");
    let mut all = vec![];
    find_sessions(&root, &mut all);
    let roots: Vec<&PathBuf> = all
        .iter()
        .filter(|p| {
            let c: Vec<_> = p.strip_prefix(&root).map(|r| r.components().collect()).unwrap_or_default();
            c.len() == 5 && c[3].as_os_str() == sid.as_str()
        })
        .collect();
    let [root_path] = roots[..] else { return None };
    let parent = root_path.parent()?;
    let mut children: Vec<&PathBuf> = all.iter().filter(|p| p.starts_with(parent) && *p != root_path).collect();
    children.sort();
    let mut usage = vec![];
    for (i, path) in std::iter::once(root_path).chain(children).enumerate() {
        let key = path.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/");
        let Ok(text) = std::fs::read_to_string(path) else {
            if i == 0 {
                return None;
            }
            continue;
        };
        let records = read_session(&text);
        let steps = has_steps(&records);
        if steps && native_session_id(&records).is_none() {
            // The converter fails here and the session is skipped.
            if i == 0 {
                return None;
            }
            continue;
        }
        if i == 0 && !steps {
            return None;
        }
        let after = manifest["usage_start"][&key].as_u64().unwrap_or(0) as usize;
        usage.push(session_usage(&records, after));
    }
    Some(sum_usage(&usage))
}

#[async_trait::async_trait]
impl Agent for MuseCode {
    fn name(&self) -> &str {
        "muse-code"
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

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        if self.version.is_some() {
            bail!(
                "Agent install failed: muse-code does not support version pinning: the installer always fetches the latest stable release."
            );
        }
        ensure_system_dependencies(cx, &["curl", "bash", "ca_certificates", "coreutils"]).await?;
        exec_agent(
            cx,
            &format!("set -euo pipefail; curl -fsSL {INSTALL_SCRIPT_URL} | MUSE_NO_MODIFY_PATH=1 bash && {PATH_EXPORT} && MUSE_NO_AUTO_UPDATE=1 muse --version"),
            &BTreeMap::new(),
        )
        .await?;
        let r = cx.exec(&format!("{PATH_EXPORT}; MUSE_NO_AUTO_UPDATE=1 muse --version"), &BTreeMap::new(), None).await?;
        if r.code == 0 && !r.stdout.is_empty() {
            self.version = Some(parse_version(&r.stdout));
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let Some(api_key) = self.model_connection().api_key.filter(|k| !k.is_empty()) else {
            bail!("META_API_KEY is required. Set it on the host or pass it with --ae META_API_KEY=...");
        };
        let (config_home, data_home) = (format!("{RUNTIME_DIR}/config"), format!("{RUNTIME_DIR}/data"));
        let env = BTreeMap::from([
            ("META_API_KEY".to_string(), api_key),
            ("MUSE_NO_AUTO_UPDATE".to_string(), "1".to_string()),
            ("XDG_CONFIG_HOME".to_string(), config_home.clone()),
            ("XDG_DATA_HOME".to_string(), data_home.clone()),
        ]);
        let sessions = format!("{LOGS}/sessions");
        let mut setup = format!("mkdir -p {config_home}/muse {data_home}/muse {sessions} && ln -sfn {sessions} {data_home}/muse/sessions");
        if let Some(d) = skills_dir(&self.args) {
            setup += &format!(" && mkdir -p {config_home}/muse/skills && cp -R {}/. {config_home}/muse/skills/", quote(&d));
        }
        exec_agent(cx, &setup, &env).await?;
        upload_text(cx, &format!("{config_home}/muse/settings.json"), &serde_json::to_string(&self.effective_config()?)?).await?;

        let load_src = self.args.load_trajectory.clone().filter(|_| !self.loaded);
        let load = load_src.is_some();
        let session_id = if let Some(src) = load_src {
            self.loaded = true;
            self.seed_load_trajectory(cx, &src).await?
        } else if resume {
            read_manifest(&cx.logs_dir)?["session_id"].as_str().unwrap_or_default().to_string()
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        let r = exec_agent(cx, &format!("find {sessions} -type f -name session.jsonl -exec wc -l {{}} +"), &BTreeMap::new()).await?;
        let usage_start = parse_positions(&r.stdout, &sessions);
        if resume && !usage_start.keys().any(|p| format!("/{p}").contains(&format!("/{session_id}/session.jsonl"))) {
            bail!("Cannot resume Muse Code: the saved session log is missing");
        }
        let manifest = json!({
            "session_id": session_id,
            "usage_start": usage_start,
            "copied_until": if load || resume { json!(usage_start) } else { json!({}) },
        });
        std::fs::create_dir_all(&cx.logs_dir)?;
        let local = cx.logs_dir.join(MANIFEST);
        std::fs::write(&local, serde_json::to_string(&manifest)?)?;
        upload_owned(cx, &local, &format!("{LOGS}/{MANIFEST}")).await?;
        exec_agent(cx, &self.run_command(&instruction, &session_id, load)?, &env).await?;
        Ok(())
    }

    fn collect_usage(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        if let Some(u) = usage_from_logs(logs_dir) {
            ctx.n_input_tokens = u.prompt;
            ctx.n_cache_tokens = u.cached;
            ctx.n_output_tokens = u.completion;
            ctx.cost_usd = u.cost;
        }
    }
}

fn parse_version(stdout: &str) -> String {
    let re = Regex::new(r"(\d+\.\d+\.\d+(?:-[\w.]+)?)").unwrap();
    re.captures(stdout).map(|c| c[1].to_string()).unwrap_or_else(|| stdout.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;
    use crate::task::McpServer;

    const SID: &str = "00000000-0000-4000-8000-000000000001";
    const CHILD: &str = "00000000-0000-4000-8000-000000000002";
    const SESSION: &str = "2026/09/09/00000000-0000-4000-8000-000000000001/session.jsonl";

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hbench-muse-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("environment")).unwrap();
        dir
    }

    fn agent_with(kwargs: Value, env: &[(&str, &str)], model: Option<&str>, mcp: Vec<McpServer>) -> Result<MuseCode> {
        MuseCode::new(AgentArgs { mcp_servers: mcp, ..common::test_args(kwargs, env, model) })
    }

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<MuseCode> {
        agent_with(kwargs, env, model, vec![])
    }

    /// Write a test session (durable envelope v1).
    fn write_session(path: &Path, sid: &str) {
        let events = vec![
            json!({"kind": "started", "prompt": "Read the answer"}),
            json!({"kind": "model_response_created", "response_id": "response-1"}),
            json!({"kind": "text_delta", "text": "Reading"}),
            json!({"kind": "assistant_tool_calls_committed", "response_id": "response-1", "tool_calls": [{"id": "fc-1", "call_id": "c1", "name": "bash", "args": "{}"}]}),
            json!({"kind": "model_completed", "model": "muse-spark-1.2", "usage": {"input_tokens": 20, "output_tokens": 5, "cached_tokens": 8, "reasoning_tokens": 2}}),
            json!({"kind": "tool_result", "call_id": "c1", "text": "42"}),
            json!({"kind": "model_response_created", "response_id": "response-2"}),
            json!({"kind": "model_completed", "usage": {"input_tokens": 40, "output_tokens": 3, "cached_tokens": 20}}),
            json!({"kind": "assistant_message_committed", "response_id": "response-2", "text": "The answer is 42."}),
            json!({"kind": "terminal", "terminal": "completed", "reason": null}),
        ];
        let mut payloads = vec![json!({"kind": "metadata", "record": {"workspace_root": "/app", "provider_id": "meta"}})];
        for (i, e) in events.into_iter().enumerate() {
            payloads.push(json!({"kind": "run", "run_id": "run-1", "event": e, "source_run_record_id": uuid::Uuid::new_v4().to_string(), "source_run_record_sequence": i + 1}));
        }
        let rows: String = payloads
            .into_iter()
            .enumerate()
            .map(|(i, p)| json!({"schema_version": 1, "id": uuid::Uuid::new_v4().to_string(), "stream": {"kind": "session", "id": sid}, "sequence": i + 1, "payload": p}).to_string() + "\n")
            .collect();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, rows).unwrap();
    }

    #[test]
    fn flags_command_and_validation() {
        let a = agent(
            json!({"reasoning_effort": "xhigh", "max_model_steps": 50}),
            &[("META_API_KEY", "test-key")],
            Some("meta/muse-spark-1.2"),
        )
        .unwrap();
        let cmd = a.run_command("do the task", SID, false).unwrap();
        for part in [
            "muse exec --json --yolo --user-input-auto-resolve --session-id 00000000-0000-4000-8000-000000000001 --prompt-file /tmp/hbench-muse/agent/prompt.txt",
            "printf '%s' 'do the task' > /tmp/hbench-muse/agent/prompt.txt",
            "--model muse-spark-1.2 --reasoning-effort xhigh --max-model-steps 50",
            "2> >(tee -a /logs/agent/muse-code.stderr.log >&2) < /dev/null | tee -a /logs/agent/muse-code.jsonl",
        ] {
            assert!(cmd.contains(part), "{part} not in {cmd}");
        }
        assert!(!cmd.contains("test-key"));
        assert_eq!(a.model_connection().api_key.as_deref(), Some("test-key"));
        assert_eq!(a.model_connection().base_url.as_deref(), Some("https://api.meta.ai/v1"));
        assert_eq!(agent(json!({}), &[], Some("openai/gpt-5")).unwrap().model_slug().as_deref(), Some("openai/gpt-5"));
        // META_BASE_URL alias is forwarded, quoted.
        let b = agent(json!({}), &[("META_BASE_URL", "https://example.com/v1?x=1&y=2")], None).unwrap();
        assert!(b.run_command("hi", SID, false).unwrap().contains("--base-url 'https://example.com/v1?x=1&y=2'"));
        let e = agent(json!({"base_url": "https://explicit/v1"}), &[("META_BASE_URL", "https://other")], None).unwrap();
        assert_eq!(e.model_connection().base_url.as_deref(), Some("https://explicit/v1"));
        assert_eq!(e.run_command("hi", SID, false).unwrap().matches("--base-url").count(), 1);
        for bad in [
            json!({"max_model_steps": 0}),
            json!({"max_model_steps": -1}),
            json!({"reasoning_effort": "none"}),
            json!({"reasoning_effort": "invalid"}),
        ] {
            assert!(agent(bad, &[], None).is_err());
        }
        for cfg in [json!({"schema_version": 2}), json!({"schema_version": true})] {
            assert!(agent(json!({"config": cfg}), &[], None).is_err());
        }
        let p = tmp().join("settings.json");
        std::fs::write(&p, "[]").unwrap();
        assert!(agent(json!({"config": p.to_str().unwrap()}), &[], None).is_err());
    }

    #[test]
    fn config_and_mcp_merge() {
        let srv = |name: &str, t: &str, url: Option<&str>, cmd: Option<&str>| McpServer {
            name: name.into(),
            transport: t.into(),
            url: url.map(String::from),
            command: cmd.map(String::from),
            args: if cmd.is_some() { vec!["--root".into(), "a b".into()] } else { vec![] },
        };
        let config = json!({"runtime_capabilities": {"verification": false}, "mcp_servers": {"files": {"transport": "stdio", "command": "old"}, "extra": {"transport": "stdio", "command": "keep"}}});
        let a = agent_with(
            json!({"config": config}),
            &[],
            None,
            vec![srv("files", "stdio", None, Some("server")), srv("remote", "streamable-http", Some("https://example.com/mcp"), None)],
        )
        .unwrap();
        let eff = a.effective_config().unwrap();
        assert_eq!(eff["schema_version"], 1);
        assert_eq!(
            eff["mcp_servers"]["files"],
            json!({"transport": "stdio", "command": "server", "args": ["--root", "a b"], "enabled": true, "mode": "required"})
        );
        assert_eq!(eff["mcp_servers"]["remote"]["transport"], "streamable_http");
        assert_eq!(eff["mcp_servers"]["extra"]["command"], "keep");
        let e = agent_with(json!({}), &[], None, vec![srv("old", "sse", Some("https://example.com/sse"), None)]);
        assert!(e.err().unwrap().to_string().contains("SSE"));
    }

    #[test]
    fn usage_with_children_and_boundaries() {
        let logs = tmp();
        write_session(&logs.join("sessions").join(SESSION), SID);
        let child = format!("2026/09/09/{SID}/subagent/{CHILD}/session.jsonl");
        write_session(&logs.join("sessions").join(&child), CHILD);
        std::fs::write(logs.join(MANIFEST), json!({"session_id": SID, "usage_start": {}, "copied_until": {}}).to_string()).unwrap();
        let mut ctx = AgentContext::default();
        MuseCode::collect_usage(&agent(json!({}), &[], None).unwrap(), &logs, &mut ctx);
        assert_eq!((ctx.n_input_tokens, ctx.n_cache_tokens, ctx.n_output_tokens, ctx.cost_usd), (Some(120), Some(56), Some(16), None));
        // Resume boundary: root lines up to 7 (first model_completed is line 6) are excluded.
        std::fs::write(logs.join(MANIFEST), json!({"session_id": SID, "usage_start": {SESSION: 7}}).to_string()).unwrap();
        let u = usage_from_logs(&logs).unwrap();
        assert_eq!((u.prompt, u.cached, u.completion), (Some(100), Some(48), Some(11)));
    }

    #[test]
    fn atif_to_native_shape_and_load_validation() {
        let traj = json!({"schema_version": "ATIF-v1.4", "session_id": SID, "agent": {"name": "x", "version": "1"},
        "steps": [
            {"step_id": 1, "source": "system", "message": "be brief"},
            {"step_id": 2, "source": "user", "message": "Read the answer"},
            {"step_id": 3, "source": "agent", "message": "Reading", "reasoning_content": "think",
             "tool_calls": [{"tool_call_id": "c1", "function_name": "bash", "arguments": {"cmd": "cat x", "n": 1}}],
             "observation": {"results": [{"source_call_id": "c1", "content": "42"}]}},
            {"step_id": 4, "source": "user", "message": [{"type": "text", "text": "again"}]}
        ]});
        let out = atif_to_native(&traj, SID).unwrap();
        let recs = read_session(&out);
        assert_eq!(native_session_id(&recs).as_deref(), Some(SID));
        assert_eq!(session_metadata(&recs)["workspace_root"], "/");
        let kinds: Vec<&str> = run_events(&recs, 0).iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                "started",
                "model_request_configured",
                "model_response_created",
                "reasoning_committed",
                "assistant_message_committed",
                "assistant_tool_calls_committed",
                "tool_result_batch_committed",
                "terminal",
                "started",
                "model_request_configured",
                "terminal"
            ]
        );
        let ev = run_events(&recs, 0);
        assert_eq!(ev[1]["base_instructions"], "be brief");
        assert_eq!(ev[5]["tool_calls"][0]["args"], "{\"cmd\": \"cat x\", \"n\": 1}");
        assert_eq!(ev[6]["results"][0], json!({"tool_call_index": 0, "tool_call_id": "c1", "text": "42"}));
        assert!(out.lines().next().unwrap().contains("\"payload_type\": \"runtime.session.metadata\""));
        assert!(!has_steps(&read_session("")) && has_steps(&recs));
        // Guard rails.
        assert!(atif_to_native(&json!({"steps": [{"source": "agent", "message": "x"}]}), SID).is_err());
        assert!(atif_to_native(&json!({"steps": [{"source": "user", "message": "a"}, {"source": "agent", "message": "b"}, {"source": "system", "message": "late"}]}), SID).is_err());
        assert!(atif_to_native(&json!({"steps": [{"source": "user", "message": "a"}, {"source": "agent", "tool_calls": [{"tool_call_id": "c1", "function_name": "f", "arguments": {}}]}]}), SID).is_err());
        // Relocation rewrites only the metadata workspace.
        let moved = relocate_session(&out, "/app");
        assert_eq!(session_metadata(&read_session(&moved))["workspace_root"], "/app");
        assert_eq!(moved.lines().nth(1), out.lines().nth(1));
        // Native load file validation at construction.
        let dir = tmp().join(SID);
        let native = dir.join("session.jsonl");
        write_session(&native, SID);
        assert!(validate_load_trajectory(&native).is_ok());
        let other = dir.join("other.jsonl");
        std::fs::copy(&native, &other).unwrap();
        assert!(validate_load_trajectory(&other).is_err());
        let bad = tmp().join("session.jsonl");
        write_session(&bad, "not-a-uuid");
        assert!(validate_load_trajectory(&bad).is_err());
        let a = agent(json!({}), &[], Some("meta/muse-spark-1.2")).unwrap();
        assert!(a.supports_load(true) && a.supports_load(false));
        assert!(
            a.run_command("hi", SID, true)
                .unwrap()
                .contains("--prompt-file /tmp/hbench-muse/agent/prompt.txt --allow-workspace-switch --model")
        );
    }

    #[test]
    fn session_parsing_and_positions() {
        let logs = tmp();
        let p = logs.join("s.jsonl");
        write_session(&p, SID);
        let data = std::fs::read_to_string(&p).unwrap();
        let dup = data.lines().nth(5).unwrap();
        let recs = read_session(&format!("{data}{dup}\n{{\"schema_version\":"));
        assert_eq!(recs.len(), 11);
        let u = session_usage(&recs, 0);
        assert_eq!((u.prompt, u.cached, u.completion, u.cost), (Some(60), Some(28), Some(8), None));
        assert_eq!(native_session_id(&recs).as_deref(), Some(SID));
        let priced = sum_usage(&[Usage { cost: Some(0.5), ..Default::default() }, Usage { cost: Some(0.25), ..Default::default() }]);
        assert_eq!(priced.cost, Some(0.75));
        let pos = parse_positions(&format!("  13 /logs/agent/sessions/{SESSION}\n  13 total\n"), "/logs/agent/sessions");
        assert_eq!(pos, BTreeMap::from([(SESSION.to_string(), 13)]));
        assert_eq!(parse_version("Muse Code 1.0.1-R2006.1\n"), "1.0.1-R2006.1");
        assert_eq!(parse_version("unexpected output"), "unexpected output");
    }
}
