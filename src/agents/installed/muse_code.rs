//! Muse Code (port of upstream's agents/installed/muse_code.py).

use super::{connection, ensure_system_dependencies, exec_agent, exec_root, upload_text};
use crate::agents::model_connection::{Resolved, Spec};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

const SPEC: Spec = Spec { default_provider: Some("meta"), api_key_envs: &["META_API_KEY"], base_url_envs: &["MUSE_CODE_BASE_URL"], passthrough: false };
const INSTALL_SCRIPT_URL: &str = "https://dev.meta.ai/install.sh";
const PATH_EXPORT: &str = "export PATH=\"$HOME/.local/bin:$PATH\"";
const LOGS: &str = "/logs/agent";
const OUTPUT_FILENAME: &str = "muse-code.jsonl";
const STDERR_FILENAME: &str = "muse-code.stderr.log";
const MANIFEST: &str = "muse-session.json";
// upstream: /tmp/hbench-muse/{environment_logs_dir.name}
const RUNTIME_DIR: &str = "/tmp/hbench-muse/agent";
const EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "ultra"];

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(MuseCode::new(args)?))
}

pub struct MuseCode {
    args: AgentArgs,
    version: Option<String>,
    base_config: Map<String, Value>,
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
        let a = MuseCode { version: args.version_kw(), args, base_config };
        a.cli_flags()?; // validate options eagerly, like upstream
        a.effective_config()?; // validate transport/config conflicts
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

    /// upstream's overridden `model_connection`: explicit base_url option wins.
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

    fn skills_dir(&self) -> Option<String> {
        self.args.task.config.environment.skills_dir.clone().or_else(|| (!self.args.skills.is_empty()).then(|| "/hbench/skills".into()))
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

    fn run_command(&self, instruction: &str, session_id: &str) -> Result<String> {
        let prompt = format!("{RUNTIME_DIR}/prompt.txt");
        let mut parts = vec!["muse exec".to_string(), "--json".into(), "--yolo".into(), "--user-input-auto-resolve".into(), format!("--session-id {session_id}"), format!("--prompt-file {prompt}")];
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

fn read_manifest(logs_dir: &Path) -> Result<Value> {
    let p = logs_dir.join(MANIFEST);
    if !p.exists() {
        bail!("Cannot resume Muse Code: no saved upstream session");
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
        if let Some(id) = row["id"].as_str().filter(|s| !s.is_empty()) {
            if !seen.insert(id.to_string()) {
                continue;
            }
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

/// Whether upstream's ATIF converter would produce any step (else the session yields no trajectory).
fn has_steps(records: &Records) -> bool {
    run_events(records, 0).iter().any(|e| match e["kind"].as_str().unwrap_or("") {
        "started" | "model_response_created" | "assistant_message_committed" | "assistant_tool_calls_committed" | "reasoning_committed"
        | "reasoning_summary_committed" | "model_completed" | "tool_result" | "context_compaction_installed" => true,
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

/// upstream's `_sum_usage`: sum known token counts; cost only when every value is priced.
fn sum_usage(values: &[Usage]) -> Usage {
    let s = |f: fn(&Usage) -> Option<i64>| {
        let known: Vec<i64> = values.iter().filter_map(f).collect();
        (!known.is_empty()).then(|| known.iter().sum())
    };
    let cost = if values.is_empty() || values.iter().any(|v| v.cost.is_none()) { None } else { Some(values.iter().filter_map(|v| v.cost).sum()) };
    Usage { prompt: s(|v| v.prompt), completion: s(|v| v.completion), cached: s(|v| v.cached), cost }
}

fn session_usage(records: &Records, after: usize) -> Usage {
    let values: Vec<Usage> = run_events(records, after)
        .into_iter()
        .filter(|e| e["kind"] == "model_completed" && e["usage"].is_object())
        .map(|e| {
            let u = &e["usage"];
            Usage { prompt: u["input_tokens"].as_i64(), completion: u["output_tokens"].as_i64(), cached: u["cached_tokens"].as_i64(), cost: u["cost_usd"].as_f64() }
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
            // upstream's converter raises here and the session is skipped.
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

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        cx.env.exec_root("[ -d /installed-agent ] || mkdir -p /installed-agent", None).await?;
        if self.version.is_some() {
            bail!("Agent install failed: muse-code does not support version pinning: the installer always fetches the latest stable release.");
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
        if let Some(d) = self.skills_dir() {
            setup += &format!(" && mkdir -p {config_home}/muse/skills && cp -R {}/. {config_home}/muse/skills/", quote(&d));
        }
        exec_agent(cx, &setup, &env).await?;
        upload_text(cx, &format!("{config_home}/muse/settings.json"), &serde_json::to_string(&self.effective_config()?)?).await?;

        let session_id = if resume { read_manifest(&cx.logs_dir)?["session_id"].as_str().unwrap_or_default().to_string() } else { uuid::Uuid::new_v4().to_string() };
        let r = exec_agent(cx, &format!("find {sessions} -type f -name session.jsonl -exec wc -l {{}} +"), &BTreeMap::new()).await?;
        let usage_start = parse_positions(&r.stdout, &sessions);
        if resume && !usage_start.keys().any(|p| format!("/{p}").contains(&format!("/{session_id}/session.jsonl"))) {
            bail!("Cannot resume Muse Code: the saved session log is missing");
        }
        let manifest = json!({
            "session_id": session_id,
            "usage_start": usage_start,
            "copied_until": if resume { json!(usage_start) } else { json!({}) },
        });
        std::fs::create_dir_all(&cx.logs_dir)?;
        let local = cx.logs_dir.join(MANIFEST);
        std::fs::write(&local, serde_json::to_string(&manifest)?)?;
        let target = format!("{LOGS}/{MANIFEST}");
        cx.env.upload_file(&local, &target).await?;
        if let Some(u) = &cx.user {
            exec_root(cx, &format!("chown {} {}", quote(u), quote(&target)), &BTreeMap::new()).await?;
        }
        exec_agent(cx, &self.run_command(&instruction, &session_id)?, &env).await?;
        Ok(())
    }

    fn populate_context_post_run(&self, logs_dir: &Path, ctx: &mut AgentContext) {
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
    use crate::task::McpServer;
    use std::sync::Arc;

    const SID: &str = "00000000-0000-4000-8000-000000000001";
    const CHILD: &str = "00000000-0000-4000-8000-000000000002";
    const SESSION: &str = "2026/09/09/00000000-0000-4000-8000-000000000001/session.jsonl";

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hbench-muse-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("environment")).unwrap();
        dir
    }

    fn agent_with(kwargs: Value, env: &[(&str, &str)], model: Option<&str>, mcp: Vec<McpServer>) -> Result<MuseCode> {
        let dir = tmp();
        std::fs::write(dir.join("task.toml"), "[environment]\ndocker_image='x'\n").unwrap();
        std::fs::write(dir.join("instruction.md"), "x").unwrap();
        let task = Arc::new(crate::task::Task::load(&dir).unwrap());
        MuseCode::new(AgentArgs {
            model_name: model.map(String::from),
            kwargs: serde_json::from_value(kwargs).unwrap(),
            extra_env: env.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            mcp_servers: mcp,
            skills: vec![],
            logs_dir: dir,
            task,
        })
    }

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<MuseCode> {
        agent_with(kwargs, env, model, vec![])
    }

    /// upstream test fixture's `_write_session` (durable envelope v1).
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
        let a = agent(json!({"reasoning_effort": "xhigh", "max_model_steps": 50}), &[("META_API_KEY", "test-key")], Some("meta/muse-spark-1.2")).unwrap();
        let cmd = a.run_command("do the task", SID).unwrap();
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
        assert!(b.run_command("hi", SID).unwrap().contains("--base-url 'https://example.com/v1?x=1&y=2'"));
        let e = agent(json!({"base_url": "https://explicit/v1"}), &[("META_BASE_URL", "https://other")], None).unwrap();
        assert_eq!(e.model_connection().base_url.as_deref(), Some("https://explicit/v1"));
        assert_eq!(e.run_command("hi", SID).unwrap().matches("--base-url").count(), 1);
        for bad in [json!({"max_model_steps": 0}), json!({"max_model_steps": -1}), json!({"reasoning_effort": "none"}), json!({"reasoning_effort": "invalid"})] {
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
        let srv = |name: &str, t: &str, url: Option<&str>, cmd: Option<&str>| McpServer { name: name.into(), transport: t.into(), url: url.map(String::from), command: cmd.map(String::from), args: if cmd.is_some() { vec!["--root".into(), "a b".into()] } else { vec![] } };
        let config = json!({"runtime_capabilities": {"verification": false}, "mcp_servers": {"files": {"transport": "stdio", "command": "old"}, "extra": {"transport": "stdio", "command": "keep"}}});
        let a = agent_with(json!({"config": config}), &[], None, vec![srv("files", "stdio", None, Some("server")), srv("remote", "streamable-http", Some("https://example.com/mcp"), None)]).unwrap();
        let eff = a.effective_config().unwrap();
        assert_eq!(eff["schema_version"], 1);
        assert_eq!(eff["mcp_servers"]["files"], json!({"transport": "stdio", "command": "server", "args": ["--root", "a b"], "enabled": true, "mode": "required"}));
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
        MuseCode::populate_context_post_run(&agent(json!({}), &[], None).unwrap(), &logs, &mut ctx);
        assert_eq!((ctx.n_input_tokens, ctx.n_cache_tokens, ctx.n_output_tokens, ctx.cost_usd), (Some(120), Some(56), Some(16), None));
        // Resume boundary: root lines up to 7 (first model_completed is line 6) are excluded.
        std::fs::write(logs.join(MANIFEST), json!({"session_id": SID, "usage_start": {SESSION: 7}}).to_string()).unwrap();
        let u = usage_from_logs(&logs).unwrap();
        assert_eq!((u.prompt, u.cached, u.completion), (Some(100), Some(48), Some(11)));
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
