//! OpenCode installed agent, incl. ACP target mode.

use super::common::parse_jsonl;
use super::common::{NVM_LOAD, deep_merge_maps, detect_version, installed_dir, nvm_load, opts, skills_dir, system_or_nvm_node};
use super::pins;
use super::{connection, ensure_system_dependencies, exec_agent, exec_root_checked, safe_bin_symlink_command};
use crate::agents::model_connection::Spec;
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::atif;
use crate::env::{ErrorKind, kind_err, quote};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const SPEC: Spec = Spec { default_provider: None, api_key_envs: &[], base_url_envs: &[], passthrough: true };
const WEB_TOOLS: [&str; 2] = ["websearch", "webfetch"];

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    let overlay = match args.kwargs.get("opencode_config") {
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(Value::Object(o)) => o.clone(),
        Some(_) => bail!("Invalid kwargs for agent 'opencode':\n  opencode_config: must be an object"),
    };
    let a = OpenCode { version: args.version_kw(), args, overlay };
    a.cli_flags()?;
    a.disable_web_search()?;
    Ok(Box::new(a))
}

pub struct OpenCode {
    args: AgentArgs,
    version: Option<String>,
    overlay: serde_json::Map<String, Value>,
}

impl OpenCode {
    fn cli_flags(&self) -> Result<String> {
        Ok(opts::string(&self.args, "variant", None)?.map(|v| format!("--variant {}", quote(&v))).unwrap_or_default())
    }
    fn disable_web_search(&self) -> Result<bool> {
        Ok(opts::boolean(&self.args, "disable_web_search", None)?.unwrap_or(false))
    }

    /// ~/.config/opencode/opencode.json: MCP servers + provider model registration, overlaid with `opencode_config`.
    fn config(&self) -> Result<Option<Value>> {
        let mut c = serde_json::Map::new();
        if !self.args.mcp_servers.is_empty() {
            let mut mcp = serde_json::Map::new();
            for s in &self.args.mcp_servers {
                let v = if s.transport() == "stdio" {
                    let cmd: Vec<String> =
                        s.command.iter().cloned().chain(s.command.as_ref().map(|_| s.args.clone()).unwrap_or_default()).collect();
                    json!({"type": "local", "command": cmd})
                } else {
                    // oauth=false disables opencode's 401-triggered OAuth auto-detection.
                    json!({"type": "remote", "url": s.url, "oauth": false})
                };
                mcp.insert(s.name.clone(), v);
            }
            c.insert("mcp".into(), Value::Object(mcp));
        }
        if let Some((prov, model_id)) = self.args.model_name.as_deref().and_then(|m| m.split_once('/')) {
            let mut pc = json!({"models": {model_id: {}}});
            if let Some(u) = connection(&self.args, SPEC).configured_base_url
                && ["anthropic", "google", "openai"].contains(&prov)
            {
                pc["options"] = json!({"baseURL": u}); // opencode reads baseURL from provider.options
            }
            c.insert("provider".into(), json!({prov: pc}));
        }
        deep_merge_maps(&mut c, &self.overlay);
        if self.disable_web_search()? {
            let mut perm = match c.remove("permission") {
                Some(Value::Object(p)) => p,
                Some(p) if p.as_bool() != Some(false) && !p.is_null() && p != "" => serde_json::Map::from_iter([("*".to_string(), p)]),
                _ => serde_json::Map::new(),
            };
            for t in WEB_TOOLS {
                perm.insert(t.into(), json!("deny"));
            }
            c.insert("permission".into(), Value::Object(perm));
        }
        Ok((!c.is_empty()).then_some(Value::Object(c)))
    }
}

/// Messages of `error` events in the run output.
pub fn error_messages(text: &str) -> Vec<String> {
    parse_jsonl(text)
        .iter()
        .filter(|e| e["type"] == "error")
        .map(|e| {
            let err = &e["error"];
            let s = |v: &Value| v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
            if err.is_object() {
                [&err["data"]["message"], &err["name"]].into_iter().find(|v| !v.is_null() && *v != "").map(s).unwrap_or_else(|| s(err))
            } else {
                s(err)
            }
        })
        .collect()
}

/// Totals over `step_start` .. `step_finish` turns (opencode.txt -> ATIF final_metrics).
pub fn parse_opencode_usage(text: &str) -> Option<AgentContext> {
    let (mut open, mut turns) = (false, 0);
    let (mut cost, mut input, mut output, mut cache) = (0.0, 0i64, 0i64, 0i64);
    for e in parse_jsonl(text) {
        match e["type"].as_str() {
            Some("step_start") => open = true,
            Some("step_finish") if open => {
                open = false;
                turns += 1;
                let p = &e["part"];
                let n = |v: &Value| v.as_i64().unwrap_or(0);
                let read = n(&p["tokens"]["cache"]["read"]);
                cost += p["cost"].as_f64().unwrap_or(0.0);
                input += n(&p["tokens"]["input"]) + read;
                output += n(&p["tokens"]["output"]);
                cache += read;
            }
            _ => {}
        }
    }
    (turns > 0).then(|| AgentContext {
        n_input_tokens: Some(input),
        n_cache_tokens: Some(cache),
        n_output_tokens: Some(output),
        cost_usd: (cost != 0.0).then_some(cost),
        ..Default::default()
    })
}

/// A `tool_use` part: the call, and its result as `(call id, output or error)`.
fn tool_call(part: &Value) -> (atif::ToolCall, atif::ObservationResult) {
    let state = &part["state"];
    let id = part["callID"].as_str().unwrap_or_default().to_string();
    let call = atif::ToolCall {
        tool_call_id: id.clone(),
        function_name: part["tool"].as_str().unwrap_or_default().into(),
        arguments: state["input"].clone(),
        extra: None,
    };
    let output = if state["status"] == "error" { &state["error"] } else { &state["output"] };
    (call, atif::ObservationResult { source_call_id: Some(id), content: Some(output.clone()), ..Default::default() })
}

/// A `step_finish` part's metrics, counted as [`parse_opencode_usage`] counts them. `tokens.input` and `tokens.output`
/// leave out cache writes and reasoning; those two stay in `extra`.
fn step_metrics(part: &Value) -> atif::Metrics {
    let n = |v: &Value| v.as_i64().unwrap_or(0);
    let tokens = &part["tokens"];
    let read = n(&tokens["cache"]["read"]);
    let uncounted = [("cache_write_tokens", n(&tokens["cache"]["write"])), ("reasoning_tokens", n(&tokens["reasoning"]))];
    let extra: serde_json::Map<String, Value> = uncounted.into_iter().filter(|(_, v)| *v != 0).map(|(k, v)| (k.into(), v.into())).collect();
    atif::Metrics {
        prompt_tokens: Some(n(&tokens["input"]) + read),
        completion_tokens: Some(n(&tokens["output"])),
        cached_tokens: Some(read),
        cost_usd: part["cost"].as_f64().filter(|c| *c != 0.0),
        extra: (!extra.is_empty()).then_some(extra),
        ..Default::default()
    }
}

fn has_content(step: &atif::Step) -> bool {
    step.reasoning_content.is_some() || step.tool_calls.is_some() || step.message != ""
}

/// The `--format=json` stream of opencode, or of its fork mimo, -> ATIF. Each `step_start` .. `step_finish` turn is one
/// model call; the stream carries neither the instruction nor the model's name.
pub fn atif_trajectory(name: &str, text: &str, version: Option<&str>, model: Option<&str>) -> Option<atif::Trajectory> {
    let (mut steps, mut open, mut session_id) = (vec![], None::<atif::Step>, None);
    for e in parse_jsonl(text) {
        session_id = session_id.or(e["sessionID"].as_str().map(String::from));
        let part = &e["part"];
        let timestamp = e["timestamp"].as_i64().and_then(atif::timestamp_of_millis);
        let event = e["type"].as_str().unwrap_or_default();
        if event == "step_start" {
            steps.extend(open.take().filter(has_content));
        }
        if !matches!(event, "step_start" | "step_finish" | "text" | "reasoning" | "tool_use") || (event == "step_finish" && open.is_none())
        {
            continue;
        }
        let step = open.get_or_insert_with(|| atif::Step { source: "agent".into(), timestamp, message: json!(""), ..Default::default() });
        match event {
            "text" => {
                let joined = [step.message.as_str().unwrap_or_default(), part["text"].as_str().unwrap_or_default()];
                step.message = json!(joined.into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n"));
            }
            "reasoning" => {
                let reasoning = part["text"].as_str().unwrap_or_default();
                step.reasoning_content = Some(match step.reasoning_content.take() {
                    Some(earlier) => format!("{earlier}\n{reasoning}"),
                    None => reasoning.into(),
                });
            }
            "tool_use" => {
                let (call, result) = tool_call(part);
                step.tool_calls.get_or_insert_default().push(call);
                step.observation.get_or_insert_default().results.push(result);
            }
            "step_finish" => {
                step.metrics = Some(step_metrics(part));
                steps.extend(open.take());
            }
            _ => {}
        }
    }
    steps.extend(open.filter(has_content));
    let agent = atif::Agent {
        name: name.into(),
        version: version.unwrap_or("unknown").into(),
        model_name: model.map(String::from),
        ..Default::default()
    };
    atif::assemble(agent, session_id, steps)
}

const VERSION_CMD: &str = concat!(nvm_load!(), "; opencode --version");

#[async_trait::async_trait]
impl Agent for OpenCode {
    fn name(&self) -> &str {
        "opencode"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    fn acp_command(&self) -> Option<Vec<String>> {
        let mut c: Vec<String> = [
            "env",
            "OPENCODE_FAKE_VCS=git",
            "XDG_DATA_HOME=/logs/agent/opencode/xdg-data",
            "XDG_STATE_HOME=/logs/agent/opencode/xdg-state",
        ]
        .map(String::from)
        .to_vec();
        if let Some(m) = &self.args.model_name {
            // Inline config outranks opencode.json, like --model in run(). JSON with `: ` spacing.
            c.push(format!("OPENCODE_CONFIG_CONTENT={{\"model\": {}}}", json!(m)));
        }
        c.extend(["opencode".into(), "acp".into()]);
        Some(c)
    }

    fn acp_env(&self) -> BTreeMap<String, String> {
        connection(&self.args, SPEC).env
    }

    async fn acp_install(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        self.args.require_provider_model()?;
        if opts::string(&self.args, "variant", None)?.is_some() {
            bail!("OpenCode variant is not supported in ACP target mode");
        }
        let none = BTreeMap::new();
        if let Some(c) = self.config()? {
            exec_agent(
                cx,
                &format!(
                    "mkdir -p ~/.config/opencode && echo {} > ~/.config/opencode/opencode.json",
                    quote(&serde_json::to_string_pretty(&c)?)
                ),
                &none,
            )
            .await?;
        }
        if let Some(d) = skills_dir(&self.args) {
            exec_agent(
                cx,
                &format!("mkdir -p ~/.config/opencode/skills && cp -r {}/* ~/.config/opencode/skills/ 2>/dev/null || true", quote(&d)),
                &none,
            )
            .await?;
        }
        // The npm bin is a native binary; it only needs to be reachable off the nvm PATH.
        let r = exec_agent(cx, &format!("{NVM_LOAD}; command -v opencode"), &none).await?;
        let Some(bin) = r.stdout.trim().lines().last() else { bail!("Could not locate the opencode binary for ACP mode") };
        exec_root_checked(cx, &safe_bin_symlink_command(bin, "/usr/local/bin/opencode"), &none).await.map(|_| ())
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        // nodejs/npm cover musl images (nvm can't); coreutils provides stdbuf.
        ensure_system_dependencies(cx, &["curl", "bash", "coreutils", "nodejs", "npm"]).await?;
        let spec = pins::OPENCODE.npm_spec(self.version.as_deref());
        exec_agent(
            cx,
            &format!("set -euo pipefail; {} && npm i -g {spec} && opencode --version", system_or_nvm_node(22)),
            &BTreeMap::new(),
        )
        .await?;
        if self.version.is_none() {
            self.version = detect_version(cx, VERSION_CMD, |s| s.trim().to_string()).await;
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        self.args.require_provider_model()?;
        let model = self.args.model_name.clone().unwrap_or_default();
        let mut env = connection(&self.args, SPEC).env;
        env.insert("OPENCODE_FAKE_VCS".into(), "git".into());
        env.insert("XDG_DATA_HOME".into(), "/logs/agent/opencode/xdg-data".into());
        env.insert("XDG_STATE_HOME".into(), "/logs/agent/opencode/xdg-state".into());
        if let Some(d) = skills_dir(&self.args) {
            exec_agent(
                cx,
                &format!("mkdir -p ~/.config/opencode/skills && cp -r {}/* ~/.config/opencode/skills/ 2>/dev/null || true", quote(&d)),
                &env,
            )
            .await?;
        }
        if let Some(c) = self.config()? {
            exec_agent(
                cx,
                &format!(
                    "mkdir -p ~/.config/opencode && echo {} > ~/.config/opencode/opencode.json",
                    quote(&serde_json::to_string_pretty(&c)?)
                ),
                &env,
            )
            .await?;
        }
        let flags = self.cli_flags()?;
        let flags_arg = if flags.is_empty() { String::new() } else { format!("{flags} ") };
        let cmd = format!(
            "{NVM_LOAD}; opencode --model={model} run --format=json {}{flags_arg}--thinking \
             --dangerously-skip-permissions -- {} 2>&1 </dev/null | stdbuf -oL tee /logs/agent/opencode.txt",
            if resume { "--continue " } else { "" },
            quote(&instruction)
        );
        // /logs isn't bind-mounted, so check the tee'd stream (same content as opencode.txt).
        let r = exec_agent(cx, &cmd, &env).await?;
        let msgs = error_messages(&r.stdout);
        if !msgs.is_empty() {
            return Err(kind_err(
                ErrorKind::NonZeroAgentExitCode,
                format!("OpenCode emitted error event(s): {}", msgs[..msgs.len().min(3)].join("; ")),
            ));
        }
        Ok(())
    }

    fn collect_usage(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        let Ok(text) = std::fs::read_to_string(logs_dir.join("opencode.txt")) else { return };
        if let Some(c) = parse_opencode_usage(&text) {
            ctx.n_input_tokens = c.n_input_tokens;
            ctx.n_cache_tokens = c.n_cache_tokens;
            ctx.n_output_tokens = c.n_output_tokens;
            ctx.cost_usd = c.cost_usd;
        }
        if let Some(t) = atif_trajectory("opencode", &text, self.version.as_deref(), self.args.model_name.as_deref()) {
            atif::write_trajectory(logs_dir, &t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;

    fn agent(kwargs: Value, env: &[(&str, &str)]) -> OpenCode {
        let args = common::test_args(kwargs.clone(), env, Some("openai/gpt-5"));
        OpenCode { version: None, overlay: kwargs.get("opencode_config").and_then(Value::as_object).cloned().unwrap_or_default(), args }
    }

    #[test]
    fn config_flags_and_validation() {
        let mut a = agent(
            json!({"variant": "high", "disable_web_search": true, "opencode_config": {"permission": {"bash": "ask", "webfetch": "allow"}, "provider": {"openai": {"options": {"timeout": 5}}}}}),
            &[("OPENAI_BASE_URL", "http://p/v1")],
        );
        a.args.mcp_servers =
            vec![serde_json::from_value(json!({"name": "fs", "transport": "stdio", "command": "npx", "args": ["-y", "fs"]})).unwrap()];
        assert_eq!(a.cli_flags().unwrap(), "--variant high");
        let c = a.config().unwrap().unwrap();
        assert_eq!(c["mcp"]["fs"], json!({"type": "local", "command": ["npx", "-y", "fs"]}));
        assert_eq!(c["provider"]["openai"], json!({"models": {"gpt-5": {}}, "options": {"baseURL": "http://p/v1", "timeout": 5}}));
        assert_eq!(c["permission"], json!({"bash": "ask", "webfetch": "deny", "websearch": "deny"}));
        assert_eq!(
            agent(json!({"disable_web_search": true, "opencode_config": {"permission": "allow"}}), &[]).config().unwrap().unwrap()["permission"]
                ["*"],
            "allow"
        );
        assert!(create(common::test_args(json!({"opencode_config": "x"}), &[], None)).is_err());
        assert!(create(common::test_args(json!({"disable_web_search": "maybe"}), &[], None)).is_err());
    }

    #[test]
    fn acp_command_contents() {
        let a = agent(json!({}), &[("OPENAI_API_KEY", "k")]);
        assert_eq!(
            a.acp_command().unwrap(),
            vec![
                "env",
                "OPENCODE_FAKE_VCS=git",
                "XDG_DATA_HOME=/logs/agent/opencode/xdg-data",
                "XDG_STATE_HOME=/logs/agent/opencode/xdg-state",
                "OPENCODE_CONFIG_CONTENT={\"model\": \"openai/gpt-5\"}",
                "opencode",
                "acp"
            ]
        );
        assert_eq!(a.acp_env().get("OPENAI_API_KEY").map(String::as_str), Some("k"));
    }

    /// A real run's stream, trimmed: one call with reasoning and a tool, one with the answer, then a call cut off.
    const STREAM: &str = r#"{"type":"step_start","timestamp":1793437566906,"sessionID":"ses_1","part":{"id":"prt_1","messageID":"msg_1","type":"step-start"}}
{"type":"reasoning","timestamp":1793437568219,"sessionID":"ses_1","part":{"id":"prt_2","type":"reasoning","text":"I'll use the write tool."}}
{"type":"tool_use","timestamp":1793437568219,"sessionID":"ses_1","part":{"type":"tool","tool":"write","callID":"call_1","state":{"status":"completed","input":{"filePath":"/app/hello.txt","content":"Hello, world!"},"output":"Wrote file successfully."}}}
{"type":"tool_use","timestamp":1793437568220,"sessionID":"ses_1","part":{"type":"tool","tool":"read","callID":"call_2","state":{"status":"error","input":{"filePath":"/x"},"error":"File not found"}}}
{"type":"step_finish","timestamp":1793437568285,"sessionID":"ses_1","part":{"reason":"tool-calls","tokens":{"total":7368,"input":7291,"output":77,"reasoning":4,"cache":{"write":9,"read":0}},"cost":0.5}}
{"type":"step_start","timestamp":1793437568772,"sessionID":"ses_1","part":{"type":"step-start"}}
{"type":"text","timestamp":1793437569124,"sessionID":"ses_1","part":{"type":"text","text":"Done"}}
{"type":"step_finish","timestamp":1793437569156,"sessionID":"ses_1","part":{"reason":"stop","tokens":{"total":7409,"input":21,"output":21,"reasoning":0,"cache":{"write":0,"read":7367}},"cost":0}}
{"type":"step_start","timestamp":1793437570000,"sessionID":"ses_1","part":{"type":"step-start"}}
{"type":"step_start","timestamp":1793437571000,"sessionID":"ses_1","part":{"type":"step-start"}}
{"type":"text","timestamp":1793437571500,"sessionID":"ses_1","part":{"type":"text","text":"partial"}}
"#;

    #[test]
    fn stream_to_atif() {
        let a = atif_trajectory("opencode", STREAM, Some("1.19.3"), Some("openai/m")).unwrap();
        assert_eq!(
            (a.session_id.as_deref(), a.agent.version.as_str(), a.agent.model_name.as_deref()),
            (Some("ses_1"), "1.19.3", Some("openai/m"))
        );
        assert_eq!(a.steps.iter().map(|s| (s.step_id, s.source.as_str())).collect::<Vec<_>>(), [(1, "agent"), (2, "agent"), (3, "agent")]);
        let call = &a.steps[0];
        assert_eq!(call.timestamp.as_deref(), Some("2026-10-31T09:06:06.906Z"));
        assert_eq!(call.reasoning_content.as_deref(), Some("I'll use the write tool."));
        assert_eq!(call.tool_calls.as_ref().unwrap()[0].arguments["filePath"], "/app/hello.txt");
        let results = &call.observation.as_ref().unwrap().results;
        assert_eq!(
            results.iter().map(|r| (r.source_call_id.clone().unwrap(), r.content.clone().unwrap())).collect::<Vec<_>>(),
            [("call_1".to_string(), json!("Wrote file successfully.")), ("call_2".to_string(), json!("File not found"))]
        );
        let m = call.metrics.as_ref().unwrap();
        assert_eq!((m.prompt_tokens, m.completion_tokens, m.cached_tokens, m.cost_usd), (Some(7291), Some(77), Some(0), Some(0.5)));
        assert_eq!(m.extra.as_ref().unwrap()["cache_write_tokens"], 9);
        assert_eq!(m.extra.as_ref().unwrap()["reasoning_tokens"], 4);
        assert_eq!((a.steps[1].message.as_str(), a.steps[1].metrics.as_ref().unwrap().cached_tokens), (Some("Done"), Some(7367)));
        assert_eq!((a.steps[2].message.as_str(), a.steps[2].metrics.is_none()), (Some("partial"), true));
        let (f, c) = (a.final_metrics.unwrap(), parse_opencode_usage(STREAM).unwrap());
        assert_eq!(
            (f.total_prompt_tokens, f.total_completion_tokens, f.total_cached_tokens, f.total_cost_usd),
            (c.n_input_tokens, c.n_output_tokens, c.n_cache_tokens, c.cost_usd)
        );
        assert!(atif_trajectory("opencode", "{\"type\":\"step_start\"}\n{\"type\":\"error\"}\n", None, None).is_none());
    }

    #[test]
    fn usage_and_errors() {
        let log = "{\"type\":\"step_start\",\"timestamp\":1,\"sessionID\":\"s\"}\n\
            {\"type\":\"text\",\"part\":{\"type\":\"text\",\"text\":\"hi\"}}\n\
            {\"type\":\"step_finish\",\"part\":{\"cost\":0.01,\"tokens\":{\"input\":100,\"output\":20,\"reasoning\":5,\"cache\":{\"read\":300,\"write\":7}}}}\n\
            {\"type\":\"step_start\"}\n{\"type\":\"step_finish\",\"part\":{\"cost\":0.02,\"tokens\":{\"input\":10,\"output\":2,\"cache\":{\"read\":0}}}}\n\
            {\"type\":\"step_finish\",\"part\":{\"cost\":9,\"tokens\":{\"input\":999}}}\n\
            {\"type\":\"error\",\"error\":{\"name\":\"APIError\",\"data\":{\"message\":\"boom\"}}}\n{\"type\":\"error\",\"error\":\"plain\"}\n";
        let c = parse_opencode_usage(log).unwrap();
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens), (Some(410), Some(300), Some(22)));
        assert!((c.cost_usd.unwrap() - 0.03).abs() < 1e-12);
        assert_eq!(error_messages(log), vec!["boom", "plain"]);
        assert!(parse_opencode_usage("{\"type\":\"text\"}\n").is_none());
    }
}
