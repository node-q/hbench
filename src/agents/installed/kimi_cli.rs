//! Kimi CLI (port of upstream's agents/installed/kimi_cli.py).

use super::{ensure_system_dependencies, exec_agent, exec_error};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const OUTPUT_FILENAME: &str = "kimi-cli.txt";
const STDERR_FILENAME: &str = "kimi-cli.stderr.log";
const API_KEY_PLACEHOLDER: &str = "__HBENCH_KIMI_API_KEY__";
const DEFAULT_MAX_CONTEXT_SIZE: i64 = 131072;
/// kimi-cli silently lets these env vars override the config file; unset them before `kimi`.
const ENV_OVERRIDES_TO_NEUTRALIZE: &str = "OPENAI_API_KEY OPENAI_BASE_URL KIMI_API_KEY KIMI_BASE_URL";

/// provider -> (kimi provider type, default base_url, api-key env names)
fn provider_config(provider: &str) -> Option<(&'static str, &'static str, &'static [&'static str])> {
    Some(match provider {
        "moonshot" => ("kimi", "https://api.moonshot.cn/v1", &["MOONSHOT_API_KEY"]),
        "kimi" => ("kimi", "https://api.kimi.com/coding/v1", &["KIMI_API_KEY", "MOONSHOT_API_KEY"]),
        "openai" => ("openai_legacy", "https://api.openai.com/v1", &["OPENAI_API_KEY"]),
        "anthropic" => ("anthropic", "https://api.anthropic.com", &["ANTHROPIC_API_KEY"]),
        "gemini" | "google" => ("gemini", "https://generativelanguage.googleapis.com", &["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
        "openrouter" => ("openai_legacy", "https://openrouter.ai/api/v1", &["OPENROUTER_API_KEY"]),
        _ => return None,
    })
}
const PROVIDERS: &str = "['anthropic', 'gemini', 'google', 'kimi', 'moonshot', 'openai', 'openrouter']";

/// upstream's `options_model` (pydantic, extra="forbid"): reject unknown kwargs and native config.
pub(super) fn validate_kwargs(name: &str, args: &AgentArgs, extra: &[&str]) -> Result<()> {
    if args.kwargs.get("config").is_some_and(|v| !v.is_null()) {
        bail!("Agent '{name}' does not support config. You can implement it.");
    }
    let unknown: Vec<String> = args
        .kwargs
        .keys()
        .filter(|k| !["version", "prompt_template_path", "config"].contains(&k.as_str()) && !extra.contains(&k.as_str()))
        .map(|k| format!("  Unknown option '{k}'."))
        .collect();
    if !unknown.is_empty() {
        bail!("Invalid kwargs for agent '{name}':\n{}\nSee hbench agent schema {name} for available options.", unknown.join("\n"));
    }
    Ok(())
}

/// `json.loads(s, strict=False)`: raw control characters are allowed inside strings.
pub(super) fn loads_lenient(s: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str(s) {
        return Some(v);
    }
    let (mut out, mut in_str, mut esc) = (String::with_capacity(s.len()), false, false);
    for c in s.chars() {
        if in_str && !esc && (c as u32) < 0x20 {
            out.push_str(&format!("\\u{:04x}", c as u32));
            continue;
        }
        if in_str && !esc && c == '\\' {
            esc = true;
        } else {
            if !esc && c == '"' {
                in_str = !in_str;
            }
            esc = false;
        }
        out.push(c);
    }
    serde_json::from_str(&out).ok()
}

pub struct KimiCli {
    args: AgentArgs,
    version: Option<String>,
    max_context_size: i64,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(KimiCli::new(args)?))
}

impl KimiCli {
    fn new(args: AgentArgs) -> Result<KimiCli> {
        validate_kwargs("kimi-cli", &args, &["api_key", "base_url", "model_info"])?;
        for k in ["api_key", "base_url"] {
            if args.kwargs.get(k).is_some_and(|v| !(v.is_null() || v.is_string() || v.is_number())) {
                bail!("Invalid kwargs for agent 'kimi-cli':\n  {k}: Input should be a valid string");
            }
        }
        if args.kwargs.get("model_info").is_some_and(|v| !(v.is_null() || v.is_object())) {
            bail!("Invalid kwargs for agent 'kimi-cli':\n  model_info: Input should be a valid dictionary");
        }
        let mut a = KimiCli { version: args.version_kw(), args, max_context_size: DEFAULT_MAX_CONTEXT_SIZE };
        a.max_context_size = a.resolve_max_context_size();
        Ok(a)
    }

    /// upstream's `_get_env(*names)`: agent env (all names) before host env (all names).
    fn get_env_any(&self, names: &[&str]) -> Option<String> {
        names
            .iter()
            .find_map(|n| self.args.extra_env.get(*n).cloned())
            .or_else(|| names.iter().find_map(|n| std::env::var(n).ok()))
    }

    fn resolve_api_key(&self, provider: &str) -> String {
        if let Some(k) = self.args.kw("api_key") {
            return k;
        }
        provider_config(provider).and_then(|(_, _, keys)| self.get_env_any(keys)).unwrap_or_default()
    }

    fn resolve_max_context_size(&self) -> i64 {
        if let Some(n) = self.get_env_any(&["KIMI_MODEL_MAX_CONTEXT_SIZE"]).and_then(|v| v.trim().parse::<i64>().ok()) {
            return n;
        }
        let Some(model) = &self.args.model_name else { return DEFAULT_MAX_CONTEXT_SIZE };
        let lowered = model.to_lowercase();
        if ["kimi-k2.7", "kimi-k2.6", "kimi-k2.5", "kimi-k2-thinking"].iter().any(|t| lowered.contains(t)) {
            return 262144;
        }
        // ponytail: upstream asks litellm's model table; we only honor `model_info` registered via kwargs.
        self.args.kwargs.get("model_info").and_then(|m| m["max_input_tokens"].as_i64()).filter(|n| *n != 0).unwrap_or(DEFAULT_MAX_CONTEXT_SIZE)
    }

    fn build_config_json(&self, provider: &str, model: &str) -> Result<String> {
        let Some((ptype, default_url, _)) = provider_config(provider) else {
            bail!("Unsupported provider '{provider}' for kimi-cli. Supported: {PROVIDERS}");
        };
        let base_url = self.args.kw("base_url").filter(|s| !s.is_empty()).unwrap_or_else(|| default_url.into());
        let api_key = self.resolve_api_key(provider);
        Ok(json!({
            "default_model": "model",
            "default_yolo": true,
            "providers": {"hbench": {"type": ptype, "base_url": base_url, "api_key": if api_key.is_empty() { "" } else { API_KEY_PLACEHOLDER }}},
            "models": {"model": {"provider": "hbench", "model": model, "max_context_size": self.max_context_size}},
        })
        .to_string())
    }

    fn mcp_config_json(&self) -> Option<String> {
        if self.args.mcp_servers.is_empty() {
            return None;
        }
        let mut servers = serde_json::Map::new();
        for s in &self.args.mcp_servers {
            let v = if s.transport() == "stdio" { json!({"command": s.command, "args": s.args}) } else { json!({"url": s.url}) };
            servers.insert(s.name.clone(), v);
        }
        Some(json!({"mcpServers": servers}).to_string())
    }

    fn skills_dir(&self) -> Option<String> {
        self.args.task.config.environment.skills_dir.clone().or_else(|| (!self.args.skills.is_empty()).then(|| "/hbench/skills".into()))
    }

    /// (setup command, run command, env) for one invocation.
    fn commands(&self, instruction: &str, resume: bool) -> Result<(String, String, BTreeMap<String, String>)> {
        let Some((provider, model)) = self.args.model_name.as_deref().and_then(|m| m.split_once('/')) else {
            bail!("Model name must be in format provider/model_name");
        };
        let config_json = self.build_config_json(provider, model)?;
        let prompt = json!({"jsonrpc": "2.0", "method": "prompt", "id": "1", "params": {"user_input": instruction}}).to_string();
        let mut env = BTreeMap::from([
            ("KIMI_SHARE_DIR".to_string(), "/logs/agent/kimi/share".to_string()),
            ("HBENCH_KIMI_CONFIG_JSON".to_string(), config_json),
            ("HBENCH_KIMI_API_KEY".to_string(), self.resolve_api_key(provider)),
        ]);
        for k in provider_config(provider).map(|p| p.2).unwrap_or(&[]) {
            if let Some(v) = self.get_env_any(&[k]).filter(|v| !v.is_empty()) {
                env.insert(k.to_string(), v);
            }
        }
        let write_config = "import json, os, pathlib; config = json.loads(os.environ['HBENCH_KIMI_CONFIG_JSON']); \
            config['providers']['hbench']['api_key'] = os.environ.get('HBENCH_KIMI_API_KEY', ''); \
            pathlib.Path('/tmp/kimi-config.json').write_text(json.dumps(config))";
        let mut setup = vec![format!("export PATH=\"$HOME/.local/bin:$PATH\"; uv run --no-project python -c {}", quote(write_config))];
        if let Some(d) = self.skills_dir() {
            setup.push(format!("mkdir -p ~/.kimi/skills && cp -r {}/* ~/.kimi/skills/ 2>/dev/null || true", quote(&d)));
        }
        let mcp = self.mcp_config_json();
        if let Some(m) = &mcp {
            setup.push(format!("mkdir -p /tmp && echo {} > /tmp/kimi-mcp.json", quote(m)));
        }
        let mcp_flag = if mcp.is_some() { "--mcp-config-file /tmp/kimi-mcp.json " } else { "" };
        let resume_flag = if resume { "--continue " } else { "" };
        let run = format!(
            "export PATH=\"$HOME/.local/bin:$PATH\"; unset {ENV_OVERRIDES_TO_NEUTRALIZE}; (echo {}; sleep 86400) | \
             kimi --config-file /tmp/kimi-config.json --wire --yolo --afk {resume_flag}{mcp_flag}2>>/logs/agent/{STDERR_FILENAME} | (\
             while IFS= read -r line; do echo \"$line\" >> /logs/agent/{OUTPUT_FILENAME}; case \"$line\" in *'\"id\":\"1\"'*) break ;; esac; done; \
             uv run --no-project python -c {}; kill 0 2>/dev/null)",
            quote(&prompt),
            quote(SAVE_SESSION)
        );
        Ok((setup.join(" && "), run, env))
    }
}

const SAVE_SESSION: &str = "import json, os, pathlib
share = pathlib.Path(os.environ['KIMI_SHARE_DIR'])
sessions = sorted((share / 'sessions').glob('*/*'), key=lambda p: p.stat().st_mtime)
if not sessions:
    raise SystemExit(0)
cfg_path = share / 'kimi.json'
try:
    cfg = json.loads(cfg_path.read_text())
except (FileNotFoundError, json.JSONDecodeError):
    cfg = {'work_dirs': []}
session_id = sessions[-1].name
work_dirs = cfg.get('work_dirs')
if not isinstance(work_dirs, list):
    work_dirs = []
    cfg['work_dirs'] = work_dirs
cwd = os.getcwd()
entry = next((item for item in work_dirs if item.get('path') == cwd), None)
if entry is None:
    entry = {'path': cwd, 'kaos': 'local'}
    work_dirs.append(entry)
entry['last_session_id'] = session_id
cfg_path.write_text(json.dumps(cfg))";

/// Wire JSONL events (`method == "event"` params); a message split by raw newlines is rejoined.
fn parse_wire_events(text: &str) -> Vec<Value> {
    let mut msgs: Vec<String> = vec![];
    for line in text.lines() {
        if line.trim_start().starts_with("{\"jsonrpc\"") {
            msgs.push(line.to_string());
        } else if let Some(last) = msgs.last_mut() {
            last.push('\n');
            last.push_str(line);
        }
    }
    msgs.iter().filter_map(|m| loads_lenient(m)).filter(|m| m["method"] == "event").map(|m| m.get("params").cloned().unwrap_or(json!({}))).collect()
}

/// upstream's trajectory final_metrics totals: each step's last StatusUpdate token_usage.
fn usage_from_events(events: &[Value]) -> Option<AgentContext> {
    let (mut steps, mut cur): (Vec<Option<Value>>, Option<Option<Value>>) = (vec![], None);
    for e in events {
        match e["type"].as_str() {
            Some("StepBegin") => steps.extend(cur.replace(None)),
            _ if cur.is_none() => {}
            Some("StatusUpdate") => cur = Some(e["payload"].get("token_usage").cloned().filter(|v| !v.is_null())),
            Some("TurnEnd") => steps.extend(cur.take()),
            _ => {}
        }
    }
    steps.extend(cur);
    if steps.is_empty() {
        return None;
    }
    let (mut prompt, mut completion, mut cached) = (0, 0, 0);
    for tu in steps.iter().flatten().filter(|t| t.as_object().is_some_and(|o| !o.is_empty())) {
        let n = |k: &str| tu[k].as_i64().unwrap_or(0);
        prompt += n("input_other") + n("input_cache_read") + n("input_cache_creation");
        completion += n("output");
        cached += n("input_cache_read");
    }
    Some(AgentContext { n_input_tokens: Some(prompt), n_output_tokens: Some(completion), n_cache_tokens: Some(cached), cost_usd: None, ..Default::default() })
}

#[async_trait::async_trait]
impl Agent for KimiCli {
    fn name(&self) -> &str {
        "kimi-cli"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        cx.env.exec_root("[ -d /installed-agent ] || mkdir -p /installed-agent", None).await?;
        ensure_system_dependencies(cx, &["curl"]).await?;
        let spec = self.version.as_ref().map(|v| format!("=={v}")).unwrap_or_default();
        exec_agent(
            cx,
            &format!(
                "set -euo pipefail; curl -LsSf https://astral.sh/uv/install.sh | bash && export PATH=\"$HOME/.local/bin:$PATH\" && \
                 uv tool install --python 3.13 kimi-cli{spec} && kimi --version"
            ),
            &BTreeMap::new(),
        )
        .await?;
        if self.version.is_none() {
            let r = cx.exec("kimi --version", &BTreeMap::new(), None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(r.stdout.trim().to_string());
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let (setup, run, env) = self.commands(&instruction, resume)?;
        exec_agent(cx, &setup, &env).await?;
        let r = cx.exec(&format!("set -o pipefail; {run}"), &env, None).await?;
        // `kill 0` ends the process group with SIGTERM (exit 143) once the prompt finished.
        if r.code != 0 && r.code != 143 {
            return Err(exec_error(&run, &r, None));
        }
        Ok(())
    }

    fn populate_context_post_run(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        let Ok(text) = std::fs::read_to_string(logs_dir.join(OUTPUT_FILENAME)) else { return };
        if let Some(c) = usage_from_events(&parse_wire_events(&text)) {
            *ctx = c;
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::Arc;

    pub fn args(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> AgentArgs {
        let dir = std::env::temp_dir().join(format!("hbench-kimi-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("environment")).unwrap();
        std::fs::write(dir.join("task.toml"), "[environment]\ndocker_image='x'\n").unwrap();
        std::fs::write(dir.join("instruction.md"), "x").unwrap();
        let task = Arc::new(crate::task::Task::load(&dir).unwrap());
        AgentArgs { load_trajectory: None,
            model_name: model.map(String::from),
            kwargs: serde_json::from_value(kwargs).unwrap(),
            extra_env: env.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            mcp_servers: vec![],
            skills: vec![],
            logs_dir: dir,
            task,
        }
    }

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<KimiCli> {
        KimiCli::new(args(kwargs, env, model))
    }

    const WIRE_TOOL_CALLS: &str = r#"{"jsonrpc":"2.0","method":"event","params":{"type":"TurnBegin","payload":{"user_input":"Read hello.py and run it"}}}
{"jsonrpc":"2.0","method":"event","params":{"type":"StepBegin","payload":{"n":1}}}
{"jsonrpc":"2.0","method":"event","params":{"type":"ContentPart","payload":{"type":"text","text":"I'll read the file."}}}
{"jsonrpc":"2.0","method":"event","params":{"type":"ToolCall","payload":{"type":"function","id":"ReadFile:0","function":{"name":"ReadFile","arguments":""},"extras":null}}}
{"jsonrpc":"2.0","method":"event","params":{"type":"StatusUpdate","payload":{"context_usage":0.045,"token_usage":{"input_other":1143,"output":40,"input_cache_read":4864,"input_cache_creation":0},"message_id":"msg-2"}}}
{"jsonrpc":"2.0","method":"event","params":{"type":"ToolResult","payload":{"tool_call_id":"ReadFile:0","return_value":{"is_error":false,"output":"print('hello world')\n","message":"1 lines read","display":[],"extras":null}}}}
{"jsonrpc":"2.0","method":"event","params":{"type":"StepBegin","payload":{"n":2}}}
{"jsonrpc":"2.0","method":"event","params":{"type":"StatusUpdate","payload":{"context_usage":0.046,"token_usage":{"input_other":205,"output":27,"input_cache_read":5888,"input_cache_creation":0},"message_id":"msg-3"}}}
{"jsonrpc":"2.0","method":"event","params":{"type":"StepBegin","payload":{"n":3}}}
{"jsonrpc":"2.0","method":"event","params":{"type":"StatusUpdate","payload":{"context_usage":0.047,"token_usage":{"input_other":261,"output":20,"input_cache_read":5888,"input_cache_creation":0},"message_id":"msg-4"}}}
{"jsonrpc":"2.0","method":"event","params":{"type":"TurnEnd","payload":{}}}
{"jsonrpc":"2.0","id":"1","result":{"status":"finished"}}"#;

    #[test]
    fn usage_from_wire() {
        let c = usage_from_events(&parse_wire_events(WIRE_TOOL_CALLS)).unwrap();
        assert_eq!(c.n_input_tokens, Some((1143 + 4864) + (205 + 5888) + (261 + 5888)));
        assert_eq!(c.n_output_tokens, Some(40 + 27 + 20));
        assert_eq!(c.n_cache_tokens, Some(4864 + 5888 + 5888));
        assert_eq!(c.cost_usd, None);
        // cache creation counts toward prompt tokens
        let e = r#"{"jsonrpc":"2.0","method":"event","params":{"type":"StepBegin","payload":{"n":1}}}
{"jsonrpc":"2.0","method":"event","params":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":100,"output":1,"input_cache_read":200,"input_cache_creation":300}}}}"#;
        assert_eq!(usage_from_events(&parse_wire_events(e)).unwrap().n_input_tokens, Some(600));
        assert!(usage_from_events(&parse_wire_events("")).is_none());
    }

    #[test]
    fn multiline_tool_result() {
        let text = "{\"jsonrpc\":\"2.0\",\"method\":\"event\",\"params\":{\"type\":\"StepBegin\",\"payload\":{\"n\":1}}}\n\
            {\"jsonrpc\":\"2.0\",\"method\":\"event\",\"params\":{\"type\":\"ToolResult\",\"payload\":{\"tool_call_id\":\"ReadFile:0\",\"return_value\":{\"output\":\"     1\tdef foo():\n     2\t    pass\n\"}}}}\n\
            {\"jsonrpc\":\"2.0\",\"method\":\"event\",\"params\":{\"type\":\"TurnEnd\",\"payload\":{}}}";
        let ev = parse_wire_events(text);
        assert_eq!(ev.len(), 3);
        assert_eq!(ev[1]["payload"]["tool_call_id"], "ReadFile:0");
        assert!(ev[1]["payload"]["return_value"]["output"].as_str().unwrap().contains("\tdef foo():\n"));
    }

    #[test]
    fn config_and_commands() {
        let a = agent(json!({}), &[("MOONSHOT_API_KEY", "sk-m")], Some("moonshot/kimi-k2-0905-preview")).unwrap();
        let cfg: Value = serde_json::from_str(&a.build_config_json("moonshot", "kimi-k2-0905-preview").unwrap()).unwrap();
        assert_eq!(cfg["providers"]["hbench"]["type"], "kimi");
        assert_eq!(cfg["providers"]["hbench"]["api_key"], API_KEY_PLACEHOLDER);
        assert_eq!(cfg["models"]["model"]["max_context_size"], 131072);
        let (setup, run, env) = a.commands("solve the task", false).unwrap();
        assert!(setup.contains("/tmp/kimi-config.json"));
        assert!(run.contains("--wire --yolo --afk ") && run.contains("solve the task") && run.contains("kill 0") && !run.contains("--continue"));
        assert_eq!(env["HBENCH_KIMI_API_KEY"], "sk-m");
        assert_eq!(env["MOONSHOT_API_KEY"], "sk-m");
        assert!(a.commands("x", true).unwrap().1.contains("--continue "));

        let o = agent(json!({}), &[], Some("openrouter/moonshotai/kimi-k2.6")).unwrap();
        let cfg: Value = serde_json::from_str(&o.build_config_json("openrouter", "moonshotai/kimi-k2.6").unwrap()).unwrap();
        assert_eq!(cfg["providers"]["hbench"]["type"], "openai_legacy");
        assert_eq!(cfg["providers"]["hbench"]["base_url"], "https://openrouter.ai/api/v1");
        assert_eq!(cfg["providers"]["hbench"]["api_key"], "");
        assert_eq!(cfg["models"]["model"]["max_context_size"], 262144);

        let m = agent(json!({"model_info": {"max_input_tokens": 200000}, "base_url": "http://x"}), &[], Some("moonshot/k")).unwrap();
        let cfg: Value = serde_json::from_str(&m.build_config_json("moonshot", "k").unwrap()).unwrap();
        assert_eq!((cfg["models"]["model"]["max_context_size"].as_i64(), cfg["providers"]["hbench"]["base_url"].as_str()), (Some(200000), Some("http://x")));
        let e = agent(json!({}), &[("KIMI_MODEL_MAX_CONTEXT_SIZE", "1048576")], Some("kimi/kimi-k2.6")).unwrap();
        assert_eq!(e.max_context_size, 1048576);
    }

    #[test]
    fn validation() {
        assert!(agent(json!({"bogus": 1}), &[], None).is_err());
        assert!(agent(json!({"config": {}}), &[], None).is_err());
        assert!(agent(json!({"model_info": "x"}), &[], None).is_err());
        let a = agent(json!({}), &[], Some("nomodel")).unwrap();
        assert!(a.commands("x", false).unwrap_err().to_string().contains("provider/model_name"));
        let b = agent(json!({}), &[], Some("acme/m")).unwrap();
        assert!(b.commands("x", false).unwrap_err().to_string().contains("Unsupported provider 'acme'"));
    }
}
