//! Kimi Code (port of upstream's agents/installed/kimi_code.py + kimi_code_trajectory.py usage totals).

use super::kimi_cli::{loads_lenient, validate_kwargs};
use super::{exec_agent, exec_root};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx, pricing};
use crate::env::quote;
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

const PACKAGE_NAME: &str = "@moonshot-ai/kimi-code";
const KIMI_CODE_HOME: &str = "/logs/agent/.kimi-code";
const OUTPUT_PATH: &str = "/logs/agent/kimi-code.txt";
const NODE_PATH_SETUP: &str = "if [ -s \"$HOME/.nvm/nvm.sh\" ]; then . \"$HOME/.nvm/nvm.sh\"; fi; export PATH=\"$HOME/.local/bin:$PATH\"; ";
const NVM_NODE_INSTALL: &str = "curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh | env -u NODE_VERSION bash && \
export NVM_DIR=\"$HOME/.nvm\" && \\. \"$NVM_DIR/nvm.sh\" || true && \
command -v nvm &>/dev/null || { echo 'Error: NVM failed to load' >&2; exit 1; } && nvm install 22 && nvm alias default 22 && npm -v";
const SYSTEM_INSTALL: &str = r#"if command -v apk >/dev/null 2>&1; then
  if ! command -v node >/dev/null 2>&1 || \
     ! command -v npm >/dev/null 2>&1; then
    apk add --no-cache ca-certificates nodejs npm
  fi
elif ! command -v curl >/dev/null 2>&1; then
  if command -v apt-get >/dev/null 2>&1; then
    apt-get update
    apt-get install -y curl
  elif command -v dnf >/dev/null 2>&1; then
    dnf install -y curl
  elif command -v yum >/dev/null 2>&1; then
    yum install -y curl
  else
    echo "curl is required to install Node.js with nvm" >&2
    exit 1
  fi
fi"#;

pub struct KimiCode {
    args: AgentArgs,
    version: Option<String>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    validate_kwargs("kimi-code", &args, &[])?;
    Ok(Box::new(KimiCode { version: args.version_kw(), args }))
}

/// Last whitespace token of the last non-empty line ("kimi 0.28.1" -> "0.28.1").
fn parse_version(stdout: &str) -> String {
    stdout.lines().map(str::trim).filter(|l| !l.is_empty()).last().and_then(|l| l.split_whitespace().last()).unwrap_or("").to_string()
}

impl KimiCode {
    fn runtime_env(&self) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> =
            [("KIMI_CODE_HOME", KIMI_CODE_HOME), ("KIMI_DISABLE_TELEMETRY", "true"), ("KIMI_CODE_NO_AUTO_UPDATE", "true"), ("NO_COLOR", "true")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        if let Some(m) = &self.args.model_name {
            env.insert("KIMI_MODEL_NAME".into(), m.clone());
        }
        env
    }

    fn mcp_config_json(&self) -> Option<String> {
        if self.args.mcp_servers.is_empty() {
            return None;
        }
        let mut servers = serde_json::Map::new();
        for s in &self.args.mcp_servers {
            let v = match s.transport() {
                "stdio" => json!({"command": s.command, "args": s.args}),
                "sse" => json!({"url": s.url, "transport": "sse"}),
                _ => json!({"url": s.url}),
            };
            servers.insert(s.name.clone(), v);
        }
        Some(json!({"mcpServers": servers}).to_string())
    }

    fn skills_dir(&self) -> Option<String> {
        self.args.task.config.environment.skills_dir.clone().or_else(|| (!self.args.skills.is_empty()).then(|| "/hbench/skills".into()))
    }

    /// (command, env) of the headless stream-json invocation.
    fn run_command(&self, instruction: &str, resume: bool) -> (String, BTreeMap<String, String>) {
        let resume_flag = if resume { "--continue " } else { "" };
        let skills_flag = self.skills_dir().map(|d| format!("--skills-dir {} ", quote(&d))).unwrap_or_default();
        let var = format!("hbench_kimi_code_instruction_{}", uuid::Uuid::new_v4().simple());
        let upper = var.to_uppercase();
        let mut env = self.runtime_env();
        env.insert(upper.clone(), instruction.to_string());
        let cmd = format!(
            "{NODE_PATH_SETUP}{var}=\"${upper}\"; unset {upper}; kimi {resume_flag}{skills_flag}--prompt \"${var}\" \
             --output-format stream-json </dev/null 2>&1 | tee {OUTPUT_PATH}"
        );
        (cmd, env)
    }
}

#[async_trait::async_trait]
impl Agent for KimiCode {
    fn name(&self) -> &str {
        "kimi-code"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        cx.env.exec_root("[ -d /installed-agent ] || mkdir -p /installed-agent", None).await?;
        exec_root(cx, SYSTEM_INSTALL, &BTreeMap::from([("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())])).await?;
        let spec = self.version.as_ref().map(|v| format!("@{v}")).unwrap_or_else(|| "@latest".into());
        exec_agent(
            cx,
            &format!(
                "set -euo pipefail\nif command -v apk >/dev/null 2>&1; then\n  node --version\n  npm --version\nelse\n  {NVM_NODE_INSTALL}\nfi\n\n\
                 mkdir -p \"$HOME/.local\"\nnpm install --global --prefix \"$HOME/.local\" {PACKAGE_NAME}{spec}\n{NODE_PATH_SETUP}kimi --version"
            ),
            &BTreeMap::new(),
        )
        .await?;
        if self.version.is_none() {
            let r = cx.exec(&format!("{NODE_PATH_SETUP}kimi --version"), &BTreeMap::new(), None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(parse_version(&r.stdout));
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        if let Some(cfg) = self.mcp_config_json() {
            let cmd = format!("mkdir -p \"$KIMI_CODE_HOME\" && printf '%s' {} > \"$KIMI_CODE_HOME/mcp.json\"", quote(&cfg));
            exec_agent(cx, &cmd, &self.runtime_env()).await?;
        }
        let (cmd, env) = self.run_command(&instruction, resume);
        exec_agent(cx, &cmd, &env).await.map(|_| ())
    }

    fn populate_context_post_run(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        if let Some(c) = usage(logs_dir, self.args.model_name.as_deref()) {
            *ctx = c;
        }
    }
}

// ---- usage totals (kimi_code_trajectory.build_trajectory's final_metrics) ----

#[derive(Default)]
struct Step {
    agent: bool,
    turn: u32,
    text: String,
    calls: Vec<String>,
    usage: Option<Value>,
    model: Option<String>,
}

#[derive(Default)]
struct Parsed {
    steps: Vec<Step>,
    session_id: Option<String>,
    model: Option<String>,
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    let Ok(bytes) = std::fs::read(path) else { return vec![] };
    String::from_utf8_lossy(&bytes).lines().filter_map(loads_lenient).filter(Value::is_object).collect()
}

fn texts(parts: &Value) -> String {
    parts.as_array().into_iter().flatten().filter_map(|p| p["text"].as_str()).collect()
}

fn id_str(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// `sessions/*/session_*/agents/main/wire.jsonl` by session id: index-ordered first, then newest.
fn wire_candidates(logs_dir: &Path, session_id: Option<&str>) -> Vec<(String, PathBuf)> {
    let home = logs_dir.join(".kimi-code");
    let mut by_sid: BTreeMap<String, PathBuf> = BTreeMap::new();
    for ws in std::fs::read_dir(home.join("sessions")).into_iter().flatten().flatten() {
        for sess in std::fs::read_dir(ws.path()).into_iter().flatten().flatten() {
            let name = sess.file_name().to_string_lossy().to_string();
            let wire = sess.path().join("agents/main/wire.jsonl");
            if name.starts_with("session_") && wire.is_file() {
                by_sid.insert(name, wire);
            }
        }
    }
    let index = read_jsonl(&home.join("session_index.jsonl"));
    let mut ordered = vec![];
    for sid in session_id.map(String::from).into_iter().chain(index.iter().rev().filter_map(|r| r["sessionId"].as_str().map(String::from))) {
        if let Some(p) = by_sid.remove(&sid) {
            ordered.push((sid, p));
        }
    }
    let mtime = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut rest: Vec<_> = by_sid.into_iter().collect();
    rest.sort_by_key(|(_, p)| std::cmp::Reverse(mtime(p)));
    ordered.extend(rest);
    ordered
}

fn parse_stream_json(path: &Path) -> Parsed {
    let mut p = Parsed::default();
    for o in read_jsonl(path) {
        match o["role"].as_str() {
            Some("meta") if o["type"] == "session.resume_hint" => p.session_id = o["session_id"].as_str().map(String::from),
            Some("assistant") => p.steps.push(Step {
                agent: true,
                text: o["content"].as_str().unwrap_or("").to_string(),
                calls: o["tool_calls"].as_array().into_iter().flatten().map(|tc| id_str(&tc["id"])).collect(),
                ..Default::default()
            }),
            _ => {}
        }
    }
    p
}

fn parse_wire(path: &Path, session_id: String) -> Parsed {
    let mut p = Parsed { session_id: Some(session_id), ..Default::default() };
    let mut current: Option<Step> = None;
    let mut turn = 0;
    let close = |cur: &mut Option<Step>, steps: &mut Vec<Step>, turn: u32| {
        if let Some(mut s) = cur.take() {
            s.turn = turn;
            steps.push(s);
        }
    };
    for o in read_jsonl(path) {
        match o["type"].as_str().unwrap_or("") {
            "turn.prompt" => {
                close(&mut current, &mut p.steps, turn);
                turn += 1;
                p.steps.push(Step { turn, text: texts(&o["input"]), ..Default::default() });
            }
            "llm.request" => {
                let m = o["model"].as_str().map(String::from);
                if m.is_some() {
                    p.model = m.clone();
                }
                if let Some(s) = current.as_mut() {
                    s.model = m;
                }
            }
            "usage.record" => {
                if let Some(s) = current.as_mut().filter(|s| s.usage.is_none()) {
                    s.usage = o.get("usage").cloned().filter(|u| !u.is_null());
                }
            }
            "context.append_message" if o["message"]["origin"]["kind"] == "injection" => {
                close(&mut current, &mut p.steps, turn);
                p.steps.push(Step { turn, text: texts(&o["message"]["content"]), ..Default::default() });
            }
            "turn.ended" => close(&mut current, &mut p.steps, turn),
            "context.append_loop_event" => {
                let ev = &o["event"];
                let et = ev["type"].as_str().unwrap_or("");
                if et == "step.begin" {
                    close(&mut current, &mut p.steps, turn);
                    current = Some(Step { agent: true, ..Default::default() });
                    continue;
                }
                let step = current.get_or_insert_with(|| Step { agent: true, ..Default::default() });
                match et {
                    "content.part" => {
                        let part = &ev["part"];
                        let think = part["type"] == "think" && part["think"].as_str().is_some_and(|t| !t.is_empty());
                        if !think && let Some(t) = part["text"].as_str() {
                            step.text.push_str(t);
                        }
                    }
                    "tool.call" => step.calls.push(id_str(&ev["toolCallId"])),
                    "step.end" => {
                        if ev["usage"].as_object().is_some_and(|u| !u.is_empty()) {
                            step.usage = Some(ev["usage"].clone());
                        }
                        close(&mut current, &mut p.steps, turn);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    close(&mut current, &mut p.steps, turn);
    p
}

/// Does the journal's current turn contain what this run's stdout shows?
fn same_invocation(wire: &Parsed, stream: &Parsed) -> bool {
    let Some(last_stream) = stream.steps.last() else { return true };
    let last_turn = wire.steps.iter().map(|s| s.turn).max().unwrap_or(0);
    let cur: Vec<&Step> = wire.steps.iter().filter(|s| s.turn == last_turn && s.agent).collect();
    let stream_ids: HashSet<&String> = stream.steps.iter().flat_map(|s| &s.calls).collect();
    let wire_ids: HashSet<&String> = cur.iter().flat_map(|s| &s.calls).collect();
    if !stream_ids.is_empty() || !wire_ids.is_empty() {
        return !stream_ids.is_disjoint(&wire_ids);
    }
    cur.last().is_some_and(|s| s.text == last_stream.text)
}

/// upstream's `_pricing_key`: `moonshotai/x` and bare `x` are priced as `moonshot/x`; other prefixes as-is.
fn pricing_key(model: &str) -> Option<String> {
    let (provider, bare) = model.rsplit_once('/').unwrap_or(("", model));
    let mut keys = vec![model.to_string()];
    if ["", "moonshot", "moonshotai"].contains(&provider) {
        keys.extend([bare.to_string(), format!("moonshot/{bare}")]);
    }
    keys.into_iter().find(|k| pricing::model_cost(k).is_some())
}

/// Token totals of the current turn (earlier `--continue` turns are copied context);
/// `None` when nothing was parsed or no usage was observed (tokens stay null, as upstream).
/// Cost is upstream's all-or-nothing LiteLLM estimate (`estimate_cost_usd`).
pub fn usage(logs_dir: &Path, model_name: Option<&str>) -> Option<AgentContext> {
    let stream_path = logs_dir.join("kimi-code.txt");
    let has_stream = stream_path.is_file();
    let stream = if has_stream { parse_stream_json(&stream_path) } else { Parsed::default() };
    let mut candidates = wire_candidates(logs_dir, stream.session_id.as_deref());
    if let Some(sid) = &stream.session_id {
        candidates.retain(|(s, _)| s == sid);
    } else if has_stream && stream.steps.is_empty() {
        candidates.clear();
    }
    let parsed = candidates
        .into_iter()
        .map(|(sid, p)| parse_wire(&p, sid))
        .find(|w| !w.steps.is_empty() && (w.session_id == stream.session_id || same_invocation(w, &stream)))
        .unwrap_or(stream);
    let last_turn = parsed.steps.iter().map(|s| s.turn).max()?;
    let used: Vec<(&Value, Option<&str>)> = parsed
        .steps
        .iter()
        .filter(|s| s.agent && s.turn == last_turn)
        .filter_map(|s| s.usage.as_ref().filter(|u| u.as_object().is_some_and(|o| !o.is_empty())).map(|u| (u, s.model.as_deref().or(parsed.model.as_deref()).or(model_name))))
        .collect();
    if used.is_empty() {
        return None;
    }
    let (mut prompt, mut out, mut cache) = (0, 0, 0);
    let (mut cost, mut priceable) = (None::<f64>, true);
    for (u, model) in used {
        let n = |k: &str| u[k].as_i64().unwrap_or(0);
        let (p, o) = (n("inputOther") + n("inputCacheRead") + n("inputCacheCreation"), n("output"));
        prompt += p;
        out += o;
        cache += n("inputCacheRead");
        let Some(model) = model.filter(|_| p != 0 || o != 0) else { continue };
        match pricing_key(model).and_then(|k| pricing::cost_per_token(&k, p, o, n("inputCacheCreation"), n("inputCacheRead"))) {
            Some((a, b)) => cost = Some(cost.unwrap_or(0.0) + a + b),
            None => priceable = false,
        }
    }
    Some(AgentContext { n_input_tokens: Some(prompt), n_output_tokens: Some(out), n_cache_tokens: Some(cache), cost_usd: cost.filter(|_| priceable), ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::kimi_cli::tests::args;

    const SID: &str = "session_8192ee32-068a-47f4-a145-829f33b1b234";
    const CALL: &str = "call_419963a4c28a47d2bb9d7315";
    // upstream's real kimi-code 2.1.1 fixture (bulky metadata/tool-schema lines dropped).
    const WIRE: &str = r#"{"type": "metadata", "protocol_version": "1.5", "created_at": 1790890457332}
{"type": "turn.prompt", "agentId": "main", "input": [{"type": "text", "text": "Create a file called `hello.txt`"}], "origin": {"kind": "user"}, "turnId": 0, "time": 1790890457382}
{"type": "context.append_message", "agentId": "main", "message": {"role": "user", "content": [{"type": "text", "text": "<system-reminder>date</system-reminder>"}], "toolCalls": [], "origin": {"kind": "injection", "variant": "date_change"}}, "time": 1790890457392}
{"type": "context.append_message", "agentId": "main", "message": {"role": "user", "content": [{"type": "text", "text": "<system-reminder>auto</system-reminder>"}], "toolCalls": [], "origin": {"kind": "injection", "variant": "permission_mode"}}, "time": 1790890457393}
{"type": "context.append_loop_event", "agentId": "main", "event": {"type": "step.begin", "turnId": "0", "step": 1}, "time": 1790890457398}
{"type": "llm.request", "agentId": "main", "kind": "loop", "provider": "openai", "model": "moonshotai/kimi-k3", "thinkingEffort": "max", "time": 1790890457404}
{"type": "usage.record", "agentId": "main", "model": "__kimi_env_model__", "usage": {"inputOther": 19745, "output": 86, "inputCacheRead": 0, "inputCacheCreation": 0}, "usageScope": "turn", "time": 1790890462008}
{"type": "context.append_loop_event", "agentId": "main", "event": {"type": "content.part", "turnId": "0", "step": 1, "part": {"type": "think", "think": "Simple task: Use Write tool.", "reasoningKey": "reasoning_content"}}, "time": 1790890462012}
{"type": "context.append_loop_event", "agentId": "main", "event": {"type": "tool.call", "turnId": "0", "step": 1, "toolCallId": "call_419963a4c28a47d2bb9d7315", "name": "Write", "args": {"path": "/app/hello.txt", "content": "Hello, world!\n"}}, "time": 1790890462047}
{"type": "context.append_loop_event", "agentId": "main", "event": {"type": "tool.result", "toolCallId": "call_419963a4c28a47d2bb9d7315", "result": {"output": "Wrote 14 bytes to /app/hello.txt", "durationMs": 5}}, "time": 1790890462055}
{"type": "context.append_loop_event", "agentId": "main", "event": {"type": "step.end", "turnId": "0", "step": 1, "finishReason": "tool_use", "usage": {"inputOther": 19745, "output": 86, "inputCacheRead": 0, "inputCacheCreation": 0}}, "time": 1790890462057}
{"type": "context.append_loop_event", "agentId": "main", "event": {"type": "step.begin", "turnId": "0", "step": 2}, "time": 1790890462060}
{"type": "usage.record", "agentId": "main", "model": "__kimi_env_model__", "usage": {"inputOther": 126, "output": 29, "inputCacheRead": 19744, "inputCacheCreation": 0}, "usageScope": "turn", "time": 1790890463661}
{"type": "context.append_loop_event", "agentId": "main", "event": {"type": "content.part", "turnId": "0", "step": 2, "part": {"type": "text", "text": "Created `/app/hello.txt` with the content:\n\n```\nHello, world!\n```"}}, "time": 1790890463664}
{"type": "context.append_loop_event", "agentId": "main", "event": {"type": "step.end", "turnId": "0", "step": 2, "finishReason": "end_turn", "usage": {"inputOther": 126, "output": 29, "inputCacheRead": 19744, "inputCacheCreation": 0}}, "time": 1790890463667}
{"type": "turn.ended", "agentId": "main", "turnId": 0, "reason": "completed", "durationMs": 6287, "time": 1790890463669}
"#;
    const STREAM: &str = r#"{"role":"meta","type":"system.version","version":"2.1.1"}
{"role":"assistant","tool_calls":[{"type":"function","id":"call_419963a4c28a47d2bb9d7315","function":{"name":"Write","arguments":"{\"path\":\"/app/hello.txt\",\"content\":\"Hello, world!\\n\"}"}}]}
{"role":"tool","tool_call_id":"call_419963a4c28a47d2bb9d7315","content":"Wrote 14 bytes to /app/hello.txt"}
{"role":"assistant","content":"Created `/app/hello.txt` with the content:\n\n```\nHello, world!\n```"}
{"role":"meta","type":"session.resume_hint","session_id":"session_8192ee32-068a-47f4-a145-829f33b1b234","command":"kimi -r x","content":"x"}
"#;

    fn logs(wire: Option<&str>, stream: Option<&str>) -> PathBuf {
        let root = std::env::temp_dir().join(format!("hbench-kimi-code-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        if let Some(s) = stream {
            std::fs::write(root.join("kimi-code.txt"), s).unwrap();
        }
        if let Some(w) = wire {
            add_session(&root, SID, w);
            std::fs::write(root.join(".kimi-code/session_index.jsonl"), format!("{{\"sessionId\":\"{SID}\"}}\n")).unwrap();
        }
        root
    }

    fn add_session(root: &Path, sid: &str, wire: &str) {
        let d = root.join(".kimi-code/sessions/wd_app_f53b52ad6d21").join(sid).join("agents/main");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("wire.jsonl"), wire).unwrap();
    }

    fn tokens(c: Option<AgentContext>) -> Option<(i64, i64, i64)> {
        c.map(|c| (c.n_input_tokens.unwrap(), c.n_output_tokens.unwrap(), c.n_cache_tokens.unwrap()))
    }

    #[test]
    fn wire_usage_totals() {
        assert_eq!(tokens(usage(&logs(Some(WIRE), Some(STREAM)), None)), Some((39615, 115, 19744)));
        // run killed before the final step.end: usage.record keeps the tokens
        let cut = WIRE.lines().take(13).collect::<Vec<_>>().join("\n");
        assert_eq!(tokens(usage(&logs(Some(&cut), Some(STREAM)), None)).unwrap().1, 86 + 29);
        // missing stream falls back to the journal
        assert_eq!(tokens(usage(&logs(Some(WIRE), None), None)), Some((39615, 115, 19744)));
    }

    #[test]
    fn litellm_cost_estimate() {
        // moonshotai/kimi-k3 has no LiteLLM entry: cost stays None.
        assert_eq!(usage(&logs(Some(WIRE), Some(STREAM)), Some("moonshot/kimi-k2.6")).unwrap().cost_usd, None);
        // moonshotai/x is priced as moonshot/x: in 6e-7, out 2.5e-6, cache read 1.5e-7.
        let wire = WIRE.replace("moonshotai/kimi-k3", "moonshotai/kimi-k2-0905-preview");
        let c = usage(&logs(Some(&wire), Some(STREAM)), None).unwrap().cost_usd.unwrap();
        let want = 19745.0 * 6e-7 + 86.0 * 2.5e-6 + 126.0 * 6e-7 + 19744.0 * 1.5e-7 + 29.0 * 2.5e-6;
        assert!((c - want).abs() < 1e-12, "{c} vs {want}");
        assert_eq!(pricing_key("kimi-k2-0905-preview").as_deref(), Some("moonshot/kimi-k2-0905-preview"));
        assert_eq!(pricing_key("openrouter/kimi-k2.5"), None);
    }

    #[test]
    fn resumed_session_counts_only_current_turn() {
        let small = r#"{"inputOther": 10, "output": 5, "inputCacheRead": 0, "inputCacheCreation": 0}"#;
        let second: String = WIRE
            .lines()
            .skip(1)
            .map(|l| {
                let l = l.replace(CALL, "call_second");
                if l.contains("\"usage\"") {
                    let i = l.find("\"usage\": {").unwrap();
                    let j = i + l[i..].find('}').unwrap() + 1;
                    format!("{}\"usage\": {small}{}", &l[..i], &l[j..])
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let root = logs(Some(&format!("{WIRE}{second}\n")), Some(&STREAM.replace(CALL, "call_second")));
        assert_eq!(tokens(usage(&root, None)), Some((20, 10, 0)));
    }

    #[test]
    fn journal_selection() {
        // stream-json only: no usage -> tokens stay null
        assert!(usage(&logs(None, Some(STREAM)), None).is_none());
        let no_hint: String = STREAM.lines().filter(|l| !l.contains("resume_hint")).map(|l| format!("{l}\n")).collect();
        // stale journal (different tool call) is not this run's
        assert!(usage(&logs(Some(WIRE), Some(&no_hint.replace(CALL, "call_new"))), None).is_none());
        // interrupted run without hint still uses its own journal
        assert!(usage(&logs(Some(WIRE), Some(&no_hint)), None).is_some());
        // unindexed current journal beats indexed stale one
        let root = logs(Some(WIRE), Some(&no_hint.replace(CALL, "call_new")));
        add_session(&root, "session_new", &WIRE.replace(CALL, "call_new").replace("19745", "1"));
        assert_eq!(tokens(usage(&root, None)).unwrap().0, 1 + 126 + 19744);
        // empty stream without hint does not adopt an older journal
        assert!(usage(&logs(Some(WIRE), Some("{\"role\":\"meta\",\"type\":\"system.version\",\"version\":\"2.1.1\"}\n")), None).is_none());
        assert!(usage(&logs(None, None), None).is_none());
    }

    #[test]
    fn command_env_and_options() {
        let a = KimiCode { version: None, args: args(json!({}), &[("KIMI_MODEL_API_KEY", "sk")], Some("kimi/kimi-for-coding")) };
        let (cmd, mut env) = a.run_command("solve the task", false);
        let var = cmd.split("hbench_kimi_code_instruction_").nth(1).unwrap()[..32].to_string();
        let upper = format!("HBENCH_KIMI_CODE_INSTRUCTION_{}", var.to_uppercase());
        assert!(cmd.contains(&format!("unset {upper}")) && cmd.contains(&format!("kimi --prompt \"$hbench_kimi_code_instruction_{var}\"")));
        assert!(!cmd.contains("solve the task") && cmd.contains("2>&1 | tee /logs/agent/kimi-code.txt") && cmd.contains("--output-format stream-json"));
        assert_eq!(env.remove(&upper).as_deref(), Some("solve the task"));
        assert_eq!(env, a.runtime_env());
        assert_eq!(env["KIMI_MODEL_NAME"], "kimi/kimi-for-coding");
        assert_eq!(env.keys().filter(|k| k.starts_with("KIMI_MODEL_")).count(), 1);
        assert!(a.run_command("x", true).0.contains("kimi --continue --prompt"));
        assert_eq!(parse_version("kimi 0.28.1\n"), "0.28.1");
        assert_eq!(parse_version("\n"), "");
        assert!(create(args(json!({"bogus": 1}), &[], None)).is_err());
        assert!(create(args(json!({"version": "0.28.1"}), &[], None)).is_ok());
    }

    #[test]
    fn mcp_config() {
        let mut a = args(json!({}), &[], None);
        let srv = |name: &str, transport: &str, url: Option<&str>, cmd: Option<&str>| crate::task::McpServer {
            name: name.into(),
            transport: transport.into(),
            url: url.map(String::from),
            command: cmd.map(String::from),
            args: if cmd.is_some() { vec!["-y".into(), "server".into()] } else { vec![] },
        };
        a.mcp_servers = vec![srv("local", "stdio", None, Some("npx")), srv("remote", "streamable-http", Some("https://e/mcp"), None), srv("legacy", "sse", Some("https://e/sse"), None)];
        let k = KimiCode { version: None, args: a };
        let v: Value = serde_json::from_str(&k.mcp_config_json().unwrap()).unwrap();
        assert_eq!(
            v,
            json!({"mcpServers": {"local": {"command": "npx", "args": ["-y", "server"]}, "remote": {"url": "https://e/mcp"}, "legacy": {"url": "https://e/sse", "transport": "sse"}}})
        );
    }
}
