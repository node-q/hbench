//! computer-1: desktop computer-use agent.
//!
//! Ported: the generic strict-JSON harness (the `litellm` provider flavor, the default for
//! models outside the anthropic/bedrock/gemini/vertex_ai routes), the native computer-use
//! providers (`native`: Anthropic/Bedrock computer tool, Gemini Computer Use, OpenAI Responses
//! `computer` tool) with model-based provider selection, and the shared pieces: the desktop runtime
//! (Xvfb + XFCE + VNC + Chromium in the container, driven by xdotool over `docker compose exec`),
//! the JSON action parser and prompts, the episode loop with double-confirmed completion,
//! image-aware context compaction, the ATIF recorder, per-episode logs and final_answer.txt.
//!

use super::llm::{ChatClient, Llm, LlmConfig, LlmError, LlmResponse, add_anthropic_caching, api_err, lookup_model_info};
use super::terminus2::{Kw, Shell, fill_template, limit_output_length, now, pyf, quote_repr};
use super::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx, ModelUsage};
use crate::env::{error_kind, quote};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};

const JSON_TEMPLATE: &str = include_str!("../../assets/agents/computer1/computer-1-json.txt");
const TIMEOUT_TEMPLATE: &str = include_str!("../../assets/agents/computer1/timeout.txt");
const NAME: &str = "computer-1";
const VERSION: &str = "1.0.0";
const AGENT_DIR: &str = "/logs/agent";
const FINAL_ANSWER: &str = "final_answer.txt";
const MAX_QUERY_RECURSION_DEPTH: usize = 2;
const MAX_OBSERVATION_BYTES: usize = 10_000;
const PROACTIVE_COMPACTION_FREE_TOKENS: i64 = 8_000;
const UNWIND_TARGET_FREE_TOKENS: i64 = 4_000;
/// Flat per-screenshot token estimate (text counters badly undercount image parts).
const IMAGE_TOKEN_ESTIMATE: i64 = 1_300;
const KEEP_LAST_SCREENSHOTS: usize = 3;
const TERMINAL_ACTION_TYPES: [&str; 3] = ["terminate", "done", "answer"];

mod native;

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Python `repr()` of a JSON value (dict-style), for `str(exec_result)` observations.
fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => quote_repr(s),
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => {
            format!("{{{}}}", o.iter().map(|(k, v)| format!("{}: {}", quote_repr(k), py_repr(v))).collect::<Vec<_>>().join(", "))
        }
    }
}

/// A computer action (serialized in full, None included, as ATIF tool-call arguments).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Action {
    #[serde(rename = "type")]
    kind: String,
    x: Option<i64>,
    y: Option<i64>,
    end_x: Option<i64>,
    end_y: Option<i64>,
    text: Option<String>,
    keys: Option<Vec<String>>,
    url: Option<String>,
    scroll_x: Option<i64>,
    scroll_y: Option<i64>,
    button: Option<String>,
    result: Option<String>,
    source: &'static str,
    model_x: Option<i64>,
    model_y: Option<i64>,
    zoom_region: Option<Vec<i64>>,
    modifier: Option<String>,
    duration: Option<f64>,
    duration_seconds: Option<f64>,
    press_enter: Option<bool>,
    clear_before_typing: Option<bool>,
    command: Option<String>,
    timeout_sec: Option<f64>,
}

impl Action {
    fn new(kind: &str) -> Action {
        Action {
            kind: kind.into(),
            x: None,
            y: None,
            end_x: None,
            end_y: None,
            text: None,
            keys: None,
            url: None,
            scroll_x: None,
            scroll_y: None,
            button: None,
            result: None,
            source: "native_prescaled",
            model_x: None,
            model_y: None,
            zoom_region: None,
            modifier: None,
            duration: None,
            duration_seconds: None,
            press_enter: None,
            clear_before_typing: None,
            command: None,
            timeout_sec: None,
        }
    }
    fn is_terminal(&self) -> bool {
        TERMINAL_ACTION_TYPES.contains(&self.kind.as_str())
    }
}

#[derive(Debug, Clone, Default)]
struct ParsedAction {
    action: Option<Action>,
    is_task_complete: bool,
    error: String,
    warning: String,
    analysis: String,
    plan: String,
}

const ALLOWED_ACTION_TYPES: [&str; 20] = [
    "click",
    "double_click",
    "triple_click",
    "right_click",
    "mouse_down",
    "mouse_up",
    "mouse_move",
    "type",
    "key",
    "keypress",
    "hold_key",
    "scroll",
    "drag",
    "zoom",
    "navigate",
    "wait",
    "bash",
    "done",
    "answer",
    "terminate",
];

fn warn_join(w: &[String]) -> String {
    if w.is_empty() { String::new() } else { format!("- {}", w.join("\n- ")) }
}

/// First balanced top-level JSON object (escapes only count inside strings).
fn extract_json_object(response: &str) -> (&str, Vec<String>) {
    let (mut start, mut end) = (None, None);
    let (mut depth, mut in_string, mut escape_next) = (0i64, false, false);
    for (i, c) in response.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if in_string {
            match c {
                '\\' => escape_next = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 && start.is_some() {
                    end = Some(i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let (Some(s), Some(e)) = (start, end) else { return ("", vec!["No valid JSON object found".into()]) };
    let mut w = vec![];
    if !response[..s].trim().is_empty() {
        w.push("Extra text detected before JSON object".into());
    }
    if !response[e..].trim().is_empty() {
        w.push("Extra text detected after JSON object".into());
    }
    (&response[s..e], w)
}

fn coerce_int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn coerce_float(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn coerce_str(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn parse_action_dict(d: &Value, w: &mut Vec<String>) -> Result<Action, String> {
    let Some(o) = d.as_object() else { return Err("Field 'action' must be an object".into()) };
    let Some(kind) = o.get("type").and_then(Value::as_str).filter(|t| !t.is_empty()) else {
        return Err("Action 'type' is missing or not a string".into());
    };
    if !ALLOWED_ACTION_TYPES.contains(&kind) {
        w.push(format!("Unknown action type: {}", quote_repr(kind)));
    }
    let mut keys = None;
    match o.get("keys") {
        None | Some(Value::Null) => {}
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => {
            keys = Some(a.iter().map(|k| k.as_str().unwrap().to_string()).collect::<Vec<_>>()).filter(|k| !k.is_empty())
        }
        Some(_) => w.push("Action 'keys' must be a list of strings; ignoring".into()),
    }
    let mut modifier = None;
    match o.get("modifier") {
        None | Some(Value::Null) => {}
        Some(Value::String(m)) => modifier = Some(m.clone()),
        Some(_) => w.push("Action 'modifier' must be a string; ignoring".into()),
    }
    let zoom_region = o
        .get("zoom_region")
        .and_then(Value::as_array)
        .filter(|a| a.len() == 4)
        .and_then(|a| a.iter().map(|v| coerce_int(Some(v))).collect::<Option<Vec<_>>>());
    if o.get("zoom_region").is_some_and(|z| !z.is_null()) && zoom_region.is_none() {
        w.push("Action 'zoom_region' must be a 4-element list of integers; ignoring".into());
    }
    let mut a = Action::new(kind);
    a.x = coerce_int(o.get("x"));
    a.y = coerce_int(o.get("y"));
    a.end_x = coerce_int(o.get("end_x"));
    a.end_y = coerce_int(o.get("end_y"));
    a.text = coerce_str(o.get("text"));
    a.keys = keys;
    a.url = coerce_str(o.get("url"));
    a.scroll_x = coerce_int(o.get("scroll_x"));
    a.scroll_y = coerce_int(o.get("scroll_y"));
    a.button = coerce_str(o.get("button"));
    a.result = coerce_str(o.get("result"));
    a.zoom_region = zoom_region;
    a.modifier = modifier;
    a.duration = coerce_float(o.get("duration"));
    a.command = coerce_str(o.get("command"));
    a.timeout_sec = coerce_float(o.get("timeout_sec"));
    Ok(a)
}

/// Port of `parse_computer_1_response`.
fn parse_response(response: &str) -> ParsedAction {
    let mut w = vec![];
    let (json_str, extra) = extract_json_object(response);
    w.extend(extra);
    let fail = |error: String, w: &[String], analysis: String, plan: String| ParsedAction {
        error,
        warning: warn_join(w),
        analysis,
        plan,
        ..Default::default()
    };
    if json_str.is_empty() {
        return fail("No valid JSON found in response".into(), &w, String::new(), String::new());
    }
    let data: Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            let mut msg = format!("Invalid JSON: {e}");
            if json_str.chars().count() < 200 {
                msg += &format!(" | Content: {}", quote_repr(json_str));
            } else {
                msg += &format!(" | Content preview: {}...", quote_repr(&json_str.chars().take(100).collect::<String>()));
            }
            return fail(msg, &w, String::new(), String::new());
        }
    };
    let Some(o) = data.as_object() else { return fail("Response must be a JSON object".into(), &w, String::new(), String::new()) };
    let text_field = |k: &str, w: &mut Vec<String>| match o.get(k) {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => {
            w.push(format!("Field '{k}' should be a string"));
            String::new()
        }
    };
    let analysis = text_field("analysis", &mut w);
    let plan = text_field("plan", &mut w);
    let Some(action_data) = o.get("action") else { return fail("Missing required field: action".into(), &w, analysis, plan) };
    match parse_action_dict(action_data, &mut w) {
        Err(e) => fail(e, &w, analysis, plan),
        Ok(action) => ParsedAction {
            is_task_complete: action.is_terminal(),
            action: Some(action),
            error: String::new(),
            warning: warn_join(&w),
            analysis,
            plan,
        },
    }
}

/// Provider-normalized model turn (`ModelStep`).
#[derive(Debug, Clone, Default)]
struct ModelStep {
    action: Option<Action>,
    message: String,
    analysis: String,
    plan: String,
    feedback: String,
    is_terminal: bool,
    needs_retry: bool,
}

fn parse_step(content: &str) -> ModelStep {
    let p = parse_response(content);
    let feedback = match (p.error.is_empty(), p.warning.is_empty()) {
        (false, false) => format!("ERROR: {}\nWARNINGS: {}", p.error, p.warning),
        (false, true) => format!("ERROR: {}", p.error),
        (true, false) => format!("WARNINGS: {}", p.warning),
        (true, true) => String::new(),
    };
    if !p.error.is_empty() {
        return ModelStep { needs_retry: true, feedback, ..Default::default() };
    }
    ModelStep {
        action: p.action,
        message: content.into(),
        analysis: p.analysis,
        plan: p.plan,
        feedback,
        is_terminal: p.is_task_complete,
        needs_retry: false,
    }
}

fn image_url_part(data_url: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": data_url, "detail": "auto"}})
}

fn mime_for_path(path: &str) -> &'static str {
    let l = path.to_lowercase();
    if l.ends_with(".png") {
        "image/png"
    } else if l.ends_with(".jpg") || l.ends_with(".jpeg") {
        "image/jpeg"
    } else {
        "image/webp"
    }
}

const DISPLAY: &str = ":1";
const RUNTIME_DIR: &str = "/tmp/computer_1_runtime";
const SCREENSHOT_DIR: &str = "/tmp/computer_1-screenshots";
const CHROME_PROFILE: &str = "/tmp/computer_1_runtime/profile";
const CHROMIUM_LOG: &str = "/tmp/computer_1_runtime/chromium.log";
const XVFB_LOG: &str = "/tmp/computer_1_runtime/xvfb.log";
const XFCE_LOG: &str = "/tmp/computer_1_runtime/xfce4.log";
const VNC_LOG: &str = "/tmp/computer_1_runtime/x11vnc.log";
const NOVNC_LOG: &str = "/tmp/computer_1_runtime/novnc.log";
const BLOCKED_KEY_COMBOS: [&str; 9] =
    ["ctrl+u", "ctrl+shift+i", "ctrl+shift+j", "ctrl+shift+c", "f12", "control+u", "control+shift+i", "control+shift+j", "control+shift+c"];
const BLOCKED_URL_SCHEMES: [&str; 3] = ["view-source:", "devtools://", "chrome-devtools://"];

/// A failed request to the desktop runtime.
#[derive(Debug)]
struct RuntimeErr {
    label: String,
    code: i64,
    detail: String,
}
impl std::fmt::Display for RuntimeErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Runtime action {} failed ({}): {}", quote_repr(&self.label), self.code, self.detail)
    }
}
impl std::error::Error for RuntimeErr {}

fn rt_err(label: &str, code: i64, detail: impl Into<String>) -> anyhow::Error {
    RuntimeErr { label: label.into(), code, detail: detail.into() }.into()
}

/// Python's `TimeoutError` from the readiness waits (the agent maps it to the timeout template).
#[derive(Debug)]
struct ReadinessTimeout(String);
impl std::fmt::Display for ReadinessTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ReadinessTimeout {}

fn xdotool_key(key: &str) -> String {
    let parts: Vec<&str> = key.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return key.into();
    }
    let alias = |p: &str| -> String {
        match p.to_lowercase().as_str() {
            "alt" | "option" => "alt",
            "arrowdown" | "down" => "Down",
            "arrowleft" | "left" => "Left",
            "arrowright" | "right" => "Right",
            "arrowup" | "up" => "Up",
            "backspace" => "BackSpace",
            "cmd" | "command" | "meta" => "super",
            "control" | "ctrl" => "ctrl",
            "delete" => "Delete",
            "end" => "End",
            "enter" | "return" => "Return",
            "esc" | "escape" => "Escape",
            "home" => "Home",
            "insert" => "Insert",
            "pagedown" => "Next",
            "pageup" => "Prior",
            "shift" => "shift",
            "space" | "spacebar" => "space",
            "tab" => "Tab",
            _ => return p.to_string(),
        }
        .to_string()
    };
    parts.iter().map(|p| alias(p)).collect::<Vec<_>>().join("+")
}

/// Modifier-only prefixes collapse into one chord (`["ctrl","l"]` -> `ctrl+l`).
fn xdotool_key_sequence(keys: Option<&Vec<String>>) -> Vec<String> {
    let r: Vec<String> = keys.into_iter().flatten().filter(|k| !k.is_empty()).map(|k| xdotool_key(k)).collect();
    if r.len() > 1 && r[..r.len() - 1].iter().all(|m| ["ctrl", "alt", "shift", "super"].contains(&m.to_lowercase().as_str())) {
        return vec![r.join("+")];
    }
    r
}

fn resolve_modifier(m: Option<&str>) -> Option<&'static str> {
    Some(match m?.trim().to_lowercase().as_str() {
        "shift" => "shift",
        "ctrl" | "control" => "ctrl",
        "alt" => "alt",
        "super" | "meta" | "cmd" | "command" => "super",
        _ => return None,
    })
}

fn is_blocked_key_combo(keys: Option<&Vec<String>>) -> bool {
    let Some(keys) = keys else { return false };
    let combo = keys.iter().map(|k| k.trim().to_lowercase()).filter(|k| !k.is_empty()).collect::<Vec<_>>().join("+");
    BLOCKED_KEY_COMBOS.contains(&combo.as_str())
}

#[derive(Debug, Clone, Copy)]
struct Geometry {
    desktop_width: i64,
    desktop_height: i64,
    window_x: i64,
    window_y: i64,
    window_width: i64,
    window_height: i64,
}

/// Port of `build_xdotool_argv`: None for actions xdotool doesn't handle.
fn build_xdotool_argv(a: &Action, g: &Geometry) -> Option<Vec<Vec<String>>> {
    let s = |v: &str| v.to_string();
    let modifier = resolve_modifier(a.modifier.as_deref());
    let (x, y) = (a.x.unwrap_or(0).to_string(), a.y.unwrap_or(0).to_string());
    let click = |button: &str, repeat: usize| {
        let mut v = vec![s("mousemove"), x.clone(), y.clone()];
        if let Some(m) = modifier {
            v.extend([s("keydown"), s(m)]);
        }
        if repeat > 1 {
            v.extend([s("click"), s("--repeat"), repeat.to_string(), s(button)]);
        } else {
            v.extend([s("click"), s(button)]);
        }
        if let Some(m) = modifier {
            v.extend([s("keyup"), s(m)]);
        }
        v
    };
    Some(match a.kind.as_str() {
        "click" => vec![click(
            match a.button.as_deref() {
                Some("right") => "3",
                Some("middle") => "2",
                _ => "1",
            },
            1,
        )],
        "double_click" => vec![click("1", 2)],
        "triple_click" => vec![click("1", 3)],
        "right_click" => vec![click("3", 1)],
        "middle_click" => vec![click("2", 1)],
        "mouse_down" => vec![vec![s("mousemove"), x, y, s("mousedown"), s("1")]],
        "mouse_up" => vec![vec![s("mousemove"), x, y, s("mouseup"), s("1")]],
        "mouse_move" => vec![vec![s("mousemove"), x, y]],
        "type" => {
            let mut c = vec![vec![s("type"), s("--clearmodifiers"), s("--"), a.text.clone().unwrap_or_default()]];
            if a.press_enter == Some(true) {
                c.push(vec![s("key"), s("--clearmodifiers"), s("Return")]);
            }
            c
        }
        "key_down" | "key_up" => {
            let op = if a.kind == "key_down" { "keydown" } else { "keyup" };
            xdotool_key_sequence(a.keys.as_ref()).into_iter().map(|k| vec![s(op), k]).collect()
        }
        "key" | "keypress" => xdotool_key_sequence(a.keys.as_ref()).into_iter().map(|k| vec![s("key"), s("--clearmodifiers"), k]).collect(),
        "drag" => {
            let ex = a.end_x.or(a.x).unwrap_or(0).to_string();
            let ey = a.end_y.or(a.y).unwrap_or(0).to_string();
            vec![vec![s("mousemove"), x, y, s("mousedown"), s("1"), s("mousemove"), ex, ey, s("mouseup"), s("1")]]
        }
        "scroll" => {
            let cx = a.x.unwrap_or(g.desktop_width / 2).to_string();
            let cy = a.y.unwrap_or(g.desktop_height / 2).to_string();
            let (sy, sx) = (a.scroll_y.unwrap_or(500), a.scroll_x.unwrap_or(0));
            let mut v = vec![s("mousemove"), cx, cy];
            if let Some(m) = modifier {
                v.extend([s("keydown"), s(m)]);
            }
            for (amount, pos, neg) in [(sy, "5", "4"), (sx, "7", "6")] {
                if amount != 0 {
                    let clicks = (amount.abs() / 100).max(1);
                    v.extend([s("click"), s("--repeat"), clicks.to_string(), s(if amount > 0 { pos } else { neg })]);
                }
            }
            if let Some(m) = modifier {
                v.extend([s("keyup"), s(m)]);
            }
            vec![v]
        }
        _ => return None,
    })
}

fn xdotool_command(argv: &[String]) -> String {
    format!("DISPLAY={DISPLAY} xdotool {}", argv.iter().map(|p| quote(p)).collect::<Vec<_>>().join(" "))
}

fn bash_inline(script: &str) -> String {
    format!("bash -lc {}", quote(script))
}

/// Owns the in-environment desktop and executes actions (`Computer1Session`).
struct Desktop {
    geo: Geometry,
    agent_dir: String,
    readiness_timeout: u64,
    request_timeout: u64,
    chromium: String,
    webp_quality: u32,
    enable_bash: bool,
    bash_timeout: f64,
    bash_max_stdout: usize,
    bash_max_stderr: usize,
    zoom: Option<[i64; 4]>,
}

impl Desktop {
    async fn raw_exec(&self, sh: &dyn Shell, cmd: &str, timeout: u64) -> Result<crate::env::ExecResult> {
        sh.sh(cmd, false, Some(Duration::from_secs(timeout))).await
    }

    /// Run `cmd`; non-zero exit -> RuntimeErr. `retries` only for idempotent setup steps.
    async fn exec(&self, sh: &dyn Shell, cmd: &str, timeout: u64, label: &str, retries: usize) -> Result<String> {
        let mut last = None;
        for attempt in 0..=retries {
            match self.raw_exec(sh, cmd, timeout).await {
                Ok(r) if r.code != 0 => {
                    let stderr = r.stderr.trim();
                    return Err(rt_err(label, r.code.into(), if stderr.is_empty() { "exec returned non-zero" } else { stderr }));
                }
                Ok(r) => return Ok(r.stdout),
                Err(e) if error_kind(&e) == "TimeoutError" => last = Some(rt_err(label, 28, format!("timed out after ~{timeout}s"))),
                Err(e) => last = Some(rt_err(label, 0, format!("environment.exec failed: {e:#}"))),
            }
            if attempt < retries {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
        Err(last.unwrap_or_else(|| rt_err(label, 0, "exec failed")))
    }

    async fn start(&mut self, sh: &dyn Shell) -> Result<()> {
        let dirs =
            [RUNTIME_DIR, SCREENSHOT_DIR, CHROME_PROFILE, self.agent_dir.as_str()].iter().map(|d| quote(d)).collect::<Vec<_>>().join(" ");
        self.exec(sh, &bash_inline(&format!("mkdir -p {dirs}")), 60, "mkdir runtime dirs", 2).await?;
        // Xvfb (reuse a display the image already started).
        let check = self.raw_exec(sh, "test -S /tmp/.X11-unix/X1 && echo present || echo missing", 5).await?;
        if !check.stdout.contains("present") {
            let g = self.geo;
            let cmd = format!(
                "setsid nohup Xvfb :1 -screen 0 {}x{}x24 -fbdir /var/tmp >> {} 2>&1 < /dev/null &",
                g.desktop_width,
                g.desktop_height,
                quote(XVFB_LOG)
            );
            self.exec(sh, &bash_inline(&cmd), 60, "start Xvfb", 2).await?;
        }
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if Instant::now() >= deadline {
                return Err(ReadinessTimeout("X11 display :1 never appeared".into()).into());
            }
            match self.raw_exec(sh, "test -S /tmp/.X11-unix/X1 && echo ok || echo wait", 10).await {
                Ok(r) if r.stdout.contains("ok") => break,
                Ok(_) => tokio::time::sleep(Duration::from_millis(250)).await,
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
        let xfce = format!("DISPLAY={DISPLAY} setsid nohup startxfce4 >> {} 2>&1 < /dev/null &", quote(XFCE_LOG));
        self.exec(sh, &bash_inline(&xfce), 60, "start xfce", 2).await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = self.raw_exec(sh, "pkill -f xfce4-panel || true", 30).await; // maximize the viewport
        let vnc = format!(
            "command -v x11vnc >/dev/null 2>&1 && DISPLAY={DISPLAY} setsid nohup x11vnc -display {DISPLAY} -forever -shared -nopw -rfbport 5900 >> {} 2>&1 < /dev/null & true",
            quote(VNC_LOG)
        );
        self.exec(sh, &bash_inline(&vnc), 60, "start x11vnc", 2).await?;
        let novnc = format!(
            "command -v websockify >/dev/null 2>&1 && [ -d /usr/share/novnc ] && setsid nohup websockify --web /usr/share/novnc 8080 localhost:5900 >> {} 2>&1 < /dev/null & true",
            quote(NOVNC_LOG)
        );
        self.exec(sh, &bash_inline(&novnc), 60, "start noVNC", 2).await?;
        self.start_chromium(sh).await?;
        self.wait_for_chromium_window(sh).await?;
        self.position_window(sh).await;
        Ok(())
    }

    fn chromium_command(&self) -> String {
        let g = self.geo;
        let args = [
            self.chromium.clone(),
            "--ignore-certificate-errors".into(),
            "--disable-dev-shm-usage".into(),
            "--no-sandbox".into(),
            "--disable-gpu".into(),
            format!("--display={DISPLAY}"),
            format!("--user-data-dir={CHROME_PROFILE}"),
            format!("--window-position={},{}", g.window_x, g.window_y),
            format!("--window-size={},{}", g.window_width, g.window_height),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-default-apps".into(),
            "--disable-dev-tools".into(),
            "--disable-extensions".into(),
            "--disable-features=IsolateOrigins,site-per-process,AutomationControlled,HttpsUpgrades".into(),
            "--disable-infobars".into(),
            "--disable-blink-features=AutomationControlled".into(),
            "--js-flags=--max-old-space-size=4096".into(),
            "--renderer-process-limit=4".into(),
            "--test-type".into(),
            "--lang=en-US".into(),
            "--remote-debugging-port=9222".into(),
            "about:blank".into(),
        ];
        let quoted = args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
        format!("DISPLAY={DISPLAY} setsid nohup {quoted} >> {} 2>&1 < /dev/null &", quote(CHROMIUM_LOG))
    }

    async fn start_chromium(&self, sh: &dyn Shell) -> Result<()> {
        self.exec(sh, &bash_inline(&self.chromium_command()), 60, "start chromium", 2).await.map(|_| ())
    }

    async fn wait_for_chromium_window(&self, sh: &dyn Shell) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(self.readiness_timeout);
        while Instant::now() < deadline {
            let probe = async {
                let w = self
                    .raw_exec(sh, &format!("DISPLAY={DISPLAY} wmctrl -l 2>/dev/null | grep -Ei 'chromium|chrome' | head -1"), 10)
                    .await?;
                if !w.stdout.trim().is_empty() {
                    return anyhow::Ok(true);
                }
                let cdp = self
                    .raw_exec(sh, "curl -fsS -o /dev/null -w '%{http_code}' --max-time 3 http://127.0.0.1:9222/json/version", 10)
                    .await?;
                Ok(cdp.stdout.trim() == "200")
            };
            match probe.await {
                Ok(true) => return Ok(()),
                Ok(false) => tokio::time::sleep(Duration::from_millis(500)).await,
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
        let tail = match self
            .raw_exec(sh, &format!("if [ -f {0} ]; then tail -n 50 {0}; else echo '(no log)'; fi", quote(CHROMIUM_LOG)), 10)
            .await
        {
            Ok(r) if !r.stdout.trim().is_empty() => r.stdout.trim().to_string(),
            Ok(_) => "(empty log)".into(),
            Err(e) => format!("(failed to tail {CHROMIUM_LOG}: {e})"),
        };
        Err(ReadinessTimeout(format!(
            "Chromium did not become ready within {}s.\n--- chromium.log tail ---\n{tail}",
            self.readiness_timeout
        ))
        .into())
    }

    async fn position_window(&self, sh: &dyn Shell) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let g = self.geo;
        let fill = g.window_x == 0 && g.window_y == 0 && g.window_width == g.desktop_width && g.window_height == g.desktop_height;
        let maximize = if fill { " && wmctrl -i -r \"$wid\" -b add,maximized_vert,maximized_horz" } else { "" };
        let inner = format!(
            "wid=$(wmctrl -l 2>/dev/null | grep -Ei 'chromium|chrome' | head -1 | awk '{{print $1}}'); if [ -n \"$wid\" ]; then wmctrl -i -r \"$wid\" -e 0,{},{},{},{}{maximize}; fi",
            g.window_x, g.window_y, g.window_width, g.window_height
        );
        let _ = self.exec(sh, &format!("DISPLAY={DISPLAY} bash -c {}", quote(&inner)), 10, "position window", 0).await;
    }

    async fn is_alive(&self, sh: &dyn Shell) -> bool {
        matches!(self.raw_exec(sh, "test -S /tmp/.X11-unix/X1 && pgrep -f chromium >/dev/null && echo ok || echo down", 5).await, Ok(r) if r.stdout.contains("ok"))
    }

    /// Kill Chromium, wipe its profile, relaunch.
    async fn reset(&self, sh: &dyn Shell) -> Result<()> {
        self.raw_exec(sh, "pkill -9 -f chromium || true", 10).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        self.raw_exec(sh, &format!("rm -rf {0} && mkdir -p {0}", quote(CHROME_PROFILE)), 10).await?;
        self.start_chromium(sh).await?;
        self.wait_for_chromium_window(sh).await?;
        self.position_window(sh).await;
        Ok(())
    }

    async fn recover_chromium_if_needed(&self, sh: &dyn Shell, action_type: &str, e: &anyhow::Error) -> Result<Option<Value>> {
        match self.raw_exec(sh, "pgrep -f chromium >/dev/null && echo up || echo down", 5).await {
            Ok(r) if !r.stdout.contains("up") => {}
            _ => return Ok(None),
        }
        self.reset(sh).await?;
        Ok(Some(json!({
            "status": "recovered", "action": action_type, "recovered": true,
            "error": "Computer process crashed; restarted Chromium. Retry the action if still needed.", "original_error": e.to_string(),
        })))
    }

    async fn x(&self, sh: &dyn Shell, argv: &[&str], label: &str) -> Result<String> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        self.exec(sh, &xdotool_command(&argv), self.request_timeout, label, 0).await
    }

    async fn execute(&mut self, sh: &dyn Shell, a: &Action) -> Result<Value> {
        let a = &native::normalize_action(a, self.geo.desktop_width, self.geo.desktop_height);
        let kind = a.kind.as_str();
        if matches!(kind, "key" | "keypress") && is_blocked_key_combo(a.keys.as_ref()) {
            return Err(rt_err(kind, 403, "Action blocked: developer tools are not available in this environment."));
        }
        if kind == "type" && a.text.as_deref().is_some_and(|t| t.to_lowercase().contains("view-source:")) {
            return Err(rt_err(kind, 403, "Action blocked: view-source is not available in this environment."));
        }
        if kind == "navigate" && a.url.as_deref().is_some_and(|u| BLOCKED_URL_SCHEMES.iter().any(|s| u.to_lowercase().starts_with(s))) {
            return Err(rt_err(kind, 403, "Action blocked: this URL scheme is not available in this environment."));
        }
        match kind {
            "wait" => {
                tokio::time::sleep(Duration::from_secs(1)).await;
                return Ok(json!({"status": "ok"}));
            }
            "screenshot" => return Ok(json!({"status": "ok"})),
            "cursor_position" => {
                let out = self.x(sh, &["getmouselocation", "--shell"], "cursor_position").await?;
                let field = |k: &str| out.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).and_then(|v| v.trim().parse::<i64>().ok());
                let (Some(x), Some(y)) = (field("X"), field("Y")) else { bail!("cursor_position: unexpected output {out:?}") };
                return Ok(json!({"status": "ok", "x": x, "y": y}));
            }
            "sleep" => {
                let d = a.duration_seconds.filter(|d| *d != 0.0).or(a.duration.filter(|d| *d != 0.0)).unwrap_or(1.0);
                if d > 0.0 && d.is_finite() {
                    tokio::time::sleep(Duration::from_secs_f64(d)).await;
                }
                return Ok(json!({"status": "ok"}));
            }
            k if TERMINAL_ACTION_TYPES.contains(&k) => return Ok(json!({"status": "done", "text": a.text})),
            "bash" => {
                if !self.enable_bash {
                    return Ok(
                        json!({"status": "error", "return_code": 2, "stdout": "", "stderr": "bash action is disabled. Enable it by running computer-1 with extra_tools=[\"bash\"]."}),
                    );
                }
                return Ok(self.execute_bash(sh, a).await);
            }
            "zoom" => {
                self.zoom = a.zoom_region.as_ref().filter(|r| r.len() == 4).map(|r| [r[0], r[1], r[2], r[3]]);
                return Ok(json!({"status": "ok"}));
            }
            _ => {}
        }
        let r = self.execute_shell_action(sh, a).await;
        match r {
            Ok(v) => Ok(v),
            Err(e) => match self.recover_chromium_if_needed(sh, kind, &e).await? {
                Some(v) => Ok(v),
                None => Err(e),
            },
        }
    }

    async fn execute_shell_action(&self, sh: &dyn Shell, a: &Action) -> Result<Value> {
        let ok = json!({"status": "ok"});
        match a.kind.as_str() {
            "navigate" => {
                self.x(sh, &["key", "--clearmodifiers", "ctrl+l"], "navigate:focus").await?;
                tokio::time::sleep(Duration::from_millis(100)).await;
                self.x(sh, &["key", "--clearmodifiers", "ctrl+a"], "navigate:selectall").await?;
                self.x(sh, &["type", "--clearmodifiers", "--", a.url.as_deref().unwrap_or("about:blank")], "navigate:type").await?;
                self.x(sh, &["key", "--clearmodifiers", "Return"], "navigate:enter").await?;
            }
            "go_back" => drop(self.x(sh, &["key", "--clearmodifiers", "alt+Left"], "go_back").await?),
            "go_forward" => drop(self.x(sh, &["key", "--clearmodifiers", "alt+Right"], "go_forward").await?),
            "type_text_at" => {
                let (x, y) = (a.x.unwrap_or(0).to_string(), a.y.unwrap_or(0).to_string());
                self.x(sh, &["mousemove", &x, &y, "click", "1"], "type_text_at:click").await?;
                if a.clear_before_typing != Some(false) {
                    self.x(sh, &["key", "--clearmodifiers", "ctrl+a"], "type_text_at:clear").await?;
                }
                if let Some(t) = a.text.as_deref().filter(|t| !t.is_empty()) {
                    self.x(sh, &["type", "--clearmodifiers", "--", t], "type_text_at:type").await?;
                }
                if a.press_enter != Some(false) {
                    self.x(sh, &["key", "--clearmodifiers", "Return"], "type_text_at:enter").await?;
                }
            }
            "hold_key" => {
                let keys = xdotool_key_sequence(a.keys.as_ref());
                if !keys.is_empty() {
                    for k in &keys {
                        self.x(sh, &["keydown", k], "hold_key:down").await?;
                    }
                    let d = a.duration.unwrap_or(1.0);
                    if d > 0.0 && d.is_finite() {
                        tokio::time::sleep(Duration::from_secs_f64(d)).await;
                    }
                    for k in &keys {
                        self.x(sh, &["keyup", k], "hold_key:up").await?;
                    }
                }
            }
            kind => {
                let Some(argvs) = build_xdotool_argv(a, &self.geo) else {
                    return Err(rt_err(kind, 400, format!("Unsupported action type: {kind}")));
                };
                for argv in argvs {
                    self.exec(sh, &xdotool_command(&argv), self.request_timeout, &format!("action:{kind}"), 0).await?;
                }
            }
        }
        Ok(ok)
    }

    /// Bounded shell command for the opt-in `bash` action.
    async fn execute_bash(&self, sh: &dyn Shell, a: &Action) -> Value {
        let cmd = a.command.clone().or_else(|| a.text.clone()).unwrap_or_default();
        if cmd.trim().is_empty() {
            return json!({"status": "error", "return_code": 2, "stdout": "", "stderr": "bash action requires a non-empty `command` (or `text`) string."});
        }
        let t = (a.timeout_sec.filter(|t| *t != 0.0).unwrap_or(self.bash_timeout).ceil() as u64).max(1);
        match sh.sh(&format!("cd / && {cmd}"), false, Some(Duration::from_secs(t))).await {
            Err(e) if error_kind(&e) == "TimeoutError" => {
                json!({"status": "timeout", "return_code": 124, "stdout": "", "stderr": format!("bash action timed out after {t}s: {e}")})
            }
            Err(e) => json!({"status": "error", "return_code": 1, "stdout": "", "stderr": format!("bash action failed to dispatch: {e}")}),
            Ok(r) => {
                let cut = |s: &str, n: usize| s.chars().take(n).collect::<String>();
                json!({
                    "status": if r.code == 0 { "ok" } else { "error" }, "return_code": r.code,
                    "stdout": cut(&r.stdout, self.bash_max_stdout), "stderr": cut(&r.stderr, self.bash_max_stderr),
                    "stdout_truncated": r.stdout.chars().count() > self.bash_max_stdout, "stderr_truncated": r.stderr.chars().count() > self.bash_max_stderr,
                })
            }
        }
    }

    /// Capture the desktop (one-shot zoom crop), encode by suffix, write to `target`.
    fn screenshot_script(&mut self, target: &str) -> String {
        let (png, webp) = (format!("{SCREENSHOT_DIR}/latest.png"), format!("{SCREENSHOT_DIR}/latest.webp"));
        let dir = target.rsplit_once('/').map(|(d, _)| if d.is_empty() { "/" } else { d }).unwrap_or(".");
        let crop = match self.zoom.take() {
            Some([x0, y0, x1, y1]) => {
                format!(" && convert {} -crop {}x{}+{x0}+{y0} +repage {}", quote(&png), (x1 - x0).max(1), (y1 - y0).max(1), quote(&png))
            }
            None => String::new(),
        };
        let output = if target.to_lowercase().ends_with(".png") {
            format!("cp {} {}", quote(&png), quote(target))
        } else {
            format!(
                "if command -v cwebp >/dev/null 2>&1; then cwebp -quiet -q {} {} -o {} >/dev/null 2>&1 && cp {} {}; else cp {} {}; fi",
                self.webp_quality,
                quote(&png),
                quote(&webp),
                quote(&webp),
                quote(target),
                quote(&png),
                quote(target)
            )
        };
        format!(
            "set -e; export DISPLAY={DISPLAY}; mkdir -p {} {}; {{ import -window root {} || scrot -o {}; }}{crop}; {output}",
            quote(SCREENSHOT_DIR),
            quote(dir),
            quote(&png),
            quote(&png)
        )
    }

    async fn fetch_screenshot(&mut self, sh: &dyn Shell, target: &str) -> Result<String> {
        let script = self.screenshot_script(target);
        self.exec(sh, &bash_inline(&script), self.request_timeout.max(30), "screenshot", 0).await?;
        Ok(target.to_string())
    }
}

async fn screenshot_data_url(sh: &dyn Shell, path: &str) -> Result<String> {
    let r = sh.sh(&format!("base64 -w0 {path} 2>/dev/null || base64 {path}"), false, None).await?;
    if r.code != 0 || r.stdout.is_empty() {
        bail!("Could not read screenshot at {path}");
    }
    Ok(format!("data:{};base64,{}", mime_for_path(path), r.stdout.trim()))
}

#[derive(Debug, Default)]
struct CChat {
    messages: Vec<Value>,
    total_input_tokens: i64,
    total_output_tokens: i64,
    total_cache_tokens: i64,
    total_cost: f64,
}

impl CChat {
    async fn chat(&mut self, llm: &dyn Llm, model: &str, prompt: &[Value], kw: &Map<String, Value>) -> Result<LlmResponse, LlmError> {
        let mut messages = self.messages.clone();
        messages.extend(prompt.iter().cloned());
        let r = llm.complete(add_anthropic_caching(messages, model), kw).await?;
        if let Some(u) = &r.usage {
            self.total_input_tokens += u.prompt_tokens;
            self.total_output_tokens += u.completion_tokens;
            self.total_cache_tokens += u.cache_tokens;
            self.total_cost += u.cost_usd;
        }
        // Native tool_calls aren't threaded back (the JSON dialect never sends tools).
        self.messages.extend(prompt.iter().cloned());
        self.messages.push(json!({"role": "assistant", "content": r.content}));
        Ok(r)
    }
}

fn is_image_part(p: &Value) -> bool {
    p["type"] == "image_url"
}

fn text_only_message(m: &Value) -> Value {
    let Some(parts) = m["content"].as_array() else { return m.clone() };
    let mut text: Vec<Value> = parts.iter().filter(|p| !is_image_part(p)).cloned().collect();
    if text.is_empty() {
        text.push(json!({"type": "text", "text": ""}));
    }
    let mut out = m.clone();
    out["content"] = Value::Array(text);
    out
}

/// Image-aware token count: text (image-stripped) + a flat estimate per screenshot.
fn count_total_tokens(llm: &dyn Llm, messages: &[Value]) -> i64 {
    let text: Vec<Value> = messages.iter().map(text_only_message).collect();
    let images = messages
        .iter()
        .filter_map(|m| m["content"].as_array())
        .map(|ps| ps.iter().filter(|p| is_image_part(p)).count() as i64)
        .sum::<i64>();
    llm.count_tokens(&text) + images * IMAGE_TOKEN_ESTIMATE
}

/// Text of a prompt payload with image parts dropped.
fn extract_prompt_text(prompt: &[Value]) -> String {
    prompt
        .iter()
        .flat_map(|t| match &t["content"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(ps) => ps.iter().filter(|p| p["type"] == "text").map(|p| p["text"].as_str().unwrap_or("").to_string()).collect(),
            _ => vec![],
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strip images from all but the newest `keep` image-bearing turns; returns images removed.
fn trim_old_screenshots(messages: &mut [Value], keep: usize) -> usize {
    let (mut removed, mut seen) = (0, 0);
    for m in messages.iter_mut().rev() {
        let Some(parts) = m["content"].as_array() else { continue };
        if !parts.iter().any(is_image_part) {
            continue;
        }
        seen += 1;
        if seen <= keep {
            continue;
        }
        let mut new: Vec<Value> = parts.iter().filter(|p| !is_image_part(p)).cloned().collect();
        removed += parts.len() - new.len();
        if !new.iter().any(|p| p["type"] == "text") {
            new.push(json!({"type": "text", "text": "(earlier screenshot removed to save context)"}));
        }
        m["content"] = Value::Array(new);
    }
    removed
}

fn user(text: impl Into<String>) -> Vec<Value> {
    vec![json!({"role": "user", "content": text.into()})]
}

fn last_n_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

#[derive(Debug, Clone)]
struct Options {
    max_turns: Option<i64>,
    temperature: f64,
    api_base: Option<String>,
    reasoning_effort: Option<String>,
    max_thinking_tokens: Option<i64>,
    model_info: Option<Value>,
    collect_rollout_details: bool,
    session_id: Option<String>,
    llm_kwargs: Map<String, Value>,
    llm_call_kwargs: Map<String, Value>,
    geo: Geometry,
    readiness_timeout: i64,
    request_timeout: i64,
    action_timeout: f64,
    enable_episode_logging: bool,
    start_url: Option<String>,
    env_io_dir: Option<String>,
    extra_tools: Vec<String>,
    enable_images: Option<bool>,
    provider: Option<String>,
    aws_region_name: Option<String>,
    gemini_auto_ack_safety: bool,
}

impl Options {
    fn parse(kwargs: &BTreeMap<String, Value>) -> Result<Options> {
        let mut kw = Kw::new(kwargs);
        let extra_tools = match kwargs.get("extra_tools") {
            None | Some(Value::Null) => vec![],
            Some(Value::Array(a)) if a.iter().all(Value::is_string) => a.iter().map(|v| v.as_str().unwrap().to_string()).collect(),
            Some(_) => {
                kw.string("extra_tools"); // records the type error
                vec![]
            }
        };
        let enable_images = kw.get("enable_images").is_some().then(|| kw.boolean("enable_images", true));
        let o = Options {
            max_turns: kw.int("max_turns"),
            temperature: kw.float("temperature").unwrap_or(0.7),
            api_base: kw.string("api_base"),
            reasoning_effort: kw.string("reasoning_effort"),
            max_thinking_tokens: kw.int("max_thinking_tokens"),
            model_info: kw.object("model_info").map(Value::Object),
            collect_rollout_details: kw.boolean("collect_rollout_details", false),
            session_id: kw.string("session_id"),
            llm_kwargs: kw.object("llm_kwargs").unwrap_or_default(),
            llm_call_kwargs: kw.object("llm_call_kwargs").unwrap_or_default(),
            geo: Geometry {
                desktop_width: kw.int("desktop_width").unwrap_or(1024),
                desktop_height: kw.int("desktop_height").unwrap_or(900),
                window_x: kw.int("window_x").unwrap_or(0),
                window_y: kw.int("window_y").unwrap_or(0),
                window_width: kw.int("window_width").unwrap_or(1024),
                window_height: kw.int("window_height").unwrap_or(900),
            },
            readiness_timeout: kw.int("runtime_readiness_timeout_sec").unwrap_or(120),
            request_timeout: kw.int("runtime_request_timeout_sec").unwrap_or(120),
            action_timeout: kw.float("runtime_action_timeout_sec").unwrap_or(60.0),
            enable_episode_logging: kw.boolean("enable_episode_logging", true),
            start_url: kw.string("start_url"),
            env_io_dir: kw.string("env_io_dir"),
            extra_tools,
            enable_images,
            provider: kw.string("provider").map(|p| p.to_lowercase()),
            aws_region_name: kw.string("aws_region_name"),
            gemini_auto_ack_safety: kw.boolean("gemini_auto_ack_safety", false),
        };
        let use_responses = kw.boolean("use_responses_api", false);
        kw.finish(NAME)?;
        if use_responses {
            bail!("computer-1 use_responses_api is not supported by hbench's port (its generic harness always uses chat completions)");
        }
        Ok(o)
    }
}

/// Whether the model has a native computer-use tool (litellm flag, else model-name patterns).
fn is_computer_use_model(model: &str) -> bool {
    let l = model.to_lowercase();
    if ["gemini-3.7-flash", "gemini-3.8-flash"].contains(&l.rsplit('/').next().unwrap_or("")) {
        return true;
    }
    if l.contains("computer-use") || l.contains("computer_use") {
        return true;
    }
    if l.contains("claude") && (["sonnet", "opus", "fable", "mythos"].iter().any(|f| l.contains(f)) || l.contains("haiku-4-5")) {
        return true;
    }
    re_gpt_version(&l).is_some_and(|v| v >= (5, 4))
}

fn re_gpt_version(l: &str) -> Option<(u32, u32)> {
    let c = regex::Regex::new(r"\bgpt-(\d+)(?:\.(\d+))?").unwrap().captures(l)?;
    Some((c[1].parse().ok()?, c.get(2).map_or(Some(0), |m| m.as_str().parse().ok())?))
}

/// Provider for `model`: the explicit override, else inferred from the model name.
fn resolve_provider_name(model: &str, override_: Option<&str>) -> Result<String> {
    let name = match override_ {
        Some(n) => {
            if !["anthropic", "bedrock", "gemini", "litellm", "openai"].contains(&n) {
                bail!(
                    "Unknown computer-1 provider {}. Available providers: ['anthropic', 'bedrock', 'gemini', 'litellm', 'openai']",
                    quote_repr(n)
                );
            }
            n.to_string()
        }
        None => {
            let prefix = model.split_once('/').map(|(p, _)| p.to_lowercase());
            match prefix.as_deref() {
                Some("anthropic") => "anthropic",
                Some("bedrock") => "bedrock",
                Some("gemini" | "vertex_ai") => "gemini",
                None if model.to_lowercase().starts_with("claude") => "anthropic",
                None if model.to_lowercase().contains("anthropic.claude") => "bedrock",
                None if model.to_lowercase().starts_with("gemini") => "gemini",
                _ => "litellm",
            }
            .to_string()
        }
    };
    if name != "litellm" && !is_computer_use_model(model) {
        bail!(
            "Model {} is not a computer-use model for the {} harness. Use a computer-use-capable model, or pass provider='litellm' to run it through the generic JSON harness.",
            quote_repr(model),
            quote_repr(&name)
        );
    }
    Ok(name)
}

/// LiteLLM `supports_vision` for known models (None = model unknown or flag unset).
fn supports_vision(model: &str) -> Option<bool> {
    lookup_model_info(model)?;
    let l = model.to_lowercase();
    if l.contains("deepseek-v4-flash") {
        return Some(false);
    }
    ["claude", "gpt-4o", "gpt-4.1", "gpt-5", "/o3", "gemini"].iter().any(|p| l.contains(p) || l == "o3").then_some(true)
}

/// The temperature to send: omit it where models reject a non-default value.
fn resolve_temperature(model: &str, t: f64) -> Option<f64> {
    let n = model.to_lowercase();
    if regex::Regex::new(r"opus-4-(\d+)").unwrap().captures(&n).is_some_and(|c| c[1].parse::<u32>().is_ok_and(|v| v >= 7)) {
        return None;
    }
    if (n.contains("bedrock") && n.contains("opus")) || n.contains("fable") || n.contains("mythos") {
        return None;
    }
    if n.contains("gpt-5") || regex::Regex::new(r"(^|/)o[1-9]\b").unwrap().is_match(&n) {
        return None;
    }
    Some(t)
}

pub struct Computer1 {
    args: AgentArgs,
    opts: Options,
    model_name: String,
    llm: Box<dyn Llm>,
    enable_images: bool,
    enable_bash: bool,
    env_io_dir: String,
    session_id: String,
    desktop: Option<Desktop>,
    chat: CChat,
    steps: Vec<Value>,
    n_episodes: i64,
    api_request_times: Vec<f64>,
    pending_completion: bool,
    early_termination_reason: Option<String>,
    wait_streak: i64,
    latest_screenshot: Option<String>,
    compaction_count: i64,
    /// Resolved provider flavor ("litellm" or a native one).
    provider: String,
    native: Option<native::Native>,
    /// Native providers: whether any turn reported usage (the context stays unset otherwise).
    usage_seen: bool,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(Computer1::new(args)?))
}

impl Computer1 {
    fn new(args: AgentArgs) -> Result<Computer1> {
        let opts = Options::parse(&args.kwargs)?;
        let model = args.model_name.clone().ok_or_else(|| anyhow!("model_name is required for computer-1"))?;
        let provider = resolve_provider_name(&model, opts.provider.as_deref())?;
        // Known to LiteLLM without vision support -> fail fast (unknown models pass).
        if provider == "litellm"
            && opts.enable_images.is_none()
            && lookup_model_info(&model).is_some()
            && supports_vision(&model) != Some(true)
        {
            bail!(
                "Model {} does not support vision input, but computer-1's generic harness is screenshot-driven. Use a vision-capable model, or pass enable_images explicitly to override litellm's metadata.",
                quote_repr(&model)
            );
        }
        let unknown: BTreeSet<String> =
            opts.extra_tools.iter().map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty() && t != "bash").collect();
        if !unknown.is_empty() {
            bail!("Unknown computer-1 extra_tools: {}. Supported tools: bash.", py_repr(&json!(unknown)));
        }
        let enable_bash = opts.extra_tools.iter().any(|t| t.trim().eq_ignore_ascii_case("bash"));
        let enable_images = opts.enable_images.unwrap_or_else(|| supports_vision(&model).unwrap_or(true));
        let session_id = uuid::Uuid::new_v4().to_string();
        let mut llm_kwargs = Map::from_iter([("prompt_cache_key".to_string(), json!(format!("computer-1-{session_id}")))]);
        llm_kwargs.extend(opts.llm_kwargs.clone());
        let cfg = LlmConfig {
            model_name: model.clone(),
            api_base: opts.api_base.clone(),
            temperature: resolve_temperature(&model, opts.temperature),
            reasoning_effort: opts.reasoning_effort.clone(),
            max_thinking_tokens: opts.max_thinking_tokens,
            session_id: opts.session_id.clone(),
            collect_rollout_details: opts.collect_rollout_details,
            model_info: opts.model_info.clone(),
            use_responses_api: false,
            llm_kwargs,
        };
        // Native providers talk to their vendor APIs directly; the litellm client is never used
        // (no JSON loop, compaction or fallback query), so its endpoint isn't resolved.
        let llm: Box<dyn Llm> =
            if provider == "litellm" { Box::new(ChatClient::new(cfg, &|k: &str| args.get_env(k))?) } else { Box::new(NoLlm) };
        let mut c = Self::with_llm(args, opts, model, llm, enable_images, enable_bash, session_id);
        c.provider = provider;
        Ok(c)
    }

    fn with_llm(
        args: AgentArgs,
        opts: Options,
        model_name: String,
        llm: Box<dyn Llm>,
        enable_images: bool,
        enable_bash: bool,
        session_id: String,
    ) -> Computer1 {
        Computer1 {
            env_io_dir: opts.env_io_dir.clone().unwrap_or_else(|| AGENT_DIR.into()).trim_end_matches('/').to_string(),
            args,
            opts,
            model_name,
            llm,
            enable_images,
            enable_bash,
            session_id,
            desktop: None,
            chat: CChat::default(),
            steps: vec![],
            n_episodes: 0,
            api_request_times: vec![],
            pending_completion: false,
            early_termination_reason: None,
            wait_streak: 0,
            latest_screenshot: None,
            compaction_count: 0,
            provider: "litellm".into(),
            native: None,
            usage_seen: false,
        }
    }

    fn new_desktop(&self) -> Desktop {
        Desktop {
            geo: self.opts.geo,
            agent_dir: self.env_io_dir.clone(),
            readiness_timeout: self.opts.readiness_timeout.max(0) as u64,
            request_timeout: self.opts.request_timeout.max(1) as u64,
            chromium: "/usr/bin/chromium".into(),
            webp_quality: 80,
            enable_bash: self.enable_bash,
            bash_timeout: 30.0,
            bash_max_stdout: 8192,
            bash_max_stderr: 4096,
            zoom: None,
        }
    }

    fn desktop(&mut self) -> Result<&mut Desktop> {
        self.desktop.as_mut().ok_or_else(|| anyhow!("Session is not set. Call setup() first."))
    }

    fn io_path(&self, name: &str) -> String {
        format!("{}/{name}", self.env_io_dir)
    }

    fn prompt_text(&self, instruction: &str) -> String {
        let (opt, fields, rule) = if self.enable_bash {
            (
                " bash,",
                "\n    \"command\": <string, optional, used by bash>,\n    \"timeout_sec\": <number, optional, per-call cap for bash>,",
                "\n- For \"bash\": provide a shell command in \"command\" (or \"text\"). The command\n  runs inside the same environment as the running app and the next\n  observation contains its stdout/stderr/exit_code. Use this to read\n  source files, list directories, grep configuration, etc. — for\n  example {\"type\": \"bash\", \"command\": \"ls /app && cat /app/start.sh\"}.\n  Output is truncated to ~8 KB stdout / 4 KB stderr per call. Optional\n  \"timeout_sec\" caps a single call; default is 30 seconds.",
            )
        } else {
            ("", "", "")
        };
        let (w, h) = (self.opts.geo.desktop_width.to_string(), self.opts.geo.desktop_height.to_string());
        fill_template(
            JSON_TEMPLATE,
            &[
                ("instruction", instruction),
                ("desktop_width", &w),
                ("desktop_height", &h),
                ("bash_type_option", opt),
                ("bash_fields", fields),
                ("bash_rule", rule),
            ],
        )
    }

    fn turn(&self, text: &str, screenshot_ref: &str, empty: &str) -> Vec<Value> {
        let mut content = vec![];
        if !text.trim().is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
        if self.enable_images && !screenshot_ref.is_empty() {
            content.push(image_url_part(screenshot_ref));
        }
        if content.is_empty() {
            content.push(json!({"type": "text", "text": empty}));
        }
        vec![json!({"role": "user", "content": content})]
    }

    /// Append an ATIF step (None fields dropped, like `exclude_none`).
    fn push_step(&mut self, mut step: Map<String, Value>) {
        step.insert("step_id".into(), json!(self.steps.len() + 1));
        step.insert("timestamp".into(), json!(now()));
        step.retain(|_, v| !v.is_null());
        self.steps.push(Value::Object(step));
    }

    fn viewer_path(&self, p: &str) -> String {
        for base in [self.env_io_dir.as_str(), AGENT_DIR] {
            let base = base.trim_end_matches('/');
            if base.is_empty() {
                continue;
            }
            if let Some(rest) = p.strip_prefix(&format!("{base}/")) {
                return rest.to_string();
            }
            if p == base {
                return String::new();
            }
        }
        p.to_string()
    }

    fn metrics(&self, before: (i64, i64, i64, f64)) -> Value {
        let c = &self.chat;
        let (cache, cost) = (c.total_cache_tokens - before.2, c.total_cost - before.3);
        let mut m = json!({"prompt_tokens": c.total_input_tokens - before.0, "completion_tokens": c.total_output_tokens - before.1});
        if cache > 0 {
            m["cached_tokens"] = json!(cache);
        }
        if cost > 0.0 {
            m["cost_usd"] = json!(cost);
        }
        m
    }

    fn image_part(&self, p: &str) -> Value {
        json!({"type": "image", "source": {"media_type": mime_for_path(p), "path": self.viewer_path(p)}})
    }

    #[allow(clippy::too_many_arguments)]
    fn record_agent_step(
        &mut self,
        episode: i64,
        resp: &LlmResponse,
        step: &ModelStep,
        is_complete: bool,
        observation: &str,
        paths: &[String],
        metrics: Value,
    ) {
        let mut parts = vec![];
        if !step.analysis.is_empty() {
            parts.push(format!("Analysis: {}", step.analysis));
        }
        if !step.plan.is_empty() {
            parts.push(format!("Plan: {}", step.plan));
        }
        let mut calls = vec![];
        let action_id = step.action.as_ref().map(|_| format!("call_{episode}_1"));
        if let (Some(a), Some(id)) = (&step.action, &action_id) {
            calls.push(json!({"tool_call_id": id, "function_name": "computer_action", "arguments": a}));
        }
        if is_complete {
            calls.push(json!({"tool_call_id": format!("call_{episode}_task_complete"), "function_name": "mark_task_complete", "arguments": {"result": step.action.as_ref().and_then(|a| a.result.clone())}}));
        }
        let content = if paths.is_empty() {
            json!(observation)
        } else {
            let mut c = vec![json!({"type": "text", "text": observation})];
            c.extend(paths.iter().map(|p| self.image_part(p)));
            Value::Array(c)
        };
        let mut result = json!({"content": content});
        if let Some(id) = action_id {
            result["source_call_id"] = json!(id);
        }
        self.push_step(Map::from_iter([
            ("source".into(), json!("agent")),
            ("model_name".into(), json!(resp.model_name.clone().unwrap_or_else(|| self.model_name.clone()))),
            ("message".into(), json!(parts.join("\n"))),
            ("reasoning_content".into(), json!(resp.reasoning_content)),
            ("tool_calls".into(), if calls.is_empty() { Value::Null } else { Value::Array(calls) }),
            ("observation".into(), json!({"results": [result]})),
            ("metrics".into(), metrics),
        ]));
    }

    fn sync_context(&self, ctx: &mut AgentContext) {
        let c = &self.chat;
        if self.native.is_none() || self.usage_seen {
            ctx.n_input_tokens = Some(c.total_input_tokens);
            ctx.n_output_tokens = Some(c.total_output_tokens);
            ctx.n_cache_tokens = Some(c.total_cache_tokens);
            ctx.cost_usd = (c.total_cost > 0.0).then_some(c.total_cost);
        }
        let md = ctx.metadata.get_or_insert_with(|| json!({}));
        if !md.is_object() {
            *md = json!({});
        }
        md["n_episodes"] = json!(self.n_episodes);
        md["api_request_times_msec"] = json!(self.api_request_times);
        md["early_termination_reason"] = json!(self.early_termination_reason);
        md["compaction_count"] = json!(self.compaction_count);
        ctx.model_usage = self.model_usage();
    }

    /// Per-model usage summed over the trajectory.
    fn model_usage(&self) -> Option<BTreeMap<String, ModelUsage>> {
        let mut usage: BTreeMap<String, ModelUsage> = BTreeMap::new();
        let mut unpriced = BTreeSet::new();
        for s in &self.steps {
            let Some(m) = s.get("metrics") else { continue };
            let name = s["model_name"].as_str().unwrap_or(&self.model_name).to_string();
            let u = usage.entry(name.clone()).or_default();
            let (p, c) = (m["prompt_tokens"].as_i64().unwrap_or(0), m["completion_tokens"].as_i64().unwrap_or(0));
            u.n_input_tokens += p;
            u.n_cache_tokens += m["cached_tokens"].as_i64().unwrap_or(0);
            u.n_output_tokens += c;
            match m["cost_usd"].as_f64() {
                Some(x) => u.cost_usd = Some(u.cost_usd.unwrap_or(0.0) + x),
                None if p > 0 || c > 0 => {
                    unpriced.insert(name);
                }
                None => {}
            }
        }
        for n in unpriced {
            usage.get_mut(&n).unwrap().cost_usd = None;
        }
        (!usage.is_empty()).then_some(usage)
    }

    fn dump_trajectory(&self) {
        if self.steps.is_empty() {
            return;
        }
        let c = &self.chat;
        let mut fm = json!({"total_prompt_tokens": c.total_input_tokens, "total_completion_tokens": c.total_output_tokens, "total_cached_tokens": c.total_cache_tokens});
        if c.total_cost > 0.0 {
            fm["total_cost_usd"] = json!(c.total_cost);
        }
        if self.native.is_some() {
            fm = aggregate_step_metrics(&self.steps);
        }
        let mut t = json!({
            "schema_version": "ATIF-v1.8", "session_id": self.session_id,
            "agent": {"name": NAME, "version": VERSION, "model_name": self.model_name},
            "steps": self.steps, "final_metrics": fm,
        });
        if let Some(r) = &self.early_termination_reason {
            t["extra"] = json!({"early_termination_reason": r});
        }
        let path = self.args.logs_dir.join("trajectory.json");
        let tmp = self.args.logs_dir.join("trajectory.json.tmp");
        let _ = std::fs::create_dir_all(&self.args.logs_dir);
        if std::fs::write(&tmp, serde_json::to_string_pretty(&t).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }

    async fn capture(&mut self, sh: &dyn Shell, name: &str) -> Result<String> {
        let target = self.io_path(name);
        let p = self.desktop()?.fetch_screenshot(sh, &target).await?;
        self.latest_screenshot = Some(p.clone());
        Ok(p)
    }

    async fn execute_action(
        &mut self,
        sh: &dyn Shell,
        action: Option<&Action>,
        episode: i64,
        index: Option<usize>,
    ) -> Result<(String, Vec<String>, bool)> {
        let shot = format!("screenshot_ep{episode}{}.webp", index.map(|i| format!("_action{i}")).unwrap_or_default());
        let Some(a) = action else { return Ok(("(no action taken)".into(), vec![self.capture(sh, &shot).await?], false)) };
        if a.is_terminal() {
            return Ok((format!("Terminal action committed: {}", a.kind), vec![self.capture(sh, &shot).await?], false));
        }
        let r = match self.desktop()?.execute(sh, a).await {
            Ok(r) => r,
            Err(e) if e.downcast_ref::<ReadinessTimeout>().is_some() => {
                let msg = fill_template(TIMEOUT_TEMPLATE, &[("timeout_sec", &pyf(self.opts.action_timeout)), ("action", &a.kind)]);
                return Ok((msg, vec![], true));
            }
            Err(e) => return Ok((format!("Action {} failed: {e}", quote_repr(&a.kind)), vec![self.capture(sh, &shot).await?], true)),
        };
        let is_error =
            !matches!(r["status"].as_str(), None | Some("ok" | "done")) || r.get("return_code").is_some_and(|c| c.as_i64() != Some(0));
        let text = if a.kind == "bash" {
            format_bash_observation(&r)
        } else if is_error {
            py_repr(&r)
        } else if a.kind == "cursor_position" {
            format!("X={}, Y={} (desktop pixels)", r["x"], r["y"])
        } else {
            String::new()
        };
        Ok((text, vec![self.capture(sh, &shot).await?], is_error))
    }

    async fn write_final_answer(&self, sh: &dyn Shell, answer: &str) -> Result<()> {
        let target = self.io_path(FINAL_ANSWER);
        let dir = target.rsplit_once('/').map(|(d, _)| if d.is_empty() { "/" } else { d }).unwrap_or(".");
        let enc = base64::engine::general_purpose::STANDARD.encode(answer.as_bytes());
        let cmd = format!("mkdir -p {} && printf '%s' {} | base64 -d > {}", quote(dir), quote(&enc), quote(&target));
        sh.sh(&cmd, false, Some(Duration::from_secs(30))).await.map(|_| ()) // a non-zero exit is only logged
    }

    /// Ensure final_answer.txt exists when the loop ended without a confirmed answer.
    async fn final_answer_fallback(&mut self, sh: &dyn Shell, instruction: &str) -> Result<()> {
        if self.early_termination_reason.as_deref() == Some("task_complete") || self.desktop.is_none() {
            return Ok(());
        }
        if sh.sh(&format!("test -f {}", quote(&self.io_path(FINAL_ANSWER))), false, Some(Duration::from_secs(10))).await?.code == 0 {
            return Ok(());
        }
        // Native providers have no chat history to ask: write an empty answer.
        let text =
            if self.native.is_some() { String::new() } else { self.extract_text_fallback(sh, instruction).await.unwrap_or_default() };
        self.write_final_answer(sh, &text).await
    }

    async fn extract_text_fallback(&mut self, sh: &dyn Shell, instruction: &str) -> Result<String> {
        let prompt = format!(
            "Based on the prior screenshots and reasoning in the chat history, briefly provide the final answer to this task: {instruction}"
        );
        // Role/content only; empty assistant turns get a placeholder (Anthropic rejects empty text).
        let history: Vec<Value> = self
            .chat
            .messages
            .iter()
            .filter(|m| !m["role"].is_null() && !m["content"].is_null())
            .map(|m| {
                let c = match &m["content"] {
                    Value::String(s) if s.trim().is_empty() => json!("(no text response)"),
                    c => c.clone(),
                };
                json!({"role": m["role"], "content": c})
            })
            .collect();
        let latest = self.latest_screenshot.clone().filter(|_| self.enable_images);
        let r = match latest {
            Some(p) => {
                let url = screenshot_data_url(sh, &p).await?;
                let mut chat = CChat { messages: history, ..Default::default() };
                let turn = vec![
                    json!({"role": "user", "content": [{"type": "text", "text": prompt}, {"type": "image_url", "image_url": {"url": url}}]}),
                ];
                chat.chat(&*self.llm, &self.model_name, &turn, &Map::new()).await?
            }
            None => self.llm.call(&prompt, &history, &Map::new(), None).await?,
        };
        Ok(r.content)
    }

    async fn fresh_prompt_after_compaction(&mut self, sh: &dyn Shell) -> Vec<Value> {
        let text = "Continue from the summary above.";
        if self.desktop.is_none() || !self.enable_images {
            return user(text);
        }
        let shot = async {
            let p = self.capture(sh, &format!("screenshot_postcompaction_{}.webp", self.n_episodes)).await?;
            screenshot_data_url(sh, &p).await
        };
        match shot.await {
            Ok(url) => vec![
                json!({"role": "user", "content": [{"type": "text", "text": format!("{text}\n\nThe current screen state is attached.")}, image_url_part(&url)]}),
            ],
            Err(_) => user(text),
        }
    }

    async fn maybe_proactively_compact(&mut self, sh: &dyn Shell, prompt: &[Value], orig: &str) -> Option<Vec<Value>> {
        if self.chat.messages.is_empty() {
            return None;
        }
        let limit = self.llm.context_limit();
        if limit - count_total_tokens(&*self.llm, &self.chat.messages) >= PROACTIVE_COMPACTION_FREE_TOKENS {
            return None;
        }
        // Stage 1 (cheap): strip old screenshots.
        if trim_old_screenshots(&mut self.chat.messages, KEEP_LAST_SCREENSHOTS) > 0
            && limit - count_total_tokens(&*self.llm, &self.chat.messages) >= PROACTIVE_COMPACTION_FREE_TOKENS
        {
            return None;
        }
        self.perform_compaction(orig, &extract_prompt_text(prompt)).await;
        Some(self.fresh_prompt_after_compaction(sh).await)
    }

    async fn reactive_compaction(&mut self, sh: &dyn Shell, current_prompt: &str, orig: &str) -> Vec<Value> {
        trim_old_screenshots(&mut self.chat.messages, KEEP_LAST_SCREENSHOTS);
        let limit = self.llm.context_limit();
        while self.chat.messages.len() > 3 && limit - count_total_tokens(&*self.llm, &self.chat.messages) < UNWIND_TARGET_FREE_TOKENS {
            self.chat.messages.drain(1..3);
        }
        self.perform_compaction(orig, current_prompt).await;
        self.fresh_prompt_after_compaction(sh).await
    }

    /// Full summary, else a short one, else raw text; then replace the history.
    async fn perform_compaction(&mut self, orig: &str, current_prompt: &str) {
        let summary = self.summary_from_history(orig).await;
        let summary = match summary {
            Some(s) => s,
            None => {
                let p = format!(
                    "Briefly summarize progress on this task: {orig}\n\nCurrent state: {}\n\nProvide a 2-3 sentence summary.",
                    last_n_chars(current_prompt, 1000)
                );
                match self.llm.call(&p, &[], &Map::new(), None).await {
                    Ok(r) => format!("{orig}\n\nSummary: {}", r.content),
                    Err(_) => format!("Task: {orig}\n\nRecent state:\n{}", last_n_chars(current_prompt, 1000)),
                }
            }
        };
        let before = count_total_tokens(&*self.llm, &self.chat.messages);
        let first = self.chat.messages.first().map(text_only_message).unwrap_or_else(|| json!({"role": "user", "content": ""}));
        self.chat.messages = vec![
            first,
            json!({"role": "user", "content": format!("Summary of previous work:\n{summary}")}),
            json!({"role": "assistant", "content": "Understood. I will continue from where the previous work left off."}),
        ];
        let after = count_total_tokens(&*self.llm, &self.chat.messages);
        self.compaction_count += 1;
        let msg = format!("Context compaction #{}: compressed {before} -> {after} tokens", self.compaction_count);
        self.push_step(Map::from_iter([("source".into(), json!("system")), ("message".into(), json!(msg))]));
    }

    async fn summary_from_history(&mut self, orig: &str) -> Option<String> {
        if self.chat.messages.is_empty()
            || count_total_tokens(&*self.llm, &self.chat.messages) > (self.llm.context_limit() as f64 * 0.9) as i64
        {
            return None;
        }
        let p = format!(
            "You are about to hand off work to a continuation of yourself. Provide a compressed narrative covering:\n1. What has been accomplished so far\n2. Key findings and discoveries\n3. Current state of the task\n4. Recommended next steps\n\nOriginal task: {orig}\n\nBe concise but preserve all critical details needed to continue."
        );
        self.llm.call(&p, &self.chat.messages, &Map::new(), None).await.ok().map(|r| r.content)
    }

    /// The client retries transient errors; overflow compacts and recurses (depth <= 2).
    fn query<'a>(
        &'a mut self,
        sh: &'a dyn Shell,
        prompt: Vec<Value>,
        log_dir: Option<PathBuf>,
        orig: &'a str,
        depth: usize,
    ) -> BoxFut<'a, Result<LlmResponse>> {
        Box::pin(async move { self.query_once(sh, &prompt, log_dir, orig, depth).await })
    }

    async fn query_once(
        &mut self,
        sh: &dyn Shell,
        prompt: &[Value],
        log_dir: Option<PathBuf>,
        orig: &str,
        depth: usize,
    ) -> Result<LlmResponse> {
        if let Some(d) = &log_dir {
            let text = match prompt {
                [p] if p["content"].is_string() => p["content"].as_str().unwrap_or_default().to_string(),
                _ => serde_json::to_string(prompt)?,
            };
            std::fs::write(d.join("prompt.txt"), text)?;
        }
        let start = Instant::now();
        match self.chat.chat(&*self.llm, &self.model_name, prompt, &self.opts.llm_call_kwargs).await {
            Ok(r) => {
                self.api_request_times.push(start.elapsed().as_secs_f64() * 1000.0);
                if let Some(d) = &log_dir {
                    std::fs::write(d.join("response.txt"), &r.content)?;
                }
                Ok(r)
            }
            Err(LlmError::ContextLength(m)) => {
                if depth >= MAX_QUERY_RECURSION_DEPTH {
                    self.early_termination_reason = Some("context_overflow".into());
                    return Err(LlmError::ContextLength(m).into());
                }
                let compacted = self.reactive_compaction(sh, &extract_prompt_text(prompt), orig).await;
                self.early_termination_reason = None;
                self.query(sh, compacted, log_dir, orig, depth + 1).await
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn run_loop(&mut self, sh: &dyn Shell, ctx: &mut AgentContext, instruction: &str, initial_shot: &str) -> Result<()> {
        let initial_ref = screenshot_data_url(sh, initial_shot).await?;
        let text = self.prompt_text(instruction);
        self.push_step(Map::from_iter([("source".into(), json!("user")), ("message".into(), json!(text))]));
        self.dump_trajectory();
        let mut prompt = self.turn(&text, &initial_ref, "(no content)");
        for episode in 0..self.opts.max_turns.unwrap_or(1_000_000) {
            self.n_episodes = episode + 1;
            if !self.desktop()?.is_alive(sh).await {
                self.early_termination_reason = Some("runtime_session_dead".into());
                return Ok(());
            }
            let log_dir = self.opts.enable_episode_logging.then(|| self.args.logs_dir.join(format!("episode-{episode}")));
            if let Some(d) = &log_dir {
                std::fs::create_dir_all(d)?;
            }
            let before = (self.chat.total_input_tokens, self.chat.total_output_tokens, self.chat.total_cache_tokens, self.chat.total_cost);
            if let Some(c) = self.maybe_proactively_compact(sh, &prompt, instruction).await {
                prompt = c;
            }
            let resp = self.query(sh, prompt.clone(), log_dir, instruction, 0).await?;
            let metrics = self.metrics(before);
            self.sync_context(ctx);
            let step = parse_step(&resp.content);

            if step.needs_retry {
                let next = format!(
                    "Previous response had parsing errors:\n{}\n\nPlease fix these issues and provide a proper JSON response.",
                    step.feedback
                );
                self.push_step(Map::from_iter([
                    ("source".into(), json!("agent")),
                    ("model_name".into(), json!(resp.model_name.clone().unwrap_or_else(|| self.model_name.clone()))),
                    ("message".into(), json!(resp.content)),
                    ("reasoning_content".into(), json!(resp.reasoning_content)),
                    ("observation".into(), json!({"results": [{"content": next}]})),
                    ("metrics".into(), metrics),
                ]));
                self.dump_trajectory();
                prompt = user(next);
                continue;
            }

            let (obs_text, mut paths, _) = self.execute_action(sh, step.action.as_ref(), episode, None).await?;
            let was_pending = self.pending_completion;
            let is_complete = step.is_terminal || step.action.as_ref().is_some_and(Action::is_terminal);
            let mut observation = if is_complete {
                if was_pending {
                    obs_text
                } else {
                    self.pending_completion = true;
                    format!(
                        "Current state:\n{obs_text}\n\nAre you sure you want to mark the task as complete? This will trigger your solution to be graded and you won't be able to make any further corrections. If so, confirm again with the same final answer."
                    )
                }
            } else {
                self.pending_completion = false;
                if step.feedback.contains("WARNINGS:") {
                    format!("Previous response had warnings:\n{}\n\n{obs_text}", step.feedback)
                } else {
                    limit_output_length(&obs_text, MAX_OBSERVATION_BYTES)
                }
            };
            // Nudge repeated waits.
            if is_complete {
                self.wait_streak = 0;
            } else if step.action.as_ref().is_some_and(|a| a.kind == "wait") {
                self.wait_streak += 1;
                if self.wait_streak > 1 {
                    observation =
                        format!("{observation}\n\nYou have now waited {} turns in a row without taking action.", self.wait_streak);
                }
            } else {
                self.wait_streak = 0;
            }
            self.record_agent_step(episode, &resp, &step, is_complete, &observation, &paths, metrics);
            self.dump_trajectory();

            if is_complete && was_pending {
                let answer = step
                    .action
                    .as_ref()
                    .and_then(|a| a.result.clone().filter(|r| !r.is_empty()).or_else(|| a.text.clone()))
                    .filter(|a| !a.is_empty())
                    .unwrap_or(step.message.clone());
                self.write_final_answer(sh, &answer).await?;
                self.early_termination_reason = Some("task_complete".into());
                return Ok(());
            }
            if paths.is_empty() {
                paths.push(self.capture(sh, &format!("screenshot_ep{episode}_follow.webp")).await?);
            }
            let r = screenshot_data_url(sh, paths.last().unwrap()).await?;
            prompt = self.turn(&observation, &r, "(no new observation)");
        }
        self.early_termination_reason = Some("max_turns_reached".into());
        Ok(())
    }

    async fn run_with(&mut self, instruction: &str, sh: &dyn Shell, ctx: &mut AgentContext) -> Result<()> {
        if self.desktop.is_none() {
            bail!("Session is not set. Call setup() first.");
        }
        self.chat = CChat::default();
        if self.provider != "litellm" && self.native.is_none() {
            let o = &self.opts;
            self.native = Some(native::Native::new(
                &self.provider,
                &self.model_name,
                o.geo.desktop_width,
                o.geo.desktop_height,
                o.aws_region_name.as_deref(),
                o.gemini_auto_ack_safety,
                &|k: &str| self.args.get_env(k),
            )?);
        }
        if let Some(url) = self.opts.start_url.clone() {
            let mut a = Action::new("navigate");
            a.url = Some(url.clone());
            self.desktop()?.execute(sh, &a).await?;
            let id = "call_start_url_navigation";
            self.push_step(Map::from_iter([
                ("source".into(), json!("agent")),
                ("model_name".into(), json!(self.model_name)),
                ("message".into(), json!("")),
                (
                    "tool_calls".into(),
                    json!([{"tool_call_id": id, "function_name": "computer_action", "arguments": {"type": "navigate", "url": url}}]),
                ),
                ("observation".into(), json!({"results": [{"source_call_id": id, "content": format!("Navigated to {url}")}]})),
            ]));
        }
        let initial = self.capture(sh, "screenshot_init.webp").await?;
        let r = match self.native.as_ref().map(|n| n.kind) {
            Some(native::Kind::OpenAi) => self.run_openai_loop(sh, ctx, instruction).await,
            Some(_) => self.run_step_loop(sh, ctx, instruction, &initial).await,
            None => self.run_loop(sh, ctx, instruction, &initial).await,
        };
        let _ = self.final_answer_fallback(sh, instruction).await; // best effort: failures are ignored
        ctx.rollout_details = Some(vec![]);
        self.sync_context(ctx);
        self.dump_trajectory();
        r.map_err(|e| match e.downcast::<LlmError>() {
            Ok(le) => le.into_anyhow(),
            Err(e) => e,
        })
    }
}

/// `_aggregate_step_metrics`: native providers' final metrics are the per-step sums.
fn aggregate_step_metrics(steps: &[Value]) -> Value {
    let mut fm = Map::new();
    for (key, total) in [
        ("prompt_tokens", "total_prompt_tokens"),
        ("completion_tokens", "total_completion_tokens"),
        ("cached_tokens", "total_cached_tokens"),
    ] {
        let vals: Vec<i64> = steps.iter().filter_map(|s| s["metrics"][key].as_i64()).collect();
        if !vals.is_empty() {
            fm.insert(total.into(), json!(vals.iter().sum::<i64>()));
        }
    }
    let cost: f64 = steps.iter().filter_map(|s| s["metrics"]["cost_usd"].as_f64()).sum();
    if cost > 0.0 {
        fm.insert("total_cost_usd".into(), json!(cost));
    }
    Value::Object(fm)
}

/// Stand-in client for native providers, which never query litellm.
struct NoLlm;

#[async_trait::async_trait]
impl Llm for NoLlm {
    async fn call(&self, _: &str, _: &[Value], _: &Map<String, Value>, _: Option<&str>) -> Result<LlmResponse, LlmError> {
        Err(api_err("APIError", "computer-1 native providers don't use the litellm client"))
    }
    fn context_limit(&self) -> i64 {
        1_000_000
    }
    fn output_limit(&self) -> Option<i64> {
        None
    }
}

fn format_bash_observation(r: &Value) -> String {
    let s = |k: &str| match &r[k] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    };
    let or_empty = |t: String| if t.trim_end().is_empty() { "(empty)".to_string() } else { t.trim_end().to_string() };
    let mut parts = vec![
        format!(
            "bash result: status={} exit_code={}",
            r["status"].as_str().unwrap_or("unknown"),
            if r["return_code"].is_null() { "?".into() } else { r["return_code"].to_string() }
        ),
        "--- stdout ---".into(),
        or_empty(s("stdout")),
    ];
    if r["stdout_truncated"] == true {
        parts.push("[stdout truncated]".into());
    }
    parts.extend(["--- stderr ---".into(), or_empty(s("stderr"))]);
    if r["stderr_truncated"] == true {
        parts.push("[stderr truncated]".into());
    }
    parts.join("\n")
}

#[async_trait::async_trait]
impl Agent for Computer1 {
    fn name(&self) -> &str {
        NAME
    }
    fn version(&self) -> Option<String> {
        Some(VERSION.into())
    }
    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        let mut d = self.new_desktop();
        d.start(cx).await?;
        self.desktop = Some(d);
        Ok(())
    }
    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        self.run_with(instruction, cx, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::llm::Usage;
    use crate::env::ExecResult;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[test]
    fn json_dialect_parser() {
        let r = parse_response(
            r#"{"analysis": "see a button", "plan": "click it", "action": {"type": "click", "x": 10.7, "y": "20", "button": "left", "keys": "bad", "zoom_region": [1, 2, 3]}}"#,
        );
        assert!(r.error.is_empty() && !r.is_task_complete);
        let a = r.action.unwrap();
        assert_eq!((a.kind.as_str(), a.x, a.y, a.keys, a.zoom_region), ("click", Some(10), Some(20), None, None));
        assert_eq!(
            r.warning,
            "- Action 'keys' must be a list of strings; ignoring\n- Action 'zoom_region' must be a 4-element list of integers; ignoring"
        );
        let r = parse_response("Sure: {\"action\": {\"type\": \"done\", \"result\": \"42\"}}");
        assert!(r.is_task_complete && r.warning == "- Extra text detected before JSON object");
        assert_eq!(parse_response("{\"analysis\": \"x\"}").error, "Missing required field: action");
        assert_eq!(parse_response("{\"action\": {\"x\": 1}}").error, "Action 'type' is missing or not a string");
        assert_eq!(parse_response("nothing").error, "No valid JSON found in response");
        assert!(parse_response("{\"action\": {\"type\": \"fly\"}}").warning.contains("Unknown action type: 'fly'"));
        let s = parse_step("{\"action\": 3}");
        assert!(s.needs_retry && s.feedback == "ERROR: Field 'action' must be an object");
        // Every ComputerAction field is recorded, unset ones as null.
        let args = serde_json::to_value(Action::new("wait")).unwrap();
        assert_eq!(
            (args["type"].clone(), args["source"].clone(), args["x"].clone()),
            (json!("wait"), json!("native_prescaled"), Value::Null)
        );
        assert!(args.get("metadata").is_none() && args.as_object().unwrap().len() == 23);
    }

    #[test]
    fn xdotool_translation() {
        let g = Geometry { desktop_width: 1024, desktop_height: 900, window_x: 0, window_y: 0, window_width: 1024, window_height: 900 };
        let mut a = Action::new("double_click");
        a.x = Some(5);
        a.y = Some(6);
        a.modifier = Some("Control".into());
        assert_eq!(
            build_xdotool_argv(&a, &g).unwrap(),
            vec![vec!["mousemove", "5", "6", "keydown", "ctrl", "click", "--repeat", "2", "1", "keyup", "ctrl"]]
        );
        let mut a = Action::new("keypress");
        a.keys = Some(vec!["ctrl".into(), "L".into()]);
        assert_eq!(build_xdotool_argv(&a, &g).unwrap(), vec![vec!["key", "--clearmodifiers", "ctrl+L"]]);
        a.keys = Some(vec!["Enter".into(), "a".into()]);
        assert_eq!(
            build_xdotool_argv(&a, &g).unwrap(),
            vec![vec!["key", "--clearmodifiers", "Return"], vec!["key", "--clearmodifiers", "a"]]
        );
        let mut a = Action::new("scroll");
        a.scroll_y = Some(-250);
        a.scroll_x = Some(50);
        assert_eq!(
            build_xdotool_argv(&a, &g).unwrap(),
            vec![vec!["mousemove", "512", "450", "click", "--repeat", "2", "4", "click", "--repeat", "1", "7"]]
        );
        assert!(build_xdotool_argv(&Action::new("navigate"), &g).is_none());
        assert_eq!(xdotool_command(&["type".into(), "--".into(), "it's".into()]), "DISPLAY=:1 xdotool type -- 'it'\\''s'");
        assert!(is_blocked_key_combo(Some(&vec!["Ctrl".into(), "U".into()])));
    }

    #[test]
    fn provider_resolution_and_options() {
        assert_eq!(resolve_provider_name("openai/gpt-5.4", None).unwrap(), "litellm");
        assert_eq!(resolve_provider_name("anthropic/claude-sonnet-4-5", None).unwrap(), "anthropic");
        assert!(resolve_provider_name("anthropic/claude-3-haiku", None).unwrap_err().to_string().contains("not a computer-use model"));
        assert!(resolve_provider_name("x/y", Some("weird")).unwrap_err().to_string().contains("Unknown computer-1 provider 'weird'"));
        assert_eq!(resolve_provider_name("anthropic/claude-sonnet-4-5", Some("litellm")).unwrap(), "litellm");
        assert!(is_computer_use_model("openai/gpt-6-astra") && !is_computer_use_model("openai/gpt-5.2"));
        assert_eq!(resolve_temperature("anthropic/claude-opus-4-7", 0.7), None);
        assert_eq!(resolve_temperature("openai/gpt-5", 0.7), None);
        assert_eq!(resolve_temperature("openai/o3", 0.7), None);
        assert_eq!(resolve_temperature("openrouter/qwen/qwen3-vl", 0.7), Some(0.7));
        assert_eq!(
            (supports_vision("openai/gpt-4o"), supports_vision("openai/gpt-3.5-turbo"), supports_vision("my/unknown")),
            (Some(true), None, None)
        );
        let o = Options::parse(
            &serde_json::from_value(json!({"desktop_width": 1280, "extra_tools": ["bash"], "enable_images": false})).unwrap(),
        )
        .unwrap();
        assert_eq!((o.geo.desktop_width, o.extra_tools.clone(), o.enable_images), (1280, vec!["bash".to_string()], Some(false)));
    }

    #[test]
    fn compaction_helpers() {
        let img = |t: &str| json!({"role": "user", "content": [{"type": "text", "text": t}, {"type": "image_url", "image_url": {"url": "data:image/webp;base64,AA"}}]});
        let mut msgs = vec![
            img("a"),
            json!({"role": "assistant", "content": "x"}),
            img("b"),
            img("c"),
            img("d"),
            json!({"role": "user", "content": [{"type": "image_url", "image_url": {"url": "u"}}]}),
        ];
        assert_eq!(trim_old_screenshots(&mut msgs, 3), 2);
        assert_eq!(msgs[0]["content"], json!([{"type": "text", "text": "a"}]));
        assert_eq!(msgs[2]["content"], json!([{"type": "text", "text": "b"}]));
        assert_eq!(msgs[5]["content"][0]["type"], "image_url");
        assert_eq!(extract_prompt_text(&[img("hello"), json!({"role": "user", "content": "plain"})]), "hello\nplain");
        assert_eq!(text_only_message(&msgs[5])["content"], json!([{"type": "text", "text": ""}]));
        let bash = json!({"status": "error", "return_code": 1, "stdout": "out\n", "stderr": "", "stdout_truncated": true});
        assert_eq!(
            format_bash_observation(&bash),
            "bash result: status=error exit_code=1\n--- stdout ---\nout\n[stdout truncated]\n--- stderr ---\n(empty)"
        );
        assert_eq!(
            py_repr(&json!({"status": "recovered", "recovered": true, "x": null})),
            "{'status': 'recovered', 'recovered': True, 'x': None}"
        );
    }

    #[derive(Default)]
    pub(super) struct MockShell {
        pub(super) log: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Shell for MockShell {
        async fn sh(&self, cmd: &str, _root: bool, _t: Option<Duration>) -> Result<ExecResult> {
            self.log.lock().unwrap().push(cmd.to_string());
            let out = if cmd.contains("pgrep -f chromium") && cmd.contains("X11") {
                "ok"
            } else if cmd.starts_with("base64 -w0") {
                "QUJD"
            } else if cmd.starts_with("test -f") {
                return Ok(ExecResult { code: 1, stdout: String::new(), stderr: String::new() });
            } else {
                ""
            };
            Ok(ExecResult { code: 0, stdout: out.into(), stderr: String::new() })
        }
    }

    #[derive(Clone, Default)]
    pub(super) struct MockLlm {
        replies: Arc<Mutex<VecDeque<Result<LlmResponse, LlmError>>>>,
        seen: Arc<Mutex<Vec<Vec<Value>>>>,
    }

    #[async_trait::async_trait]
    impl Llm for MockLlm {
        async fn call(&self, prompt: &str, history: &[Value], _: &Map<String, Value>, _: Option<&str>) -> Result<LlmResponse, LlmError> {
            let mut m = history.to_vec();
            m.extend(user(prompt));
            self.complete(m, &Map::new()).await
        }
        async fn complete(&self, messages: Vec<Value>, _: &Map<String, Value>) -> Result<LlmResponse, LlmError> {
            self.seen.lock().unwrap().push(messages);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(LlmResponse { content: "fallback answer".into(), ..Default::default() }))
        }
        fn context_limit(&self) -> i64 {
            1_000_000
        }
        fn output_limit(&self) -> Option<i64> {
            None
        }
    }

    fn reply(c: &str) -> Result<LlmResponse, LlmError> {
        Ok(LlmResponse {
            content: c.into(),
            model_name: Some("m-1".into()),
            usage: Some(Usage { prompt_tokens: 50, completion_tokens: 5, cache_tokens: 0, cost_usd: 0.0 }),
            ..Default::default()
        })
    }

    pub(super) fn agent(kwargs: Value, llm: &MockLlm) -> (Computer1, PathBuf) {
        let root = std::env::temp_dir().join(format!("hbench-c1-{}", uuid::Uuid::new_v4()));
        let task_dir = root.join("task");
        std::fs::create_dir_all(task_dir.join("environment")).unwrap();
        std::fs::write(task_dir.join("task.toml"), "[environment]\ndocker_image='x'\n").unwrap();
        std::fs::write(task_dir.join("instruction.md"), "x").unwrap();
        let logs = root.join("trial/agent");
        std::fs::create_dir_all(&logs).unwrap();
        let args = AgentArgs {
            load_trajectory: None,
            model_name: Some("openai/gpt-5.4".into()),
            kwargs: serde_json::from_value(kwargs).unwrap(),
            extra_env: BTreeMap::new(),
            mcp_servers: vec![],
            skills: vec![],
            logs_dir: logs.clone(),
            task: Arc::new(crate::task::Task::load(&task_dir).unwrap()),
        };
        let opts = Options::parse(&args.kwargs).unwrap();
        let mut c = Computer1::with_llm(args, opts, "openai/gpt-5.4".into(), Box::new(llm.clone()), true, false, "sess".into());
        c.desktop = Some(c.new_desktop());
        (c, logs)
    }

    #[tokio::test]
    async fn loop_clicks_then_confirms_answer() {
        let llm = MockLlm::default();
        *llm.replies.lock().unwrap() = VecDeque::from(vec![
            reply(r#"{"analysis": "a", "plan": "p", "action": {"type": "click", "x": 3, "y": 4}}"#),
            reply("not json"),
            reply(r#"{"action": {"type": "answer", "result": "42"}}"#),
            reply(r#"{"action": {"type": "answer", "result": "42"}}"#),
        ]);
        let (mut c, logs) = agent(json!({}), &llm);
        let sh = MockShell::default();
        let mut ctx = AgentContext::default();
        c.run_with("Find the answer", &sh, &mut ctx).await.unwrap();

        let cmds = sh.log.lock().unwrap().clone();
        assert!(cmds.contains(&"DISPLAY=:1 xdotool mousemove 3 4 click 1".to_string()));
        let enc = base64::engine::general_purpose::STANDARD.encode("42");
        assert!(cmds.iter().any(|c| c == &format!("mkdir -p /logs/agent && printf '%s' {enc} | base64 -d > /logs/agent/final_answer.txt")));
        assert_eq!(c.early_termination_reason.as_deref(), Some("task_complete"));
        // First request: template text + screenshot; follow-ups carry the observation + screenshot.
        let seen = llm.seen.lock().unwrap().clone();
        assert!(seen[0][0]["content"][0]["text"].as_str().unwrap().contains("Task instructions:\nFind the answer"));
        assert_eq!(seen[0][0]["content"][1]["image_url"]["url"], "data:image/webp;base64,QUJD");
        assert!(
            seen[2].last().unwrap()["content"]
                .as_str()
                .unwrap()
                .starts_with("Previous response had parsing errors:\nERROR: No valid JSON found in response")
        );
        assert!(seen[3].last().unwrap()["content"][0]["text"].as_str().unwrap().contains("confirm again with the same final answer"));
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens), (Some(200), Some(20)));
        assert_eq!(ctx.metadata.as_ref().unwrap()["early_termination_reason"], "task_complete");
        assert!(
            logs.join("episode-0/prompt.txt").exists()
                && std::fs::read_to_string(logs.join("episode-1/response.txt")).unwrap() == "not json"
        );

        let t: Value = serde_json::from_str(&std::fs::read_to_string(logs.join("trajectory.json")).unwrap()).unwrap();
        let steps = t["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 5);
        assert_eq!(steps[1]["tool_calls"][0]["arguments"]["x"], 3);
        assert_eq!(
            steps[1]["observation"]["results"][0]["content"][1],
            json!({"type": "image", "source": {"media_type": "image/webp", "path": "screenshot_ep0.webp"}})
        );
        assert_eq!(
            steps[2]["observation"]["results"][0]["content"].as_str().unwrap().lines().next(),
            Some("Previous response had parsing errors:")
        );
        assert_eq!(
            steps[4]["tool_calls"][1],
            json!({"tool_call_id": "call_3_task_complete", "function_name": "mark_task_complete", "arguments": {"result": "42"}})
        );
        assert_eq!(t["extra"]["early_termination_reason"], "task_complete");
    }

    #[tokio::test]
    async fn max_turns_writes_fallback_answer_and_overflow_compacts() {
        let llm = MockLlm::default();
        *llm.replies.lock().unwrap() = VecDeque::from(vec![
            reply(r#"{"action": {"type": "wait"}}"#),
            Err(LlmError::ContextLength("prompt is too long".into())),
            reply("SUMMARY"),
            reply(r#"{"action": {"type": "wait"}}"#),
        ]);
        let (mut c, _) = agent(json!({"max_turns": 2, "enable_episode_logging": false}), &llm);
        let sh = MockShell::default();
        let mut ctx = AgentContext::default();
        c.run_with("t", &sh, &mut ctx).await.unwrap();
        assert_eq!(c.early_termination_reason.as_deref(), Some("max_turns_reached"));
        assert_eq!(c.compaction_count, 1);
        let seen = llm.seen.lock().unwrap().clone();
        assert!(
            seen[2].last().unwrap()["content"]
                .as_str()
                .unwrap()
                .starts_with("You are about to hand off work to a continuation of yourself.")
        );
        assert_eq!(seen[3][1]["content"], "Summary of previous work:\nSUMMARY");
        assert!(seen[3].last().unwrap()["content"][0]["text"].as_str().unwrap().starts_with("Continue from the summary above."));
        // No confirmed answer: the fallback asks the model and writes its reply.
        let enc = base64::engine::general_purpose::STANDARD.encode("fallback answer");
        assert!(sh.log.lock().unwrap().iter().any(|c| c.contains(&enc)));
        assert_eq!(ctx.metadata.as_ref().unwrap()["compaction_count"], 1);
    }

    #[tokio::test]
    async fn desktop_execute_guards_and_bash() {
        let sh = MockShell::default();
        let (c, _) = agent(json!({}), &MockLlm::default());
        let mut d = c.new_desktop();
        let mut a = Action::new("keypress");
        a.keys = Some(vec!["ctrl".into(), "u".into()]);
        assert!(d.execute(&sh, &a).await.unwrap_err().to_string().contains("(403): Action blocked: developer tools"));
        let mut b = Action::new("bash");
        b.command = Some("ls".into());
        assert_eq!(d.execute(&sh, &b).await.unwrap()["return_code"], 2); // disabled by default
        d.enable_bash = true;
        assert_eq!(d.execute(&sh, &b).await.unwrap()["status"], "ok");
        let mut z = Action::new("zoom");
        z.zoom_region = Some(vec![10, 20, 110, 70]);
        d.execute(&sh, &z).await.unwrap();
        let s = d.screenshot_script("/logs/agent/shot.png");
        assert!(s.contains("-crop 100x50+10+20 +repage") && s.ends_with("cp /tmp/computer_1-screenshots/latest.png /logs/agent/shot.png"));
        assert!(!d.screenshot_script("/logs/agent/x.webp").contains("-crop")); // one-shot
    }
}
