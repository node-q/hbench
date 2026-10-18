//! Terminus-2: the reference LLM agent. It runs
//! host-side, drives a tmux session inside the container through `docker compose exec`, and talks
//! to the model through `llm.rs`. Writes an ATIF `trajectory.json` (plus summarization subagent
//! trajectories) to the agent logs dir; the pane log and asciinema recording land in /logs/agent.

use super::llm::{Chat, ChatClient, Llm, LlmConfig, LlmError, LlmResponse, content_text};
use super::{Agent, AgentArgs, AgentContext, AgentCtx, ModelUsage};
use crate::agents::installed::{APT_INSTALL_FN, pins};
use crate::env::{ExecResult, quote};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

const JSON_TEMPLATE: &str = include_str!("../../assets/agents/terminus2/terminus-json-plain.txt");
const XML_TEMPLATE: &str = include_str!("../../assets/agents/terminus2/terminus-xml-plain.txt");
const TIMEOUT_TEMPLATE: &str = include_str!("../../assets/agents/terminus2/timeout.txt");
const NAME: &str = "terminus-2";
const VERSION: &str = "2.0.0";

mod tmux;
pub use tmux::{Shell, Tmux};

/// Re-asks after a truncated (output-length) response before giving up.
const MAX_OUTPUT_LENGTH_RETRIES: usize = 3;

/// Python `str.format` with named fields (`{{` / `}}` escapes).
pub(crate) fn fill_template(tpl: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(tpl.len());
    let mut it = tpl.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '{' | '}' if it.peek() == Some(&c) => {
                it.next();
                out.push(c);
            }
            '{' => {
                let name: String = it.by_ref().take_while(|c| *c != '}').collect();
                out.push_str(vars.iter().find(|(k, _)| *k == name).map(|(_, v)| *v).unwrap_or(""));
            }
            c => out.push(c),
        }
    }
    out
}

/// Python float `str()`.
pub(crate) fn pyf(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 { format!("{f:.1}") } else { format!("{f}") }
}

/// Python `repr()` of a str.
pub(crate) fn quote_repr(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::from(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn py_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.into(),
        Some(v) => v.to_string(),
    }
}

/// Python `s[-n:]`.
fn last_chars(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if count <= n {
        return s;
    }
    &s[s.char_indices().nth(count - n).map(|(i, _)| i).unwrap_or(0)..]
}

fn re(p: &str) -> Regex {
    Regex::new(p).expect("static regex")
}

pub(crate) fn now() -> Option<String> {
    Some(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string())
}

/// Keep the first and last `max_bytes/2` bytes.
pub(crate) fn limit_output_length(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_string();
    }
    let half = max_bytes / 2;
    let mut a = half;
    while !output.is_char_boundary(a) {
        a -= 1;
    }
    let mut b = output.len() - half;
    while !output.is_char_boundary(b) {
        b += 1;
    }
    let omitted = output.len() - a - (output.len() - b);
    format!("{}\n[... output limited to {max_bytes} bytes; {omitted} interior bytes omitted ...]\n{}", &output[..a], &output[b..])
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCommand {
    pub keystrokes: String,
    pub duration: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ParseResult {
    pub commands: Vec<ParsedCommand>,
    pub is_task_complete: bool,
    pub error: String,
    pub warning: String,
    pub analysis: String,
    pub plan: String,
}

fn warn_join(w: &[String]) -> String {
    if w.is_empty() { String::new() } else { format!("- {}", w.join("\n- ")) }
}

fn fail(error: impl Into<String>, w: &[String], analysis: String, plan: String) -> ParseResult {
    ParseResult { error: error.into(), warning: warn_join(w), analysis, plan, ..Default::default() }
}

fn auto_corrected(fix: &str, existing: &str) -> String {
    let auto = format!("AUTO-CORRECTED: {fix} - please fix this in future responses");
    if existing.is_empty() { format!("- {auto}") } else { format!("- {auto}\n{existing}") }
}

fn newline_warning(i: usize) -> String {
    format!(
        "Command {i} should end with newline when followed by another command. Otherwise the two commands will be concatenated together on the same line."
    )
}

/// Warn when sections/fields appear out of `analysis -> plan -> commands` order.
fn check_order(positions: Vec<(&str, usize)>, what: &str, w: &mut Vec<String>) {
    if positions.len() < 2 {
        return;
    }
    let expected: Vec<&str> = positions.iter().map(|(n, _)| *n).collect();
    let mut sorted = positions.clone();
    sorted.sort_by_key(|(_, p)| *p);
    let actual: Vec<&str> = sorted.iter().map(|(n, _)| *n).collect();
    if actual != expected {
        w.push(format!("{what} appear in wrong order. Found: {}, expected: {}", actual.join(" → "), expected.join(" → ")));
    }
}

pub mod json_parser {
    use super::*;

    pub fn parse_response(response: &str) -> ParseResult {
        let result = try_parse(response);
        if result.error.is_empty() {
            return result;
        }
        /// `fix(response, parse_error)` returns corrected JSON text, if it can.
        type JsonFix = fn(&str, &str) -> Option<String>;
        let fixes: [(&str, JsonFix); 2] = [
            ("Fixed incomplete JSON by adding missing closing brace", fix_incomplete_json),
            ("Extracted JSON from mixed content", fix_mixed_content),
        ];
        for (name, fix) in fixes {
            if let Some(corrected) = fix(response, &result.error) {
                let mut r = try_parse(&corrected);
                if r.error.is_empty() {
                    r.warning = auto_corrected(name, &r.warning);
                    return r;
                }
            }
        }
        result
    }

    fn try_parse(response: &str) -> ParseResult {
        let mut w = vec![];
        let (content, extra) = extract_json_content(response);
        w.extend(extra);
        if content.is_empty() {
            return fail("No valid JSON found in response", &w, String::new(), String::new());
        }
        let data: Value = match serde_json::from_str(content) {
            Ok(v) => v,
            Err(e) => {
                let mut msg = format!("Invalid JSON: {e}");
                if content.chars().count() < 200 {
                    msg += &format!(" | Content: {}", quote_repr(content));
                } else {
                    msg += &format!(" | Content preview: {}...", quote_repr(&content.chars().take(100).collect::<String>()));
                }
                return fail(msg, &w, String::new(), String::new());
            }
        };
        if let Some(e) = validate(&data, content, &mut w) {
            return fail(e, &w, String::new(), String::new());
        }
        let o = data.as_object().expect("validated");
        let is_complete = match o.get("task_complete") {
            Some(Value::String(s)) => matches!(s.to_lowercase().as_str(), "true" | "1" | "yes"),
            Some(v) => py_truthy(v),
            None => false,
        };
        let (analysis, plan) = (py_str(o.get("analysis")), py_str(o.get("plan")));
        let empty = vec![];
        match parse_commands(o["commands"].as_array().unwrap_or(&empty), &mut w) {
            Err(e) if is_complete => {
                w.push(e);
                ParseResult { is_task_complete: true, warning: warn_join(&w), analysis, plan, ..Default::default() }
            }
            Err(e) => fail(e, &w, analysis, plan),
            Ok(commands) => {
                ParseResult { commands, is_task_complete: is_complete, error: String::new(), warning: warn_join(&w), analysis, plan }
            }
        }
    }

    fn extract_json_content(response: &str) -> (&str, Vec<String>) {
        let (mut start, mut end) = (None, None);
        let (mut depth, mut in_string, mut escape_next) = (0i64, false, false);
        for (i, c) in response.char_indices() {
            if escape_next {
                escape_next = false;
                continue;
            }
            match c {
                '\\' => escape_next = true,
                '"' => in_string = !in_string,
                '{' if !in_string => {
                    if depth == 0 {
                        start = Some(i);
                    }
                    depth += 1;
                }
                '}' if !in_string => {
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

    fn validate(data: &Value, content: &str, w: &mut Vec<String>) -> Option<String> {
        let Some(o) = data.as_object() else { return Some("Response must be a JSON object".into()) };
        let missing: Vec<&str> = ["analysis", "plan", "commands"].into_iter().filter(|f| !o.contains_key(*f)).collect();
        if !missing.is_empty() {
            return Some(format!("Missing required fields: {}", missing.join(", ")));
        }
        for f in ["analysis", "plan"] {
            if !o[f].is_string() {
                w.push(format!("Field '{f}' should be a string"));
            }
        }
        if !o["commands"].is_array() {
            return Some("Field 'commands' must be an array".into());
        }
        let positions = ["analysis", "plan", "commands"]
            .into_iter()
            .filter_map(|f| re(&format!(r#""({f})"\s*:"#)).find(content).map(|m| (f, m.start())))
            .collect();
        check_order(positions, "Fields", w);
        if o.get("task_complete").is_some_and(|t| !(t.is_null() || t.is_boolean() || t.is_string())) {
            w.push("Field 'task_complete' should be a boolean or string".into());
        }
        None
    }

    fn parse_commands(data: &[Value], w: &mut Vec<String>) -> Result<Vec<ParsedCommand>, String> {
        let mut out = vec![];
        for (i, c) in data.iter().enumerate() {
            let n = i + 1;
            let Some(o) = c.as_object() else { return Err(format!("Command {n} must be an object")) };
            let Some(k) = o.get("keystrokes") else { return Err(format!("Command {n} missing required 'keystrokes' field")) };
            let Some(k) = k.as_str() else { return Err(format!("Command {n} 'keystrokes' must be a string")) };
            let duration = match o.get("duration") {
                Some(Value::Number(d)) => d.as_f64().unwrap_or(1.0),
                Some(Value::Bool(b)) => f64::from(u8::from(*b)), // Python bools are ints
                Some(_) => {
                    w.push(format!("Command {n}: Invalid duration value, using default 1.0"));
                    1.0
                }
                None => {
                    w.push(format!("Command {n}: Missing duration field, using default 1.0"));
                    1.0
                }
            };
            let unknown: Vec<&str> = o.keys().map(String::as_str).filter(|f| *f != "keystrokes" && *f != "duration").collect();
            if !unknown.is_empty() {
                w.push(format!("Command {n}: Unknown fields: {}", unknown.join(", ")));
            }
            if i + 1 < data.len() && !k.is_empty() && !k.ends_with('\n') {
                w.push(newline_warning(n));
            }
            out.push(ParsedCommand { keystrokes: k.to_string(), duration });
        }
        Ok(out)
    }

    fn fix_incomplete_json(response: &str, error: &str) -> Option<String> {
        if !["Invalid JSON", "Expecting", "Unterminated", "No valid JSON found"].iter().any(|p| error.contains(p)) {
            return None;
        }
        let missing = response.matches('{').count() as i64 - response.matches('}').count() as i64;
        (missing > 0).then(|| format!("{response}{}", "}".repeat(missing as usize)))
    }

    fn fix_mixed_content(response: &str, _: &str) -> Option<String> {
        re(r"\{[^{}]*(?:\{[^{}]*\}[^{}]*)*\}")
            .find_iter(response)
            .map(|m| m.as_str())
            .find(|m| serde_json::from_str::<Value>(m).is_ok())
            .map(String::from)
    }
}

pub mod xml_parser {
    use super::*;

    const SECTIONS: [&str; 4] = ["analysis", "plan", "commands", "task_complete"];

    // An auto-fix appending `</response>` would key on an error `_try_parse` never
    // emits ("Missing </response> closing tag"), so it can't fire and is omitted.
    pub fn parse_response(response: &str) -> ParseResult {
        let mut w = vec![];
        check_extra_text(response, &mut w);
        let content = extract_response_content(response);
        if content.is_empty() {
            return fail("No <response> tag found", &w, String::new(), String::new());
        }
        let is_complete = re(r"(?i)<task_complete>\s*true\s*</task_complete>").is_match(content);
        let sections = extract_sections(content, &mut w);
        let analysis = sections.get("analysis").cloned().unwrap_or_default();
        let plan = sections.get("plan").cloned().unwrap_or_default();
        let ok = |commands, w: &[String], analysis, plan| ParseResult {
            commands,
            is_task_complete: is_complete,
            warning: warn_join(w),
            analysis,
            plan,
            ..Default::default()
        };
        match sections.get("commands") {
            Some(c) if !c.is_empty() => {
                let commands = parse_xml_commands(c, &mut w);
                ok(commands, &w, analysis, plan)
            }
            Some(_) => {
                if !is_complete {
                    w.push(
                        "Commands section is empty; not taking any action. If you want to wait a specific amount of time please use `sleep`, but if you're waiting for a command to finish then continue to wait."
                            .into(),
                    );
                }
                ok(vec![], &w, analysis, plan)
            }
            None if is_complete => ok(vec![], &w, analysis, plan),
            None => fail("Missing <commands> section", &w, analysis, plan),
        }
    }

    fn check_extra_text(response: &str, w: &mut Vec<String>) {
        let Some(start) = response.find("<response>") else { return };
        if !response[..start].trim().is_empty() {
            w.push("Extra text detected before <response> tag".into());
        }
        if let Some(end) = response.find("</response>")
            && !response[end + "</response>".len()..].trim().is_empty()
        {
            w.push("Extra text detected after </response> tag".into());
            let total = response.matches("<response>").count();
            if total > 1 {
                w.push(format!(
                    "IMPORTANT: Only issue one <response> block at a time. You issued {total} and only the first was executed."
                ));
            }
        }
    }

    fn extract_response_content(response: &str) -> &str {
        let Some(start) = response.find("<response>") else { return "" };
        let body = &response[start + "<response>".len()..];
        match body.find("</response>") {
            Some(e) => body[..e].trim(),
            None => body.trim(),
        }
    }

    fn extract_sections(content: &str, w: &mut Vec<String>) -> BTreeMap<&'static str, String> {
        let mut s = BTreeMap::new();
        for name in SECTIONS {
            if let Some(c) = re(&format!(r"(?s)<{name}>(.*?)</{name}>")).captures(content) {
                s.insert(name, c[1].trim().to_string());
            } else if re(&format!(r"<{name}\s*/>")).is_match(content) {
                s.insert(name, String::new());
            }
        }
        for name in &SECTIONS[..3] {
            if !s.contains_key(name) {
                w.push(format!("Missing <{name}> section"));
            }
        }
        let mut seen: Vec<String> = vec![];
        for tag in find_top_level_tags(content) {
            if !SECTIONS.contains(&tag.as_str()) && !seen.contains(&tag) {
                w.push(format!("Unknown tag found: <{tag}>, expected analysis/plan/commands/task_complete"));
                seen.push(tag);
            }
        }
        for name in SECTIONS {
            let count = re(&format!(r"<{name}(?:\s|>|/>)")).find_iter(content).count();
            if count > 1 {
                w.push(if name == "commands" {
                    format!("IMPORTANT: Only issue one <commands> block at a time. You issued {count} and only the first was executed.")
                } else {
                    format!("Multiple <{name}> sections found")
                });
            }
        }
        let positions =
            SECTIONS[..3].iter().filter_map(|n| re(&format!(r"<{n}(?:\s|>|/>)")).find(content).map(|m| (*n, m.start()))).collect();
        check_order(positions, "Sections", w);
        s
    }

    fn parse_xml_commands(xml: &str, w: &mut Vec<String>) -> Vec<ParsedCommand> {
        let matches: Vec<(String, String)> =
            re(r"(?s)<keystrokes([^>]*)>(.*?)</keystrokes>").captures_iter(xml).map(|c| (c[1].to_string(), c[2].to_string())).collect();
        let duration_re = re(r#"duration\s*=\s*["']([^"']*)["']"#);
        let mut out = vec![];
        for (i, (attrs, keys)) in matches.iter().enumerate() {
            let n = i + 1;
            check_attribute_issues(attrs, n, w);
            let mut duration = 1.0;
            match duration_re.captures(attrs) {
                Some(m) => match m[1].trim().parse::<f64>() {
                    Ok(d) => duration = d,
                    Err(_) => w.push(format!("Command {n}: Invalid duration value '{}', using default 1.0", &m[1])),
                },
                None => w.push(format!("Command {n}: Missing duration attribute, using default 1.0")),
            }
            if i + 1 < matches.len() && !keys.ends_with('\n') {
                w.push(newline_warning(n));
            }
            out.push(ParsedCommand { keystrokes: keys.clone(), duration });
        }
        for (entity, ch) in [("&lt;", "<"), ("&gt;", ">"), ("&amp;", "&"), ("&quot;", "\""), ("&apos;", "'")] {
            if xml.contains(entity) {
                w.push(format!(
                    "Warning: {entity} is read verbatim and not converted to {ch}. NEVER USE {entity}, unless you want these exact characters to appear directly in the output."
                ));
            }
        }
        if xml.contains("\\r\\n") {
            w.push("Warning: \\r\\n line endings are not necessary - use \\n instead for simpler output".into());
        }
        out
    }

    fn check_attribute_issues(attrs: &str, n: usize, w: &mut Vec<String>) {
        for c in re(r#"(\w+)\s*=\s*([^"'\s>]+)"#).captures_iter(attrs) {
            w.push(format!("Command {n}: Attribute '{}' value should be quoted: {}=\"{}\"", &c[1], &c[1], &c[2]));
        }
        for c in re(r"(\w+)\s*=\s*'([^']*)'").captures_iter(attrs) {
            w.push(format!("Command {n}: Use double quotes for attribute '{}': {}=\"{}\"", &c[1], &c[1], &c[2]));
        }
        for c in re(r"(\w+)\s*=").captures_iter(attrs) {
            if &c[1] != "duration" {
                w.push(format!("Command {n}: Unknown attribute '{}' - known attributes are: duration", &c[1]));
            }
        }
    }

    fn find_top_level_tags(content: &str) -> Vec<String> {
        let (mut tags, mut depth, mut i) = (vec![], 0i64, 0);
        let b = content.as_bytes();
        while i < b.len() {
            if b[i] != b'<' {
                i += 1;
                continue;
            }
            let Some(rel) = content[i..].find('>') else { break };
            let end = i + rel;
            let tc = &content[i + 1..end];
            i = end + 1;
            if tc.starts_with('!') || tc.starts_with('?') {
                continue;
            }
            if tc.starts_with('/') {
                depth -= 1;
                continue;
            }
            let name = if tc.contains(' ') { tc.split_whitespace().next().unwrap_or("") } else { tc };
            if depth == 0 {
                tags.push(name.strip_suffix('/').unwrap_or(name).to_string());
            }
            if !tc.ends_with('/') {
                depth += 1;
            }
        }
        tags
    }

    /// (clean response up to `</response>` if it parses cleanly, has-multiple-blocks).
    pub fn salvage_truncated_response(t: &str) -> (Option<String>, bool) {
        let Some(ce) = t.find("</commands>") else { return (None, false) };
        let Some(end) = t[ce..].find("</response>").map(|x| x + ce + "</response>".len()) else { return (None, false) };
        let clean = &t[..end];
        let r = parse_response(clean);
        let l = r.warning.to_lowercase();
        let multi = l.contains("only issue one") && l.contains("block at a time");
        if r.error.is_empty() && !multi { (Some(clean.to_string()), false) } else { (None, multi) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParserKind {
    Json,
    Xml,
}

impl ParserKind {
    fn from_name(n: &str) -> Result<ParserKind> {
        match n {
            "json" => Ok(ParserKind::Json),
            "xml" => Ok(ParserKind::Xml),
            _ => bail!("Unknown parser_name: {n}. Use 'json' or 'xml'."),
        }
    }
    fn name(self) -> &'static str {
        if self == ParserKind::Json { "json" } else { "xml" }
    }
    pub fn parse(self, response: &str) -> ParseResult {
        match self {
            ParserKind::Json => json_parser::parse_response(response),
            ParserKind::Xml => xml_parser::parse_response(response),
        }
    }
    fn template(self) -> &'static str {
        if self == ParserKind::Json { JSON_TEMPLATE } else { XML_TEMPLATE }
    }
    fn response_type(self) -> &'static str {
        if self == ParserKind::Json { "JSON response" } else { "response" }
    }
    fn completion_confirmation(self, terminal_output: &str) -> String {
        let again = match self {
            ParserKind::Json => "include \"task_complete\": true in your JSON response again.",
            ParserKind::Xml => "include <task_complete>true</task_complete> again.",
        };
        format!(
            "Current terminal state:\n{terminal_output}\n\nAre you sure you want to mark the task as complete? This will trigger your solution to be graded and you won't be able to make any further corrections. If so, {again}"
        )
    }
}

#[derive(Serialize, Clone, Debug, Default)]
struct Step {
    step_id: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<String>,
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_name: Option<String>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observation: Option<Observation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics: Option<Metrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_copied_context: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra: Option<Value>,
}

impl Step {
    fn new(step_id: usize, source: &'static str, message: impl Into<String>) -> Step {
        Step { step_id, timestamp: now(), source, message: message.into(), ..Default::default() }
    }
}

#[derive(Serialize, Clone, Debug)]
struct ToolCall {
    tool_call_id: String,
    function_name: &'static str,
    arguments: Value,
}

#[derive(Serialize, Clone, Debug, Default)]
struct Observation {
    results: Vec<ObsResult>,
}

#[derive(Serialize, Clone, Debug, Default)]
struct ObsResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    source_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subagent_trajectory_ref: Option<Vec<SubagentRef>>,
}

#[derive(Serialize, Clone, Debug)]
struct SubagentRef {
    session_id: String,
    trajectory_path: String,
    extra: Value,
}

#[derive(Serialize, Clone, Debug, Default)]
struct Metrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completion_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_token_ids: Option<Vec<i64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completion_token_ids: Option<Vec<i64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    logprobs: Option<Vec<f64>>,
}

#[derive(Serialize)]
struct Trajectory<'a> {
    schema_version: &'static str,
    session_id: &'a str,
    agent: Value,
    steps: &'a [Step],
    final_metrics: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    continued_trajectory_ref: Option<String>,
}

fn write_trajectory(path: &std::path::Path, t: &Trajectory) {
    // A failed dump is logged and ignored.
    let _ = std::fs::write(path, serde_json::to_string_pretty(t).unwrap_or_default());
}

fn final_metrics(prompt: i64, completion: i64, cached: i64, cost: f64) -> Value {
    let mut m = json!({"total_prompt_tokens": prompt, "total_completion_tokens": completion, "total_cached_tokens": cached});
    if cost > 0.0 {
        m["total_cost_usd"] = json!(cost);
    }
    m
}

#[derive(Debug, Clone)]
struct Command {
    keystrokes: String,
    duration_sec: f64,
}

#[derive(Debug, Clone, Default)]
struct SubagentMetrics {
    prompt: i64,
    completion: i64,
    cached: i64,
    cost: f64,
}

/// Terminus2Options (kwargs).
#[derive(Debug, Clone)]
struct Options {
    max_episodes: Option<i64>,
    parser: ParserKind,
    api_base: Option<String>,
    temperature: Option<f64>,
    reasoning_effort: Option<String>,
    collect_rollout_details: bool,
    session_id: Option<String>,
    enable_summarize: bool,
    proactive_summarization_threshold: i64,
    max_thinking_tokens: Option<i64>,
    model_info: Option<Value>,
    raw_content: bool,
    linear_history: bool,
    pane_width: i64,
    pane_height: i64,
    store_all_messages: bool,
    record_terminal_session: bool,
    interleaved_thinking: bool,
    use_responses_api: bool,
    llm_kwargs: Map<String, Value>,
    llm_call_kwargs: Map<String, Value>,
    session_id_headers: Vec<String>,
}

/// Pydantic-style (lax) kwarg coercion, collecting errors.
pub(crate) struct Kw<'a> {
    m: &'a BTreeMap<String, Value>,
    errors: Vec<String>,
}

impl<'a> Kw<'a> {
    pub(crate) fn new(m: &'a BTreeMap<String, Value>) -> Kw<'a> {
        Kw { m, errors: vec![] }
    }
    /// The "Invalid kwargs" error, if any option failed.
    pub(crate) fn finish(self, agent: &str) -> Result<()> {
        if self.errors.is_empty() {
            return Ok(());
        }
        bail!(
            "Invalid kwargs for agent '{agent}':\n{}\nSee `hbench agents schema {agent}` for available options.",
            self.errors.iter().map(|e| format!("  {e}")).collect::<Vec<_>>().join("\n")
        )
    }
    pub(crate) fn get(&self, k: &str) -> Option<&Value> {
        self.m.get(k).filter(|v| !v.is_null())
    }
    fn bad<T>(&mut self, k: &str, what: &str) -> Option<T> {
        self.errors.push(format!("{k}: Input should be a valid {what}"));
        None
    }
    pub(crate) fn int(&mut self, k: &str) -> Option<i64> {
        let v = self.get(k)?.clone();
        let r = match &v {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
            Value::String(s) => s.trim().parse().ok(),
            Value::Bool(b) => Some(i64::from(*b)),
            _ => None,
        };
        r.or_else(|| self.bad(k, "integer"))
    }
    pub(crate) fn float(&mut self, k: &str) -> Option<f64> {
        let r = match self.get(k)? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        };
        r.or_else(|| self.bad(k, "number"))
    }
    pub(crate) fn boolean(&mut self, k: &str, default: bool) -> bool {
        let Some(v) = self.get(k) else { return default };
        crate::agents::installed::common::opts::lax_bool(v).or_else(|| self.bad(k, "boolean")).unwrap_or(default)
    }
    pub(crate) fn string(&mut self, k: &str) -> Option<String> {
        match self.get(k)? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => self.bad(k, "string"),
        }
    }
    pub(crate) fn object(&mut self, k: &str) -> Option<Map<String, Value>> {
        match self.get(k)? {
            Value::Object(o) => Some(o.clone()),
            _ => self.bad(k, "dictionary"),
        }
    }
}

impl Options {
    fn parse(kwargs: &BTreeMap<String, Value>) -> Result<Options> {
        let mut kw = Kw::new(kwargs);
        // Precedence: max_turns > max_episodes > episodes (deprecated).
        let max_episodes = kw.int("max_turns").or_else(|| kw.int("max_episodes")).or_else(|| kw.int("episodes"));
        let parser_name = kw.string("parser_name").unwrap_or_else(|| "json".into());
        let reasoning_effort = kw.string("reasoning_effort");
        if let Some(e) = &reasoning_effort
            && !["none", "minimal", "low", "medium", "high", "xhigh", "max", "default"].contains(&e.as_str())
        {
            kw.errors
                .push("reasoning_effort: Input should be 'none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max' or 'default'".into());
        }
        let tc = kw.object("trajectory_config").unwrap_or_default();
        let session_id_headers = match kw.get("session_id_headers").cloned() {
            None => vec![],
            Some(Value::Array(a)) if a.iter().all(Value::is_string) => a.iter().map(|v| v.as_str().unwrap().to_string()).collect(),
            Some(_) => {
                kw.errors.push("session_id_headers: Input should be a valid list".into());
                vec![]
            }
        };
        let llm_backend = kw.string("llm_backend").unwrap_or_else(|| "litellm".into());
        let o = Options {
            max_episodes,
            parser: ParserKind::Json,
            api_base: kw.string("api_base"),
            temperature: kw.float("temperature"),
            reasoning_effort,
            collect_rollout_details: kw.boolean("collect_rollout_details", false),
            session_id: kw.string("session_id"),
            enable_summarize: kw.boolean("enable_summarize", true),
            proactive_summarization_threshold: kw.int("proactive_summarization_threshold").unwrap_or(8000),
            max_thinking_tokens: kw.int("max_thinking_tokens"),
            model_info: kw.object("model_info").map(Value::Object),
            raw_content: tc.get("raw_content").is_some_and(py_truthy),
            linear_history: tc.get("linear_history").is_some_and(py_truthy),
            pane_width: kw.int("tmux_pane_width").unwrap_or(160),
            pane_height: kw.int("tmux_pane_height").unwrap_or(40),
            store_all_messages: kw.boolean("store_all_messages", false),
            record_terminal_session: kw.boolean("record_terminal_session", true),
            interleaved_thinking: kw.boolean("interleaved_thinking", false),
            use_responses_api: kw.boolean("use_responses_api", false),
            llm_kwargs: kw.object("llm_kwargs").unwrap_or_default(),
            llm_call_kwargs: kw.object("llm_call_kwargs").unwrap_or_default(),
            session_id_headers,
        };
        kw.boolean("suppress_max_turns_warning", false);
        kw.finish(NAME)?;
        let parser = ParserKind::from_name(&parser_name)?;
        match llm_backend.as_str() {
            "litellm" => {}
            "tinker" => {
                bail!("llm_backend 'tinker' needs the Tinker Python SDK, which hbench (Rust) can't load; use the default 'litellm' backend")
            }
            other => bail!("Unknown llm_backend: {other:?}. Supported backends: ['litellm', 'tinker']"),
        }
        let header = re(r"^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$");
        if let Some(h) = o.session_id_headers.iter().find(|h| !header.is_match(h)) {
            bail!("Invalid HTTP header name in session_id_headers: {}", quote_repr(h));
        }
        if o.pane_width <= 0 || o.pane_height <= 0 {
            bail!("pane_width and pane_height must be positive integers.");
        }
        if o.llm_call_kwargs.get("extra_headers").is_some_and(|h| !h.is_object()) {
            bail!("llm_call_kwargs.extra_headers must be a mapping");
        }
        Ok(Options { parser, ..o })
    }
}

pub struct Terminus2 {
    args: AgentArgs,
    opts: Options,
    model_name: String,
    llm: Box<dyn Llm>,
    max_episodes: i64,
    session: Option<Tmux>,
    // Per-run state (reset by `reset_per_run_state`; multi-step trials reuse the instance).
    chat: Chat,
    pending_completion: bool,
    api_request_times: Vec<f64>,
    n_episodes: i64,
    session_id: String,
    steps: Vec<Step>,
    call_kwargs: Map<String, Value>,
    summarization_count: usize,
    pending_subagent_refs: Option<Vec<SubagentRef>>,
    pending_handoff_prompt: Option<String>,
    sub_metrics: SubagentMetrics,
    sub_rollout: Vec<Value>,
    last_response_model_name: Option<String>,
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl Terminus2 {
    pub fn new(args: AgentArgs) -> Result<Terminus2> {
        let opts = Options::parse(&args.kwargs)?;
        let model = args.model_name.clone().ok_or_else(|| anyhow!("model_name is required for Terminus 2"))?;
        let mut llm_kwargs = opts.llm_kwargs.clone();
        llm_kwargs.remove("litellm_debug");
        let temperature = opts.temperature.or_else(|| llm_kwargs.remove("temperature").and_then(|t| t.as_f64()));
        let cfg = LlmConfig {
            model_name: model.clone(),
            api_base: opts.api_base.clone(),
            temperature,
            reasoning_effort: opts.reasoning_effort.clone(),
            max_thinking_tokens: opts.max_thinking_tokens,
            session_id: opts.session_id.clone(),
            collect_rollout_details: opts.collect_rollout_details,
            model_info: opts.model_info.clone(),
            use_responses_api: opts.use_responses_api,
            llm_kwargs,
        };
        let llm = ChatClient::new(cfg, &|k: &str| args.get_env(k))?;
        Ok(Self::with_llm(args, opts, model, Box::new(llm)))
    }

    fn with_llm(args: AgentArgs, opts: Options, model_name: String, llm: Box<dyn Llm>) -> Terminus2 {
        Terminus2 {
            max_episodes: opts.max_episodes.unwrap_or(1_000_000),
            chat: Chat::new(opts.interleaved_thinking),
            call_kwargs: opts.llm_call_kwargs.clone(),
            args,
            opts,
            model_name,
            llm,
            session: None,
            pending_completion: false,
            api_request_times: vec![],
            n_episodes: 0,
            session_id: String::new(),
            steps: vec![],
            summarization_count: 0,
            pending_subagent_refs: None,
            pending_handoff_prompt: None,
            sub_metrics: SubagentMetrics::default(),
            sub_rollout: vec![],
            last_response_model_name: None,
        }
    }

    fn new_session(&self) -> Tmux {
        Tmux {
            name: NAME.into(),
            logging_path: "/logs/agent/terminus_2.pane".into(),
            recording_path: self.opts.record_terminal_session.then(|| "/logs/agent/recording.cast".into()),
            width: self.opts.pane_width,
            height: self.opts.pane_height,
            extra_env: self.args.extra_env.clone(),
            previous_buffer: None,
        }
    }

    fn tmux(&mut self) -> Result<&mut Tmux> {
        self.session.as_mut().ok_or_else(|| anyhow!("Session is not set"))
    }

    fn skills_dir(&self) -> Option<String> {
        self.args.task.config.environment.skills_dir.clone().or_else(|| (!self.args.skills.is_empty()).then(|| "/hbench/skills".into()))
    }

    fn track(&mut self, start: Instant) {
        self.api_request_times.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    /// Chat + subagent totals: (input, output, cache, cost).
    fn totals(&self) -> (i64, i64, i64, f64) {
        let (c, s) = (&self.chat, &self.sub_metrics);
        (c.total_input_tokens + s.prompt, c.total_output_tokens + s.completion, c.total_cache_tokens + s.cached, c.total_cost + s.cost)
    }

    fn reset_per_run_state(&mut self) {
        self.steps.clear();
        self.api_request_times.clear();
        self.n_episodes = 0;
        self.summarization_count = 0;
        self.sub_metrics = SubagentMetrics::default();
        self.sub_rollout.clear();
        self.pending_completion = false;
        self.pending_subagent_refs = None;
        self.pending_handoff_prompt = None;
        // The orchestrated session id is `{trial_name}__agent`; logs_dir is <trial>/agent.
        let orchestrated = self.args.logs_dir.parent().and_then(|p| p.file_name()).map(|n| format!("{}__agent", n.to_string_lossy()));
        self.session_id = self.opts.session_id.clone().or(orchestrated).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if !self.opts.session_id_headers.is_empty() {
            let headers = self.call_kwargs.entry("extra_headers").or_insert_with(|| json!({}));
            for h in &self.opts.session_id_headers {
                headers[h] = json!(self.session_id);
            }
        }
    }

    /// Mirror usage and metadata into the context after every LLM call: a timed-out run is
    /// cancelled (no `finally` in Rust), so the context must already be current.
    fn sync_context(&self, ctx: &mut AgentContext) {
        let (i, o, c, cost) = self.totals();
        ctx.n_input_tokens = Some(i);
        ctx.n_output_tokens = Some(o);
        ctx.n_cache_tokens = Some(c);
        ctx.cost_usd = (cost > 0.0).then_some(cost);
        let mut rd = self.chat.rollout_details();
        rd.extend(self.sub_rollout.iter().cloned());
        ctx.rollout_details = Some(rd);
        let mut md = json!({"n_episodes": self.n_episodes, "api_request_times_msec": self.api_request_times, "summarization_count": self.summarization_count});
        if self.opts.store_all_messages {
            md["all_messages"] = json!(self.chat.messages);
        }
        ctx.metadata = Some(md);
        ctx.model_usage = self.model_usage();
    }

    /// Per-model usage summed over the main trajectory.
    fn model_usage(&self) -> Option<BTreeMap<String, ModelUsage>> {
        let mut usage: BTreeMap<String, ModelUsage> = BTreeMap::new();
        let mut unpriced = BTreeSet::new();
        for s in &self.steps {
            let Some(m) = &s.metrics else { continue };
            let name = s.model_name.clone().unwrap_or_else(|| self.model_name.clone());
            let u = usage.entry(name.clone()).or_default();
            u.n_input_tokens += m.prompt_tokens.unwrap_or(0);
            u.n_cache_tokens += m.cached_tokens.unwrap_or(0);
            u.n_output_tokens += m.completion_tokens.unwrap_or(0);
            match m.cost_usd {
                Some(c) => u.cost_usd = Some(u.cost_usd.unwrap_or(0.0) + c),
                None if m.prompt_tokens.unwrap_or(0) > 0 || m.completion_tokens.unwrap_or(0) > 0 => {
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

    async fn build_skills_section(&self, sh: &dyn Shell) -> Result<Option<String>> {
        let Some(dir) = self.skills_dir() else { return Ok(None) };
        if sh.sh(&format!("test -d {}", quote(&dir)), true, Some(Duration::from_secs(60))).await?.code != 0 {
            return Ok(None);
        }
        let ten = Some(Duration::from_secs(10));
        let r = sh.sh(&format!("find {} -mindepth 2 -maxdepth 2 -name SKILL.md -type f | sort", quote(&dir)), false, ten).await?;
        if r.code != 0 || r.stdout.trim().is_empty() {
            return Ok(None);
        }
        let mut entries = vec![];
        for path in r.stdout.trim().lines() {
            let c = sh.sh(&format!("cat {}", quote(path)), false, ten).await?;
            if c.code != 0 || c.stdout.is_empty() {
                continue;
            }
            if let Some((name, desc)) = parse_skill_frontmatter(&c.stdout) {
                entries.push((name, desc, path.to_string()));
            }
        }
        if entries.is_empty() {
            return Ok(None);
        }
        let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        let mut xml = String::from("\n<available_skills>\n");
        for (n, d, l) in entries {
            xml += &format!(
                "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>\n",
                esc(&n),
                esc(&d),
                esc(&l)
            );
        }
        xml += "</available_skills>";
        Ok(Some(xml))
    }

    /// The agent run against any shell (AgentCtx in production, a mock in tests).
    pub async fn run_with(&mut self, instruction: &str, sh: &dyn Shell, ctx: &mut AgentContext) -> Result<()> {
        self.reset_per_run_state();
        self.chat = Chat::new(self.opts.interleaved_thinking);
        let r = self.run_inner(instruction, sh, ctx).await;
        self.sync_context(ctx);
        self.dump_trajectory();
        r.map_err(|e| match e.downcast::<LlmError>() {
            Ok(le) => le.into_anyhow(),
            Err(e) => e,
        })
    }

    async fn run_inner(&mut self, instruction: &str, sh: &dyn Shell, ctx: &mut AgentContext) -> Result<()> {
        let terminal_state = limit_output_length(&self.tmux()?.get_incremental_output(sh).await?, 10_000);
        let mut augmented = format!("{instruction}{}", mcp_info(&self.args.mcp_servers));
        if let Some(skills) = self.build_skills_section(sh).await? {
            augmented += &skills;
        }
        let initial_prompt =
            fill_template(self.opts.parser.template(), &[("instruction", &augmented), ("terminal_state", &terminal_state)]);
        self.steps.push(Step::new(1, "user", initial_prompt.clone()));
        ctx.n_input_tokens = Some(0);
        ctx.n_output_tokens = Some(0);
        ctx.n_cache_tokens = Some(0);
        ctx.cost_usd = None;
        self.run_agent_loop(initial_prompt, instruction, sh, ctx).await
    }

    async fn run_agent_loop(&mut self, initial_prompt: String, orig: &str, sh: &dyn Shell, ctx: &mut AgentContext) -> Result<()> {
        let mut prompt = initial_prompt;
        for episode in 0..self.max_episodes {
            self.n_episodes = episode + 1;
            if !self.tmux()?.is_alive(sh).await? {
                return Ok(());
            }
            if !orig.is_empty()
                && self.opts.enable_summarize
                && let Some((p, refs)) = self.check_proactive_summarization(sh, orig).await
            {
                prompt = p.clone();
                self.pending_subagent_refs = refs;
                self.pending_handoff_prompt = Some(p);
            }
            let before = (self.chat.total_input_tokens, self.chat.total_output_tokens, self.chat.total_cache_tokens, self.chat.total_cost);
            let (commands, is_task_complete, feedback, analysis, plan, resp) = self.handle_llm_interaction(sh, &prompt, orig).await?;

            if let Some(refs) = self.pending_subagent_refs.take().filter(|r| !r.is_empty()) {
                let mut s = Step::new(self.steps.len() + 1, "system", "Performed context summarization and handoff to continue task.");
                s.observation =
                    Some(Observation { results: vec![ObsResult { subagent_trajectory_ref: Some(refs), ..Default::default() }] });
                s.extra = Some(json!({"context_management": {"type": "compaction", "boundary": "replace"}}));
                self.steps.push(s);
            }
            if let Some(handoff) = self.pending_handoff_prompt.take().filter(|h| !h.is_empty()) {
                if self.opts.linear_history {
                    self.split_trajectory_on_summarization();
                } else {
                    self.steps.push(Step::new(self.steps.len() + 1, "user", handoff));
                }
            }
            let message_content = if self.opts.raw_content {
                resp.content.clone()
            } else {
                let mut parts = vec![];
                if !analysis.is_empty() {
                    parts.push(format!("Analysis: {analysis}"));
                }
                if !plan.is_empty() {
                    parts.push(format!("Plan: {plan}"));
                }
                parts.join("\n")
            };
            self.sync_context(ctx);
            let cache_used = self.chat.total_cache_tokens - before.2;
            let step_cost = self.chat.total_cost - before.3;
            let metrics = Metrics {
                prompt_tokens: Some(self.chat.total_input_tokens - before.0),
                completion_tokens: Some(self.chat.total_output_tokens - before.1),
                cached_tokens: (cache_used > 0).then_some(cache_used),
                cost_usd: (step_cost > 0.0).then_some(step_cost),
                prompt_token_ids: resp.prompt_token_ids.clone(),
                completion_token_ids: resp.completion_token_ids.clone(),
                logprobs: resp.logprobs.clone(),
            };
            if resp.model_name.is_some() {
                self.last_response_model_name = resp.model_name.clone();
            }
            let mut step = Step::new(self.steps.len() + 1, "agent", "");
            step.model_name = Some(resp.model_name.clone().unwrap_or_else(|| self.model_name.clone()));
            step.reasoning_content = resp.reasoning_content.clone();
            step.metrics = Some(metrics);

            if feedback.contains("ERROR:") {
                prompt = format!(
                    "Previous response had parsing errors:\n{feedback}\n\nPlease fix these issues and provide a proper {}.",
                    self.opts.parser.response_type()
                );
                step.message = resp.content.clone();
                step.observation = Some(Observation { results: vec![ObsResult { content: Some(prompt.clone()), ..Default::default() }] });
                self.steps.push(step);
                self.dump_trajectory();
                continue;
            }

            let (_timed_out, terminal_output) = self.execute_commands(sh, &commands).await?;
            let was_pending = self.pending_completion;
            let observation = if is_task_complete {
                if self.pending_completion {
                    terminal_output
                } else {
                    self.pending_completion = true;
                    self.opts.parser.completion_confirmation(&terminal_output)
                }
            } else {
                self.pending_completion = false;
                if feedback.contains("WARNINGS:") {
                    format!("Previous response had warnings:\n{feedback}\n\n{}", limit_output_length(&terminal_output, 10_000))
                } else {
                    limit_output_length(&terminal_output, 10_000)
                }
            };

            let mut results = vec![];
            if self.opts.raw_content {
                results.push(ObsResult { content: Some(observation.clone()), ..Default::default() });
            } else {
                let mut calls: Vec<ToolCall> = commands
                    .iter()
                    .enumerate()
                    .map(|(i, c)| ToolCall {
                        tool_call_id: format!("call_{episode}_{}", i + 1),
                        function_name: "bash_command",
                        arguments: json!({"keystrokes": c.keystrokes, "duration": c.duration_sec}),
                    })
                    .collect();
                if !commands.is_empty() {
                    // Batches share one terminal output: only single commands link precisely.
                    let source_call_id = (commands.len() == 1).then(|| format!("call_{episode}_1"));
                    results.push(ObsResult { source_call_id, content: Some(observation.clone()), ..Default::default() });
                }
                if is_task_complete {
                    calls.push(ToolCall {
                        tool_call_id: format!("call_{episode}_task_complete"),
                        function_name: "mark_task_complete",
                        arguments: json!({}),
                    });
                }
                if commands.is_empty() {
                    results.push(ObsResult { content: Some(observation.clone()), ..Default::default() });
                }
                step.tool_calls = (!calls.is_empty()).then_some(calls);
            }
            step.message = message_content;
            step.observation = Some(Observation { results });
            self.steps.push(step);
            self.sync_context(ctx);
            self.dump_trajectory();

            if is_task_complete && was_pending {
                return Ok(());
            }
            prompt = observation;
        }
        Ok(())
    }

    async fn handle_llm_interaction(
        &mut self,
        sh: &dyn Shell,
        prompt: &str,
        orig: &str,
    ) -> Result<(Vec<Command>, bool, String, String, String, LlmResponse)> {
        let resp = self.query_llm(sh, prompt.to_string(), orig, 0).await?;
        let r = self.opts.parser.parse(&resp.content);
        let feedback = match (r.error.is_empty(), r.warning.is_empty()) {
            (false, false) => format!("ERROR: {}\nWARNINGS: {}", r.error, r.warning),
            (false, true) => format!("ERROR: {}", r.error),
            (true, false) => format!("WARNINGS: {}", r.warning),
            (true, true) => String::new(),
        };
        let commands =
            r.commands.iter().map(|c| Command { keystrokes: c.keystrokes.clone(), duration_sec: c.duration.min(60.0) }).collect();
        Ok((commands, r.is_task_complete, feedback, r.analysis, r.plan, resp))
    }

    async fn execute_commands(&mut self, sh: &dyn Shell, commands: &[Command]) -> Result<(bool, String)> {
        let t = self.tmux()?;
        for c in commands {
            if !t.send_keys(sh, vec![c.keystrokes.clone()], false, c.duration_sec, 180.0).await? {
                let state = limit_output_length(&t.get_incremental_output(sh).await?, 10_000);
                let msg = fill_template(
                    TIMEOUT_TEMPLATE,
                    &[("timeout_sec", &pyf(c.duration_sec)), ("command", &c.keystrokes), ("terminal_state", &state)],
                );
                return Ok((true, msg));
            }
        }
        Ok((false, limit_output_length(&t.get_incremental_output(sh).await?, 10_000)))
    }

    /// Query the LLM (the client retries transient errors). Truncated output re-asks up to
    /// `MAX_OUTPUT_LENGTH_RETRIES` times (`depth`), then fails with the output-length error.
    fn query_llm<'a>(&'a mut self, sh: &'a dyn Shell, prompt: String, orig: &'a str, depth: usize) -> BoxFut<'a, Result<LlmResponse>> {
        Box::pin(async move { self.query_llm_once(sh, &prompt, orig, depth).await })
    }

    async fn query_llm_once(&mut self, sh: &dyn Shell, prompt: &str, orig: &str, depth: usize) -> Result<LlmResponse> {
        let start = Instant::now();
        let err = match self.chat.chat(&*self.llm, prompt, &self.call_kwargs).await {
            Ok(r) => {
                self.track(start);
                return Ok(r);
            }
            Err(e) => e,
        };
        match err {
            LlmError::ContextLength(m) => {
                if !self.opts.enable_summarize {
                    return Err(LlmError::ContextLength(m).into());
                }
                self.unwind_messages_to_free_tokens(4000);
                // Fallback 1: full summary; 2: short summary; 3: no LLM at all.
                let mut summary_prompt = match self.summarize(sh, orig).await {
                    Ok((p, refs)) => {
                        self.pending_subagent_refs = refs;
                        self.pending_handoff_prompt = Some(p.clone());
                        Some(p)
                    }
                    Err(_) => None,
                };
                if summary_prompt.is_none() {
                    let short = async {
                        let screen = self.tmux()?.capture_pane(sh, false).await?;
                        let p = format!(
                            "Briefly continue this task: {orig}\n\nCurrent state: {}\n\nNext steps (2-3 sentences):",
                            last_chars(&screen, 1000)
                        );
                        let r = self.llm.call(&p, &[], &self.call_kwargs, None).await?;
                        anyhow::Ok(format!("{orig}\n\nSummary: {}", r.content))
                    };
                    summary_prompt = short.await.ok();
                }
                let summary_prompt = match summary_prompt {
                    Some(p) => p,
                    None => format!("{orig}\n\nCurrent state: {}", last_chars(&self.tmux()?.capture_pane(sh, false).await?, 1000)),
                };
                let start = Instant::now();
                Ok(match self.chat.chat(&*self.llm, &summary_prompt, &self.call_kwargs).await {
                    Ok(r) => {
                        self.track(start);
                        r
                    }
                    Err(_) => {
                        LlmResponse { content: "Technical difficulties. Please continue with the task.".into(), ..Default::default() }
                    }
                })
            }
            LlmError::OutputLength { msg, truncated } => {
                if self.opts.parser == ParserKind::Xml
                    && let (Some(salvaged), _) = xml_parser::salvage_truncated_response(&truncated)
                {
                    // Wrap the salvaged string in a response.
                    return Ok(LlmResponse { content: salvaged, ..Default::default() });
                }
                if depth >= MAX_OUTPUT_LENGTH_RETRIES {
                    return Err(LlmError::OutputLength { msg, truncated }.into());
                }
                let warnings = self.opts.parser.parse(&truncated).warning;
                let limit = match self.llm.output_limit() {
                    Some(l) => format!("{l} tokens"),
                    None => "the maximum output length".into(),
                };
                let mut error_msg = format!(
                    "ERROR!! NONE of the actions you just requested were performed because you exceeded {limit}. Your outputs must be less than {limit}. Re-issue this request, breaking it into chunks each of which is less than {limit}."
                );
                if !warnings.is_empty() {
                    error_msg += &format!("\n\nParser warnings from your truncated response:\n{warnings}");
                }
                self.chat.messages.push(json!({"role": "user", "content": prompt}));
                self.chat.messages.push(json!({"role": "assistant", "content": truncated}));
                self.chat.reset_response_chain();
                self.query_llm(sh, error_msg, orig, depth + 1).await
            }
            e => Err(e.into()),
        }
    }

    async fn check_proactive_summarization(&mut self, sh: &dyn Shell, orig: &str) -> Option<(String, Option<Vec<SubagentRef>>)> {
        let free = self.llm.context_limit() - self.llm.count_tokens(&self.chat.messages);
        if free < self.opts.proactive_summarization_threshold { self.summarize(sh, orig).await.ok() } else { None }
    }

    fn unwind_messages_to_free_tokens(&mut self, target_free: i64) {
        let limit = self.llm.context_limit();
        while self.chat.messages.len() > 1 {
            if limit - self.llm.count_tokens(&self.chat.messages) >= target_free {
                break;
            }
            let n = self.chat.messages.len();
            if n < 2 {
                break;
            }
            self.chat.messages.truncate(n - 2);
        }
        self.chat.reset_response_chain();
    }

    /// Three subagents (summary, questions, answers) compress the history for a fresh handoff.
    async fn summarize(&mut self, sh: &dyn Shell, orig: &str) -> Result<(String, Option<Vec<SubagentRef>>)> {
        if self.chat.messages.is_empty() {
            return Ok((orig.to_string(), None));
        }
        self.summarization_count += 1;
        let n = self.summarization_count;
        let mut refs = vec![];
        let steps_to_include = 1 + (self.chat.messages.len() - 1) / 2;

        let summary_prompt = format!(
            "You are about to hand off your work to another AI agent.
            Please provide a comprehensive summary of what you have
            accomplished so far on this task:

Original Task: {orig}

Based on the conversation history, please provide a detailed summary covering:
1. **Major Actions Completed** - List each significant command you executed
            and what you learned from it.
2. **Important Information Learned** - A summary of crucial findings, file
            locations, configurations, error messages, or system state discovered.
3. **Challenging Problems Addressed** - Any significant issues you
            encountered and how you resolved them.
4. **Current Status** - Exactly where you are in the task completion process.

Be comprehensive and detailed. The next agent needs to understand everything
            that has happened so far in order to continue."
        );
        let history = self.chat.messages.clone();
        let (summary, r) = self
            .run_subagent(
                &summary_prompt,
                &history,
                self.copied_steps(steps_to_include),
                format!("{}-summarization-{n}-summary", self.session_id),
                "terminus-2-summarization-summary",
                "summary",
                format!("Context summarization {n}: Step 1 - Summary generation"),
            )
            .await?;
        refs.push(r);

        let screen = self.tmux()?.capture_pane(sh, false).await?;
        let question_prompt = format!(
            "You are picking up work from a previous AI agent on this task:

**Original Task:** {orig}

**Summary from Previous Agent:**
{}

**Current Terminal Screen:**
{screen}

Please begin by asking several questions (at least five, more if necessary)
about the current state of the solution that are not answered in the summary
from the prior agent. After you ask these questions you will be on your own,
so ask everything you need to know.",
            summary.content
        );
        let (questions, r) = self
            .run_subagent(
                &question_prompt,
                &[],
                vec![],
                format!("{}-summarization-{n}-questions", self.session_id),
                "terminus-2-summarization-questions",
                "questions",
                format!("Context summarization {n}: Step 2 - Question asking"),
            )
            .await?;
        refs.push(r);

        let mut answer_steps = self.copied_steps(steps_to_include);
        let mut s = Step::new(answer_steps.len() + 1, "user", summary_prompt.clone());
        s.is_copied_context = Some(true);
        answer_steps.push(s);
        let mut s = Step::new(answer_steps.len() + 1, "agent", summary.content.clone());
        s.model_name = Some(summary.model_name.clone().unwrap_or_else(|| self.model_name.clone()));
        s.reasoning_content = summary.reasoning_content.clone();
        s.is_copied_context = Some(true);
        s.extra = Some(json!({"note": "Copied from summary subagent - metrics already recorded there"}));
        answer_steps.push(s);
        let mut answers_history = self.chat.messages.clone();
        answers_history.push(json!({"role": "user", "content": summary_prompt}));
        answers_history.push(json!({"role": "assistant", "content": summary.content}));
        let (answers, r) = self
            .run_subagent(
                &format!(
                    "The next agent has a few questions for you, please answer each of them one by one in detail:\n\n{}",
                    questions.content
                ),
                &answers_history,
                answer_steps,
                format!("{}-summarization-{n}-answers", self.session_id),
                "terminus-2-summarization-answers",
                "answers",
                format!("Context summarization {n}: Step 3 - Answer providing"),
            )
            .await?;
        refs.push(r);

        // Only the questions stay in history; the answers arrive as the handoff prompt.
        let first = self.chat.messages[0].clone();
        self.chat.messages =
            vec![first, json!({"role": "user", "content": question_prompt}), json!({"role": "assistant", "content": questions.content})];
        self.chat.reset_response_chain();
        let handoff = format!(
            "Here are the answers the other agent provided.\n\n{}\n\nContinue working on this task from where the previous agent left off. You can no longer ask questions. Please follow the spec to interact with the terminal.",
            answers.content
        );
        Ok((handoff, Some(refs)))
    }

    /// First `n` main steps, marked as copied context with metrics stripped.
    fn copied_steps(&self, n: usize) -> Vec<Step> {
        let mut steps: Vec<Step> = self.steps.iter().take(n).cloned().collect();
        for s in &mut steps {
            s.is_copied_context = Some(true);
            if s.metrics.take().is_some() {
                let extra = s.extra.get_or_insert_with(|| json!({}));
                extra["note"] = json!("Metrics omitted to avoid duplication - already recorded in parent trajectory");
            }
        }
        steps
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_subagent(
        &mut self,
        prompt: &str,
        history: &[Value],
        mut steps: Vec<Step>,
        session_id: String,
        agent_name: &str,
        suffix: &str,
        summary_text: String,
    ) -> Result<(LlmResponse, SubagentRef)> {
        steps.push(Step::new(steps.len() + 1, "user", prompt));
        let start = Instant::now();
        let r = self.llm.call(prompt, history, &self.call_kwargs, None).await?;
        self.track(start);
        let mut s = Step::new(steps.len() + 1, "agent", r.content.clone());
        s.model_name = Some(r.model_name.clone().unwrap_or_else(|| self.model_name.clone()));
        s.reasoning_content = r.reasoning_content.clone();
        let u = r.usage.clone().unwrap_or_default();
        if let Some(u) = &r.usage {
            self.sub_metrics.prompt += u.prompt_tokens;
            self.sub_metrics.completion += u.completion_tokens;
            self.sub_metrics.cached += u.cache_tokens;
            self.sub_metrics.cost += u.cost_usd;
            s.metrics = Some(Metrics {
                prompt_tokens: Some(u.prompt_tokens),
                completion_tokens: Some(u.completion_tokens),
                cached_tokens: Some(u.cache_tokens),
                cost_usd: (u.cost_usd > 0.0).then_some(u.cost_usd),
                prompt_token_ids: r.prompt_token_ids.clone(),
                completion_token_ids: r.completion_token_ids.clone(),
                logprobs: r.logprobs.clone(),
            });
        }
        steps.push(s);
        if self.opts.collect_rollout_details {
            let mut d = Map::new();
            if let Some(p) = &r.prompt_token_ids {
                d.insert("prompt_token_ids".into(), json!([p]));
            }
            if let Some(c) = &r.completion_token_ids {
                d.insert("completion_token_ids".into(), json!([c]));
            }
            if let Some(l) = &r.logprobs {
                d.insert("logprobs".into(), json!([l]));
            }
            if !d.is_empty() {
                self.sub_rollout.push(Value::Object(d));
            }
        }
        let file = format!("trajectory.summarization-{}-{suffix}.json", self.summarization_count);
        let agent = json!({"name": agent_name, "version": VERSION, "model_name": self.model_name,
            "extra": {"parent_session_id": self.session_id, "summarization_index": self.summarization_count}});
        let t = Trajectory {
            schema_version: "ATIF-v1.8",
            session_id: &session_id,
            agent,
            steps: &steps,
            final_metrics: final_metrics(u.prompt_tokens, u.completion_tokens, u.cache_tokens, u.cost_usd),
            continued_trajectory_ref: None,
        };
        write_trajectory(&self.args.logs_dir.join(&file), &t);
        Ok((r, SubagentRef { session_id, trajectory_path: file, extra: json!({"summary": summary_text}) }))
    }

    /// linear_history: close the current segment and restart the trajectory from the chat history.
    fn split_trajectory_on_summarization(&mut self) {
        self.dump_trajectory_with_continuation_index(self.summarization_count - 1);
        let base = self.session_id.split("-cont-").next().unwrap_or_default().to_string();
        self.session_id = format!("{base}-cont-{}", self.summarization_count);
        let msgs = &self.chat.messages[..self.chat.messages.len().saturating_sub(1)];
        self.steps = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let agent = m["role"] == "assistant";
                let source = if agent {
                    "agent"
                } else if m["role"] == "system" {
                    "system"
                } else {
                    "user"
                };
                Step {
                    step_id: i + 1,
                    source,
                    message: content_text(&m["content"]),
                    model_name: agent.then(|| self.last_response_model_name.clone().unwrap_or_else(|| self.model_name.clone())),
                    is_copied_context: Some(true),
                    ..Default::default()
                }
            })
            .collect();
    }

    fn dump_trajectory(&self) {
        self.dump_trajectory_with_continuation_index(self.summarization_count);
    }

    fn dump_trajectory_with_continuation_index(&self, idx: usize) {
        let (i, o, c, cost) = self.totals();
        let mut extra = json!({"parser": self.opts.parser.name()});
        if let Some(t) = self.opts.temperature {
            extra["temperature"] = json!(t);
        }
        if !self.opts.llm_kwargs.is_empty() {
            extra["llm_kwargs"] = json!(self.opts.llm_kwargs);
        }
        let linear = self.opts.linear_history;
        if linear && idx > 0 {
            extra["continuation_index"] = json!(idx);
        }
        let t = Trajectory {
            schema_version: "ATIF-v1.8",
            session_id: &self.session_id,
            agent: json!({"name": NAME, "version": VERSION, "model_name": self.model_name, "extra": extra}),
            steps: &self.steps,
            final_metrics: final_metrics(i, o, c, cost),
            continued_trajectory_ref: (linear && idx < self.summarization_count).then(|| format!("trajectory.cont-{}.json", idx + 1)),
        };
        let file = if linear && idx > 0 { format!("trajectory.cont-{idx}.json") } else { "trajectory.json".into() };
        write_trajectory(&self.args.logs_dir.join(file), &t);
    }
}

/// MCP servers appended to the instruction (Terminus-2 and dspy-rlm).
pub(crate) fn mcp_info(servers: &[crate::task::McpServer]) -> String {
    if servers.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n\nMCP Servers:\nThe following MCP servers are available for this task.\n");
    for m in servers {
        if m.transport() == "stdio" {
            s += &format!("- {}: stdio transport, command: {} {}\n", m.name, m.command.as_deref().unwrap_or("None"), m.args.join(" "));
        } else {
            s += &format!("- {}: {} transport, url: {}\n", m.name, m.transport(), m.url.as_deref().unwrap_or("None"));
        }
    }
    s
}

fn parse_skill_frontmatter(content: &str) -> Option<(String, String)> {
    let c = re(r"(?s)^---\n(.*?)\n---").captures(content)?;
    let fm: serde_yaml_ng::Value = serde_yaml_ng::from_str(&c[1]).ok()?;
    let s = |v: &serde_yaml_ng::Value| {
        v.as_str().map(String::from).unwrap_or_else(|| serde_yaml_ng::to_string(v).unwrap_or_default().trim().to_string())
    };
    Some((s(fm.get("name")?), s(fm.get("description")?)))
}

#[async_trait::async_trait]
impl Agent for Terminus2 {
    fn name(&self) -> &str {
        NAME
    }
    fn version(&self) -> Option<String> {
        Some(VERSION.into())
    }
    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        let mut t = self.new_session();
        t.start(cx).await?;
        self.session = Some(t);
        Ok(())
    }
    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        self.run_with(instruction, cx, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::tmux::MAX_COMMAND_LENGTH;
    use super::*;
    use crate::agents::llm::Usage;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[test]
    fn json_parser_basics() {
        let resp = r#"{"analysis": "Need to list files.", "plan": "Run ls.", "commands": [{"keystrokes": "ls -la\n", "duration": 0.1}, {"keystrokes": "cd project\n", "duration": 0.1}], "task_complete": false}"#;
        let r = json_parser::parse_response(resp);
        assert_eq!((r.error.as_str(), r.warning.as_str(), r.is_task_complete), ("", "", false));
        assert_eq!(
            r.commands,
            vec![
                ParsedCommand { keystrokes: "ls -la\n".into(), duration: 0.1 },
                ParsedCommand { keystrokes: "cd project\n".into(), duration: 0.1 }
            ]
        );
        assert_eq!((r.analysis.as_str(), r.plan.as_str()), ("Need to list files.", "Run ls."));

        // Trailing-newline validation.
        let mk = |cmds: Value| json!({"analysis": "test", "plan": "test", "commands": cmds}).to_string();
        let r = json_parser::parse_response(&mk(json!([{"keystrokes": "", "duration": 10.0}, {"keystrokes": "ls\n", "duration": 1.0}])));
        assert!(r.error.is_empty() && !r.warning.contains("should end with newline"));
        assert!(
            json_parser::parse_response(&mk(json!([{"keystrokes": "ls", "duration": 1.0}, {"keystrokes": "pwd\n", "duration": 1.0}])))
                .warning
                .contains("should end with newline")
        );
        let r = json_parser::parse_response(&mk(
            json!([{"keystrokes": "ls\n", "duration": 1.0}, {"keystrokes": "", "duration": 2.0}, {"keystrokes": "", "duration": 3.0}, {"keystrokes": "pwd\n", "duration": 1.0}]),
        ));
        assert!(!r.warning.contains("should end with newline"));

        // Extra text, missing duration, unknown fields, wrong order, string task_complete.
        let r = json_parser::parse_response(
            "Sure!\n{\"plan\": \"p\", \"analysis\": \"a\", \"commands\": [{\"keystrokes\": \"x\\n\", \"foo\": 1}], \"task_complete\": \"yes\"}\nDone.",
        );
        assert!(r.error.is_empty() && r.is_task_complete);
        assert_eq!(
            r.warning,
            "- Extra text detected before JSON object\n- Extra text detected after JSON object\n- Fields appear in wrong order. Found: plan → analysis → commands, expected: analysis → plan → commands\n- Command 1: Missing duration field, using default 1.0\n- Command 1: Unknown fields: foo"
        );
        // Errors.
        assert_eq!(json_parser::parse_response("no json here").error, "No valid JSON found in response");
        assert_eq!(json_parser::parse_response(r#"{"analysis": "a", "plan": "p"}"#).error, "Missing required fields: commands");
        assert_eq!(
            json_parser::parse_response(r#"{"analysis": "a", "plan": "p", "commands": "ls"}"#).error,
            "Field 'commands' must be an array"
        );
        assert_eq!(
            json_parser::parse_response(r#"{"analysis": "a", "plan": "p", "commands": [{"duration": 1}]}"#).error,
            "Command 1 missing required 'keystrokes' field"
        );
        // Parse errors become warnings once the task is complete.
        let r = json_parser::parse_response(r#"{"analysis": "a", "plan": "p", "commands": [1], "task_complete": true}"#);
        assert!(r.error.is_empty() && r.is_task_complete && r.warning.contains("Command 1 must be an object"));
        // Auto-fix: missing closing brace.
        let r = json_parser::parse_response(r#"{"analysis": "a", "plan": "p", "commands": [{"keystrokes": "ls\n", "duration": 1}]"#);
        assert!(r.error.is_empty() && r.warning.starts_with("- AUTO-CORRECTED: Fixed incomplete JSON by adding missing closing brace"));
        assert_eq!(r.commands.len(), 1);
        let r = json_parser::parse_response(r#"{"analysis": "a", "plan": "p", "commands": [}"#);
        assert!(
            r.error.starts_with("Invalid JSON: ") && r.error.contains(r#" | Content: '{"analysis": "a", "plan": "p", "commands": [}'"#)
        );
    }

    const XML_OK: &str = "<response>\n<analysis>\nI see a prompt.\n</analysis>\n<plan>\nList then cd.\n</plan>\n<commands>\n<keystrokes duration=\"0.1\">ls -la\n</keystrokes>\n<keystrokes duration=\"0.1\">cd project\n</keystrokes>\n</commands>\n<task_complete>true</task_complete>\n</response>";

    #[test]
    fn xml_parser_basics() {
        let r = xml_parser::parse_response(XML_OK);
        assert_eq!((r.error.as_str(), r.warning.as_str(), r.is_task_complete), ("", "", true));
        assert_eq!(
            r.commands,
            vec![
                ParsedCommand { keystrokes: "ls -la\n".into(), duration: 0.1 },
                ParsedCommand { keystrokes: "cd project\n".into(), duration: 0.1 }
            ]
        );
        assert_eq!((r.analysis.as_str(), r.plan.as_str()), ("I see a prompt.", "List then cd."));

        let r = xml_parser::parse_response(
            "pre <response><plan>p</plan><analysis>a</analysis><commands><keystrokes duration=2 extra='x'>echo &amp; hi</keystrokes><keystrokes>pwd\n</keystrokes></commands><foo/></response> post",
        );
        assert!(r.error.is_empty() && !r.is_task_complete);
        assert_eq!(r.commands[0].duration, 1.0); // unquoted: ignored
        assert_eq!(r.commands[1].duration, 1.0);
        for w in [
            "Extra text detected before <response> tag",
            "Extra text detected after </response> tag",
            "Unknown tag found: <foo>",
            "Sections appear in wrong order. Found: plan → analysis → commands",
            "Command 1: Attribute 'duration' value should be quoted: duration=\"2\"",
            "Command 1: Use double quotes for attribute 'extra': extra=\"x\"",
            "Command 1: Unknown attribute 'extra' - known attributes are: duration",
            "Command 1 should end with newline",
            "Command 2: Missing duration attribute, using default 1.0",
            "Warning: &amp; is read verbatim",
        ] {
            assert!(r.warning.contains(w), "missing warning {w:?} in {}", r.warning);
        }
        assert_eq!(xml_parser::parse_response("hello").error, "No <response> tag found");
        assert_eq!(
            xml_parser::parse_response("<response><analysis>a</analysis><plan>p</plan></response>").error,
            "Missing <commands> section"
        );
        let r = xml_parser::parse_response("<response><analysis>a</analysis><plan>p</plan><commands></commands></response>");
        assert!(r.error.is_empty() && r.commands.is_empty() && r.warning.contains("Commands section is empty"));
        let r = xml_parser::parse_response(
            "<response><analysis>a</analysis><plan>p</plan><commands><keystrokes duration=\"1\">a\n</keystrokes></commands><commands></commands></response>",
        );
        assert!(r.warning.contains("IMPORTANT: Only issue one <commands> block at a time. You issued 2"));

        // Salvage: a clean prefix up to </response> survives truncation.
        let (s, multi) = xml_parser::salvage_truncated_response(&format!("{XML_OK}\n<response><analysis>more"));
        assert_eq!((s.as_deref(), multi), (Some(XML_OK), false));
        assert_eq!(xml_parser::salvage_truncated_response("<response><analysis>a</analysis><commands>"), (None, false));
    }

    #[test]
    fn prompt_rendering_and_helpers() {
        let p = fill_template(JSON_TEMPLATE, &[("instruction", "Do {x}"), ("terminal_state", "$ ")]);
        assert!(p.contains("{\n  \"analysis\": \"Analyze") && p.contains("{\"keystrokes\": \"\", \"duration\": 10.0}"));
        assert!(
            p.ends_with("Task Description:\nDo {x}\n\nCurrent terminal state:\n$ \n")
                || p.ends_with("Task Description:\nDo {x}\n\nCurrent terminal state:\n$ ")
        );
        let p = fill_template(XML_TEMPLATE, &[("instruction", "T"), ("terminal_state", "S")]);
        assert!(p.contains("<keystrokes duration=\"0.1\">ls -la") && p.contains("Task Description:\nT\n"));
        let t = fill_template(TIMEOUT_TEMPLATE, &[("timeout_sec", &pyf(5.0)), ("command", "sleep 99\n"), ("terminal_state", "S")]);
        assert!(t.starts_with("Previous command:\nsleep 99\n") && t.contains("timed out after 5.0 seconds") && t.ends_with("\n\nS ")); // template ends with a space
        assert_eq!((pyf(1.0), pyf(0.1), pyf(60.0)), ("1.0".to_string(), "0.1".to_string(), "60.0".to_string()));
        assert_eq!(quote_repr("it's"), "\"it's\"");
        assert_eq!(quote_repr("a\nb'\""), "'a\\nb\\'\"'");
        assert_eq!(last_chars("héllo", 3), "llo");
        let long = "é".repeat(6000);
        let l = limit_output_length(&long, 10_000);
        assert!(l.contains("[... output limited to 10000 bytes; 2000 interior bytes omitted ...]"));
        assert_eq!(limit_output_length("short", 10), "short");
        let sk = "---\nname: pdf\ndescription: Work with PDFs & forms\n---\nbody";
        assert_eq!(parse_skill_frontmatter(sk), Some(("pdf".into(), "Work with PDFs & forms".into())));
        assert_eq!(parse_skill_frontmatter("no frontmatter"), None);
    }

    #[derive(Default)]
    struct MockShell {
        log: Mutex<Vec<String>>,
        screens: Mutex<VecDeque<String>>,
        dead: bool,
    }

    #[async_trait::async_trait]
    impl Shell for MockShell {
        async fn sh(&self, cmd: &str, _root: bool, _t: Option<Duration>) -> Result<ExecResult> {
            self.log.lock().unwrap().push(cmd.to_string());
            let ok = |s: &str| Ok(ExecResult { code: 0, stdout: s.into(), stderr: String::new() });
            if cmd.starts_with("tmux has-session") {
                return Ok(ExecResult { code: i32::from(self.dead), stdout: String::new(), stderr: String::new() });
            }
            if cmd.starts_with("tmux capture-pane") {
                let mut q = self.screens.lock().unwrap();
                let s = if q.len() > 1 { q.pop_front().unwrap() } else { q.front().cloned().unwrap_or_else(|| "$ ".into()) };
                return ok(&s);
            }
            if cmd.starts_with("test -d") {
                return Ok(ExecResult { code: 1, stdout: String::new(), stderr: String::new() });
            }
            ok("")
        }
    }

    impl MockShell {
        fn commands(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    #[derive(Clone, Default)]
    struct MockLlm {
        replies: Arc<Mutex<VecDeque<Result<LlmResponse, LlmError>>>>,
        prompts: Arc<Mutex<Vec<(String, usize)>>>,
        context_limit: i64,
    }

    impl MockLlm {
        fn new(replies: Vec<Result<LlmResponse, LlmError>>) -> MockLlm {
            MockLlm { replies: Arc::new(Mutex::new(replies.into())), context_limit: 1_000_000, ..Default::default() }
        }
        fn prompts(&self) -> Vec<(String, usize)> {
            self.prompts.lock().unwrap().clone()
        }
    }

    fn reply(content: &str) -> Result<LlmResponse, LlmError> {
        Ok(LlmResponse {
            content: content.into(),
            model_name: Some("mock-model-v1".into()),
            usage: Some(Usage { prompt_tokens: 100, completion_tokens: 10, cache_tokens: 40, cost_usd: 0.01 }),
            ..Default::default()
        })
    }

    #[async_trait::async_trait]
    impl Llm for MockLlm {
        async fn call(&self, prompt: &str, history: &[Value], _: &Map<String, Value>, _: Option<&str>) -> Result<LlmResponse, LlmError> {
            self.prompts.lock().unwrap().push((prompt.to_string(), history.len()));
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| reply(r#"{"analysis": "", "plan": "", "commands": [], "task_complete": true}"#))
        }
        fn context_limit(&self) -> i64 {
            self.context_limit
        }
        fn output_limit(&self) -> Option<i64> {
            Some(4096)
        }
    }

    fn args(kwargs: Value, logs_dir: &std::path::Path) -> AgentArgs {
        let dir = std::env::temp_dir().join(format!("hbench-t2-task-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("environment")).unwrap();
        std::fs::write(dir.join("task.toml"), "[environment]\ndocker_image='x'\n").unwrap();
        std::fs::write(dir.join("instruction.md"), "x").unwrap();
        AgentArgs {
            load_trajectory: None,
            model_name: Some("openai/gpt-4o".into()),
            kwargs: serde_json::from_value(kwargs).unwrap(),
            extra_env: BTreeMap::new(),
            mcp_servers: vec![],
            skills: vec![],
            logs_dir: logs_dir.to_path_buf(),
            task: Arc::new(crate::task::Task::load(&dir).unwrap()),
        }
    }

    fn agent(kwargs: Value, llm: &MockLlm) -> (Terminus2, std::path::PathBuf) {
        let logs = std::env::temp_dir().join(format!("hbench-t2-{}", uuid::Uuid::new_v4())).join("my-trial").join("agent");
        std::fs::create_dir_all(&logs).unwrap();
        let a = args(kwargs, &logs);
        let opts = Options::parse(&a.kwargs).unwrap();
        let mut t = Terminus2::with_llm(a, opts, "openai/gpt-4o".into(), Box::new(llm.clone()));
        t.session = Some(t.new_session());
        (t, logs)
    }

    fn cmd_json(keys: &str, complete: bool) -> String {
        json!({"analysis": "looking", "plan": "act", "commands": [{"keystrokes": keys, "duration": 0.01}], "task_complete": complete})
            .to_string()
    }

    #[tokio::test]
    async fn loop_runs_commands_and_double_confirms_completion() {
        let llm = MockLlm::new(vec![
            reply(&cmd_json("ls\n", false)),
            reply(r#"{"analysis": "done", "plan": "", "commands": [], "task_complete": true}"#),
            reply(&cmd_json("echo ok\n", true)),
        ]);
        let (mut t, logs) = agent(json!({}), &llm);
        let sh = MockShell::default();
        let mut ctx = AgentContext::default();
        t.run_with("Create a file", &sh, &mut ctx).await.unwrap();

        let prompts = llm.prompts();
        assert_eq!(prompts.len(), 3);
        assert!(prompts[0].0.contains("Task Description:\nCreate a file\n\nCurrent terminal state:\nCurrent Terminal Screen:\n$ "));
        assert!(
            prompts[2].0.starts_with("Current terminal state:\n")
                && prompts[2].0.contains("Are you sure you want to mark the task as complete?")
        );
        assert_eq!(prompts[2].1, 4); // chat history grows by user+assistant per turn
        let cmds = sh.commands();
        assert!(cmds.iter().any(|c| c == "tmux send-keys -t terminus-2 -- 'ls\n'"));
        assert!(cmds.iter().any(|c| c == "tmux send-keys -t terminus-2 -- 'echo ok\n'"));
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens, ctx.n_cache_tokens), (Some(300), Some(30), Some(120)));
        assert!((ctx.cost_usd.unwrap() - 0.03).abs() < 1e-12);
        assert_eq!(ctx.metadata.as_ref().unwrap()["n_episodes"], 3);
        assert_eq!(ctx.model_usage.as_ref().unwrap()["mock-model-v1"].n_input_tokens, 300);

        let traj: Value = serde_json::from_str(&std::fs::read_to_string(logs.join("trajectory.json")).unwrap()).unwrap();
        assert_eq!(traj["schema_version"], "ATIF-v1.8");
        assert_eq!(traj["session_id"], "my-trial__agent");
        assert_eq!(
            traj["agent"],
            json!({"name": "terminus-2", "version": "2.0.0", "model_name": "openai/gpt-4o", "extra": {"parser": "json"}})
        );
        let steps = traj["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 4);
        assert_eq!((steps[0]["source"].as_str(), steps[1]["step_id"].as_i64()), (Some("user"), Some(2)));
        assert_eq!(steps[1]["message"], "Analysis: looking\nPlan: act");
        assert_eq!(
            steps[1]["tool_calls"],
            json!([{"tool_call_id": "call_0_1", "function_name": "bash_command", "arguments": {"keystrokes": "ls\n", "duration": 0.01}}])
        );
        assert_eq!(steps[1]["observation"]["results"][0]["source_call_id"], "call_0_1");
        assert_eq!(steps[1]["metrics"], json!({"prompt_tokens": 100, "completion_tokens": 10, "cached_tokens": 40, "cost_usd": 0.01}));
        assert_eq!(
            steps[2]["tool_calls"],
            json!([{"tool_call_id": "call_1_task_complete", "function_name": "mark_task_complete", "arguments": {}}])
        );
        assert!(steps[2]["observation"]["results"][0]["content"].as_str().unwrap().contains("Are you sure"));
        assert_eq!(steps[3]["tool_calls"].as_array().unwrap().len(), 2);
        let fm = &traj["final_metrics"];
        assert_eq!(
            (fm["total_prompt_tokens"].as_i64(), fm["total_completion_tokens"].as_i64(), fm["total_cached_tokens"].as_i64()),
            (Some(300), Some(30), Some(120))
        );
        assert!((fm["total_cost_usd"].as_f64().unwrap() - 0.03).abs() < 1e-12);
    }

    #[tokio::test]
    async fn parse_errors_feed_back_and_max_turns_stops() {
        let llm = MockLlm::new(vec![reply("I will just chat"), reply(&cmd_json("ls\n", false)), reply(&cmd_json("ls\n", false))]);
        let (mut t, logs) = agent(json!({"max_turns": 2}), &llm);
        let mut ctx = AgentContext::default();
        t.run_with("task", &MockShell::default(), &mut ctx).await.unwrap();
        let p = llm.prompts();
        assert_eq!(p.len(), 2);
        assert_eq!(
            p[1].0,
            "Previous response had parsing errors:\nERROR: No valid JSON found in response\nWARNINGS: - No valid JSON object found\n\nPlease fix these issues and provide a proper JSON response."
        );
        let traj: Value = serde_json::from_str(&std::fs::read_to_string(logs.join("trajectory.json")).unwrap()).unwrap();
        assert_eq!(traj["steps"][1]["message"], "I will just chat");
        assert_eq!(traj["steps"][1]["observation"]["results"][0]["content"], p[1].0.as_str());
    }

    #[tokio::test]
    async fn dead_session_ends_loop() {
        let llm = MockLlm::new(vec![]);
        let (mut t, _) = agent(json!({}), &llm);
        let sh = MockShell { dead: true, ..Default::default() };
        t.run_with("task", &sh, &mut AgentContext::default()).await.unwrap();
        assert!(llm.prompts().is_empty());
    }

    #[tokio::test]
    async fn context_overflow_triggers_summarization_handoff() {
        let llm = MockLlm::new(vec![
            reply(&cmd_json("ls\n", false)),
            Err(LlmError::ContextLength("prompt is too long".into())),
            reply("SUMMARY"),
            reply("QUESTIONS?"),
            reply("ANSWERS"),
            reply(r#"{"analysis": "", "plan": "", "commands": [], "task_complete": true}"#),
        ]);
        let (mut t, logs) = agent(json!({}), &llm);
        let mut ctx = AgentContext::default();
        t.run_with("orig task", &MockShell::default(), &mut ctx).await.unwrap();
        let p = llm.prompts();
        assert!(
            p[2].0.starts_with("You are about to hand off your work to another AI agent.") && p[2].0.contains("Original Task: orig task")
        );
        assert_eq!(p[2].1, 2); // summary subagent sees the (unwound) chat
        assert!(p[3].0.contains("**Summary from Previous Agent:**\nSUMMARY") && p[3].1 == 0);
        assert!(p[4].0.ends_with("one by one in detail:\n\nQUESTIONS?") && p[4].1 == 4);
        assert!(p[5].0.starts_with("Here are the answers the other agent provided.\n\nANSWERS\n\nContinue working"));
        assert_eq!(p[5].1, 3); // [first, question prompt, questions]
        for f in ["summary", "questions", "answers"] {
            let sub: Value =
                serde_json::from_str(&std::fs::read_to_string(logs.join(format!("trajectory.summarization-1-{f}.json"))).unwrap()).unwrap();
            assert_eq!(sub["agent"]["extra"]["summarization_index"], 1);
        }
        let traj: Value = serde_json::from_str(&std::fs::read_to_string(logs.join("trajectory.json")).unwrap()).unwrap();
        let steps = traj["steps"].as_array().unwrap();
        let sys = steps.iter().find(|s| s["source"] == "system").unwrap();
        assert_eq!(
            sys["observation"]["results"][0]["subagent_trajectory_ref"][1]["trajectory_path"],
            "trajectory.summarization-1-questions.json"
        );
        assert!(steps.iter().any(|s| s["source"] == "user" && s["message"].as_str().unwrap().starts_with("Here are the answers")));
        assert_eq!(ctx.metadata.as_ref().unwrap()["summarization_count"], 1);
        // Subagent usage is included in the totals.
        assert_eq!(ctx.n_input_tokens, Some(100 * 6));
    }

    #[tokio::test]
    async fn context_overflow_without_summarization_fails_with_kind() {
        let llm = MockLlm::new(vec![Err(LlmError::ContextLength("too long".into()))]);
        let (mut t, _) = agent(json!({"enable_summarize": false}), &llm);
        let e = t.run_with("task", &MockShell::default(), &mut AgentContext::default()).await.unwrap_err();
        assert_eq!(crate::env::error_kind(&e), "ContextLengthExceededError");
        assert_eq!(llm.prompts().len(), 1); // not retried
    }

    #[tokio::test]
    async fn output_overflow_reprompts_or_salvages() {
        let llm = MockLlm::new(vec![Err(LlmError::OutputLength { msg: "cut".into(), truncated: r#"{"analysis": "a""#.into() })]);
        let (mut t, _) = agent(json!({}), &llm);
        t.run_with("task", &MockShell::default(), &mut AgentContext::default()).await.unwrap();
        let p = llm.prompts();
        assert!(p[1].0.starts_with("ERROR!! NONE of the actions you just requested were performed because you exceeded 4096 tokens."));
        assert_eq!(p[1].1, 2); // truncated exchange was appended to history

        // Re-asks are bounded: the output-length error surfaces after MAX_OUTPUT_LENGTH_RETRIES.
        let cut = || Err(LlmError::OutputLength { msg: "cut".into(), truncated: "x".into() });
        let llm = MockLlm::new((0..=MAX_OUTPUT_LENGTH_RETRIES + 1).map(|_| cut()).collect());
        let (mut t, _) = agent(json!({}), &llm);
        let e = t.run_with("task", &MockShell::default(), &mut AgentContext::default()).await.unwrap_err();
        assert_eq!(crate::env::error_kind(&e), "OutputLengthExceededError");
        assert_eq!(llm.prompts().len(), MAX_OUTPUT_LENGTH_RETRIES + 1);

        let llm = MockLlm::new(vec![
            Err(LlmError::OutputLength { msg: "cut".into(), truncated: format!("{XML_OK}<response>trunc") }),
            reply(XML_OK),
        ]);
        let (mut t, _) = agent(json!({"parser_name": "xml"}), &llm);
        let sh = MockShell::default();
        t.run_with("task", &sh, &mut AgentContext::default()).await.unwrap();
        assert_eq!(llm.prompts().len(), 2);
        assert!(llm.prompts()[1].0.contains("include <task_complete>true</task_complete> again."));
        assert!(sh.commands().iter().any(|c| c == "tmux send-keys -t terminus-2 -- 'cd project\n'"));
    }

    #[tokio::test]
    async fn proactive_summarization_when_context_is_nearly_full() {
        let mut llm = MockLlm::new(vec![
            reply(&cmd_json("ls\n", false)),
            reply("S"),
            reply("Q"),
            reply("A"),
            reply(&cmd_json("pwd\n", true)),
            reply(&cmd_json("pwd\n", true)),
        ]);
        llm.context_limit = 9000; // after one exchange, free tokens drop below the 8000 threshold
        let (mut t, _) = agent(json!({}), &llm);
        let mut ctx = AgentContext::default();
        t.run_with(&"x".repeat(2000), &MockShell::default(), &mut ctx).await.unwrap();
        let p = llm.prompts();
        assert!(p[1].0.starts_with("You are about to hand off"));
        assert!(p[4].0.starts_with("Here are the answers"));
        assert!(ctx.metadata.as_ref().unwrap()["summarization_count"].as_i64().unwrap() >= 1);
    }

    #[test]
    fn options_validation() {
        let o = Options::parse(
            &serde_json::from_value(
                json!({"max_episodes": "5", "parser_name": "xml", "temperature": 0.2, "trajectory_config": {"raw_content": true}}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            (o.max_episodes, o.parser, o.temperature, o.raw_content, o.record_terminal_session),
            (Some(5), ParserKind::Xml, Some(0.2), true, true)
        );
        let o = Options::parse(&serde_json::from_value(json!({"max_turns": 3, "max_episodes": 9, "episodes": 7})).unwrap()).unwrap();
        assert_eq!(o.max_episodes, Some(3));
        let err = |v: Value| Options::parse(&serde_json::from_value(v).unwrap()).unwrap_err().to_string();
        assert!(err(json!({"max_turns": "lots"})).contains("max_turns: Input should be a valid integer"));
        assert!(err(json!({"parser_name": "yaml"})).contains("Unknown parser_name: yaml"));
        assert!(err(json!({"reasoning_effort": "huge"})).contains("reasoning_effort"));
        assert!(err(json!({"session_id_headers": ["Invalid Header"]})).contains("Invalid HTTP header name"));
        assert!(err(json!({"llm_backend": "tinker"})).contains("Tinker"));
        assert!(err(json!({"tmux_pane_width": 0})).contains("positive"));

        let logs = std::env::temp_dir().join("hbench-t2-new");
        let mut a = args(json!({}), &logs);
        a.model_name = None;
        assert!(Terminus2::new(a).err().unwrap().to_string().contains("model_name is required"));
        assert!(Terminus2::new(args(json!({"temperature": 0.5}), &logs)).is_ok());
    }

    #[test]
    fn session_id_headers_and_reset() {
        let llm = MockLlm::new(vec![]);
        let (mut t, _) = agent(
            json!({"session_id_headers": ["Modal-Session-ID"], "llm_call_kwargs": {"extra_headers": {"X-Custom": "unchanged"}}}),
            &llm,
        );
        t.reset_per_run_state();
        assert_eq!(t.session_id, "my-trial__agent");
        assert_eq!(t.call_kwargs["extra_headers"], json!({"Modal-Session-ID": "my-trial__agent", "X-Custom": "unchanged"}));
        let (mut t, _) = agent(json!({"session_id": "constructor-session"}), &llm);
        t.reset_per_run_state();
        assert_eq!(t.session_id, "constructor-session");
    }

    fn tmux() -> Tmux {
        Tmux {
            name: "test-session".into(),
            logging_path: "/tmp/tmux.log".into(),
            recording_path: None,
            width: 160,
            height: 40,
            extra_env: BTreeMap::new(),
            previous_buffer: None,
        }
    }

    #[test]
    fn tmux_commands() {
        let t = tmux();
        assert_eq!(
            t.send_keys_command(&["echo hello world".into(), "Enter".into()]),
            "tmux send-keys -t test-session -- 'echo hello world' Enter"
        );
        assert_eq!(t.send_keys_command(&["-x".into()]), "tmux send-keys -t test-session -- -x");
        let keys: Vec<String> = (0..96).map(|i| format!("key{i:04}{}", "x".repeat(490))).collect();
        let batches = t.batch_keys(&keys).unwrap();
        assert!(batches.len() >= 2 && batches.iter().all(|b| t.send_keys_command(b).len() <= MAX_COMMAND_LENGTH));
        assert_eq!(batches.concat(), keys);
        assert!(t.batch_keys(&["x".repeat(MAX_COMMAND_LENGTH * 2)]).is_err());
        assert!(
            t.key_requires_paste(&"é".repeat(MAX_COMMAND_LENGTH * 9 / 16))
                && !t.key_requires_paste(&"x".repeat(MAX_COMMAND_LENGTH * 9 / 16))
        );
        assert!(t.key_requires_paste(&"a'".repeat(MAX_COMMAND_LENGTH / 3)));
        let mut t = tmux();
        t.extra_env.insert("FOO".into(), "a b".into());
        assert_eq!(
            t.start_command(),
            "export TERM=xterm-256color && export SHELL=/bin/bash && tmux new-session -e 'FOO=a b' -x 160 -y 40 -d -s test-session 'bash --login' \\; pipe-pane -t test-session 'cat > /tmp/tmux.log'"
        );
    }

    #[tokio::test]
    async fn tmux_paste_and_blocking() {
        let t = tmux();
        let sh = MockShell::default();
        let long = format!("cat > /app/answer.txt << 'EOF'\n{}EOF\n", "answer line\n".repeat(MAX_COMMAND_LENGTH / 6));
        t.send_keys(&sh, vec!["echo before".into(), long.clone(), "Enter".into()], false, 0.0, 180.0).await.unwrap();
        let cmds = sh.commands();
        let staged: String = cmds.iter().filter_map(|c| c.strip_prefix("printf %s ")).map(|c| c.split(' ').next().unwrap()).collect();
        assert_eq!(String::from_utf8(base64::engine::general_purpose::STANDARD.decode(staged).unwrap()).unwrap(), long);
        let paste = cmds.iter().position(|c| c.contains("paste-buffer")).unwrap();
        assert_eq!(cmds[0], "tmux send-keys -t test-session -- 'echo before'");
        assert!(cmds[paste + 1].starts_with("rm -f ") && cmds.last().unwrap() == "tmux send-keys -t test-session -- Enter");

        let sh = MockShell::default();
        assert!(t.send_keys(&sh, vec!["make\n".into()], true, 0.0, 30.0).await.unwrap());
        assert_eq!(
            sh.commands(),
            vec!["tmux send-keys -t test-session -- make '; tmux wait -S done' Enter", "timeout 30.0s tmux wait done"]
        );
    }

    #[tokio::test]
    async fn tmux_incremental_output() {
        let mut t = tmux();
        let sh = MockShell::default();
        *sh.screens.lock().unwrap() = VecDeque::from(vec!["$ ".to_string(), "$ ".into(), "$ ls\nfile\n$ ".into(), "$ ls\nfile\n$ ".into()]);
        assert_eq!(t.get_incremental_output(&sh).await.unwrap(), "Current Terminal Screen:\n$ ");
        assert_eq!(t.get_incremental_output(&sh).await.unwrap(), "New Terminal Output:\n$ ls\nfile\n$ ");
        assert_eq!(t.get_incremental_output(&sh).await.unwrap(), "New Terminal Output:\n\n$ ");
    }
}
