//! computer-1's native computer-use providers and their step / self-driving loops.
//!
//! - `anthropic` / `bedrock`: Claude's `computer_2025xxxx` tool over the Messages API (beta).
//!   Bedrock sends the same body re-shaped for its invoke endpoint, signed with AWS SigV4.
//! - `gemini`: Gemini Computer Use through the Interactions API (`previous_interaction_id` chain).
//! - `openai`: the GA `computer` tool through the Responses API; drives its own loop.
//!
//! Requests go through `llm::send_raw` (host curl). Retries mirror the vendor SDKs' defaults.
//! Usage is recorded without pricing native turns (cost stays unset).

use super::{Action, Computer1, SCREENSHOT_DIR, quote, repr_value};
use crate::agents::AgentContext;
use crate::agents::llm::{LlmResponse, Request, Usage, backoff, http_status_kind, send_raw};
use crate::agents::terminus2::{Shell, quote_repr};
use crate::env::{ErrorKind, kind_err, tail};
use crate::fmt::{json_spaced, truthy, type_name};
use crate::task::hex;
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Integer from a JSON value: bools as 0/1, floats truncated, numeric strings (`_` separators allowed) parsed.
fn coerce_int(v: &Value) -> Result<i64, String> {
    match v {
        Value::Bool(b) => Ok(*b as i64),
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64))
            .ok_or_else(|| "integer out of range".into()),
        Value::String(s) => s.trim().replace('_', "").parse::<i64>().map_err(|_| format!("invalid integer: {}", quote_repr(s))),
        other => Err(format!("expected an integer, got {}", type_name(other))),
    }
}

/// Float from a JSON value: bools as 0/1, numbers as-is, numeric strings parsed.
fn coerce_float(v: &Value) -> Result<f64, String> {
    match v {
        Value::Bool(b) => Ok(*b as i64 as f64),
        Value::Number(n) => Ok(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.trim().parse::<f64>().map_err(|_| format!("invalid number: {}", quote_repr(s))),
        other => Err(format!("expected a number, got {}", type_name(other))),
    }
}

/// A string as-is, any other value in literal form (`True`, `None`, `{'k': 1}`).
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => repr_value(other),
    }
}

/// `d[k]` as text, or empty when the key is missing or not truthy.
fn str_or_empty(d: &Map<String, Value>, k: &str) -> String {
    d.get(k).filter(|v| truthy(v)).map(text_of).unwrap_or_default()
}

fn strip_data_url(r: &str) -> &str {
    if r.starts_with("data:") { r.split_once(',').map_or("", |(_, d)| d) } else { r }
}

fn media_type_for_data_url(r: &str) -> &str {
    match r.strip_prefix("data:") {
        Some(rest) => {
            Some(rest.split(',').next().unwrap_or("").split(';').next().unwrap_or("")).filter(|m| !m.is_empty()).unwrap_or("image/webp")
        }
        None => "image/webp",
    }
}

pub(super) const ANTHROPIC_SCALED: &str = "anthropic_scaled";
pub(super) const NORMALIZED: &str = "normalized_completion";

/// Undo Anthropic's internal screenshot downscale: model coordinates -> desktop pixels.
pub(super) fn anthropic_scale(x: i64, y: i64, w: i64, h: i64) -> (i64, i64) {
    let (long, total) = (w.max(h) as f64, (w * h) as f64);
    let les = if long > 1568.0 { 1568.0 / long } else { 1.0 };
    let tps = if total > 1_150_000.0 { (1_150_000.0 / total).sqrt() } else { 1.0 };
    let scale = les.min(tps).min(1.0);
    if scale >= 1.0 { (x, y) } else { ((x as f64 / scale) as i64, (y as f64 / scale) as i64) }
}

/// 0..999 grid -> desktop pixels (round half to even).
fn scale_normalized(x: i64, y: i64, w: i64, h: i64) -> (i64, i64) {
    let s = |v: i64, d: i64| ((v as f64 * (d - 1) as f64 / 999.0).round_ties_even() as i64).clamp(0, d - 1);
    (s(x, w), s(y, h))
}

/// Scale an action given on the normalized 0..999 grid to desktop pixels; other actions pass through.
pub(super) fn normalize_action(a: &Action, w: i64, h: i64) -> Action {
    let mut a = a.clone();
    if a.source != NORMALIZED {
        return a;
    }
    if let (Some(x), Some(y)) = (a.x, a.y) {
        (a.model_x, a.model_y) = (Some(x), Some(y));
        let (x, y) = scale_normalized(x, y, w, h);
        (a.x, a.y) = (Some(x), Some(y));
    }
    if let (Some(x), Some(y)) = (a.end_x, a.end_y) {
        let (x, y) = scale_normalized(x, y, w, h);
        (a.end_x, a.end_y) = (Some(x), Some(y));
    }
    if let Some(r) = a.zoom_region.as_ref().filter(|r| r.len() == 4) {
        let (x0, y0) = scale_normalized(r[0], r[1], w, h);
        let (x1, y1) = scale_normalized(r[2], r[3], w, h);
        a.zoom_region = Some(vec![x0, y0, x1, y1]);
    }
    a
}

/// An Anthropic `computer` tool input as an action. Ok(None) = unknown action.
pub(super) fn translate_anthropic(d: &Map<String, Value>, w: i64, h: i64) -> Result<Option<Action>, String> {
    let action = d.get("action").map(text_of).unwrap_or_default();
    if action == "screenshot" || action == "cursor_position" {
        return Ok(Some(Action::new(&action)));
    }
    let pair = |v: Option<&Value>| -> Result<Option<(i64, i64)>, String> {
        match v.and_then(Value::as_array).filter(|a| a.len() == 2) {
            Some(a) => Ok(Some((coerce_int(&a[0])?, coerce_int(&a[1])?))),
            None => Ok(None),
        }
    };
    let (raw_x, raw_y) = pair(d.get("coordinate"))?.unwrap_or((0, 0));
    let (x, y) = anthropic_scale(raw_x, raw_y, w, h);
    let modifier = if ["left_click", "right_click", "double_click", "triple_click", "middle_click", "scroll"].contains(&action.as_str()) {
        d.get("text")
            .and_then(Value::as_str)
            .map(str::to_lowercase)
            .filter(|m| ["shift", "ctrl", "control", "alt", "super"].contains(&m.as_str()))
    } else {
        None
    };
    let at = |kind: &str| {
        let mut a = Action::new(kind);
        (a.x, a.y, a.model_x, a.model_y, a.source) = (Some(x), Some(y), Some(raw_x), Some(raw_y), ANTHROPIC_SCALED);
        a
    };
    let plain = |kind: &str| {
        let mut a = Action::new(kind);
        a.source = ANTHROPIC_SCALED;
        a
    };
    let split_keys = |s: String| s.split('+').map(str::trim).filter(|k| !k.is_empty()).map(String::from).collect::<Vec<_>>();
    let a = match action.as_str() {
        "left_click" | "right_click" | "double_click" | "triple_click" | "middle_click" => {
            let mut a = at(match action.as_str() {
                "left_click" | "middle_click" => "click",
                k => k,
            });
            if action == "middle_click" {
                a.button = Some("middle".into());
            }
            a.modifier = modifier;
            a
        }
        "left_mouse_down" => at("mouse_down"),
        "left_mouse_up" => at("mouse_up"),
        "mouse_move" => at("mouse_move"),
        "left_click_drag" => {
            let (sx, sy) = pair(d.get("start_coordinate"))?.unwrap_or((raw_x, raw_y));
            let (sx, sy) = anthropic_scale(sx, sy, w, h);
            let mut a = at("drag");
            (a.x, a.y, a.end_x, a.end_y) = (Some(sx), Some(sy), Some(x), Some(y));
            a
        }
        "type" => {
            let mut a = plain("type");
            a.text = Some(str_or_empty(d, "text"));
            a
        }
        "key" => {
            let mut a = plain("keypress");
            a.keys = Some(split_keys(str_or_empty(d, "text")));
            a
        }
        "hold_key" => {
            let mut a = plain("hold_key");
            let k = d.get("key").filter(|v| truthy(v)).or(d.get("text").filter(|v| truthy(v))).map(text_of).unwrap_or_default();
            a.keys = Some(split_keys(k));
            a.duration = Some(d.get("duration").map(coerce_float).transpose()?.unwrap_or(1.0));
            a
        }
        "scroll" => {
            let dir = d.get("scroll_direction").map(text_of).unwrap_or_else(|| "down".into());
            let amount = d.get("scroll_amount").map(coerce_int).transpose()?.unwrap_or(3);
            let (sx, sy) = match dir.as_str() {
                "up" => (0, -1),
                "right" => (1, 0),
                "left" => (-1, 0),
                _ => (0, 1),
            };
            let mut a = plain("scroll");
            (a.x, a.y, a.scroll_x, a.scroll_y, a.modifier) = (Some(x), Some(y), Some(sx * amount * 100), Some(sy * amount * 100), modifier);
            a
        }
        "wait" => plain("wait"),
        "zoom" => match d.get("region").and_then(Value::as_array).filter(|r| r.len() == 4) {
            Some(r) => {
                let r = r.iter().map(coerce_int).collect::<Result<Vec<_>, _>>()?;
                let (x0, y0) = anthropic_scale(r[0], r[1], w, h);
                let (x1, y1) = anthropic_scale(r[2], r[3], w, h);
                let mut a = plain("zoom");
                a.zoom_region = Some(vec![x0, y0, x1, y1]);
                a
            }
            None => return Ok(None),
        },
        _ => return Ok(None),
    };
    Ok(Some(a))
}

/// An OpenAI `computer` action as an action; coordinates are already desktop pixels. None = skip.
pub(super) fn translate_openai(a: &Value) -> Option<Action> {
    let kind = a.get("type").filter(|v| truthy(v)).map(text_of).unwrap_or_default();
    let coord = |k: &str| a.get(k).filter(|v| truthy(v)).map_or(0, |v| coerce_int(v).unwrap_or(0));
    let modifier = || {
        a["keys"]
            .as_array()?
            .iter()
            .map(|k| text_of(k).to_lowercase())
            .find(|k| ["shift", "ctrl", "control", "alt", "option", "super", "cmd", "meta"].contains(&k.as_str()))
    };
    let xy = |kind: &str| {
        let mut r = Action::new(kind);
        (r.x, r.y) = (Some(coord("x")), Some(coord("y")));
        r
    };
    Some(match kind.as_str() {
        "screenshot" | "" => return None,
        "wait" => Action::new("wait"),
        "click" => {
            let button = a.get("button").filter(|v| truthy(v)).map(text_of).unwrap_or_else(|| "left".into()).to_lowercase();
            let (k, btn) = match button.as_str() {
                "right" => ("right_click", None),
                "middle" | "wheel" => ("click", Some("middle")),
                "back" | "forward" => ("click", Some("left")),
                _ => ("click", None),
            };
            let mut r = xy(k);
            (r.button, r.modifier) = (btn.map(String::from), modifier());
            r
        }
        "double_click" => {
            let mut r = xy("double_click");
            r.modifier = modifier();
            r
        }
        "move" => xy("mouse_move"),
        "scroll" => {
            let pick = |a1: &str, a2: &str| {
                a.get(a1).filter(|v| !v.is_null()).or(a.get(a2)).filter(|v| truthy(v)).map_or(0, |v| coerce_int(v).unwrap_or(0))
            };
            let mut r = xy("scroll");
            (r.scroll_x, r.scroll_y, r.modifier) = (Some(pick("scroll_x", "scrollX")), Some(pick("scroll_y", "scrollY")), modifier());
            r
        }
        "type" => {
            let mut r = Action::new("type");
            r.text = Some(a.get("text").filter(|v| truthy(v)).map(text_of).unwrap_or_default());
            r
        }
        "keypress" => {
            let mut r = Action::new("keypress");
            r.keys = Some(match a.get("keys").filter(|v| truthy(v)) {
                None => vec![],
                Some(Value::Array(k)) => k.iter().map(text_of).collect(),
                Some(k) => vec![text_of(k)],
            });
            r
        }
        "drag" => {
            let pts: Vec<(i64, i64)> = a["path"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| {
                    let (px, py) = match (p.get("x"), p.get("y"), p.as_array()) {
                        (Some(x), y, _) => (x.clone(), y.cloned().unwrap_or(Value::Null)),
                        (None, _, Some(l)) if l.len() == 2 => (l[0].clone(), l[1].clone()),
                        _ => (Value::Null, Value::Null),
                    };
                    let i = |v: &Value| if truthy(v) { coerce_int(v).unwrap_or(0) } else { 0 };
                    (i(&px), i(&py))
                })
                .collect();
            if pts.len() < 2 {
                return None;
            }
            let mut r = Action::new("drag");
            (r.x, r.y, r.end_x, r.end_y) = (Some(pts[0].0), Some(pts[0].1), Some(pts[pts.len() - 1].0), Some(pts[pts.len() - 1].1));
            r
        }
        _ => return None,
    })
}

/// A Gemini Computer Use function call as an action. Ok(None) = unknown function.
pub(super) fn translate_gemini(name: &str, d: &Map<String, Value>) -> Result<Option<Action>, String> {
    let int = |k: &str| d.get(k).map_or(Ok(0), coerce_int);
    let mk = |kind: &str, source: &'static str| {
        let mut a = Action::new(kind);
        a.source = source;
        a
    };
    let a = match name {
        "click" | "double_click" | "triple_click" | "middle_click" | "right_click" | "mouse_down" | "mouse_up" | "move" => {
            let mut a = mk(if name == "move" { "mouse_move" } else { name }, NORMALIZED);
            (a.x, a.y) = (Some(int("x")?), Some(int("y")?));
            a
        }
        "type" => {
            let mut a = mk("type", "native_prescaled");
            a.text = Some(str_or_empty(d, "text"));
            a.press_enter = Some(d.get("press_enter").is_some_and(truthy));
            a
        }
        "drag_and_drop" => {
            let mut a = mk("drag", NORMALIZED);
            (a.x, a.y, a.end_x, a.end_y) = (Some(int("start_x")?), Some(int("start_y")?), Some(int("end_x")?), Some(int("end_y")?));
            a
        }
        "scroll" => {
            let (x, y) = (int("x")?, int("y")?);
            let mag = match d.get("magnitude_in_pixels").filter(|v| truthy(v)) {
                Some(v) => coerce_int(v)?,
                None => 300,
            };
            let mag = if mag == 0 { 300 } else { mag };
            let dir = d.get("direction").map(text_of).unwrap_or_else(|| "down".into()).to_lowercase();
            let (sx, sy) = match dir.as_str() {
                "up" => (0, -mag),
                "left" => (-mag, 0),
                "right" => (mag, 0),
                _ => (0, mag),
            };
            let mut a = mk("scroll", NORMALIZED);
            (a.x, a.y, a.scroll_x, a.scroll_y) = (Some(x), Some(y), Some(sx), Some(sy));
            a
        }
        "press_key" | "hotkey" | "key_down" | "key_up" => {
            let keys: Vec<String> = if name == "hotkey" {
                let k: Vec<String> = d.get("keys").and_then(Value::as_array).map(|k| k.iter().map(text_of).collect()).unwrap_or_default();
                if k.is_empty() { vec![] } else { vec![k.join("+")] }
            } else {
                Some(str_or_empty(d, "key")).filter(|k| !k.is_empty()).into_iter().collect()
            };
            if keys.is_empty() {
                return Ok(None);
            }
            let mut a = mk(if matches!(name, "press_key" | "hotkey") { "keypress" } else { name }, "native_prescaled");
            a.keys = Some(keys);
            a
        }
        "zoom_region" => {
            let (x1, y1, x2, y2) = (int("x1")?, int("y1")?, int("x2")?, int("y2")?);
            let mut a = mk("zoom", NORMALIZED);
            a.zoom_region = Some(vec![x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)]);
            a
        }
        "navigate" => {
            let mut a = mk("navigate", "native_prescaled");
            a.url = Some(d.get("url").map(text_of).unwrap_or_default());
            a
        }
        "go_back" | "go_forward" => mk(name, "native_prescaled"),
        "wait" => {
            let mut a = mk("sleep", "native_prescaled");
            a.duration_seconds = Some(match d.get("seconds").filter(|v| truthy(v)) {
                Some(v) => Some(coerce_float(v)?).filter(|s| *s != 0.0).unwrap_or(1.0),
                None => 1.0,
            });
            a
        }
        "take_screenshot" => mk("screenshot", "native_prescaled"),
        _ => return Ok(None),
    };
    Ok(Some(a))
}

/// One tool call from a native provider response, with its translated action or the reason it has none.
#[derive(Debug, Clone)]
pub(super) struct NativeCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
    pub action: Option<Action>,
    pub error: Option<String>,
}

/// One API response: all its calls, usage counted once.
#[derive(Debug, Clone, Default)]
pub(super) struct NativeStep {
    pub calls: Vec<NativeCall>,
    pub message: String,
    pub usage: Option<Usage>,
    pub response_id: Option<String>,
    pub is_terminal: bool,
}

/// The outcome of executing one tool call.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ExecResult {
    pub call_id: String,
    /// "success" | "error" | "not_executed"
    pub status: &'static str,
    pub text: String,
    pub screenshot_paths: Vec<String>,
    pub screenshot_ref: Option<String>,
}

impl ExecResult {
    pub fn new(call_id: &str, status: &'static str, text: impl Into<String>) -> ExecResult {
        ExecResult { call_id: call_id.into(), status, text: text.into(), screenshot_paths: vec![], screenshot_ref: None }
    }
}

/// A step from parsed calls; fails unless every call has a unique, non-empty id.
fn make_step(
    calls: Vec<NativeCall>,
    message: String,
    usage: Option<Usage>,
    response_id: Option<String>,
    is_terminal: bool,
) -> Result<NativeStep> {
    let ids: std::collections::BTreeSet<&str> = calls.iter().map(|c| c.call_id.as_str()).collect();
    if calls.iter().any(|c| c.call_id.is_empty()) || ids.len() != calls.len() {
        bail!("Native tool calls require nonempty, unique IDs");
    }
    let is_terminal = is_terminal && calls.is_empty();
    Ok(NativeStep { calls, message, usage, response_id, is_terminal })
}

fn validate_results(step: &NativeStep, results: &[ExecResult]) -> Result<()> {
    if !step.calls.iter().map(|c| &c.call_id).eq(results.iter().map(|r| &r.call_id)) {
        bail!("Expected one ordered execution result per tool call");
    }
    Ok(())
}

/// Token usage from a response's usage object (chat-completions, Responses or Messages field names).
pub(super) fn usage_from_any(u: &Value) -> Option<Usage> {
    let g = |k: &str| u.get(k).filter(|v| !v.is_null());
    let prompt = g("prompt_tokens").or(g("input_tokens"));
    let completion = g("completion_tokens").or(g("output_tokens"));
    let nested = || {
        ["input_tokens_details", "prompt_tokens_details"]
            .iter()
            .find_map(|p| u.get(*p).filter(|d| !d.is_null())?.get("cached_tokens").filter(|c| !c.is_null()))
    };
    let cache = g("cache_tokens").or(g("cache_read_input_tokens")).or_else(nested);
    if prompt.is_none() && completion.is_none() {
        return None;
    }
    let i = |v: Option<&Value>| v.filter(|v| truthy(v)).map_or(0, |v| coerce_int(v).unwrap_or(0));
    Some(Usage {
        prompt_tokens: i(prompt),
        completion_tokens: i(completion),
        cache_tokens: i(cache),
        cost_usd: g("cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
    })
}

/// ATIF step metrics for a native turn (native turns are never priced).
pub(super) fn native_metrics(u: Option<&Usage>) -> Value {
    let mut m = Map::new();
    if let Some(u) = u {
        m.insert("prompt_tokens".into(), json!(u.prompt_tokens));
        m.insert("completion_tokens".into(), json!(u.completion_tokens));
        if u.cache_tokens > 0 {
            m.insert("cached_tokens".into(), json!(u.cache_tokens));
        }
        if u.cost_usd > 0.0 {
            m.insert("cost_usd".into(), json!(u.cost_usd));
        }
    }
    Value::Object(m)
}

// Transport (SDK-equivalent HTTP: retries, error class names, Bedrock SigV4)

#[async_trait::async_trait]
pub(super) trait Transport: Send + Sync {
    async fn post(&self, body: &Value) -> Result<Value>;
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Sdk {
    /// As the anthropic / openai SDKs do: 2 retries on 408/409/429/5xx and connection errors.
    Stainless,
    /// google-genai with a retry wrapper: 4 attempts on 429/500/503/504.
    Gemini,
}

#[derive(Debug, Clone)]
struct AwsCreds {
    key_id: String,
    secret: String,
    token: Option<String>,
    region: String,
}

struct Http {
    sdk: Sdk,
    url: String,
    headers: Vec<(String, String)>,
    aws: Option<AwsCreds>,
}

/// The error for an HTTP error status, with the message the vendor SDK gives.
fn status_error(sdk: Sdk, status: u16, body: &str) -> anyhow::Error {
    let msg = match sdk {
        Sdk::Gemini => {
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            format!("{status} {}. {}", v["error"]["status"].as_str().unwrap_or(""), tail(body, 4000))
        }
        Sdk::Stainless => format!("Error code: {status} - {}", tail(body, 4000)),
    };
    kind_err(http_status_kind(status), msg)
}

#[async_trait::async_trait]
impl Transport for Http {
    async fn post(&self, body: &Value) -> Result<Value> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut headers = self.headers.clone();
            if let Some(c) = &self.aws {
                headers.extend(sigv4_headers(
                    c,
                    &self.url,
                    &serde_json::to_vec(body)?,
                    &chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
                ));
            }
            let req = Request { headers, body: body.clone(), timeout: Duration::from_secs(600) };
            let mut hint = Duration::ZERO;
            let (retry, err) = match send_raw(&self.url, &req).await {
                Ok((s, text, _)) if (200..300).contains(&s) => {
                    return serde_json::from_str(&text).map_err(|e| anyhow!("invalid JSON from {}: {e}: {}", self.url, tail(&text, 1000)));
                }
                Ok((s, text, retry_after)) => (
                    {
                        hint = retry_after.unwrap_or_default();
                        match self.sdk {
                            Sdk::Stainless => matches!(s, 408 | 409 | 429) || s >= 500,
                            Sdk::Gemini => matches!(s, 429 | 500 | 503 | 504),
                        }
                    },
                    status_error(self.sdk, s, &text),
                ),
                Err(e) => {
                    let msg = match self.sdk {
                        Sdk::Stainless if e.kind() == ErrorKind::ApiResponseStalled => "Request timed out.".to_string(),
                        Sdk::Stainless => format!("Connection error. ({e})"),
                        Sdk::Gemini => e.to_string(),
                    };
                    (self.sdk == Sdk::Stainless, kind_err(e.kind(), msg))
                }
            };
            let (max, base, cap) = match self.sdk {
                Sdk::Stainless => (3, 0.5, 8.0),
                Sdk::Gemini => (4, 2.0, 30.0),
            };
            if !retry || attempt >= max {
                return Err(err);
            }
            tokio::time::sleep(backoff(attempt, base, cap, hint)).await;
        }
    }
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut k = if key.len() > 64 { Sha256::digest(key).to_vec() } else { key.to_vec() };
    k.resize(64, 0);
    let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
    let inner = Sha256::new().chain_update(pad(0x36)).chain_update(msg).finalize();
    Sha256::new().chain_update(pad(0x5c)).chain_update(inner).finalize().to_vec()
}

/// RFC 3986 encoding of a URL path: unreserved characters and '/' kept, everything else percent-encoded.
fn uri_encode_path(p: &str) -> String {
    p.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// AWS SigV4 headers (`x-amz-date`, optional `x-amz-security-token`, `authorization`) for a
/// POST of `body` to `url`, signing `host;x-amz-date[;x-amz-security-token]` for service `bedrock`.
fn sigv4_headers(c: &AwsCreds, url: &str, body: &[u8], amz_date: &str) -> Vec<(String, String)> {
    sigv4("POST", c, "bedrock", url, body, amz_date)
}

fn sigv4(method: &str, c: &AwsCreds, service: &str, url: &str, body: &[u8], amz_date: &str) -> Vec<(String, String)> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (host, path_q) = rest.split_once('/').map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));
    let (path, query) = path_q.split_once('?').unwrap_or((&path_q, ""));
    let date = &amz_date[..8];
    let mut hdrs = vec![("host", host.to_string()), ("x-amz-date", amz_date.to_string())];
    if let Some(t) = &c.token {
        hdrs.push(("x-amz-security-token", t.clone()));
    }
    let signed = hdrs.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(";");
    let canon_headers: String = hdrs.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let mut q: Vec<&str> = query.split('&').filter(|s| !s.is_empty()).collect();
    q.sort();
    let canon_query = q.iter().map(|kv| if kv.contains('=') { kv.to_string() } else { format!("{kv}=") }).collect::<Vec<_>>().join("&");
    let canonical =
        format!("{method}\n{}\n{canon_query}\n{canon_headers}\n{signed}\n{}", uri_encode_path(path), hex(&Sha256::digest(body)));
    let scope = format!("{date}/{}/{service}/aws4_request", c.region);
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex(&Sha256::digest(canonical.as_bytes())));
    let mut key = hmac_sha256(format!("AWS4{}", c.secret).as_bytes(), date.as_bytes());
    for part in [c.region.as_str(), service, "aws4_request"] {
        key = hmac_sha256(&key, part.as_bytes());
    }
    let sig = hex(&hmac_sha256(&key, to_sign.as_bytes()));
    let mut out: Vec<(String, String)> = hdrs.into_iter().filter(|(k, _)| *k != "host").map(|(k, v)| (k.to_string(), v)).collect();
    out.push((
        "authorization".into(),
        format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}", c.key_id),
    ));
    out
}

const ANTHROPIC_SYSTEM_PROMPT: &str = "You are a computer use agent with access to a browser desktop environment. Interact with the computer using the provided tool. Be efficient and precise. When the task is complete, respond with the final answer without using tools.";

const GEMINI_NOTE: &str = "Use the computer-use tool to operate the visible browser. Coordinates are on a
normalized 0-999 grid. You may return multiple actions in one turn when their
targets are already known. Actions execute in order; if one fails, remaining
actions in that turn are not executed. Wait for the results when later actions
depend on a changed screen. Use zoom_region when a click target is too small to
locate confidently, and wait for its image before choosing coordinates. Finish
with a text response when the task is complete.";

/// (beta, tool type) for a model. Only Sonnet 4.5 / Haiku 4.5 keep the legacy tool.
pub(super) fn cua_protocol(model: &str) -> (&'static str, &'static str) {
    let l = model.to_lowercase();
    if ["claude-sonnet-4-5", "claude-haiku-4-5"].iter().any(|p| l.contains(p)) {
        ("computer-use-2025-01-24", "computer_20250124")
    } else {
        ("computer-use-2025-11-24", "computer_20251124")
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Kind {
    Anthropic,
    Bedrock,
    Gemini,
    OpenAi,
}

/// One native provider instance (conversation state + transport).
pub(super) struct Native {
    pub kind: Kind,
    /// Model name with the provider's routing prefixes stripped (what the SDK receives).
    pub model: String,
    w: i64,
    h: i64,
    beta: &'static str,
    tool_type: &'static str,
    /// Anthropic/Bedrock conversation.
    pub messages: Vec<Value>,
    auto_ack_safety: bool,
    interaction_id: Option<String>,
    pub transport: Box<dyn Transport>,
}

impl Native {
    pub fn with_transport(kind: Kind, model: &str, w: i64, h: i64, auto_ack_safety: bool, transport: Box<dyn Transport>) -> Native {
        let prefixes: &[&str] = match kind {
            Kind::Anthropic | Kind::Bedrock => &["bedrock/", "anthropic/"],
            Kind::Gemini => &["gemini/", "vertex_ai/"],
            Kind::OpenAi => &["openai/"],
        };
        let mut m = model;
        for p in prefixes {
            m = m.strip_prefix(p).unwrap_or(m);
        }
        let (beta, tool_type) = cua_protocol(m);
        Native { kind, model: m.into(), w, h, beta, tool_type, messages: vec![], auto_ack_safety, interaction_id: None, transport }
    }

    /// The provider plus an HTTP transport configured like the vendor SDK's client from `get` (env).
    pub fn new(
        name: &str,
        model: &str,
        w: i64,
        h: i64,
        aws_region: Option<&str>,
        auto_ack_safety: bool,
        get: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Native> {
        let kind = match name {
            "anthropic" => Kind::Anthropic,
            "bedrock" => Kind::Bedrock,
            "gemini" => Kind::Gemini,
            "openai" => Kind::OpenAi,
            other => bail!("Unknown computer-1 provider {}", quote_repr(other)),
        };
        let mut n = Native::with_transport(kind, model, w, h, auto_ack_safety, Box::new(NoTransport));
        n.transport = Box::new(n.http(aws_region, get)?);
        Ok(n)
    }

    fn http(&self, aws_region: Option<&str>, get: &dyn Fn(&str) -> Option<String>) -> Result<Http> {
        let n = self;
        let h = |k: &str, v: String| (k.to_string(), v);
        Ok(match n.kind {
            Kind::Anthropic => {
                let base = get("ANTHROPIC_BASE_URL").unwrap_or_else(|| "https://api.anthropic.com".into());
                let mut headers = vec![h("anthropic-version", "2023-06-01".into()), h("anthropic-beta", n.beta.into())];
                match (get("ANTHROPIC_API_KEY"), get("ANTHROPIC_AUTH_TOKEN")) {
                    (Some(k), _) => headers.push(h("x-api-key", k)),
                    (None, Some(t)) => headers.push(h("authorization", format!("Bearer {t}"))),
                    (None, None) => bail!(
                        "Could not resolve authentication method. Expected either api_key or auth_token to be set (ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN) for computer-1's 'anthropic' provider"
                    ),
                }
                Http { sdk: Sdk::Stainless, url: format!("{}/v1/messages?beta=true", base.trim_end_matches('/')), headers, aws: None }
            }
            Kind::Bedrock => {
                // The region is `aws_region_name`, else us-east-1; AWS_REGION is not consulted.
                let region = aws_region.unwrap_or("us-east-1").to_string();
                let (Some(key_id), Some(secret)) = (get("AWS_ACCESS_KEY_ID"), get("AWS_SECRET_ACCESS_KEY")) else {
                    bail!(
                        "computer-1's 'bedrock' provider signs requests with AWS SigV4 from AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY (+ optional AWS_SESSION_TOKEN); hbench doesn't read AWS profiles, SSO or instance roles"
                    )
                };
                let base = get("ANTHROPIC_BEDROCK_BASE_URL").unwrap_or_else(|| format!("https://bedrock-runtime.{region}.amazonaws.com"));
                let model_path = uri_encode_path(&n.model).replace("%3A", ":").replace('/', "%2F");
                let aws = AwsCreds { key_id, secret, token: get("AWS_SESSION_TOKEN"), region };
                Http {
                    sdk: Sdk::Stainless,
                    url: format!("{}/model/{model_path}/invoke", base.trim_end_matches('/')),
                    headers: vec![],
                    aws: Some(aws),
                }
            }
            Kind::Gemini => {
                // Always the Gemini Developer API with an API key (never Vertex AI).
                let Some(key) = get("GOOGLE_API_KEY").or_else(|| get("GEMINI_API_KEY")) else {
                    bail!("computer-1's 'gemini' provider needs an API key: set GOOGLE_API_KEY or GEMINI_API_KEY")
                };
                let base = get("GOOGLE_GEMINI_BASE_URL").unwrap_or_else(|| "https://generativelanguage.googleapis.com".into());
                Http {
                    sdk: Sdk::Gemini,
                    url: format!("{}/v1beta/interactions", base.trim_end_matches('/')),
                    headers: vec![h("x-goog-api-key", key)],
                    aws: None,
                }
            }
            Kind::OpenAi => {
                let Some(key) = get("OPENAI_API_KEY") else {
                    bail!(
                        "The api_key client option must be set either by passing api_key to the client or by setting the OPENAI_API_KEY environment variable"
                    )
                };
                let base = get("OPENAI_BASE_URL").unwrap_or_else(|| "https://api.openai.com/v1".into());
                let mut headers = vec![h("authorization", format!("Bearer {key}"))];
                if let Some(o) = get("OPENAI_ORG_ID") {
                    headers.push(h("openai-organization", o));
                }
                if let Some(p) = get("OPENAI_PROJECT_ID") {
                    headers.push(h("openai-project", p));
                }
                Http { sdk: Sdk::Stainless, url: format!("{}/responses", base.trim_end_matches('/')), headers, aws: None }
            }
        })
    }

    /// Screenshot payload format the API needs ("png" reads latest.png env-side).
    pub fn payload_png(&self) -> bool {
        self.kind == Kind::Gemini
    }

    fn anthropic_tools(&self) -> Value {
        let mut t = json!({"type": self.tool_type, "name": "computer", "display_width_px": self.w, "display_height_px": self.h, "display_number": 1});
        if self.tool_type == "computer_20251124" {
            t["enable_zoom"] = json!(true);
        }
        json!([t])
    }

    fn image_block(r: &str) -> Value {
        json!({"type": "image", "source": {"type": "base64", "media_type": media_type_for_data_url(r), "data": strip_data_url(r)}})
    }

    /// Messages with a rolling `cache_control` on the latest user message's last block.
    pub fn messages_with_cache_control(&self) -> Vec<Value> {
        let mut m = self.messages.clone();
        if let Some(last) = m.iter_mut().rev().find(|x| x["role"] == "user")
            && let Some(b) = last["content"].as_array_mut().and_then(|c| c.last_mut()).filter(|b| b.is_object())
        {
            b["cache_control"] = json!({"type": "ephemeral"});
        }
        m
    }

    /// The Messages API body with the computer-use beta (Bedrock: reshaped for the invoke endpoint).
    pub fn anthropic_body(&self) -> Value {
        let mut b = json!({
            "model": self.model,
            "max_tokens": 4096,
            "system": [{"type": "text", "text": ANTHROPIC_SYSTEM_PROMPT, "cache_control": {"type": "ephemeral"}}],
            "messages": self.messages_with_cache_control(),
            "tools": self.anthropic_tools(),
        });
        if self.kind == Kind::Bedrock {
            let o = b.as_object_mut().unwrap();
            o.remove("model");
            o.insert("anthropic_version".into(), json!("bedrock-2023-05-31"));
            o.insert("anthropic_beta".into(), json!([self.beta]));
        }
        b
    }

    fn anthropic_step(&self, resp: &Value) -> Result<NativeStep> {
        let (mut calls, mut texts) = (vec![], vec![]);
        for block in resp["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") => texts.push(block.get("text").filter(|v| truthy(v)).map(text_of).unwrap_or_default()),
                Some("tool_use") => {
                    let id = block.get("id").filter(|v| truthy(v)).map(text_of).unwrap_or_default();
                    let name = block.get("name").filter(|v| truthy(v)).map(text_of).unwrap_or_default();
                    let input = block.get("input").cloned().unwrap_or(json!({}));
                    let translated = if name != "computer" {
                        Err(format!("Unsupported tool: {name}"))
                    } else if let Some(d) = input.as_object() {
                        translate_anthropic(d, self.w, self.h).and_then(|a| {
                            a.ok_or_else(|| {
                                format!("Unsupported action: {}", d.get("action").map(text_of).unwrap_or_else(|| "None".into()))
                            })
                        })
                    } else {
                        Err("Computer input must be an object".into())
                    };
                    let (action, error) = match translated {
                        Ok(a) => (Some(a), None),
                        Err(e) => (None, Some(e)),
                    };
                    calls.push(NativeCall {
                        call_id: id,
                        name,
                        arguments: if input.is_object() { input } else { json!({}) },
                        action,
                        error,
                    });
                }
                _ => {}
            }
        }
        let stop = resp.get("stop_reason").and_then(Value::as_str);
        if stop == Some("max_tokens") {
            for c in &mut calls {
                c.error = Some("Response truncated at max_tokens; no actions executed".into());
            }
        }
        let id = resp.get("id").filter(|v| truthy(v)).map(text_of);
        make_step(
            calls,
            texts.join("\n").trim().to_string(),
            usage_from_any(&resp["usage"]),
            id,
            matches!(stop, None | Some("end_turn" | "stop_sequence" | "refusal")),
        )
    }

    async fn anthropic_turn(&mut self, content: Vec<Value>) -> Result<NativeStep> {
        self.messages.push(json!({"role": "user", "content": content}));
        let resp = self.transport.post(&self.anthropic_body()).await?;
        self.messages.push(json!({"role": "assistant", "content": resp["content"].as_array().cloned().unwrap_or_default()}));
        self.anthropic_step(&resp)
    }

    fn gemini_image(r: &str) -> Value {
        let b64 = base64::engine::general_purpose::STANDARD;
        let data = b64.decode(strip_data_url(r).trim()).map(|b| b64.encode(b)).unwrap_or_else(|_| strip_data_url(r).to_string());
        json!({"type": "image", "data": data, "mime_type": "image/png"})
    }

    pub fn gemini_request(&self, input: Vec<Value>) -> Value {
        let coord =
            json!({"type": "integer", "description": "Coordinate on Gemini's normalized 0-999 screen grid.", "minimum": 0, "maximum": 999});
        let tools = json!([
            {"type": "computer_use", "environment": "browser", "enable_prompt_injection_detection": true},
            {"type": "function", "name": "zoom_region",
             "description": "Crop the next screenshot to a rectangular region. Use this to inspect tiny text or small click targets.",
             "parameters": {"type": "object", "properties": {"x1": coord, "y1": coord, "x2": coord, "y2": coord}, "required": ["x1", "y1", "x2", "y2"]}},
        ]);
        let mut r = json!({"model": self.model, "input": input, "tools": tools});
        if let Some(id) = &self.interaction_id {
            r["previous_interaction_id"] = json!(id);
        }
        r
    }

    fn gemini_safety_checked(&self, args: &Map<String, Value>) -> Result<()> {
        let Some(d) = args.get("safety_decision").and_then(Value::as_object) else { return Ok(()) };
        let explanation = d.get("explanation").map(text_of).unwrap_or_default();
        match d.get("decision").and_then(Value::as_str) {
            Some("blocked") => bail!("Gemini blocked the requested computer action: {explanation}"),
            Some("require_confirmation") if !self.auto_ack_safety => {
                bail!("Gemini Computer Use requires safety confirmation. Set gemini_auto_ack_safety=true for an unattended run.")
            }
            _ => Ok(()),
        }
    }

    fn gemini_step(&self, resp: &Value) -> Result<NativeStep> {
        let (mut calls, mut texts, mut intents) = (vec![], vec![], vec![]);
        for s in resp["steps"].as_array().into_iter().flatten() {
            match s["type"].as_str() {
                Some("model_output") => texts
                    .extend(s["content"].as_array().into_iter().flatten().filter_map(|c| c.get("text").filter(|t| truthy(t)).map(text_of))),
                Some("function_call") => {
                    let name = s.get("name").filter(|v| truthy(v)).map(text_of).unwrap_or_default();
                    let id = s.get("id").filter(|v| truthy(v)).map(text_of).unwrap_or_default();
                    let raw = s.get("arguments").cloned().unwrap_or(json!({}));
                    let args = raw.as_object().cloned().unwrap_or_default();
                    // Check every call before returning any actions to the executor.
                    self.gemini_safety_checked(&args)?;
                    let (action, error) = if !raw.is_object() {
                        (None, Some("Gemini function arguments must be an object".to_string()))
                    } else {
                        match translate_gemini(&name, &args) {
                            Ok(Some(a)) => (Some(a), None),
                            Ok(None) => (None, Some(format!("Unsupported Gemini action: {name}"))),
                            Err(e) => (None, Some(e)),
                        }
                    };
                    let intent = str_or_empty(&args, "intent");
                    if !intent.is_empty() {
                        intents.push(intent);
                    }
                    calls.push(NativeCall { call_id: id, name, arguments: Value::Object(args), action, error });
                }
                _ => {}
            }
        }
        let usage = resp.get("usage").filter(|u| !u.is_null()).map(|u| {
            let i = |k: &str| u.get(k).filter(|v| truthy(v)).map_or(0, |v| coerce_int(v).unwrap_or(0));
            Usage {
                prompt_tokens: i("total_input_tokens"),
                completion_tokens: i("total_output_tokens"),
                cache_tokens: i("total_cached_tokens"),
                cost_usd: 0.0,
            }
        });
        let message = if texts.is_empty() { intents } else { texts }.join("\n").trim().to_string();
        let is_terminal = calls.is_empty();
        make_step(calls, message, usage, resp.get("id").filter(|v| !v.is_null()).map(text_of), is_terminal)
    }

    async fn gemini_turn(&mut self, input: Vec<Value>) -> Result<NativeStep> {
        let resp = self.transport.post(&self.gemini_request(input)).await?;
        self.interaction_id = resp.get("id").filter(|v| !v.is_null()).map(text_of);
        self.gemini_step(&resp)
    }

    pub fn openai_request(&self, input: Value, previous_response_id: Option<&str>, cache_key: &str) -> Value {
        let mut r = json!({"model": self.model, "tools": [{"type": "computer"}], "input": input, "truncation": "auto", "prompt_cache_key": cache_key});
        if let Some(id) = previous_response_id {
            r["previous_response_id"] = json!(id);
        }
        r
    }

    pub async fn initial(&mut self, instruction: &str, shot: &str) -> Result<NativeStep> {
        match self.kind {
            Kind::Anthropic | Kind::Bedrock => {
                self.messages.clear();
                self.anthropic_turn(vec![json!({"type": "text", "text": instruction}), Self::image_block(shot)]).await
            }
            Kind::Gemini => {
                self.gemini_turn(vec![json!({"type": "text", "text": format!("{instruction}\n\n{GEMINI_NOTE}")}), Self::gemini_image(shot)])
                    .await
            }
            Kind::OpenAi => bail!("the openai provider drives its own loop"),
        }
    }

    pub async fn follow_up(&mut self, prev: &NativeStep, results: &[ExecResult], shot: &str, extra: Option<&str>) -> Result<NativeStep> {
        validate_results(prev, results)?;
        match self.kind {
            Kind::Anthropic | Kind::Bedrock => {
                let mut content: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        let mut blocks = vec![json!({"type": "text", "text": if r.text.is_empty() { "OK" } else { &r.text }})];
                        if let Some(s) = &r.screenshot_ref {
                            blocks.push(Self::image_block(s));
                        }
                        json!({"type": "tool_result", "tool_use_id": r.call_id, "is_error": r.status != "success", "content": blocks})
                    })
                    .collect();
                content.push(json!({"type": "text", "text": extra.unwrap_or("Continue with the task.")}));
                // A trailing screenshot/zoom already shows the state after the batch.
                if results.last().is_none_or(|r| r.screenshot_ref.as_deref() != Some(shot)) {
                    content.push(Self::image_block(shot));
                }
                self.anthropic_turn(content).await
            }
            Kind::Gemini => {
                if prev.calls.is_empty() {
                    bail!("Gemini follow-up missing serialized function call");
                }
                let mut input = vec![];
                for (call, r) in prev.calls.iter().zip(results) {
                    let mut res = Map::new();
                    res.insert("url".into(), json!(""));
                    res.insert("status".into(), json!(r.status));
                    if call.arguments["safety_decision"]["decision"] == "require_confirmation" {
                        if !self.auto_ack_safety {
                            bail!("Gemini Computer Use requires safety confirmation.");
                        }
                        res.insert("safety_acknowledgement".into(), json!(true));
                    }
                    if r.status != "success" {
                        res.insert("error".into(), json!(r.text));
                    }
                    input.push(json!({
                        "type": "function_result", "name": call.name, "call_id": call.call_id, "is_error": r.status != "success",
                        // Computer Use requires the PNG first; a leading text block is rejected with HTTP 400.
                        "result": [Self::gemini_image(r.screenshot_ref.as_deref().unwrap_or(shot)), {"type": "text", "text": json_spaced(&Value::Object(res))}],
                    }));
                }
                if let Some(e) = extra.filter(|e| !e.is_empty()) {
                    input.push(json!({"type": "text", "text": e}));
                }
                self.gemini_turn(input).await
            }
            Kind::OpenAi => bail!("the openai provider drives its own loop"),
        }
    }
}

/// Placeholder until `Native::new` installs the HTTP transport.
struct NoTransport;

#[async_trait::async_trait]
impl Transport for NoTransport {
    async fn post(&self, _: &Value) -> Result<Value> {
        bail!("computer-1 native provider has no transport")
    }
}

impl Computer1 {
    fn native(&mut self) -> Result<&mut Native> {
        self.native.as_mut().ok_or_else(|| anyhow!("computer-1 native provider is not set"))
    }

    /// `latest.png` (PNG-payload providers) or the recorded file as a data URL.
    async fn payload_ref(&mut self, sh: &dyn Shell, path: &str) -> Result<String> {
        if self.native.as_ref().is_some_and(Native::payload_png) {
            return self.desktop()?.latest_png_data_url(sh).await;
        }
        super::screenshot_data_url(sh, path).await
    }

    fn accumulate(&mut self, u: Option<&Usage>) {
        if let Some(u) = u {
            self.usage_seen = true;
            self.chat.total_input_tokens += u.prompt_tokens;
            self.chat.total_output_tokens += u.completion_tokens;
            self.chat.total_cache_tokens += u.cache_tokens;
            if u.cost_usd > 0.0 {
                self.chat.total_cost += u.cost_usd;
            }
        }
    }

    /// Record one API turn as an ATIF step: all its calls, each matched to its own outcome.
    fn record_native_step(&mut self, step: &NativeStep, results: &[ExecResult]) {
        let model = self.native.as_ref().map_or_else(|| self.model_name.clone(), |n| n.model.clone());
        let obs: Vec<Value> = results
            .iter()
            .map(|r| {
                let mut parts = vec![json!({"type": "text", "text": if r.text.is_empty() { r.status } else { &r.text }})];
                parts.extend(r.screenshot_paths.iter().map(|p| self.image_part(p)));
                json!({"source_call_id": r.call_id, "content": parts, "extra": {"execution_status": r.status}})
            })
            .collect();
        let calls: Vec<Value> =
            step.calls.iter().map(|c| json!({"tool_call_id": c.call_id, "function_name": c.name, "arguments": c.arguments})).collect();
        self.push_step(Map::from_iter([
            ("source".into(), json!("agent")),
            ("model_name".into(), json!(model)),
            ("message".into(), json!(step.message)),
            ("tool_calls".into(), if calls.is_empty() { Value::Null } else { Value::Array(calls) }),
            ("observation".into(), if obs.is_empty() { Value::Null } else { json!({"results": obs}) }),
            ("metrics".into(), native_metrics(step.usage.as_ref())),
            ("extra".into(), json!({"response_id": step.response_id})),
        ]));
    }

    /// Execute one ordered batch; the first failure skips the rest.
    async fn execute_native_calls(&mut self, sh: &dyn Shell, step: &NativeStep, episode: i64) -> Vec<ExecResult> {
        let mut results: Vec<ExecResult> = vec![];
        for (i, call) in step.calls.iter().enumerate() {
            if results.last().is_some_and(|r| r.status != "success") {
                results.push(ExecResult::new(
                    &call.call_id,
                    "not_executed",
                    "Not executed: an earlier computer action in this turn failed.",
                ));
                continue;
            }
            let r = match (&call.error, &call.action) {
                (Some(e), _) if !e.is_empty() => ExecResult::new(&call.call_id, "error", e.clone()),
                (_, None) => ExecResult::new(
                    &call.call_id,
                    "error",
                    call.error.clone().filter(|e| !e.is_empty()).unwrap_or_else(|| "Unsupported computer action".into()),
                ),
                (_, Some(a)) => match self.execute_action(sh, Some(a), episode, Some(i)).await {
                    Ok((text, paths, is_err)) => {
                        let mut r = ExecResult::new(
                            &call.call_id,
                            if is_err { "error" } else { "success" },
                            if text.is_empty() { "OK".into() } else { text },
                        );
                        r.screenshot_paths = paths;
                        if let Some(p) = r.screenshot_paths.last().cloned().filter(|_| matches!(a.kind.as_str(), "screenshot" | "zoom")) {
                            match self.payload_ref(sh, &p).await {
                                Ok(s) => r.screenshot_ref = Some(s),
                                Err(e) => r = ExecResult::new(&call.call_id, "error", e.to_string()),
                            }
                        }
                        r
                    }
                    Err(e) => ExecResult::new(&call.call_id, "error", e.to_string()),
                },
            };
            results.push(r);
        }
        // Cancellation drops the future, so nothing is recorded for the interrupted turn.
        results
    }

    /// The step loop (Anthropic / Bedrock / Gemini).
    pub(super) async fn run_step_loop(
        &mut self,
        sh: &dyn Shell,
        ctx: &mut AgentContext,
        instruction: &str,
        initial_shot: &str,
    ) -> Result<()> {
        self.push_step(Map::from_iter([("source".into(), json!("user")), ("message".into(), json!(instruction))]));
        self.dump_trajectory();
        let shot = self.payload_ref(sh, initial_shot).await?;
        let mut step = self.native()?.initial(instruction, &shot).await?;
        let max = self.opts.max_turns;
        for episode in 0..max.unwrap_or(1_000_000) {
            self.n_episodes = episode + 1;
            self.accumulate(step.usage.as_ref());
            self.sync_context(ctx);
            if !self.desktop()?.is_alive(sh).await {
                self.early_termination_reason = Some("runtime_session_dead".into());
                let r: Vec<ExecResult> =
                    step.calls.iter().map(|c| ExecResult::new(&c.call_id, "not_executed", "Runtime session ended")).collect();
                self.record_native_step(&step, &r);
                return Ok(());
            }
            let results = self.execute_native_calls(sh, &step, episode).await;
            self.record_native_step(&step, &results);
            self.dump_trajectory();
            if step.is_terminal {
                self.write_final_answer(sh, &step.message).await?;
                self.early_termination_reason = Some("task_complete".into());
                return Ok(());
            }
            // Do not issue an unconsumed (and unaccounted) request at the limit.
            if max.is_some_and(|m| episode + 1 >= m) {
                break;
            }
            let shot = match results.last() {
                Some(l) if !l.screenshot_paths.is_empty() => match &l.screenshot_ref {
                    Some(r) => r.clone(),
                    None => self.payload_ref(sh, l.screenshot_paths.last().unwrap()).await?,
                },
                _ => {
                    let p = self.capture(sh, &format!("screenshot_ep{episode}_follow.webp")).await?;
                    self.payload_ref(sh, &p).await?
                }
            };
            step = self.native()?.follow_up(&step, &results, &shot, None).await?;
        }
        self.early_termination_reason = Some("max_turns_reached".into());
        Ok(())
    }

    /// The OpenAI episode loop: Responses API chained with `previous_response_id`.
    pub(super) async fn run_openai_loop(&mut self, sh: &dyn Shell, ctx: &mut AgentContext, instruction: &str) -> Result<()> {
        self.push_step(Map::from_iter([("source".into(), json!("user")), ("message".into(), json!(instruction))]));
        self.dump_trajectory();
        let key = format!("computer-1-{}", self.session_id);
        let body = self.native()?.openai_request(json!(instruction), None, &key);
        let mut resp = self.native()?.transport.post(&body).await?;
        for episode in 0..self.opts.max_turns.unwrap_or(1_000_000) {
            self.n_episodes = episode + 1;
            if !self.desktop()?.is_alive(sh).await {
                self.early_termination_reason = Some("runtime_session_dead".into());
                return Ok(());
            }
            let usage = usage_from_any(&resp["usage"]);
            self.accumulate(usage.as_ref());
            self.sync_context(ctx);
            let output = resp["output"].as_array().cloned().unwrap_or_default();
            let message: String = output
                .iter()
                .filter(|i| i["type"] == "message")
                .flat_map(|i| i["content"].as_array().cloned().unwrap_or_default())
                .filter_map(|b| b.get("text").filter(|t| truthy(t)).map(text_of))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
            let Some(call) = output.iter().find(|i| i["type"] == "computer_call") else {
                self.record_openai_step(episode, &message, None, usage.as_ref(), &[]);
                self.write_final_answer(sh, &message).await?;
                self.early_termination_reason = Some("task_complete".into());
                return Ok(());
            };
            let mut last = None;
            for raw in call["actions"].as_array().into_iter().flatten() {
                let Some(a) = translate_openai(raw) else { continue };
                if let Err(e) = self.desktop()?.execute(sh, &a).await {
                    eprintln!("computer-1: OpenAI action {} failed: {e}", a.kind);
                }
                last = Some(a);
            }
            let path = self.capture(sh, &format!("screenshot_ep{episode}.webp")).await?;
            self.record_openai_step(episode, &message, last.as_ref(), usage.as_ref(), &[path]);
            // OpenAI recommends full-resolution PNG screenshots; the recorded artifact stays WebP.
            let png = self.desktop()?.latest_png_data_url(sh).await?;
            let mut out = json!({"type": "computer_call_output", "call_id": call["call_id"], "output": {"type": "computer_screenshot", "image_url": png, "detail": "original"}});
            if let Some(p) = call.get("pending_safety_checks").filter(|p| truthy(p)) {
                out["acknowledged_safety_checks"] = p.clone();
            }
            let prev = resp["id"].as_str().map(String::from);
            let body = self.native()?.openai_request(json!([out]), prev.as_deref(), &key);
            resp = self.native()?.transport.post(&body).await?;
        }
        self.early_termination_reason = Some("max_turns_reached".into());
        Ok(())
    }

    fn record_openai_step(&mut self, episode: i64, message: &str, action: Option<&Action>, usage: Option<&Usage>, paths: &[String]) {
        let model = self.native.as_ref().map(|n| n.model.clone());
        let resp = LlmResponse { content: message.into(), model_name: model, usage: usage.cloned(), ..Default::default() };
        let step = super::ModelStep { action: action.cloned(), analysis: message.into(), ..Default::default() };
        self.record_agent_step(episode, &resp, &step, action.is_none(), message, paths, native_metrics(usage));
        self.dump_trajectory();
    }
}

impl super::Desktop {
    /// Base64 data-url of the most recent capture as PNG.
    pub(super) async fn latest_png_data_url(&self, sh: &dyn Shell) -> Result<String> {
        let png = format!("{SCREENSHOT_DIR}/latest.png");
        let out = self
            .exec(
                sh,
                &format!("base64 -w0 {0} 2>/dev/null || base64 {0}", quote(&png)),
                self.request_timeout.max(30),
                "screenshot:png-b64",
                0,
            )
            .await?;
        let enc = out.trim();
        if enc.is_empty() {
            bail!("Could not read screenshot at {png}");
        }
        Ok(format!("data:image/png;base64,{enc}"))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{MockLlm, MockShell, agent};
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MockApi {
        replies: Arc<Mutex<VecDeque<Value>>>,
        seen: Arc<Mutex<Vec<Value>>>,
    }

    #[async_trait::async_trait]
    impl Transport for MockApi {
        async fn post(&self, body: &Value) -> Result<Value> {
            self.seen.lock().unwrap().push(body.clone());
            self.replies.lock().unwrap().pop_front().ok_or_else(|| anyhow!("no reply queued"))
        }
    }

    fn api(replies: Vec<Value>) -> MockApi {
        let a = MockApi::default();
        *a.replies.lock().unwrap() = replies.into();
        a
    }

    fn provider(kind: Kind, model: &str, a: &MockApi) -> Native {
        Native::with_transport(kind, model, 1024, 900, false, Box::new(a.clone()))
    }

    /// A canned Anthropic response with one tool_use per action.
    fn response(actions: Vec<Value>, stop: &str) -> Value {
        let content: Vec<Value> = actions
            .into_iter()
            .enumerate()
            .map(|(i, a)| json!({"type": "tool_use", "name": "computer", "id": format!("call-{i}"), "input": a}))
            .collect();
        json!({"id": "response-1", "stop_reason": stop, "content": content, "usage": {"input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 0}})
    }

    /// A canned Gemini response with one function call per entry.
    fn gemini_response(calls: Vec<(&str, Value)>) -> Value {
        let steps: Vec<Value> = calls
            .into_iter()
            .enumerate()
            .map(|(i, (n, a))| json!({"type": "function_call", "id": format!("g{i}"), "name": n, "arguments": a}))
            .collect();
        json!({"id": "interaction-1", "steps": steps, "usage": {"total_input_tokens": 100, "total_output_tokens": 20}})
    }

    fn ok(id: &str) -> ExecResult {
        ExecResult::new(id, "success", "")
    }

    #[test]
    fn coordinates() {
        assert_eq!(anthropic_scale(500, 400, 1024, 900), (500, 400));
        let s = (1_150_000f64 / (1200.0 * 1200.0)).sqrt();
        assert_eq!(anthropic_scale(600, 600, 1200, 1200), ((600.0 / s) as i64, (600.0 / s) as i64));
        let mut z = Action::new("zoom");
        (z.zoom_region, z.source) = (Some(vec![0, 0, 999, 999]), NORMALIZED);
        assert_eq!(normalize_action(&z, 1024, 900).zoom_region, Some(vec![0, 0, 1023, 899]));
        let mut c = Action::new("click");
        (c.x, c.y, c.source) = (Some(500), Some(250), NORMALIZED);
        let n = normalize_action(&c, 1024, 900);
        assert_eq!((n.x, n.y, n.model_x, n.model_y), (Some(512), Some(225), Some(500), Some(250)));
        c.source = "native_prescaled";
        assert_eq!(normalize_action(&c, 1024, 900), c);
    }

    #[test]
    fn provider_selection_and_protocol() {
        use super::super::{is_computer_use_model, resolve_provider_name};
        for (m, p) in [
            ("anthropic/claude-opus-4-7", "anthropic"),
            ("claude-fable-5-1", "anthropic"),
            ("bedrock/us.anthropic.claude-opus-4-7-v1:0", "bedrock"),
            ("us.anthropic.claude-opus-4-7-v1:0", "bedrock"),
            ("gemini/gemini-3.7-flash", "gemini"),
            ("vertex_ai/gemini-2.5-computer-use-preview", "gemini"),
            ("openai/gpt-5.5", "litellm"), // OpenAI is opt-in via provider='openai'
            ("openrouter/qwen/qwen3-vl", "litellm"),
        ] {
            assert_eq!(resolve_provider_name(m, None).unwrap(), p, "{m}");
        }
        assert_eq!(resolve_provider_name("openai/gpt-6-astra", Some("openai")).unwrap(), "openai");
        assert!(
            resolve_provider_name("openai/gpt-5.2", Some("openai"))
                .unwrap_err()
                .to_string()
                .contains("is not a computer-use model for the 'openai' harness")
        );
        assert!(resolve_provider_name("gemini/gemini-2.0-flash", None).is_err());
        for m in ["gpt-5.4", "gpt-5.10", "gpt-7", "openai/gpt-5.4-turbo"] {
            assert!(is_computer_use_model(m), "{m}");
        }
        for m in ["gpt-5.3", "gpt-5", "gpt-4o", "claude-3-haiku"] {
            assert!(!is_computer_use_model(m), "{m}");
        }
        assert_eq!(cua_protocol("claude-opus-4-7"), ("computer-use-2025-11-24", "computer_20251124"));
        assert_eq!(cua_protocol("global.anthropic.claude-opus-4-6-v1:0"), ("computer-use-2025-11-24", "computer_20251124"));
        assert_eq!(cua_protocol("global.anthropic.claude-sonnet-4-5-20250929-v1:0"), ("computer-use-2025-01-24", "computer_20250124"));
    }

    #[test]
    fn anthropic_translation() {
        let t = |v: Value| translate_anthropic(v.as_object().unwrap(), 1024, 900);
        let a = t(json!({"action": "left_click", "coordinate": [100, 200]})).unwrap().unwrap();
        assert_eq!((a.kind.as_str(), a.x, a.y, a.source), ("click", Some(100), Some(200), "anthropic_scaled"));
        let a = t(json!({"action": "scroll", "coordinate": [512, 450], "scroll_direction": "up", "scroll_amount": 2, "text": "shift"}))
            .unwrap()
            .unwrap();
        assert_eq!((a.kind.as_str(), a.scroll_x, a.scroll_y, a.modifier.as_deref()), ("scroll", Some(0), Some(-200), Some("shift")));
        assert_eq!(t(json!({"action": "zoom", "region": [10, 20, 110, 220]})).unwrap().unwrap().zoom_region, Some(vec![10, 20, 110, 220]));
        assert_eq!(t(json!({"action": "screenshot"})).unwrap().unwrap().kind, "screenshot");
        let a = t(json!({"action": "middle_click", "coordinate": [1, 2]})).unwrap().unwrap();
        assert_eq!((a.kind.as_str(), a.button.as_deref()), ("click", Some("middle")));
        let a = t(json!({"action": "left_click_drag", "start_coordinate": [1, 2], "coordinate": [30, 40]})).unwrap().unwrap();
        assert_eq!((a.x, a.y, a.end_x, a.end_y, a.model_x), (Some(1), Some(2), Some(30), Some(40), Some(30)));
        assert_eq!(t(json!({"action": "key", "text": "ctrl+ l"})).unwrap().unwrap().keys, Some(vec!["ctrl".into(), "l".into()]));
        let a = t(json!({"action": "hold_key", "text": "shift", "duration": 2})).unwrap().unwrap();
        assert_eq!((a.keys, a.duration), (Some(vec!["shift".to_string()]), Some(2.0)));
        assert_eq!(t(json!({"action": "left_click", "coordinate": ["bad", 1]})).unwrap_err(), "invalid integer: 'bad'");
        assert_eq!(t(json!({"action": "missing"})).unwrap(), None);
        // Larger desktops reverse Anthropic's downscale.
        let a = translate_anthropic(json!({"action": "left_click", "coordinate": [784, 400]}).as_object().unwrap(), 2048, 700)
            .unwrap()
            .unwrap();
        assert_eq!((a.x, a.model_x), (Some(1024), Some(784)));
    }

    #[test]
    fn openai_translation() {
        let a = translate_openai(&json!({"type": "click", "button": "left", "x": 10, "y": 20})).unwrap();
        assert_eq!((a.kind.as_str(), a.x, a.y, a.source), ("click", Some(10), Some(20), "native_prescaled"));
        assert_eq!(translate_openai(&json!({"type": "click", "button": "right", "x": 5, "y": 6})).unwrap().kind, "right_click");
        assert_eq!(
            translate_openai(&json!({"type": "click", "button": "wheel", "x": 5, "y": 6})).unwrap().button.as_deref(),
            Some("middle")
        );
        assert_eq!(
            translate_openai(&json!({"type": "double_click", "x": 7, "y": 8, "keys": ["SHIFT"]})).unwrap().modifier.as_deref(),
            Some("shift")
        );
        assert_eq!(translate_openai(&json!({"type": "type", "text": "hi"})).unwrap().text.as_deref(), Some("hi"));
        assert_eq!(translate_openai(&json!({"type": "keypress", "keys": ["Enter"]})).unwrap().keys, Some(vec!["Enter".into()]));
        assert_eq!(translate_openai(&json!({"type": "scroll", "x": 1, "y": 2, "scrollX": 0, "scrollY": 300})).unwrap().scroll_y, Some(300));
        let d = translate_openai(&json!({"type": "drag", "path": [{"x": 1, "y": 2}, [5, 5], {"x": 9, "y": 8}]})).unwrap();
        assert_eq!((d.kind.as_str(), d.x, d.y, d.end_x, d.end_y), ("drag", Some(1), Some(2), Some(9), Some(8)));
        assert!(translate_openai(&json!({"type": "drag", "path": [{"x": 1, "y": 2}]})).is_none());
        assert!(translate_openai(&json!({"type": "screenshot"})).is_none());
        assert_eq!(translate_openai(&json!({"type": "move", "x": 3, "y": 4})).unwrap().kind, "mouse_move");
    }

    #[test]
    fn gemini_translation() {
        let t = |n: &str, v: Value| translate_gemini(n, v.as_object().unwrap());
        for k in ["click", "double_click", "right_click"] {
            let a = t(k, json!({"x": 633, "y": 185})).unwrap().unwrap();
            assert_eq!((a.kind.as_str(), a.x, a.y, a.source), (k, Some(633), Some(185), "normalized_completion"));
        }
        let a = t("zoom_region", json!({"x1": 300, "y1": 400, "x2": 100, "y2": 200})).unwrap().unwrap();
        assert_eq!((a.kind.as_str(), a.zoom_region), ("zoom", Some(vec![100, 200, 300, 400])));
        let a = t("type", json!({"text": "hello", "press_enter": true})).unwrap().unwrap();
        assert_eq!((a.text.as_deref(), a.press_enter, a.source), (Some("hello"), Some(true), "native_prescaled"));
        assert_eq!(t("hotkey", json!({"keys": ["Control", "A"]})).unwrap().unwrap().keys, Some(vec!["Control+A".into()]));
        assert_eq!(t("press_key", json!({"key": "Enter"})).unwrap().unwrap().kind, "keypress");
        assert_eq!(t("press_key", json!({})).unwrap(), None);
        let a = t("scroll", json!({"x": 1, "y": 2, "direction": "left", "magnitude_in_pixels": 0})).unwrap().unwrap();
        assert_eq!((a.scroll_x, a.scroll_y), (Some(-300), Some(0)));
        let a = t("wait", json!({"seconds": 3})).unwrap().unwrap();
        assert_eq!((a.kind.as_str(), a.duration_seconds), ("sleep", Some(3.0)));
        let a = t("drag_and_drop", json!({"start_x": 1, "start_y": 2, "end_x": 3, "end_y": 4})).unwrap().unwrap();
        assert_eq!((a.x, a.end_y), (Some(1), Some(4)));
        assert_eq!(t("click", json!({"x": "invalid", "y": 30})).unwrap_err(), "invalid integer: 'invalid'");
        assert_eq!(t("take_screenshot", json!({})).unwrap().unwrap().kind, "screenshot");
        assert_eq!(t("unknown", json!({})).unwrap(), None);
    }

    #[test]
    fn usage_and_metrics() {
        let u = usage_from_any(&json!({"input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 30})).unwrap();
        assert_eq!((u.prompt_tokens, u.completion_tokens, u.cache_tokens, u.cost_usd), (100, 20, 30, 0.0));
        assert_eq!(
            usage_from_any(&json!({"input_tokens": 200, "output_tokens": 10, "input_tokens_details": {"cached_tokens": 128}}))
                .unwrap()
                .cache_tokens,
            128
        );
        assert_eq!(
            usage_from_any(&json!({"prompt_tokens": 50, "completion_tokens": 5, "prompt_tokens_details": {"cached_tokens": 40}}))
                .unwrap()
                .cache_tokens,
            40
        );
        assert_eq!(usage_from_any(&json!({"prompt_tokens": 10, "completion_tokens": 2, "cache_tokens": 7})).unwrap().cache_tokens, 7);
        assert!(usage_from_any(&Value::Null).is_none() && usage_from_any(&json!({"foo": 1})).is_none());
        // Native turns are never priced: no cost_usd, cached only when > 0.
        assert_eq!(native_metrics(Some(&u)), json!({"prompt_tokens": 100, "completion_tokens": 20, "cached_tokens": 30}));
        assert_eq!(native_metrics(None), json!({}));
        let m: Map<String, Value> = Map::from_iter([
            ("url".to_string(), json!("")),
            ("status".to_string(), json!("error")),
            ("error".to_string(), json!("caf\u{e9} \"x\"")),
        ]);
        assert_eq!(json_spaced(&Value::Object(m)), r#"{"url": "", "status": "error", "error": "caf\u00e9 \"x\""}"#);
    }

    #[test]
    fn sigv4_signing() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // AWS SigV4 test suite "get-vanilla".
        let c = AwsCreds {
            key_id: "AKIDEXAMPLE".into(),
            secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            token: None,
            region: "us-east-1".into(),
        };
        let h = sigv4("GET", &c, "service", "https://example.amazonaws.com/", b"", "20150830T123600Z");
        assert_eq!(
            h.last().unwrap().1,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        // Bedrock invoke: ':' is %3A in the canonical path; session token is signed (signature cross-checked against an independent HMAC-SHA256).
        let c = AwsCreds { key_id: "AKID".into(), secret: "SECRET".into(), token: Some("TOK".into()), region: "us-west-2".into() };
        let h = sigv4_headers(
            &c,
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/us.anthropic.claude-opus-4-7-v1:0/invoke",
            br#"{"a":1}"#,
            "20260101T000000Z",
        );
        assert_eq!(h[0], ("x-amz-date".to_string(), "20260101T000000Z".to_string()));
        assert_eq!(h[1], ("x-amz-security-token".to_string(), "TOK".to_string()));
        assert!(h[2].1.ends_with(
            "SignedHeaders=host;x-amz-date;x-amz-security-token, Signature=a825c5356adc2506a35b8206bd66ea5463c5a3b86fa8903f7fcebd6c9eba02a8"
        ));
    }

    #[test]
    fn transports_follow_sdk_clients() {
        let env =
            |vars: &'static [(&'static str, &'static str)]| move |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        let n = provider(Kind::Anthropic, "anthropic/claude-opus-4-7", &MockApi::default());
        let h = n.http(None, &env(&[("ANTHROPIC_API_KEY", "sk")])).unwrap();
        assert_eq!(h.url, "https://api.anthropic.com/v1/messages?beta=true");
        assert!(
            h.headers.contains(&("anthropic-beta".into(), "computer-use-2025-11-24".into()))
                && h.headers.contains(&("x-api-key".into(), "sk".into()))
        );
        assert!(n.http(None, &env(&[])).err().unwrap().to_string().contains("Could not resolve authentication method"));
        let b = provider(Kind::Bedrock, "bedrock/us.anthropic.claude-sonnet-4-5-v1:0", &MockApi::default());
        let creds: &'static [(&'static str, &'static str)] =
            &[("AWS_ACCESS_KEY_ID", "A"), ("AWS_SECRET_ACCESS_KEY", "S"), ("AWS_REGION", "ap-south-1")];
        let h = b.http(Some("eu-west-1"), &env(creds)).unwrap();
        assert_eq!(h.url, "https://bedrock-runtime.eu-west-1.amazonaws.com/model/us.anthropic.claude-sonnet-4-5-v1:0/invoke");
        let h = b.http(None, &env(creds)).unwrap();
        assert_eq!(h.aws.unwrap().region, "us-east-1"); // `aws_region_name` defaults to us-east-1
        assert!(b.http(None, &env(&[])).err().unwrap().to_string().contains("SigV4"));
        let g = provider(Kind::Gemini, "gemini/gemini-3.8-flash", &MockApi::default());
        let h = g.http(None, &env(&[("GEMINI_API_KEY", "g")])).unwrap();
        assert_eq!(
            (h.url.as_str(), h.sdk, h.headers[0].1.as_str()),
            ("https://generativelanguage.googleapis.com/v1beta/interactions", Sdk::Gemini, "g")
        );
        let o = provider(Kind::OpenAi, "openai/gpt-5.5", &MockApi::default());
        assert_eq!(o.http(None, &env(&[("OPENAI_API_KEY", "k")])).unwrap().url, "https://api.openai.com/v1/responses");
        assert!(o.http(None, &env(&[])).is_err());
        let e = status_error(Sdk::Stainless, 429, "{}");
        assert_eq!((crate::env::error_kind(&e).as_str(), e.to_string()), ("ApiRateLimitError", "Error code: 429 - {}".to_string()));
        let e = status_error(Sdk::Gemini, 400, r#"{"error": {"status": "INVALID_ARGUMENT"}}"#);
        assert_eq!(crate::env::error_kind(&e), "ApiBadRequestError");
        assert!(e.to_string().starts_with("400 INVALID_ARGUMENT."));
    }

    #[tokio::test]
    async fn anthropic_request_and_batch_parsing() {
        let a = api(vec![
            response(vec![json!({"action": "left_click", "coordinate": [10, 20]}), json!({"action": "screenshot"})], "tool_use"),
            json!({"content": [], "stop_reason": "end_turn"}),
        ]);
        let mut p = provider(Kind::Anthropic, "anthropic/claude-opus-4-7", &a);
        let step = p.initial("task", "data:image/webp;base64,initial").await.unwrap();
        let body = a.seen.lock().unwrap()[0].clone();
        assert_eq!(body["model"], "claude-opus-4-7");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(
            body["tools"],
            json!([{"type": "computer_20251124", "name": "computer", "display_width_px": 1024, "display_height_px": 900, "display_number": 1, "enable_zoom": true}])
        );
        assert_eq!(body["system"][0]["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(
            body["messages"][0]["content"][1],
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/webp", "data": "initial"}, "cache_control": {"type": "ephemeral"}})
        );
        assert!(p.messages[0]["content"][1].get("cache_control").is_none()); // history not mutated
        assert_eq!(step.calls.iter().map(|c| c.action.as_ref().unwrap().kind.as_str()).collect::<Vec<_>>(), ["click", "screenshot"]);
        assert!(!step.is_terminal && step.usage.as_ref().unwrap().prompt_tokens == 100);
        // Results follow immediately; a trailing screenshot isn't sent twice.
        let mut shot = ok("call-1");
        shot.screenshot_ref = Some("data:image/webp;base64,s".into());
        let last = p.follow_up(&step, &[ok("call-0"), shot], "data:image/webp;base64,s", None).await.unwrap();
        assert!(last.is_terminal);
        let c = &p.messages[2]["content"];
        assert_eq!(
            (c[0]["tool_use_id"].as_str(), c[0]["content"][0]["text"].as_str(), c[0]["is_error"].as_bool()),
            (Some("call-0"), Some("OK"), Some(false))
        );
        assert_eq!(c[1]["content"][1]["source"]["data"], "s");
        assert_eq!(c[2], json!({"type": "text", "text": "Continue with the task."}));
        assert_eq!(c.as_array().unwrap().len(), 3);
        assert_eq!(a.seen.lock().unwrap()[1]["messages"][2]["content"][2]["cache_control"], json!({"type": "ephemeral"}));
        assert!(p.follow_up(&last, &[ok("x")], "s", None).await.unwrap_err().to_string().contains("one ordered execution result"));

        // Legacy tool + Bedrock body shape.
        let b = provider(Kind::Bedrock, "bedrock/global.anthropic.claude-sonnet-4-5-20250929-v1:0", &a);
        let body = b.anthropic_body();
        assert!(body.get("model").is_none());
        assert_eq!(
            (&body["anthropic_version"], &body["anthropic_beta"], &body["tools"][0]["type"]),
            (&json!("bedrock-2023-05-31"), &json!(["computer-use-2025-01-24"]), &json!("computer_20250124"))
        );
        assert!(body["tools"][0].get("enable_zoom").is_none());

        // Unsupported tool / bad input / truncation / refusal / ids.
        let mut raw = response(vec![json!({"action": "screenshot"}), json!("not an object"), json!({"action": "missing"})], "tool_use");
        raw["content"][0]["name"] = json!("unsupported_tool");
        let s = p.anthropic_step(&raw).unwrap();
        assert_eq!(
            s.calls.iter().map(|c| c.error.clone().unwrap()).collect::<Vec<_>>(),
            ["Unsupported tool: unsupported_tool", "Computer input must be an object", "Unsupported action: missing"]
        );
        assert_eq!(s.calls[1].arguments, json!({}));
        let s = p.anthropic_step(&response(vec![json!({"action": "type", "text": "partial"})], "max_tokens")).unwrap();
        assert_eq!(s.calls[0].error.as_deref(), Some("Response truncated at max_tokens; no actions executed"));
        assert!(!s.is_terminal);
        let s = p.anthropic_step(&json!({"content": [{"type": "text", "text": "I cannot help."}], "stop_reason": "refusal"})).unwrap();
        assert!(s.is_terminal && s.message == "I cannot help." && s.usage.is_none());
        let mut raw = response(vec![json!({"action": "screenshot"}); 2], "tool_use");
        raw["content"][1]["id"] = json!("call-0");
        assert!(p.anthropic_step(&raw).unwrap_err().to_string().contains("unique IDs"));
    }

    #[tokio::test]
    async fn gemini_request_and_results() {
        let a = api(vec![
            gemini_response(vec![("click", json!({"x": 10, "y": 20, "intent": "Click it"})), ("take_screenshot", json!({}))]),
            json!({"steps": []}),
        ]);
        let mut p = provider(Kind::Gemini, "gemini/gemini-3.8-flash", &a);
        let step = p.initial("Do it", "data:image/png;base64,ZmluYWw=").await.unwrap();
        let body = a.seen.lock().unwrap()[0].clone();
        assert_eq!(body["model"], "gemini-3.8-flash");
        assert!(body["input"][0]["text"].as_str().unwrap().starts_with("Do it\n\nUse the computer-use tool"));
        assert_eq!(body["input"][1], json!({"type": "image", "data": "ZmluYWw=", "mime_type": "image/png"}));
        assert_eq!(body["tools"][0], json!({"type": "computer_use", "environment": "browser", "enable_prompt_injection_detection": true}));
        assert!(body.get("previous_interaction_id").is_none());
        assert_eq!((step.message.as_str(), step.usage.as_ref().unwrap().completion_tokens), ("Click it", 20));
        let mut zoom = ok("g1");
        zoom.screenshot_ref = Some("data:image/png;base64,em9vbQ==".into());
        let err = ExecResult::new("g0", "error", "click failed");
        p.follow_up(&step, &[err, zoom], "data:image/png;base64,ZmluYWw=", Some("Check")).await.unwrap();
        let sent = a.seen.lock().unwrap()[1].clone();
        assert_eq!(sent["previous_interaction_id"], "interaction-1");
        let input = sent["input"].as_array().unwrap();
        assert_eq!(
            (input[0]["call_id"].as_str(), input[0]["is_error"].as_bool(), input[0]["name"].as_str()),
            (Some("g0"), Some(true), Some("click"))
        );
        assert_eq!(input[0]["result"][0]["data"], "ZmluYWw=");
        assert_eq!(input[0]["result"][1]["text"], r#"{"url": "", "status": "error", "error": "click failed"}"#);
        assert_eq!(input[1]["result"][0]["data"], "em9vbQ==");
        assert_eq!(input[2], json!({"type": "text", "text": "Check"}));
        // Safety: checked for every call before anything executes; auto-ack is per call.
        for outcome in ["blocked", "require_confirmation"] {
            let r = gemini_response(vec![
                ("click", json!({"x": 1, "y": 2})),
                ("click", json!({"x": 3, "y": 4, "safety_decision": {"decision": outcome}})),
            ]);
            assert!(p.gemini_step(&r).is_err());
        }
        p.auto_ack_safety = true;
        let s = p
            .gemini_step(&gemini_response(vec![
                ("click", json!({"x": 1, "y": 2})),
                ("click", json!({"x": 3, "y": 4, "safety_decision": {"decision": "require_confirmation"}})),
            ]))
            .unwrap();
        *a.replies.lock().unwrap() = vec![json!({"steps": []})].into();
        p.follow_up(&s, &[ok("g0"), ok("g1")], "data:image/png;base64,AA==", None).await.unwrap();
        let sent = a.seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(sent["input"][0]["result"][1]["text"], r#"{"url": "", "status": "success"}"#);
        assert_eq!(sent["input"][1]["result"][1]["text"], r#"{"url": "", "status": "success", "safety_acknowledgement": true}"#);
        // Malformed calls become errors; text-only is terminal.
        let s = p.gemini_step(&gemini_response(vec![("unknown", json!({})), ("click", json!("not an object"))])).unwrap();
        assert_eq!(
            s.calls.iter().map(|c| c.error.clone().unwrap()).collect::<Vec<_>>(),
            ["Unsupported Gemini action: unknown", "Gemini function arguments must be an object"]
        );
        let s = p.gemini_step(&json!({"steps": [{"type": "model_output", "content": [{"text": "Done"}]}]})).unwrap();
        assert!(s.is_terminal && s.message == "Done" && s.calls.is_empty());
    }

    fn native_agent(kind: Kind, model: &str, a: &MockApi, kwargs: Value) -> (Computer1, std::path::PathBuf) {
        let (mut c, logs) = agent(kwargs, &MockLlm::default());
        c.native = Some(provider(kind, model, a));
        c.provider = "native".into();
        (c, logs)
    }

    fn trajectory(logs: &std::path::Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(logs.join("trajectory.json")).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn anthropic_step_loop_end_to_end() {
        let a = api(vec![
            response(
                vec![
                    json!({"action": "left_click", "coordinate": [3, 4]}),
                    json!({"action": "key", "text": "ctrl+u"}),
                    json!({"action": "type", "text": "x"}),
                ],
                "tool_use",
            ),
            json!({"id": "final", "content": [{"type": "text", "text": "Finished"}], "stop_reason": "end_turn", "usage": {"input_tokens": 40, "output_tokens": 10, "cache_read_input_tokens": 30}}),
        ]);
        let (mut c, logs) = native_agent(Kind::Anthropic, "anthropic/claude-opus-4-7", &a, json!({}));
        let sh = MockShell::default();
        let mut ctx = AgentContext::default();
        c.run_with("task", &sh, &mut ctx).await.unwrap();

        let cmds = sh.log.lock().unwrap().clone();
        assert!(cmds.contains(&"DISPLAY=:1 xdotool mousemove 3 4 click 1".to_string()));
        assert!(!cmds.iter().any(|c| c.contains("xdotool type"))); // blocked key stops the batch
        assert!(cmds.iter().any(|c| c.contains("/logs/agent/screenshot_ep0_action0.webp")));
        assert!(cmds.iter().any(|c| c.contains("/logs/agent/screenshot_ep0_follow.webp")));
        let enc = base64::engine::general_purpose::STANDARD.encode("Finished");
        assert!(cmds.iter().any(|c| c.contains(&enc)));
        assert_eq!(c.early_termination_reason.as_deref(), Some("task_complete"));
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens, ctx.n_cache_tokens, ctx.cost_usd), (Some(140), Some(30), Some(30), None));

        let seen = a.seen.lock().unwrap().clone();
        let follow = &seen[1]["messages"][2]["content"];
        assert_eq!(
            follow
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["type"] == "tool_result")
                .map(|b| b["is_error"].as_bool().unwrap())
                .collect::<Vec<_>>(),
            [false, true, true]
        );
        assert!(follow[1]["content"][0]["text"].as_str().unwrap().contains("(403): Action blocked: developer tools"));
        assert_eq!(follow[2]["content"][0]["text"], "Not executed: an earlier computer action in this turn failed.");
        assert_eq!(follow.as_array().unwrap().last().unwrap()["source"]["data"], "QUJD");

        let t = trajectory(&logs);
        let steps = t["steps"].as_array().unwrap();
        assert_eq!(steps[0], json!({"step_id": 1, "timestamp": steps[0]["timestamp"], "source": "user", "message": "task"}));
        assert_eq!(steps[1]["model_name"], "claude-opus-4-7");
        assert_eq!(
            steps[1]["tool_calls"][0],
            json!({"tool_call_id": "call-0", "function_name": "computer", "arguments": {"action": "left_click", "coordinate": [3, 4]}})
        );
        let r0 = &steps[1]["observation"]["results"][0];
        assert_eq!(
            (r0["source_call_id"].as_str(), r0["extra"]["execution_status"].as_str(), r0["content"][0]["text"].as_str()),
            (Some("call-0"), Some("success"), Some("OK"))
        );
        assert_eq!(r0["content"][1]["source"]["path"], "screenshot_ep0_action0.webp");
        assert_eq!(steps[1]["metrics"], json!({"prompt_tokens": 100, "completion_tokens": 20}));
        assert_eq!(steps[1]["extra"], json!({"response_id": "response-1"}));
        assert_eq!((steps[2]["message"].as_str(), steps[2].get("tool_calls"), steps[2].get("observation")), (Some("Finished"), None, None));
        assert_eq!(t["final_metrics"], json!({"total_prompt_tokens": 140, "total_completion_tokens": 30, "total_cached_tokens": 30}));
        assert_eq!(ctx.model_usage.as_ref().unwrap()["claude-opus-4-7"].cost_usd, None);
    }

    #[tokio::test]
    async fn step_loop_limits_dead_session_and_fallback() {
        // max_turns: no unconsumed request; the fallback writes an empty answer (no chat-client query).
        let a = api(vec![response(vec![json!({"action": "screenshot"})], "tool_use")]);
        let (mut c, logs) = native_agent(Kind::Bedrock, "bedrock/us.anthropic.claude-opus-4-7-v1:0", &a, json!({"max_turns": 1}));
        let sh = MockShell::default();
        let mut ctx = AgentContext::default();
        c.run_with("task", &sh, &mut ctx).await.unwrap();
        assert_eq!(a.seen.lock().unwrap().len(), 1);
        assert_eq!(c.early_termination_reason.as_deref(), Some("max_turns_reached"));
        assert_eq!(ctx.n_output_tokens, Some(20));
        assert!(
            sh.log.lock().unwrap().iter().any(|c| c == "mkdir -p /logs/agent && printf '%s' '' | base64 -d > /logs/agent/final_answer.txt")
        );
        let t = trajectory(&logs);
        assert_eq!(t["steps"][1]["observation"]["results"][0]["content"][1]["source"]["path"], "screenshot_ep0_action0.webp");

        // A dead runtime still accounts the response and marks every call not executed.
        #[derive(Default)]
        struct Dead(MockShell);
        #[async_trait::async_trait]
        impl Shell for Dead {
            async fn sh(&self, cmd: &str, root: bool, t: Option<Duration>) -> Result<crate::env::ExecResult> {
                if cmd.contains("pgrep -f chromium") && cmd.contains("X11") {
                    return Ok(crate::env::ExecResult { code: 0, stdout: "down".into(), stderr: String::new() });
                }
                self.0.sh(cmd, root, t).await
            }
        }
        let a = api(vec![response(vec![json!({"action": "type", "text": "x"})], "tool_use")]);
        let (mut c, logs) = native_agent(Kind::Anthropic, "claude-opus-4-7", &a, json!({}));
        let mut ctx = AgentContext::default();
        c.run_with("task", &Dead::default(), &mut ctx).await.unwrap();
        assert_eq!(c.early_termination_reason.as_deref(), Some("runtime_session_dead"));
        assert_eq!(ctx.n_output_tokens, Some(20));
        let t = trajectory(&logs);
        assert_eq!(t["steps"][1]["observation"]["results"][0]["extra"]["execution_status"], "not_executed");

        // No usage at all: the context stays unset.
        let a = api(vec![json!({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn"})]);
        let (mut c, _) = native_agent(Kind::Anthropic, "claude-opus-4-7", &a, json!({}));
        let mut ctx = AgentContext::default();
        c.run_with("task", &MockShell::default(), &mut ctx).await.unwrap();
        assert_eq!((ctx.n_input_tokens, c.early_termination_reason.as_deref()), (None, Some("task_complete")));
    }

    #[tokio::test]
    async fn gemini_step_loop_uses_png_payloads() {
        let a = api(vec![
            gemini_response(vec![("click", json!({"x": 500, "y": 250})), ("take_screenshot", json!({}))]),
            json!({"steps": [{"type": "model_output", "content": [{"type": "text", "text": "PASS"}]}], "usage": {"total_input_tokens": 40, "total_output_tokens": 10}}),
        ]);
        let (mut c, _) = native_agent(Kind::Gemini, "gemini/gemini-3.8-flash", &a, json!({}));
        let sh = MockShell::default();
        let mut ctx = AgentContext::default();
        c.run_with("Complete the calibration", &sh, &mut ctx).await.unwrap();
        let cmds = sh.log.lock().unwrap().clone();
        assert!(cmds.contains(&"DISPLAY=:1 xdotool mousemove 512 225 click 1".to_string())); // 0-999 grid scaled
        assert!(cmds.iter().any(|c| c.starts_with("base64 -w0 /tmp/computer_1-screenshots/latest.png")));
        assert_eq!((ctx.n_output_tokens, c.early_termination_reason.as_deref()), (Some(30), Some("task_complete")));
        let seen = a.seen.lock().unwrap().clone();
        assert_eq!(seen[1]["input"].as_array().unwrap().len(), 2);
        assert_eq!(seen[1]["input"][1]["result"][0]["data"], "QUJD");
    }

    #[tokio::test]
    async fn openai_self_driving_loop() {
        let a = api(vec![
            json!({"id": "r1", "output": [{"type": "reasoning"}, {"type": "computer_call", "call_id": "c1", "actions": [{"type": "click", "x": 7, "y": 8}, {"type": "screenshot"}], "pending_safety_checks": [{"id": "s1"}]}], "usage": {"input_tokens": 100, "output_tokens": 5, "input_tokens_details": {"cached_tokens": 64}}}),
            json!({"id": "r2", "output": [{"type": "message", "content": [{"type": "output_text", "text": "The answer is 42"}]}], "usage": {"input_tokens": 50, "output_tokens": 3}}),
        ]);
        let (mut c, logs) = native_agent(Kind::OpenAi, "openai/gpt-5.5", &a, json!({}));
        c.session_id = "sess".into();
        let sh = MockShell::default();
        let mut ctx = AgentContext::default();
        c.run_with("Find it", &sh, &mut ctx).await.unwrap();
        let seen = a.seen.lock().unwrap().clone();
        assert_eq!(
            seen[0],
            json!({"model": "gpt-5.5", "tools": [{"type": "computer"}], "input": "Find it", "truncation": "auto", "prompt_cache_key": "computer-1-sess"})
        );
        assert_eq!(seen[1]["previous_response_id"], "r1");
        assert_eq!(
            seen[1]["input"],
            json!([{"type": "computer_call_output", "call_id": "c1", "output": {"type": "computer_screenshot", "image_url": "data:image/png;base64,QUJD", "detail": "original"}, "acknowledged_safety_checks": [{"id": "s1"}]}])
        );
        assert!(sh.log.lock().unwrap().contains(&"DISPLAY=:1 xdotool mousemove 7 8 click 1".to_string()));
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens, ctx.n_cache_tokens), (Some(150), Some(8), Some(64)));
        assert_eq!(c.early_termination_reason.as_deref(), Some("task_complete"));
        let t = trajectory(&logs);
        let steps = t["steps"].as_array().unwrap();
        assert_eq!(steps[1]["tool_calls"][0]["arguments"]["x"], 7);
        assert_eq!(steps[1]["observation"]["results"][0]["content"][1]["source"]["path"], "screenshot_ep0.webp");
        assert_eq!(steps[1]["metrics"], json!({"prompt_tokens": 100, "completion_tokens": 5, "cached_tokens": 64}));
        assert_eq!(steps[2]["message"], "Analysis: The answer is 42");
        assert_eq!(
            steps[2]["tool_calls"],
            json!([{"tool_call_id": "call_1_task_complete", "function_name": "mark_task_complete", "arguments": {"result": null}}])
        );
        let enc = base64::engine::general_purpose::STANDARD.encode("The answer is 42");
        assert!(sh.log.lock().unwrap().iter().any(|c| c.contains(&enc)));
    }
}
