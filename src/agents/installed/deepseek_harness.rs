//! DeepSeek Harness (`dsh`, npm `@deepseek-ai/dsh`): DeepSeek's own coding-agent harness, run in its
//! one-shot `headless` profile with the `--json` event stream.
//! Follows the installed-agent conventions of the other agents.

use super::common::parse_jsonl;
use super::common::{detect_version, installed_dir, nvm_node_install_snippet, opts};
use super::pins;
use super::{classify, connection, ensure_system_dependencies, exec_agent, upload_text};
use crate::agents::model_connection::Spec;
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx, pricing};
use crate::atif;
use crate::env::{ErrorKind, kind_err, quote};
use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

const NAME: &str = "deepseek-harness";
const SPEC: Spec = Spec {
    default_provider: Some("deepseek"),
    api_key_envs: &["DEEPSEEK_API_KEY"],
    base_url_envs: &["DEEPSEEK_BASE_URL"],
    passthrough: false,
};
/// `anthropic/<id>` runs through dsh's bundled pi-ai adapter, which speaks native Anthropic Messages.
const ANTHROPIC: Spec = Spec {
    default_provider: Some("anthropic"),
    api_key_envs: &["ANTHROPIC_API_KEY"],
    base_url_envs: &["ANTHROPIC_BASE_URL"],
    passthrough: false,
};
const EFFORTS: [&str; 4] = ["off", "low", "high", "max"];
/// dsh's own default route (`agent-default-model` in the shipped profile).
const DEFAULT_MODEL: &str = "deepseek-flash";
const STREAM: &str = "/logs/agent/dsh.jsonl";
const SESSIONS: &str = "/logs/agent/dsh-sessions";
const RUN_START: &str = "/logs/agent/dsh-run-start";
const PATCH: &str = "/logs/agent/dsh-patch.yml";
const INSTRUCTION: &str = "/tmp/hbench-dsh-instruction.md";
const NVM: &str = "[ -f ~/.nvm/nvm.sh ] && . ~/.nvm/nvm.sh;";
const EXPORT_TIMEOUT: Duration = Duration::from_secs(120);
const STREAM_NOTE: &str = "Rebuilt from dsh's output stream: this run has no exported session log. \
                           The instruction and timestamps are missing, and long text is cut at 8 KiB.";

/// Copy every persisted session (zstd JSONL under $DSH_HOME/sessions) out as plain JSONL.
/// The log is appended as one zstd frame per flush and Node's zstd (>= 22.15; dsh needs >= 22.19)
/// decodes only the first frame, so decode frame by frame: a magic-number match inside compressed
/// data just fails to decode and the frame is extended to the next match.
const EXPORT_SESSIONS_JS: &str = r#"const fs=require("fs"),p=require("path"),z=require("zlib");
const home=process.env.DSH_HOME||p.join(require("os").homedir(),".dsh"),out=process.argv[1],M=Buffer.from([0x28,0xb5,0x2f,0xfd]);
const unzstd=b=>{const parts=[];let s=0;while(s<b.length){let e=b.indexOf(M,s+4);for(;;){const end=e<0?b.length:e;
try{parts.push(z.zstdDecompressSync(b.subarray(s,end)));s=end;break}catch(_){if(e<0){s=b.length;break}e=b.indexOf(M,e+4)}}}return Buffer.concat(parts)};
fs.mkdirSync(out,{recursive:true});
const walk=d=>{for(const e of fs.readdirSync(d,{withFileTypes:true})){const f=p.join(d,e.name);
if(e.isDirectory())walk(f);else if(/^session\.v\d+\.jsonl(\.zstd)?$/.test(e.name)){const b=fs.readFileSync(f);
fs.writeFileSync(p.join(out,p.basename(d)+".jsonl"),e.name.endsWith(".zstd")?unzstd(b):b);}}};
try{walk(p.join(home,"sessions"))}catch(e){}"#;

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    let a = DeepSeekHarness { version: args.version_kw(), args, session_id: None };
    a.route()?;
    a.effort()?;
    Ok(Box::new(a))
}

pub struct DeepSeekHarness {
    args: AgentArgs,
    version: Option<String>,
    /// Session of the previous run, adopted with `--session-id` on resume.
    session_id: Option<String>,
}

impl DeepSeekHarness {
    /// `deepseek/<id>` (or a bare id) -> dsh's `deepseek-official` route model id.
    fn route(&self) -> Result<Option<String>> {
        let Some(m) = self.args.model_name.as_deref() else { return Ok(None) };
        match m.split_once('/') {
            Some(("deepseek" | "anthropic", id)) if !id.is_empty() => Ok(Some(id.to_string())),
            None if !m.is_empty() => Ok(Some(m.to_string())),
            _ => bail!("{NAME} serves DeepSeek and Anthropic models: use deepseek/<model> or anthropic/<model>"),
        }
    }
    fn anthropic(&self) -> bool {
        self.args.model_name.as_deref().is_some_and(|m| m.starts_with("anthropic/"))
    }
    fn effort(&self) -> Result<Option<String>> {
        opts::choice(&self.args, "reasoning_effort", None, &EFFORTS)
    }

    /// Cordis patch overriding `agent-default-model` (a replaced config must restate provider + model).
    fn patch(&self) -> Result<Option<String>> {
        let (model, effort) = (self.route()?, self.effort()?);
        if model.is_none() && effort.is_none() {
            return Ok(None);
        }
        let provider = if self.anthropic() { "anthropic" } else { "deepseek-official" };
        let mut cfg = json!({"provider": provider, "model": model.as_deref().unwrap_or(DEFAULT_MODEL)});
        if let Some(e) = effort {
            cfg["reasoningEffort"] = json!(e);
        }
        let mut patch = vec![json!({"id": "agent-default-model", "config": cfg})];
        if self.anthropic() {
            let mut route = json!({"apiKeyEnv": "ANTHROPIC_API_KEY"});
            if let Some(u) = connection(&self.args, ANTHROPIC).configured_base_url {
                route["baseURL"] = json!(u);
            }
            patch.push(json!({"id": "llm-pi-ai", "config": {"providers": {"anthropic": route}}}));
        }
        // JSON is valid YAML.
        Ok(Some(serde_json::to_string_pretty(&patch)?))
    }

    fn env(&self) -> BTreeMap<String, String> {
        let mut env = connection(&self.args, if self.anthropic() { ANTHROPIC } else { SPEC }).env;
        // Headless runs can't answer approval prompts; the container is the sandbox.
        env.insert("DSH_PERMISSION_MODE".into(), "danger-full-access".into());
        env
    }

    fn command(&self, resume: bool, patch: bool) -> String {
        let mut flags = String::new();
        if patch {
            flags += &format!(" --patch {PATCH}");
        }
        flags += " --json";
        if let (true, Some(id)) = (resume, &self.session_id) {
            flags += &format!(" --session-id {}", quote(id));
        }
        format!(
            "{NVM} date +%s%3N > {RUN_START}; dsh --profile headless{flags} - < {INSTRUCTION} 2>&1 | stdbuf -oL tee {STREAM}; rc=$?; exit $rc"
        )
    }
}

fn events(text: &str) -> Vec<Value> {
    parse_jsonl(text).into_iter().filter(Value::is_object).collect()
}

/// Error type for a failed run: the provider status on the last errored `turn_end`, else output patterns.
pub fn failure_kind(output: &str) -> ErrorKind {
    let reason = events(output).into_iter().rev().find(|e| e["type"] == "status" && e["phase"] == "turn_end").map(|e| e["reason"].clone());
    if let Some(r) = reason.filter(|r| r["kind"] == "error") {
        let err = &r["error"];
        match (err["status"].as_i64(), err["code"].as_str()) {
            (Some(401 | 403), _) | (_, Some("AUTH")) => return ErrorKind::AgentAuthentication,
            (Some(402), _) => return ErrorKind::ApiUsageLimit,
            (Some(429), _) => return ErrorKind::ApiRateLimit,
            (Some(500), _) => return ErrorKind::ApiInternalServer,
            (Some(502..=504), _) => return ErrorKind::ApiOverloaded,
            _ => return classify(err["message"].as_str().unwrap_or_default()),
        }
    }
    classify(output)
}

fn text_of(content: &Value, kind: &str) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == kind)
        .filter_map(|b| b["text"].as_str())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn iso(ms: &Value) -> Option<String> {
    atif::timestamp_of_millis(ms.as_i64()?)
}

/// (prompt incl. cache, completion, cache read, cache write) of one model call.
fn usage(u: &Value) -> Option<(i64, i64, i64, i64)> {
    let u = u.as_object()?;
    let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    let (read, write) = (n("cacheReadTokens"), n("cacheWriteTokens"));
    Some((n("inputTokens") + read + write, n("outputTokens"), read, write))
}

fn cost(model: &str, (p, c, read, write): (i64, i64, i64, i64)) -> Option<f64> {
    let key = pricing::pricing_key(&format!("deepseek/{model}"))?;
    pricing::cost_per_token(&key, p, c, write, read).map(|(a, b)| a + b)
}

fn metrics(u: (i64, i64, i64, i64), model: Option<&str>) -> atif::Metrics {
    atif::Metrics {
        prompt_tokens: Some(u.0),
        completion_tokens: Some(u.1),
        cached_tokens: Some(u.2),
        cost_usd: model.and_then(|m| cost(m, u)),
        ..Default::default()
    }
}

/// One session log (`dsh-sessions/<id>.jsonl`) -> ATIF, counting only events at or after `since_ms`.
pub fn session_trajectory(text: &str, since_ms: i64, version: &str, model: Option<&str>) -> Option<atif::Trajectory> {
    let evs = events(text);
    let session_id = evs.iter().find(|e| e["type"] == "session").and_then(|e| e["id"].as_str()).map(String::from);
    // The route header is logged once per session, before a resumed step's window: read it from the whole log.
    let model_name = evs
        .iter()
        .rev()
        .find_map(|e| (e["type"] == "request/header").then(|| e["data"]["header"]["config"]["model"].as_str()).flatten())
        .map(String::from)
        .or(model.map(String::from));
    let mut steps: Vec<atif::Step> = vec![];
    for e in evs.iter().filter(|e| e["time"].as_i64().unwrap_or(i64::MAX) >= since_ms) {
        let d = &e["data"];
        match e["type"].as_str().unwrap_or_default() {
            "user/message" if d["source"]["kind"] == "user" => {
                steps.push(atif::Step {
                    source: "user".into(),
                    timestamp: iso(&e["time"]),
                    message: json!(text_of(&d["content"], "text")),
                    ..Default::default()
                });
            }
            "assistant/message" => {
                let msg = &d["message"];
                let m = msg["source"]["model"].as_str().map(String::from).or(model_name.clone());
                let metrics = usage(&e["data"]["usage"]).map(|u| metrics(u, m.as_deref()));
                let reasoning = text_of(&msg["content"], "reasoning");
                steps.push(atif::Step {
                    source: "agent".into(),
                    timestamp: iso(&e["time"]),
                    model_name: m,
                    message: json!(text_of(&msg["content"], "text")),
                    reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
                    metrics,
                    ..Default::default()
                });
            }
            "tool/call" => {
                let Some(step) = steps.last_mut().filter(|s| s.source == "agent") else { continue };
                let raw = d["arguments"].as_str().unwrap_or_default();
                let arguments = serde_json::from_str::<Value>(raw).unwrap_or_else(|_| json!(raw));
                step.tool_calls.get_or_insert_with(Vec::new).push(atif::ToolCall {
                    tool_call_id: d["callId"].as_str().unwrap_or_default().into(),
                    function_name: d["name"].as_str().unwrap_or_default().into(),
                    arguments,
                    extra: None,
                });
            }
            "tool/result" => {
                let Some(step) = steps.iter_mut().rev().find(|s| s.source == "agent") else { continue };
                let msg = &d["message"];
                let mut extra = Map::new();
                if msg["isError"] == true {
                    extra.insert("is_error".into(), json!(true));
                }
                step.observation.get_or_insert_with(Default::default).results.push(atif::ObservationResult {
                    source_call_id: msg["toolCallId"].as_str().map(String::from),
                    content: Some(json!(text_of(&msg["content"], "text"))),
                    extra: (!extra.is_empty()).then_some(extra),
                    ..Default::default()
                });
            }
            _ => {}
        }
    }
    atif::assemble(atif::Agent { name: NAME.into(), version: version.into(), model_name, ..Default::default() }, session_id, steps)
}

/// dsh's `--json` stdout (`dsh.jsonl`) -> ATIF, for a run with no exported session.
/// The stream has every model call, but no timestamps or instruction, and cuts long text at 8 KiB.
pub fn stream_trajectory(text: &str, version: &str, model: Option<&str>) -> Option<atif::Trajectory> {
    let evs = events(text);
    let session_id = evs.iter().find(|e| e["type"] == "session").and_then(|e| e["sessionId"].as_str()).map(String::from);
    let mut steps: Vec<atif::Step> = vec![];
    for e in &evs {
        let kind = e["type"].as_str().unwrap_or_default();
        if kind == "status" && e["phase"] == "step_start" {
            steps.push(atif::Step {
                source: "agent".into(),
                model_name: model.map(String::from),
                message: json!(""),
                ..Default::default()
            });
            continue;
        }
        let Some(step) = steps.last_mut() else { continue };
        match kind {
            "status" if e["phase"] == "step_end" => step.metrics = usage(&e["usage"]).map(|u| metrics(u, model)),
            "thinking" => step.reasoning_content = e["text"].as_str().map(String::from),
            "text" => step.message = e["text"].clone(),
            "tool_call" => step.tool_calls.get_or_insert_with(Vec::new).push(atif::ToolCall {
                tool_call_id: e["callId"].as_str().unwrap_or_default().into(),
                function_name: e["tool"].as_str().unwrap_or_default().into(),
                arguments: e["input"].clone(),
                extra: None,
            }),
            "tool_result" => {
                let mut extra = Map::new();
                if e["status"] == "error" {
                    extra.insert("is_error".into(), json!(true));
                }
                step.observation.get_or_insert_with(Default::default).results.push(atif::ObservationResult {
                    source_call_id: e["callId"].as_str().map(String::from),
                    content: Some(e["result"].clone()),
                    extra: (!extra.is_empty()).then_some(extra),
                    ..Default::default()
                });
            }
            _ => {}
        }
    }
    // A step the run was stopped in may hold nothing yet.
    steps.retain(|s| s.metrics.is_some() || s.reasoning_content.is_some() || s.tool_calls.is_some() || s.message != "");
    let agent = atif::Agent { name: NAME.into(), version: version.into(), model_name: model.map(String::from), ..Default::default() };
    let mut t = atif::assemble(agent, session_id, steps)?;
    t.notes = Some(STREAM_NOTE.into());
    Some(t)
}

/// [`stream_trajectory`] of a trial's agent logs.
pub fn stream_trajectory_of(logs_dir: &Path, version: &str, model: Option<&str>) -> Option<atif::Trajectory> {
    stream_trajectory(&std::fs::read_to_string(logs_dir.join("dsh.jsonl")).ok()?, version, model)
}

#[async_trait::async_trait]
impl Agent for DeepSeekHarness {
    fn name(&self) -> &str {
        NAME
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "bash", "coreutils"]).await?;
        let spec = pins::DEEPSEEK_HARNESS.npm_spec(self.version.as_deref());
        exec_agent(
            cx,
            &format!("set -euo pipefail; {} && npm i -g {spec} && dsh --version", nvm_node_install_snippet(22)),
            &BTreeMap::new(),
        )
        .await?;
        if self.version.is_none() {
            self.version = detect_version(cx, &format!("{NVM} dsh --version"), |s| s.trim().to_string()).await;
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        upload_text(cx, INSTRUCTION, &instruction).await?;
        let patch = self.patch()?;
        if let Some(p) = &patch {
            upload_text(cx, PATCH, p).await?;
        }
        let r = cx.exec(&format!("set -o pipefail; {}", self.command(resume, patch.is_some())), &self.env(), None).await?;
        if let Some(id) = events(&r.stdout).iter().find(|e| e["type"] == "session").and_then(|e| e["sessionId"].as_str()) {
            self.session_id = Some(id.to_string());
        }
        if r.code != 0 {
            let out = format!("{}\n{}", r.stdout, r.stderr);
            let detail = out.lines().rfind(|l| l.starts_with("dsh: ")).unwrap_or("dsh exited non-zero").to_string();
            return Err(kind_err(failure_kind(&out), format!("Command failed (exit {}): {detail}", r.code)));
        }
        Ok(())
    }

    async fn export_session(&self, cx: &AgentCtx<'_>) -> Result<()> {
        cx.exec(
            &format!("{NVM} node -e {} {SESSIONS} >/dev/null 2>&1 || true", quote(EXPORT_SESSIONS_JS)),
            &self.env(),
            Some(EXPORT_TIMEOUT),
        )
        .await
        .map(|_| ())
    }

    fn collect_usage(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        let since = std::fs::read_to_string(logs_dir.join("dsh-run-start")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let main_id = std::fs::read_to_string(logs_dir.join("dsh.jsonl"))
            .ok()
            .and_then(|t| events(&t).into_iter().find(|e| e["type"] == "session"))
            .and_then(|e| e["sessionId"].as_str().map(String::from));
        let model = self.route().ok().flatten().unwrap_or_else(|| DEFAULT_MODEL.into());
        let version = self.version.clone().unwrap_or_else(|| "unknown".into());
        let dir = std::fs::read_dir(logs_dir.join("dsh-sessions"));
        let mut paths: Vec<_> =
            dir.into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect();
        paths.sort();
        // Totals cover every session this run touched (subagents included); the trajectory is the main session.
        let (mut found, mut i, mut o, mut c, mut cost) = (false, 0, 0, 0, None::<f64>);
        for p in &paths {
            let Ok(text) = std::fs::read_to_string(p) else { continue };
            let Some(t) = session_trajectory(&text, since, &version, Some(&model)) else { continue };
            let fm = t.final_metrics.clone().unwrap_or_default();
            found = true;
            i += fm.total_prompt_tokens.unwrap_or(0);
            o += fm.total_completion_tokens.unwrap_or(0);
            c += fm.total_cached_tokens.unwrap_or(0);
            if let Some(x) = fm.total_cost_usd {
                cost = Some(cost.unwrap_or(0.0) + x);
            }
            if main_id.as_deref().is_some_and(|id| p.file_stem().is_some_and(|s| s == id)) {
                atif::write_trajectory(logs_dir, &t);
            }
        }
        // Older runs that timed out, and any run whose export failed.
        if !found && let Some(t) = stream_trajectory_of(logs_dir, &version, Some(&model)) {
            atif::write_trajectory(logs_dir, &t);
            let fm = t.final_metrics.unwrap_or_default();
            (found, i, o, c, cost) = (
                true,
                fm.total_prompt_tokens.unwrap_or(0),
                fm.total_completion_tokens.unwrap_or(0),
                fm.total_cached_tokens.unwrap_or(0),
                fm.total_cost_usd,
            );
        }
        if found {
            (ctx.n_input_tokens, ctx.n_output_tokens, ctx.n_cache_tokens, ctx.cost_usd) = (Some(i), Some(o), Some(c), cost);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;

    fn agent(kwargs: Value, model: Option<&str>) -> Result<DeepSeekHarness> {
        let args = common::test_args(kwargs, &[("DEEPSEEK_API_KEY", "k")], model);
        let a = DeepSeekHarness { version: None, args, session_id: None };
        a.route()?;
        a.effort()?;
        Ok(a)
    }

    #[test]
    fn model_patch_env_and_command() {
        let a = agent(json!({"reasoning_effort": "max"}), Some("deepseek/deepseek-v4-pro")).unwrap();
        let p: Value = serde_json::from_str(&a.patch().unwrap().unwrap()).unwrap();
        assert_eq!(
            p,
            json!([{"id": "agent-default-model", "config": {"provider": "deepseek-official", "model": "deepseek-v4-pro", "reasoningEffort": "max"}}])
        );
        assert!(agent(json!({}), None).unwrap().patch().unwrap().is_none());
        assert_eq!(agent(json!({"reasoning_effort": "low"}), None).unwrap().patch().unwrap().unwrap().matches(DEFAULT_MODEL).count(), 1);
        assert!(agent(json!({}), Some("openai/gpt-5")).is_err());
        let args = common::test_args(json!({}), &[("ANTHROPIC_API_KEY", "ak")], Some("anthropic/claude-x"));
        let c = DeepSeekHarness { version: None, args, session_id: None };
        let p: Value = serde_json::from_str(&c.patch().unwrap().unwrap()).unwrap();
        assert_eq!(p[0]["config"], json!({"provider": "anthropic", "model": "claude-x"}));
        assert_eq!(p[1], json!({"id": "llm-pi-ai", "config": {"providers": {"anthropic": {"apiKeyEnv": "ANTHROPIC_API_KEY"}}}}));
        assert_eq!(c.env()["ANTHROPIC_API_KEY"], "ak");
        assert!(agent(json!({"reasoning_effort": "medium"}), None).is_err());
        let env = a.env();
        assert_eq!((env["DEEPSEEK_API_KEY"].as_str(), env["DSH_PERMISSION_MODE"].as_str()), ("k", "danger-full-access"));
        let mut a = a;
        let cmd = a.command(true, true);
        assert!(cmd.contains("dsh --profile headless --patch /logs/agent/dsh-patch.yml --json - < /tmp/hbench-dsh-instruction.md 2>&1 | stdbuf -oL tee /logs/agent/dsh.jsonl; rc=$?"));
        // A timeout never reaches the end of this command: the export is export_session's.
        assert!(!cmd.contains("zstd"));
        assert!(!cmd.contains("--session-id"));
        a.session_id = Some("session-1".into());
        assert!(a.command(true, false).contains("dsh --profile headless --json --session-id session-1 -"));
        assert!(!a.command(false, false).contains("--session-id"));
    }

    #[test]
    fn failure_kinds() {
        let ev = |status: u16, code: &str| {
            format!(
                "{{\"type\":\"status\",\"phase\":\"turn_end\",\"turn\":1,\"reason\":{{\"kind\":\"error\",\"error\":{{\"message\":\"m\",\"code\":\"{code}\",\"status\":{status}}}}}}}\n{{\"type\":\"final\",\"text\":\"\"}}\ndsh: X: m"
            )
        };
        assert_eq!(failure_kind(&ev(401, "AUTH")), "AgentAuthenticationError");
        assert_eq!(failure_kind(&ev(402, "BALANCE")), "ApiUsageLimitError");
        assert_eq!(failure_kind(&ev(429, "RATE")), "ApiRateLimitError");
        assert_eq!(failure_kind(&ev(503, "X")), "ApiOverloadedError");
        assert_eq!(failure_kind("dsh: boom"), "NonZeroAgentExitCodeError");
        assert_eq!(failure_kind("curl: (6) Could not resolve host: x"), "NetworkConnectionError");
    }

    #[test]
    fn session_to_trajectory() {
        let log = [
            json!({"type": "session", "version": 4, "id": "session-a", "cwd": "/app"}),
            json!({"type": "request/header", "seq": 1, "time": 900, "data": {"header": {"config": {"provider": "deepseek-official", "model": "deepseek-v4-flash"}}}}),
            json!({"type": "user/message", "seq": 2, "time": 500, "data": {"content": [{"type": "text", "text": "old step"}], "source": {"kind": "user"}}}),
            json!({"type": "user/message", "seq": 3, "time": 1000, "data": {"content": [{"type": "text", "text": "do it"}], "source": {"kind": "user"}}}),
            json!({"type": "assistant/message", "seq": 4, "time": 1100, "data": {"message": {"content": [{"type": "reasoning", "text": "hmm"}, {"type": "reasoning", "text": ""}, {"type": "text", "text": "ok"}], "source": {"model": "deepseek-v4-flash"}}, "usage": {"inputTokens": 1000, "outputTokens": 100, "cacheReadTokens": 4000, "cacheWriteTokens": 0}}}),
            json!({"type": "tool/call", "seq": 5, "time": 1101, "data": {"callId": "c1", "name": "bash", "arguments": "{\"command\":\"ls\"}"}}),
            json!({"type": "tool/result", "seq": 6, "time": 1200, "data": {"message": {"toolCallId": "c1", "content": [{"type": "text", "text": "a.txt"}], "isError": true}}}),
            json!({"type": "assistant/message", "seq": 7, "time": 1300, "data": {"message": {"content": [{"type": "text", "text": "done"}], "source": {"model": "deepseek-v4-flash"}}, "usage": {"inputTokens": 10, "outputTokens": 5, "cacheReadTokens": 5000, "cacheWriteTokens": 2}}}),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        let t = session_trajectory(&log, 1000, "0.2.0", None).unwrap();
        assert_eq!(t.session_id.as_deref(), Some("session-a"));
        assert_eq!(t.agent.model_name.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(t.steps.iter().map(|s| s.source.as_str()).collect::<Vec<_>>(), ["user", "agent", "agent"]);
        assert_eq!(t.steps[0].message, "do it");
        let s = &t.steps[1];
        assert_eq!((s.step_id, s.message.as_str(), s.reasoning_content.as_deref()), (2, Some("ok"), Some("hmm")));
        let tc = &s.tool_calls.as_ref().unwrap()[0];
        assert_eq!((tc.tool_call_id.as_str(), tc.function_name.as_str(), &tc.arguments), ("c1", "bash", &json!({"command": "ls"})));
        let r = &s.observation.as_ref().unwrap().results[0];
        assert_eq!(
            (r.source_call_id.as_deref(), r.content.clone(), r.extra.as_ref().map(|e| e["is_error"].clone())),
            (Some("c1"), Some(json!("a.txt")), Some(json!(true)))
        );
        let fm = t.final_metrics.unwrap();
        assert_eq!(
            (fm.total_prompt_tokens, fm.total_completion_tokens, fm.total_cached_tokens, fm.total_steps),
            (Some(5000 + 5012), Some(105), Some(9000), Some(3))
        );
        // LiteLLM deepseek/deepseek-v4-flash: in 3e-7, out 1.2e-6, cache read 6e-9, cache write 0.
        let want = (1000.0 + 10.0) * 3e-7 + 9000.0 * 6e-9 + 105.0 * 1.2e-6;
        assert!((fm.total_cost_usd.unwrap() - want).abs() < 1e-12, "{:?} vs {want}", fm.total_cost_usd);
    }

    fn stream() -> String {
        [
            json!({"type": "session", "sessionId": "session-a", "cwd": "/app"}).to_string(),
            json!({"type": "status", "phase": "turn_start", "turn": 1}).to_string(),
            json!({"type": "status", "phase": "step_start", "turn": 1, "step": 1}).to_string(),
            json!({"type": "thinking", "text": "hmm", "truncated": false}).to_string(),
            json!({"type": "tool_call", "callId": "c1", "tool": "bash", "input": {"command": "ls"}}).to_string(),
            json!({"type": "tool_result", "callId": "c1", "status": "error", "result": "boom"}).to_string(),
            json!({"type": "status", "phase": "step_end", "turn": 1, "step": 1, "usage": {"inputTokens": 1000, "outputTokens": 100, "cacheReadTokens": 4000, "cacheWriteTokens": 0}}).to_string(),
            json!({"type": "status", "phase": "step_start", "turn": 1, "step": 2}).to_string(),
            json!({"type": "tool_call", "callId": "c2", "tool": "read", "input": {"path": "a"}}).to_string(),
            json!({"type": "status", "phase": "step_start", "turn": 1, "step": 3}).to_string(),
            "{\"type\":\"thinking\",\"te".to_string(),
        ]
        .join("\n")
    }

    #[test]
    fn stream_to_trajectory() {
        let t = stream_trajectory(&stream(), "0.2.0", Some("deepseek-v4-flash")).unwrap();
        assert_eq!(t.session_id.as_deref(), Some("session-a"));
        assert!(t.notes.as_deref().unwrap().contains("Rebuilt from dsh's output stream"));
        // The step the run was killed in has no usage yet; the empty one after it is dropped.
        assert_eq!(t.steps.iter().map(|s| (s.step_id, s.metrics.is_some())).collect::<Vec<_>>(), [(1, true), (2, false)]);
        let s = &t.steps[0];
        assert_eq!((s.source.as_str(), s.reasoning_content.as_deref(), s.timestamp.as_ref()), ("agent", Some("hmm"), None));
        let tc = &s.tool_calls.as_ref().unwrap()[0];
        assert_eq!((tc.tool_call_id.as_str(), tc.function_name.as_str(), &tc.arguments), ("c1", "bash", &json!({"command": "ls"})));
        let r = &s.observation.as_ref().unwrap().results[0];
        assert_eq!(
            (r.source_call_id.as_deref(), r.content.clone(), r.extra.as_ref().map(|e| e["is_error"].clone())),
            (Some("c1"), Some(json!("boom")), Some(json!(true)))
        );
        let fm = t.final_metrics.unwrap();
        assert_eq!(
            (fm.total_prompt_tokens, fm.total_completion_tokens, fm.total_cached_tokens, fm.total_steps),
            (Some(5000), Some(100), Some(4000), Some(2))
        );
        assert!(fm.total_cost_usd.is_some());
        assert!(stream_trajectory("{\"type\":\"session\",\"sessionId\":\"s\"}\ndsh: boom", "0.2.0", None).is_none());
    }

    #[test]
    fn collect_usage_falls_back_to_the_stream() {
        let dir = std::env::temp_dir().join(format!("hbench-dsh-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("dsh-sessions")).unwrap();
        std::fs::write(dir.join("dsh.jsonl"), stream()).unwrap();
        let mut ctx = AgentContext::default();
        agent(json!({}), None).unwrap().collect_usage(&dir, &mut ctx);
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens, ctx.n_cache_tokens), (Some(5000), Some(100), Some(4000)));
        assert!(ctx.cost_usd.is_some());
        let v: Value = serde_json::from_slice(&std::fs::read(dir.join("trajectory.json")).unwrap()).unwrap();
        assert_eq!(v["steps"].as_array().unwrap().len(), 2);

        // An exported session wins: it has the full text.
        std::fs::remove_file(dir.join("trajectory.json")).unwrap();
        let session = [
            json!({"type": "session", "id": "session-a"}),
            json!({"type": "user/message", "time": 1000, "data": {"content": [{"type": "text", "text": "do it"}], "source": {"kind": "user"}}}),
        ]
        .map(|e| e.to_string())
        .join("\n");
        std::fs::write(dir.join("dsh-sessions/session-a.jsonl"), session).unwrap();
        agent(json!({}), None).unwrap().collect_usage(&dir, &mut AgentContext::default());
        let v: Value = serde_json::from_slice(&std::fs::read(dir.join("trajectory.json")).unwrap()).unwrap();
        assert!(v["notes"].is_null() && v["steps"][0]["message"] == "do it", "{v}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
