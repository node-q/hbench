//! ATIF (Agent Trajectory Interchange Format) models + Claude Code session-jsonl -> ATIF
//! converter.

use crate::fmt::json_spaced_utf8;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

type Obj = Map<String, Value>;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Trajectory {
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trajectory_id: Option<String>,
    pub agent: Agent,
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_metrics: Option<FinalMetrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continued_trajectory_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Obj>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_trajectories: Option<Vec<Trajectory>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Agent {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_definitions: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Obj>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Step {
    pub step_id: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// "system" | "user" | "agent"
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<Value>,
    /// String, or a list of ContentPart objects (multimodal).
    pub message: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Metrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_copied_context: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_call_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Obj>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub tool_call_id: String,
    pub function_name: String,
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Obj>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Observation {
    pub results: Vec<ObservationResult>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ObservationResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_trajectory_ref: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Obj>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Metrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_token_ids: Option<Vec<i64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_token_ids: Option<Vec<i64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Vec<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Obj>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FinalMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_prompt_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_completion_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cached_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_steps: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Obj>,
}

/// Tool-result text of a value: strings as-is, `True`/`False`/`None` literals, spaced JSON otherwise.
fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        _ => json_spaced_utf8(v),
    }
}

fn stringify(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| json_spaced_utf8(v))
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        _ => true,
    }
}

fn nonempty_str(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn join_nonblank<I: IntoIterator<Item = String>>(parts: I, strip: bool) -> String {
    parts
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| if strip { p.trim().to_string() } else { p })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Every file/dir under `dir`, depth-first in name order (symlinked dirs are not followed).
pub fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        out.push(p.clone());
        if is_dir {
            out.extend(walk(&p));
        }
    }
    out
}

fn is_jsonl(p: &Path) -> bool {
    p.is_file() && p.extension().is_some_and(|e| e == "jsonl")
}

/// `(text, reasoning, tool_use blocks)` from an assistant message's content.
fn extract_text_reasoning_tool_uses(content: &Value) -> (String, Option<String>, Vec<Obj>) {
    if let Some(s) = content.as_str() {
        return (s.trim().to_string(), None, vec![]);
    }
    let (mut text, mut reasoning, mut tools) = (vec![], vec![], vec![]);
    match content {
        Value::Array(blocks) => {
            for block in blocks {
                let Some(b) = block.as_object() else {
                    text.push(stringify(block));
                    continue;
                };
                match b.get("type").and_then(Value::as_str) {
                    Some("tool_use") => tools.push(b.clone()),
                    Some("thinking" | "reasoning" | "analysis") => {
                        let v = b.get("text").filter(|v| !v.is_null()).or(b.get("thinking")).unwrap_or(&Value::Null);
                        reasoning.push(v.as_str().map(|s| s.trim().to_string()).unwrap_or_else(|| stringify(v)));
                    }
                    Some("redacted_thinking") => {
                        // OpenRouter tunnels plain reasoning as `openrouter.reasoning:<b64 json>`; real ciphertext is dropped.
                        if let Some(payload) = b.get("data").and_then(Value::as_str).and_then(|d| d.strip_prefix("openrouter.reasoning:")) {
                            use base64::Engine;
                            let engine = base64::engine::GeneralPurpose::new(
                                &base64::alphabet::STANDARD,
                                base64::engine::general_purpose::PAD_INDIFFERENT,
                            );
                            if let Some(t) = engine
                                .decode(payload.trim_end_matches('='))
                                .ok()
                                .and_then(|b| serde_json::from_str::<Value>(&String::from_utf8_lossy(&b)).ok())
                                .and_then(|inner| inner.get("text").and_then(Value::as_str).map(|s| s.trim().to_string()))
                            {
                                reasoning.push(t);
                            }
                        }
                    }
                    Some("code") if b.get("code").is_some_and(Value::is_string) => text.push(b["code"].as_str().unwrap().to_string()),
                    _ => text.push(b.get("text").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| stringify(block))),
                }
            }
        }
        Value::Null => {}
        other => text.push(stringify(other)),
    }
    let reasoning = join_nonblank(reasoning, true);
    (join_nonblank(text, true), (!reasoning.is_empty()).then_some(reasoning), tools)
}

fn build_metrics(usage: Option<&Value>) -> Option<Metrics> {
    let u = usage?.as_object()?;
    let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    let cached = n("cache_read_input_tokens");
    let extra: Obj = u.iter().filter(|(k, _)| *k != "input_tokens" && *k != "output_tokens").map(|(k, v)| (k.clone(), v.clone())).collect();
    Some(Metrics {
        prompt_tokens: Some(n("input_tokens") + cached + n("cache_creation_input_tokens")),
        completion_tokens: Some(n("output_tokens")),
        cached_tokens: Some(cached),
        extra: (!extra.is_empty()).then_some(extra),
        ..Default::default()
    })
}

fn format_tool_result(block: &Obj, tool_use_result: Option<&Value>) -> (Option<String>, Option<Obj>) {
    let mut parts: Vec<String> = vec![];
    match block.get("content") {
        Some(Value::String(s)) => {
            if !s.trim().is_empty() {
                parts.push(s.trim().to_string())
            }
        }
        Some(Value::Array(items)) => parts.extend(items.iter().map(|i| stringify(i).trim().to_string()).filter(|s| !s.is_empty())),
        None | Some(Value::Null) => {}
        Some(other) => parts.push(stringify(other)),
    }
    let mut metadata: Option<Obj> = None;
    if let Some(tur) = tool_use_result.and_then(Value::as_object) {
        metadata = Some(Obj::from_iter([("tool_use_result".to_string(), Value::Object(tur.clone()))]));
        let mut chunks = vec![];
        if truthy(tur.get("stdout")) {
            chunks.push(format!("[stdout]\n{}", scalar_text(&tur["stdout"])).trim_end().to_string());
        }
        if truthy(tur.get("stderr")) {
            chunks.push(format!("[stderr]\n{}", scalar_text(&tur["stderr"])).trim_end().to_string());
        }
        let exit = if truthy(tur.get("exitCode")) { tur.get("exitCode") } else { tur.get("exit_code") };
        if let Some(e) = exit.filter(|e| !e.is_null() && e.as_i64() != Some(0)) {
            chunks.push(format!("[exit_code] {}", scalar_text(e)));
        }
        if truthy(tur.get("interrupted")) {
            chunks.push(format!("[interrupted] {}", scalar_text(&tur["interrupted"])));
        }
        if truthy(tur.get("isImage")) {
            chunks.push(format!("[is_image] {}", scalar_text(&tur["isImage"])));
        }
        let rest: Obj = tur
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "stdout" | "stderr" | "exitCode" | "exit_code" | "interrupted" | "isImage"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !rest.is_empty() {
            chunks.push(format!("[metadata] {}", json_spaced_utf8(&Value::Object(rest))));
        }
        if !chunks.is_empty() {
            parts.push(chunks.join("\n"));
        }
    }
    if block.get("is_error") == Some(&Value::Bool(true)) {
        parts.push("[error] tool reported failure".into());
        metadata.get_or_insert_default().insert("is_error".into(), true.into());
    }
    if let Some(m) = metadata.as_mut() {
        m.entry("raw_tool_result").or_insert_with(|| Value::Object(block.clone()));
    }
    let text = parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n").trim().to_string();
    ((!text.is_empty()).then_some(text), metadata)
}

fn identity_extra(ev: &Value) -> Obj {
    let mut m = Obj::new();
    m.insert("is_sidechain".into(), ev.get("isSidechain").cloned().unwrap_or(Value::Bool(false)));
    if truthy(ev.get("agentId")) {
        m.insert("agent_id".into(), ev["agentId"].clone());
    }
    m
}

fn first_event_model(events: &[Value], include_sidechain: bool) -> Option<String> {
    events
        .iter()
        .filter(|e| include_sidechain || !truthy(e.get("isSidechain")))
        .find_map(|e| nonempty_str(e.get("message").filter(|m| m.is_object()).and_then(|m| m.get("model"))))
}

struct CallSpec {
    call_id: String,
    tool_name: String,
    arguments: Value,
    extra: Option<Obj>,
    output: Option<String>,
    result_extra: Option<Obj>,
}

/// Normalized events (one per future ATIF step).
enum Norm {
    UserMsg {
        timestamp: Option<String>,
        text: String,
        extra: Obj,
    },
    Turn {
        timestamp: Option<String>,
        text: String,
        reasoning: Option<String>,
        metrics: Option<Box<Metrics>>,
        extra: Option<Obj>,
        model_name: Option<String>,
        calls: Vec<CallSpec>,
    },
    Orphan {
        timestamp: Option<String>,
        call_id: String,
        tool_name: String,
        output: Option<String>,
        extra: Obj,
        model_name: Option<String>,
    },
}

/// The single Claude session dir under `<logs_dir>/sessions/projects` (None if 0 or >1).
pub fn session_dir(logs_dir: &Path) -> Option<PathBuf> {
    let root = logs_dir.join("sessions").join("projects");
    let dirs: BTreeSet<PathBuf> = walk(&root)
        .into_iter()
        .filter(|p| is_jsonl(p))
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .filter(|d| !d.strip_prefix(&root).unwrap_or(d).components().any(|c| c.as_os_str() == "subagents"))
        .collect();
    if dirs.len() == 1 { dirs.into_iter().next() } else { None }
}

/// First stream-json `result` event's `total_cost_usd` in `<logs_dir>/claude-code.txt`.
fn stream_total_cost(logs_dir: &Path) -> Option<f64> {
    let text = std::fs::read_to_string(logs_dir.join("claude-code.txt")).ok()?;
    let ev = text
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|e| e["type"] == "result")?;
    ev.get("total_cost_usd")?.as_f64()
}

/// Convert a Claude Code session directory into an ATIF trajectory.
pub fn convert_session(session_dir: &Path, logs_dir: &Path) -> Option<Trajectory> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(session_dir).ok()?.flatten().map(|e| e.path()).filter(|p| is_jsonl(p)).collect();
    files.sort();
    files.extend(
        walk(session_dir).into_iter().filter(|p| is_jsonl(p) && p.parent().and_then(Path::file_name).is_some_and(|n| n == "subagents")),
    );
    let mut seen_uuids = HashSet::new();
    let mut events: Vec<Value> = vec![];
    for f in files {
        for line in std::fs::read_to_string(&f).unwrap_or_default().lines() {
            let Ok(ev) = serde_json::from_str::<Value>(line.trim()) else { continue };
            if let Some(u) = nonempty_str(ev.get("uuid"))
                && !seen_uuids.insert(u)
            {
                continue;
            }
            events.push(ev);
        }
    }
    if events.is_empty() {
        return None;
    }
    events.sort_by(|a, b| a["timestamp"].as_str().unwrap_or("").cmp(b["timestamp"].as_str().unwrap_or("")));

    let session_id = events
        .iter()
        .find_map(|e| e.get("sessionId").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| session_dir.file_name().unwrap_or_default().to_string_lossy().into_owned());
    let version = events.iter().find_map(|e| nonempty_str(e.get("version"))).unwrap_or_else(|| "unknown".into());
    let collect = |k: &str| -> BTreeSet<String> { events.iter().filter_map(|e| nonempty_str(e.get(k))).collect() };
    let mut agent_extra = Obj::new();
    for (key, src) in [("cwds", "cwd"), ("git_branches", "gitBranch"), ("agent_ids", "agentId")] {
        let s = collect(src);
        if !s.is_empty() {
            agent_extra.insert(key.into(), json!(s));
        }
    }
    let default_model = first_event_model(&events, false).or_else(|| first_event_model(&events, true));

    let mut last_usage: HashMap<String, Value> = HashMap::new();
    for ev in events.iter().filter(|e| e["type"] == "assistant") {
        if let (Some(mid), Some(u)) = (nonempty_str(ev["message"].get("id")), ev["message"].get("usage").filter(|u| !u.is_null())) {
            last_usage.insert(mid, u.clone());
        }
    }

    let mut norm: Vec<Norm> = vec![];
    let mut pending: HashMap<String, (usize, usize)> = HashMap::new();
    let mut completed: HashSet<String> = HashSet::new();
    let mut seen_msg_ids: HashSet<String> = HashSet::new();
    let mut turn_by_msgid: HashMap<String, usize> = HashMap::new();

    for ev in &events {
        let Some(message) = ev.get("message").and_then(Value::as_object) else { continue };
        let timestamp = ev.get("timestamp").and_then(Value::as_str).map(str::to_string);
        match ev.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let (text, reasoning, tool_blocks) = extract_text_reasoning_tool_uses(message.get("content").unwrap_or(&Value::Null));
                let msg_id = nonempty_str(message.get("id"));
                let mut metrics = match &msg_id {
                    Some(m) if seen_msg_ids.contains(m) => None,
                    Some(m) => {
                        seen_msg_ids.insert(m.clone());
                        build_metrics(last_usage.get(m).or(message.get("usage")))
                    }
                    None => build_metrics(message.get("usage")),
                };
                let mut extra = Obj::new();
                for k in ["stop_reason", "stop_sequence", "requestId"] {
                    if let Some(v) = message.get(k).filter(|v| !v.is_null()) {
                        extra.insert(k.into(), v.clone());
                    }
                }
                if truthy(ev.get("id")) {
                    extra.insert("id".into(), ev["id"].clone());
                }
                extra.extend(identity_extra(ev));
                if truthy(ev.get("cwd")) {
                    extra.entry("cwd").or_insert(ev["cwd"].clone());
                }
                if truthy(ev.get("userType")) && ev["userType"] != "external" {
                    extra.insert("user_type".into(), ev["userType"].clone());
                }
                let model_name = nonempty_str(message.get("model")).or(default_model.clone());
                let idx = match msg_id.as_ref().and_then(|m| turn_by_msgid.get(m)) {
                    Some(&i) => i,
                    None => {
                        norm.push(Norm::Turn {
                            timestamp,
                            text: String::new(),
                            reasoning: None,
                            metrics: None,
                            extra: (!extra.is_empty()).then_some(extra),
                            model_name,
                            calls: vec![],
                        });
                        if let Some(m) = &msg_id {
                            turn_by_msgid.insert(m.clone(), norm.len() - 1);
                        }
                        norm.len() - 1
                    }
                };
                let Norm::Turn { text: t, reasoning: r, metrics: m, calls, .. } = &mut norm[idx] else { unreachable!() };
                if !text.is_empty() {
                    *t = if t.is_empty() { text } else { format!("{t}\n\n{text}").trim().to_string() };
                }
                if let Some(rs) = reasoning.filter(|_| message.get("role").and_then(Value::as_str) == Some("assistant")) {
                    *r = Some(match r.take() {
                        Some(prev) => format!("{prev}\n\n{rs}"),
                        None => rs,
                    });
                }
                if m.is_none() {
                    *m = metrics.take().map(Box::new);
                }
                for tb in tool_blocks {
                    let Some(call_id) = nonempty_str(tb.get("id")).or_else(|| nonempty_str(tb.get("tool_use_id"))) else { continue };
                    if pending.contains_key(&call_id) || completed.contains(&call_id) {
                        continue;
                    }
                    let raw = tb.get("input").cloned().unwrap_or(Value::Null);
                    let arguments = if raw.is_object() { raw.clone() } else { json!({"input": raw}) };
                    let mut ce = Obj::new();
                    if !raw.is_null() {
                        ce.insert("raw_arguments".into(), raw);
                    }
                    for (src, dst) in [("status", "status"), ("is_error", "tool_use_is_error")] {
                        if let Some(v) = tb.get(src).filter(|v| !v.is_null()) {
                            ce.insert(dst.into(), v.clone());
                        }
                    }
                    if truthy(tb.get("name")) {
                        ce.entry("tool_use_name").or_insert(tb["name"].clone());
                    }
                    calls.push(CallSpec {
                        call_id: call_id.clone(),
                        tool_name: tb.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                        arguments,
                        extra: (!ce.is_empty()).then_some(ce),
                        output: None,
                        result_extra: None,
                    });
                    pending.insert(call_id, (idx, calls.len() - 1));
                }
            }
            Some("user") => {
                let content = message.get("content").unwrap_or(&Value::Null);
                match content {
                    Value::String(s) => {
                        if !s.trim().is_empty() {
                            norm.push(Norm::UserMsg { timestamp, text: s.clone(), extra: identity_extra(ev) });
                        }
                    }
                    Value::Array(blocks) => {
                        let mut parts: Vec<String> = vec![];
                        for block in blocks {
                            let b = block.as_object();
                            let btype = b.and_then(|b| b.get("type")).and_then(Value::as_str);
                            if btype == Some("text")
                                && let Some(t) = block["text"].as_str()
                            {
                                parts.push(t.to_string());
                                continue;
                            }
                            if btype == Some("tool_result") {
                                let b = b.unwrap();
                                let call_id = nonempty_str(b.get("tool_use_id"));
                                let (output, metadata) = format_tool_result(b, ev.get("toolUseResult"));
                                let is_error = b.get("is_error").filter(|v| !v.is_null()).cloned();
                                if let Some((ti, ci)) = call_id.as_ref().and_then(|c| pending.remove(c)) {
                                    let mut re = Obj::new();
                                    if let Some(m) = metadata.filter(|m| !m.is_empty()) {
                                        re.insert("tool_result_metadata".into(), Value::Object(m));
                                    }
                                    if let Some(e) = is_error {
                                        re.insert("tool_result_is_error".into(), e);
                                    }
                                    if let Norm::Turn { calls, .. } = &mut norm[ti] {
                                        calls[ci].output = output;
                                        calls[ci].result_extra = (!re.is_empty()).then_some(re);
                                    }
                                    completed.insert(call_id.unwrap());
                                    continue;
                                }
                                // Orphan result (e.g. replayed after compaction): keep its output as a standalone call step.
                                if call_id.as_ref().is_some_and(|c| completed.contains(c)) {
                                    continue;
                                }
                                let Some(tool_name) = nonempty_str(b.get("name")).or_else(|| nonempty_str(b.get("tool_name"))) else {
                                    continue;
                                };
                                let mut extra = identity_extra(ev);
                                if let Some(m) = metadata.clone().filter(|m| !m.is_empty()) {
                                    extra.insert("tool_result_metadata".into(), Value::Object(m));
                                }
                                if let Some(e) = is_error {
                                    extra.insert("tool_result_is_error".into(), e);
                                }
                                if let Some(m) = metadata {
                                    extra.entry("metadata").or_insert(Value::Object(m));
                                }
                                if let Some(c) = call_id {
                                    completed.insert(c.clone());
                                    norm.push(Norm::Orphan {
                                        timestamp: timestamp.clone(),
                                        call_id: c,
                                        tool_name,
                                        output,
                                        extra,
                                        model_name: default_model.clone(),
                                    });
                                }
                                continue;
                            }
                            parts.push(stringify(block));
                        }
                        let text = join_nonblank(parts, false);
                        if !text.is_empty() {
                            norm.push(Norm::UserMsg { timestamp, text, extra: identity_extra(ev) });
                        }
                    }
                    Value::Null => {}
                    other => {
                        let text = stringify(other);
                        if !text.trim().is_empty() {
                            norm.push(Norm::UserMsg { timestamp, text, extra: identity_extra(ev) });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let mut steps: Vec<Step> = vec![];
    for n in norm {
        let step_id = steps.len() + 1;
        let step = match n {
            Norm::UserMsg { timestamp, text, extra } => {
                Step { step_id, timestamp, source: "user".into(), message: text.into(), extra: Some(extra), ..Default::default() }
            }
            Norm::Turn { timestamp, text, reasoning, metrics, extra, model_name, calls } => {
                let results: Vec<ObservationResult> = calls
                    .iter()
                    .filter(|c| c.output.is_some())
                    .map(|c| ObservationResult {
                        source_call_id: Some(c.call_id.clone()),
                        content: c.output.clone().map(Value::from),
                        extra: c.result_extra.clone(),
                        ..Default::default()
                    })
                    .collect();
                let tool_calls: Vec<ToolCall> = calls
                    .into_iter()
                    .map(|c| ToolCall { tool_call_id: c.call_id, function_name: c.tool_name, arguments: c.arguments, extra: c.extra })
                    .collect();
                Step {
                    step_id,
                    timestamp,
                    source: "agent".into(),
                    model_name: model_name.or(default_model.clone()),
                    message: text.into(),
                    reasoning_content: reasoning,
                    tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                    observation: (!results.is_empty()).then_some(Observation { results }),
                    metrics: metrics.map(|m| *m),
                    llm_call_count: Some(1),
                    extra,
                    ..Default::default()
                }
            }
            Norm::Orphan { timestamp, call_id, tool_name, output, extra, model_name } => Step {
                step_id,
                timestamp,
                source: "agent".into(),
                model_name: model_name.or(default_model.clone()),
                message: "".into(),
                tool_calls: Some(vec![ToolCall {
                    tool_call_id: call_id.clone(),
                    function_name: tool_name,
                    arguments: json!({}),
                    extra: None,
                }]),
                observation: output.map(|o| Observation {
                    results: vec![ObservationResult { source_call_id: Some(call_id), content: Some(o.into()), ..Default::default() }],
                }),
                llm_call_count: Some(1),
                extra: Some(extra),
                ..Default::default()
            },
        };
        steps.push(step);
    }
    if steps.is_empty() {
        return None;
    }

    let ms: Vec<&Metrics> = steps.iter().filter_map(|s| s.metrics.as_ref()).collect();
    let total = |f: fn(&Metrics) -> Option<i64>| ms.iter().filter_map(|m| f(m)).reduce(|a, b| a + b);
    let mut tiers = BTreeSet::new();
    let (mut creation, mut read): (Option<i64>, Option<i64>) = (None, None);
    for x in ms.iter().filter_map(|m| m.extra.as_ref()) {
        if let Some(t) = x.get("service_tier").and_then(Value::as_str) {
            tiers.insert(t.to_string());
        }
        if let Some(v) = x.get("cache_creation_input_tokens").filter(|v| v.is_i64() || v.is_u64()).and_then(Value::as_i64) {
            creation = Some(creation.unwrap_or(0) + v);
        }
        if let Some(v) = x.get("cache_read_input_tokens").filter(|v| v.is_i64() || v.is_u64()).and_then(Value::as_i64) {
            read = Some(read.unwrap_or(0) + v);
        }
    }
    let mut final_extra = Obj::new();
    if !tiers.is_empty() {
        final_extra.insert("service_tiers".into(), json!(tiers));
    }
    if let Some(c) = creation {
        final_extra.insert("total_cache_creation_input_tokens".into(), c.into());
    }
    if let Some(r) = read {
        final_extra.insert("total_cache_read_input_tokens".into(), r.into());
    }
    let final_metrics = FinalMetrics {
        total_prompt_tokens: total(|m| m.prompt_tokens),
        total_completion_tokens: total(|m| m.completion_tokens),
        total_cached_tokens: total(|m| m.cached_tokens),
        // No LiteLLM price-estimate fallback here, so cost is only the reported one.
        total_cost_usd: stream_total_cost(logs_dir),
        total_steps: Some(steps.len()),
        extra: (!final_extra.is_empty()).then_some(final_extra),
    };
    Some(Trajectory {
        schema_version: "ATIF-v1.7".into(),
        session_id: Some(session_id),
        agent: Agent {
            name: "claude-code".into(),
            version,
            model_name: default_model,
            tool_definitions: None,
            extra: (!agent_extra.is_empty()).then_some(agent_extra),
        },
        steps,
        final_metrics: Some(final_metrics),
        ..Default::default()
    })
}

/// ATIF trajectory (JSON) for a claude-code agent logs dir (`<trial>/agent`), if a single session exists.
pub fn claude_code_trajectory(logs_dir: &Path) -> Option<Value> {
    convert_session(&session_dir(logs_dir)?, logs_dir).and_then(|t| serde_json::to_value(t).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("hbench-atif-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_jsonl(p: &Path, events: &[Value]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, events.iter().map(|e| e.to_string() + "\n").collect::<String>()).unwrap();
    }

    /// A realistic Claude Code 2.x session: instruction, thinking + text + two parallel tool calls split across
    /// two events sharing one message id (with streaming usage updates), Bash toolUseResult, a failing tool,
    /// a final answer, a duplicated uuid, plus a subagent transcript under `<sid>/subagents/`.
    fn session(sid: &str) -> (Vec<Value>, Vec<Value>) {
        let base = |ty: &str, uuid: &str, ts: &str| json!({"type": ty, "uuid": uuid, "timestamp": ts, "sessionId": sid, "version": "2.1.50", "cwd": "/app", "gitBranch": "main", "userType": "external", "isSidechain": false});
        let with = |mut e: Value, k: &str, v: Value| {
            e[k] = v;
            e
        };
        let main = vec![
            with(base("user", "u1", "2026-01-01T00:00:00.000Z"), "message", json!({"role": "user", "content": "Create hello.txt\n"})),
            with(
                base("assistant", "a1", "2026-01-01T00:00:01.000Z"),
                "message",
                json!({"id": "msg_1", "model": "claude-opus-4-6", "role": "assistant", "stop_reason": null,
                       "content": [{"type": "thinking", "thinking": "  I should check the dir. ", "signature": "sig"}, {"type": "text", "text": "Let me look."},
                                   {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}],
                       "usage": {"input_tokens": 10, "output_tokens": 1, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 5, "service_tier": "standard"}}),
            ),
            with(
                base("assistant", "a2", "2026-01-01T00:00:01.500Z"),
                "message",
                json!({"id": "msg_1", "model": "claude-opus-4-6", "role": "assistant", "stop_reason": "tool_use",
                       "content": [{"type": "tool_use", "id": "toolu_2", "name": "Read", "input": {"file_path": "/app/x"}}],
                       "usage": {"input_tokens": 10, "output_tokens": 40, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 5, "service_tier": "standard"}}),
            ),
            with(
                with(
                    base("user", "u2", "2026-01-01T00:00:02.000Z"),
                    "message",
                    json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt\n"}]}),
                ),
                "toolUseResult",
                json!({"stdout": "a.txt", "stderr": "", "interrupted": false, "isImage": false}),
            ),
            with(
                base("user", "u3", "2026-01-01T00:00:02.100Z"),
                "message",
                json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_2", "content": "File does not exist.", "is_error": true}]}),
            ),
            // replayed event: same uuid, must be dropped
            with(
                base("user", "u3", "2026-01-01T00:00:02.100Z"),
                "message",
                json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_2", "content": "dup"}]}),
            ),
            with(
                base("assistant", "a3", "2026-01-01T00:00:04.000Z"),
                "message",
                json!({"id": "msg_2", "model": "claude-opus-4-6", "role": "assistant", "stop_reason": "end_turn",
                       "content": [{"type": "text", "text": "Done."}], "usage": {"input_tokens": 3, "output_tokens": 7, "cache_read_input_tokens": 200, "cache_creation_input_tokens": 0}}),
            ),
            json!({"type": "summary", "summary": "x", "leafUuid": "a3"}),
        ];
        let sub = vec![with(
            with(with(base("assistant", "s1", "2026-01-01T00:00:03.000Z"), "isSidechain", json!(true)), "agentId", json!("agent-7")),
            "message",
            json!({"id": "msg_s", "model": "claude-haiku-4-5", "role": "assistant", "content": [{"type": "text", "text": "Explored."}], "usage": {"input_tokens": 1, "output_tokens": 2}}),
        )];
        (main, sub)
    }

    #[test]
    fn converts_realistic_session() {
        let logs = tmp();
        let sid = "0ae161f9-9d22-4708-8d5c-1f453f0eb05c";
        let proj = logs.join("sessions/projects/-app");
        let (main, sub) = session(sid);
        write_jsonl(&proj.join(format!("{sid}.jsonl")), &main);
        write_jsonl(&proj.join(sid).join("subagents/agent-agent-7.jsonl"), &sub);
        std::fs::write(logs.join("claude-code.txt"), "{\"type\":\"system\"}\n{\"type\":\"result\",\"total_cost_usd\":0.0421}\n").unwrap();
        assert_eq!(session_dir(&logs), Some(proj.clone()));

        let v = claude_code_trajectory(&logs).unwrap();
        let t: Trajectory = serde_json::from_value(v.clone()).unwrap();
        assert_eq!((t.schema_version.as_str(), t.session_id.as_deref()), ("ATIF-v1.7", Some(sid)));
        assert_eq!(
            (t.agent.name.as_str(), t.agent.version.as_str(), t.agent.model_name.as_deref()),
            ("claude-code", "2.1.50", Some("claude-opus-4-6"))
        );
        assert_eq!(t.agent.extra.as_ref().unwrap()["agent_ids"], json!(["agent-7"]));
        let ids: Vec<usize> = t.steps.iter().map(|s| s.step_id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);

        let s1 = &t.steps[0];
        assert_eq!((s1.source.as_str(), s1.message.as_str()), ("user", Some("Create hello.txt\n")));
        assert!(s1.model_name.is_none() && s1.llm_call_count.is_none());

        // one LLM turn bundles thinking, text and both tool calls; usage = last streamed usage
        let s2 = &t.steps[1];
        assert_eq!(s2.message, "Let me look.");
        assert_eq!(s2.reasoning_content.as_deref(), Some("I should check the dir."));
        let calls = s2.tool_calls.as_ref().unwrap();
        assert_eq!(calls.iter().map(|c| c.function_name.as_str()).collect::<Vec<_>>(), ["Bash", "Read"]);
        assert_eq!(calls[0].arguments, json!({"command": "ls"}));
        let m = s2.metrics.as_ref().unwrap();
        assert_eq!((m.prompt_tokens, m.completion_tokens, m.cached_tokens), (Some(115), Some(40), Some(100)));
        let obs = &s2.observation.as_ref().unwrap().results;
        assert_eq!(obs[0].content, Some(json!("a.txt\n\n[stdout]\na.txt")));
        assert_eq!(obs[1].content, Some(json!("File does not exist.\n\n[error] tool reported failure")));
        assert_eq!(obs[1].extra.as_ref().unwrap()["tool_result_is_error"], true);
        assert_eq!(s2.extra.as_ref().unwrap()["is_sidechain"], false);

        // subagent step sorted chronologically, keeps its own model
        let s3 = &t.steps[2];
        assert_eq!((s3.message.as_str(), s3.model_name.as_deref()), (Some("Explored."), Some("claude-haiku-4-5")));
        assert_eq!(s3.extra.as_ref().unwrap()["agent_id"], "agent-7");
        assert_eq!(t.steps[3].message, "Done.");

        let f = t.final_metrics.unwrap();
        assert_eq!(
            (f.total_prompt_tokens, f.total_completion_tokens, f.total_cached_tokens, f.total_steps),
            (Some(115 + 203 + 1), Some(49), Some(300), Some(4))
        );
        assert_eq!(f.total_cost_usd, Some(0.0421));
        assert_eq!(f.extra.unwrap()["service_tiers"], json!(["standard"]));
        // No nulls on model fields
        assert!(v["steps"][0].get("tool_calls").is_none() && v.get("notes").is_none());
    }

    #[test]
    fn reasoning_and_edge_blocks() {
        use base64::Engine;
        let enc = base64::engine::general_purpose::STANDARD.encode(br#"{"text": " routed reasoning ", "type": "reasoning.text"}"#);
        let (text, r, tools) = extract_text_reasoning_tool_uses(&json!([
            {"type": "redacted_thinking", "data": "opaque"},
            {"type": "redacted_thinking", "data": format!("openrouter.reasoning:{}", enc.trim_end_matches('='))},
            {"type": "thinking", "text": "a"}, {"type": "thinking", "thinking": "b"},
            {"type": "image", "source": {"x": 1}},
            {"type": "text", "text": "hi"}
        ]));
        assert_eq!(r.as_deref(), Some("routed reasoning\n\na\n\nb"));
        assert_eq!(text, "{\"type\": \"image\", \"source\": {\"x\": 1}}\n\nhi");
        assert!(tools.is_empty());
        assert!(build_metrics(Some(&json!({"input_tokens": null, "output_tokens": 3}))).unwrap().prompt_tokens == Some(0));
    }

    #[test]
    fn no_or_ambiguous_session() {
        let logs = tmp();
        assert!(claude_code_trajectory(&logs).is_none());
        write_jsonl(&logs.join("sessions/projects/a/x.jsonl"), &[json!({"type": "user", "message": {"content": "hi"}})]);
        assert!(claude_code_trajectory(&logs).is_some());
        write_jsonl(&logs.join("sessions/projects/b/y.jsonl"), &[json!({"type": "user", "message": {"content": "hi"}})]);
        assert!(claude_code_trajectory(&logs).is_none());
    }
}
