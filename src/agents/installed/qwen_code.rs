//! Qwen Code installed agent.

use super::common::{installed_dir, nvm_node_install_snippet, skills_dir};
use super::trae_agent::{iso_timestamp_ok, opt_str};
use super::{connection, ensure_system_dependencies, exec_agent};
use crate::agents::model_connection::Spec;
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

const SPEC: Spec = Spec { default_provider: Some("openai"), api_key_envs: &[], base_url_envs: &[], passthrough: true };

pub struct QwenCode {
    /// Agent args with option env (api_key/base_url) layered on top.
    args: AgentArgs,
    version: Option<String>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(QwenCode::new(args)?))
}

impl QwenCode {
    fn new(mut args: AgentArgs) -> Result<QwenCode> {
        for (kw, env) in [("api_key", "OPENAI_API_KEY"), ("base_url", "OPENAI_BASE_URL")] {
            // Explicit kwarg (None = opt-out) > Env fallback (agent env > host env).
            let v = match args.kwargs.get(kw) {
                Some(Value::Null) => None,
                Some(v) => Some(opt_str(kw, v)?),
                None => args.extra_env.get(env).cloned().or_else(|| std::env::var(env).ok()),
            };
            if let Some(v) = v {
                args.extra_env.insert(env.into(), v);
            }
        }
        Ok(QwenCode { version: args.version_kw(), args })
    }

    fn run_env(&self) -> BTreeMap<String, String> {
        let mut env = connection(&self.args, SPEC).env;
        let model = match &self.args.model_name {
            Some(m) => m.split_once('/').map(|(_, n)| n.to_string()).unwrap_or(m.clone()),
            None => std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "qwen3-coder-plus".into()),
        };
        env.insert("OPENAI_MODEL".into(), model);
        env
    }

    fn skills_command(&self) -> Option<String> {
        let d = skills_dir(&self.args)?;
        Some(format!("mkdir -p ~/.qwen/skills && cp -r {}/* ~/.qwen/skills/ 2>/dev/null || true", quote(&d)))
    }

    fn mcp_command(&self) -> Option<String> {
        if self.args.mcp_servers.is_empty() {
            return None;
        }
        let mut servers = serde_json::Map::new();
        for s in &self.args.mcp_servers {
            let v = match s.transport() {
                "stdio" => json!({"command": s.command, "args": s.args}),
                "streamable-http" => json!({"httpUrl": s.url}),
                _ => json!({"url": s.url}),
            };
            servers.insert(s.name.clone(), v);
        }
        let cfg = serde_json::to_string_pretty(&json!({"mcpServers": servers})).unwrap();
        Some(format!("mkdir -p ~/.qwen && echo {} > ~/.qwen/settings.json", quote(&cfg)))
    }

    fn run_command(instruction: &str, resume: bool) -> String {
        let resume_flag = if resume { "--continue " } else { "" };
        format!(
            ". ~/.nvm/nvm.sh; qwen --yolo --auth-type openai --openai-api-key \"$OPENAI_API_KEY\" \
             --openai-base-url \"${{OPENAI_BASE_URL:-https://api.openai.com/v1}}\" --chat-recording {resume_flag}--prompt={} \
             2>&1 | stdbuf -oL tee /logs/agent/qwen-code.txt",
            quote(instruction)
        )
    }
}

fn parse_jsonl(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .map(|t| t.lines().filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok()).filter(Value::is_object).collect())
        .unwrap_or_default()
}

/// Token totals (prompt, completion, cached) the ATIF converter would report;
/// Ok(None) when it yields no steps, Err where conversion fails.
pub fn events_usage(events: &[Value]) -> Result<Option<(i64, i64, i64)>> {
    let (mut steps, mut i, mut o, mut c) = (0, 0, 0, 0);
    for e in events {
        let Some(msg) = e["message"].as_object() else { continue };
        let parts = msg.get("parts").and_then(Value::as_array).cloned().unwrap_or_default();
        let ts_ok = || iso_timestamp_ok(&e["timestamp"]);
        match e["type"].as_str() {
            Some("user") => {
                if parts.iter().any(|p| !p.is_object()) {
                    bail!("non-object part");
                }
                if parts.iter().any(|p| p["text"].as_str().is_some_and(|t| !t.is_empty())) {
                    if !ts_ok() {
                        bail!("Invalid ISO 8601 timestamp");
                    }
                    steps += 1;
                }
            }
            Some("assistant") => {
                if !ts_ok() {
                    bail!("Invalid ISO 8601 timestamp");
                }
                let u = &e["usageMetadata"];
                i += u["promptTokenCount"].as_i64().unwrap_or(0);
                o += u["candidatesTokenCount"].as_i64().unwrap_or(0);
                c += u["cachedContentTokenCount"].as_i64().unwrap_or(0);
                steps += 1;
            }
            _ => {}
        }
    }
    Ok((steps > 0).then_some((i, o, c)))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") && p.parent().and_then(Path::file_name).is_some_and(|n| n == "chats") {
            out.push(p);
        }
    }
}

/// `qwen-sessions/**/chats/*.jsonl` with the latest event timestamp (then mtime).
fn find_session_jsonl(logs_dir: &Path) -> Option<PathBuf> {
    let mut files = vec![];
    walk(&logs_dir.join("qwen-sessions"), &mut files);
    files.into_iter().max_by_key(|p| {
        let ts = parse_jsonl(p).iter().filter_map(|e| e["timestamp"].as_str().map(String::from)).max().unwrap_or_default();
        (ts, std::fs::metadata(p).and_then(|m| m.modified()).ok())
    })
}

/// Usage summed over embedded subagent transcripts (those whose parent chain reaches a root).
fn subagent_usage(session: &Path) -> (i64, i64, i64) {
    let (Some(stem), Some(project)) = (session.file_stem().and_then(|s| s.to_str()), session.parent().and_then(Path::parent)) else {
        return (0, 0, 0);
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(project.join("subagents").join(stem))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("agent-") && n.ends_with(".jsonl")))
        .collect();
    files.sort();
    let mut usage: HashMap<String, (i64, i64, i64)> = HashMap::new();
    let mut parents: HashMap<String, Option<String>> = HashMap::new();
    for f in files {
        let events = parse_jsonl(&f);
        if events.is_empty() {
            continue;
        }
        let meta_path = f.with_file_name(format!("{}.meta.json", f.file_stem().unwrap().to_string_lossy()));
        let meta: Value = std::fs::read_to_string(meta_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .filter(Value::is_object)
            .unwrap_or(json!({}));
        let id = meta["agentId"].as_str().or_else(|| events.iter().find_map(|e| e["agentId"].as_str())).map(String::from);
        let Some(id) = id.filter(|i| !i.is_empty() && !usage.contains_key(i)) else { continue };
        // A malformed transcript is skipped on its own; siblings survive.
        let Ok(Some(u)) = events_usage(&events) else { continue };
        usage.insert(id.clone(), u);
        parents.insert(id, meta["parentAgentId"].as_str().map(String::from));
    }
    let mut total = (0, 0, 0);
    for (id, u) in &usage {
        let (mut cur, mut seen) = (id.clone(), HashSet::new());
        let reachable = loop {
            match &parents[&cur] {
                Some(p) if usage.contains_key(p) => {
                    if !seen.insert(p.clone()) {
                        break false; // cycle: never reached from a root
                    }
                    cur = p.clone();
                }
                _ => break true,
            }
        };
        if reachable {
            total = (total.0 + u.0, total.1 + u.1, total.2 + u.2);
        }
    }
    total
}

#[async_trait::async_trait]
impl Agent for QwenCode {
    fn name(&self) -> &str {
        "qwen-coder"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl"]).await?;
        let vs = self.version.as_deref().unwrap_or("latest");
        exec_agent(
            cx,
            &format!("set -euo pipefail; {} && npm install -g @qwen-code/qwen-code@{vs} && qwen --version", nvm_node_install_snippet(22)),
            &BTreeMap::new(),
        )
        .await?;
        if self.version.is_none()
            && let Ok(r) = cx.exec(". ~/.nvm/nvm.sh; qwen --version", &BTreeMap::new(), None).await
            && r.code == 0
            && !r.stdout.is_empty()
        {
            self.version = Some(r.stdout.trim().to_string());
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let env = self.run_env();
        for c in [self.skills_command(), self.mcp_command()].into_iter().flatten() {
            exec_agent(cx, &c, &env).await?;
        }
        let r = exec_agent(cx, &Self::run_command(&instruction, resume), &env).await;
        let _ = exec_agent(cx, "cp -r ~/.qwen/projects/ /logs/agent/qwen-sessions/ 2>/dev/null || true", &BTreeMap::new()).await;
        r.map(|_| ())
    }

    fn populate_context_post_run(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        let Some(session) = find_session_jsonl(logs_dir) else { return };
        let events = parse_jsonl(&session);
        let Ok(Some((i, o, c))) = events_usage(&events) else { return };
        let s = subagent_usage(&session);
        ctx.cost_usd = None;
        ctx.n_input_tokens = Some(i + s.0);
        ctx.n_output_tokens = Some(o + s.1);
        ctx.n_cache_tokens = Some(c + s.2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common::test_args as args;

    fn user(text: &str, extra: Value) -> Value {
        let mut v = json!({"sessionId": "session-1", "timestamp": "2026-01-01T00:00:00Z", "type": "user", "version": "0.19.11", "message": {"role": "user", "parts": [{"text": text}]}});
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v
    }
    fn assistant(parts: Value, usage: Value, extra: Value) -> Value {
        let mut v = json!({"sessionId": "session-1", "timestamp": "2026-01-01T00:00:01Z", "type": "assistant", "model": "qwen3-coder-plus", "message": {"role": "model", "parts": parts}, "usageMetadata": usage});
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v
    }
    fn write_jsonl(p: &Path, events: &[Value]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, events.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n").unwrap();
    }

    #[test]
    fn env_flags_and_options() {
        let a = QwenCode::new(args(
            json!({"api_key": "kw-key"}),
            &[("OPENAI_API_KEY", "env-key"), ("OPENAI_API_BASE", "http://b")],
            Some("openai/qwen3-coder-plus"),
        ))
        .unwrap();
        let env = a.run_env();
        assert_eq!(
            (env["OPENAI_API_KEY"].as_str(), env["OPENAI_API_BASE"].as_str(), env["OPENAI_MODEL"].as_str()),
            ("kw-key", "http://b", "qwen3-coder-plus")
        );
        let b = QwenCode::new(args(json!({"base_url": "http://kw"}), &[("OPENAI_API_BASE", "http://b")], Some("m"))).unwrap();
        assert_eq!(b.run_env()["OPENAI_BASE_URL"], "http://kw");
        assert!(!b.run_env().contains_key("OPENAI_API_BASE"));
        assert!(QwenCode::new(args(json!({"api_key": true}), &[], None)).is_err());
        let cmd = QwenCode::run_command("fix it's bug", true);
        assert!(cmd.contains("--chat-recording --continue --prompt='fix it'\\''s bug' 2>&1 | stdbuf -oL tee /logs/agent/qwen-code.txt"));
        assert!(!QwenCode::run_command("x", false).contains("--continue"));
    }

    #[test]
    fn mcp_and_skills() {
        let mut ar = args(json!({}), &[], None);
        assert!(QwenCode::new(ar.clone()).unwrap().mcp_command().is_none());
        assert!(QwenCode::new(ar.clone()).unwrap().skills_command().is_none());
        let s: Vec<crate::task::McpServer> = serde_json::from_value(json!([
            {"name": "a", "transport": "sse", "url": "http://a/sse"},
            {"name": "h", "transport": "streamable-http", "url": "http://h/mcp"},
            {"name": "s", "transport": "stdio", "command": "npx", "args": ["-y", "my-mcp"]}
        ]))
        .unwrap();
        ar.mcp_servers = s;
        ar.skills = vec!["x".into()];
        let a = QwenCode::new(ar).unwrap();
        let cmd = a.mcp_command().unwrap();
        let cfg: Value = serde_json::from_str(&cmd[cmd.find('\'').unwrap() + 1..cmd.rfind('\'').unwrap()]).unwrap();
        assert_eq!(cfg["mcpServers"]["a"], json!({"url": "http://a/sse"}));
        assert_eq!(cfg["mcpServers"]["h"], json!({"httpUrl": "http://h/mcp"}));
        assert_eq!(cfg["mcpServers"]["s"], json!({"command": "npx", "args": ["-y", "my-mcp"]}));
        assert_eq!(a.skills_command().unwrap(), "mkdir -p ~/.qwen/skills && cp -r /hbench/skills/* ~/.qwen/skills/ 2>/dev/null || true");
    }

    #[test]
    fn usage_with_subagents() {
        let a = QwenCode::new(args(json!({}), &[], Some("qwen3-coder-plus"))).unwrap();
        let logs = a.args.logs_dir.clone();
        let project = logs.join("qwen-sessions/project");
        let fc = json!([{"functionCall": {"id": "call_parent", "name": "agent", "args": {}}}]);
        write_jsonl(
            &project.join("chats/session-1.jsonl"),
            &[
                user("Delegate.", json!({})),
                assistant(fc, json!({"promptTokenCount": 100, "candidatesTokenCount": 10, "cachedContentTokenCount": 5}), json!({})),
            ],
        );
        let sub = |id: &str, parent: Value, events: &[Value]| {
            let p = project.join(format!("subagents/session-1/agent-{id}.jsonl"));
            write_jsonl(&p, events);
            std::fs::write(
                p.with_file_name(format!("agent-{id}.meta.json")),
                json!({"agentId": id, "agentType": "Explore", "parentAgentId": parent}).to_string(),
            )
            .unwrap();
        };
        let su = json!({"promptTokenCount": 700, "candidatesTokenCount": 70, "cachedContentTokenCount": 35});
        sub("Explore-call_parent", Value::Null, &[user("Inspect.", json!({})), assistant(json!([{"text": "ok"}]), su.clone(), json!({}))]);
        // Nested child counts; a malformed (bad timestamp) sibling is skipped.
        sub(
            "Child-call_x",
            json!("Explore-call_parent"),
            &[assistant(json!([{"text": "c"}]), json!({"promptTokenCount": 1, "candidatesTokenCount": 1}), json!({}))],
        );
        sub("Bad-call_y", Value::Null, &[user("bad", json!({"timestamp": "not-a-timestamp"})), assistant(json!([]), su, json!({}))]);
        let mut ctx = AgentContext::default();
        a.populate_context_post_run(&logs, &mut ctx);
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens, ctx.n_cache_tokens, ctx.cost_usd), (Some(801), Some(81), Some(40), None));

        // Thought parts / non-step events; no steps -> no usage.
        assert_eq!(events_usage(&[json!({"type": "tool_result", "message": {"parts": []}})]).unwrap(), None);
        assert!(events_usage(&[user("x", json!({"timestamp": "nope"}))]).is_err());
    }
}
