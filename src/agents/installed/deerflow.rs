//! DeerFlow: installs the harness package from git into a uv venv and drives it with
//! `assets/agents/deerflow/deerflow_runner.py`.

use super::common::opts::kw_int;
use super::common::{installed_dir, upload_asset};
use super::{ensure_system_dependencies, exec_agent, exec_error, exec_root};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

const RUNNER: &str = include_str!("../../../assets/agents/deerflow/deerflow_runner.py");
const DEERFLOW_REPO: &str = "https://github.com/bytedance/deer-flow.git";
const DEERFLOW_REF: &str = "7a6c4a994a86583d2a3c056ee9d0f157d4f030c2";
const REPO_DIR: &str = "/installed-agent/deer-flow";
const VENV_DIR: &str = "/opt/hbench-deerflow-venv";
const PROJECT_DIR: &str = "/installed-agent/deerflow-project";
const RUNNER_PATH: &str = "/installed-agent/deerflow-project/deerflow_runner.py";
const VENV_PYTHON: &str = "/opt/hbench-deerflow-venv/bin/python";
const MODEL_NAME: &str = "hbench-model";
const SUMMARY_PATH: &str = "/logs/agent/deerflow_summary.json";
const MAX_SUMMARY_BYTES: u64 = 65_536;
const FORWARDED_ENV_VARS: [&str; 14] = [
    "OPENROUTER_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "DEEPSEEK_API_KEY",
    "TAVILY_API_KEY",
    "SERPER_API_KEY",
    "JINA_API_KEY",
    "EXA_API_KEY",
    "FIRECRAWL_API_KEY",
    "LANGSMITH_API_KEY",
    "LANGFUSE_PUBLIC_KEY",
    "LANGFUSE_SECRET_KEY",
    "LANGFUSE_HOST",
];
const WEB_SEARCH_BACKENDS: [(&str, &str); 4] = [
    ("TAVILY_API_KEY", "deerflow.community.tavily.tools:web_search_tool"),
    ("SERPER_API_KEY", "deerflow.community.serper.tools:web_search_tool"),
    ("EXA_API_KEY", "deerflow.community.exa.tools:web_search_tool"),
    ("FIRECRAWL_API_KEY", "deerflow.community.firecrawl.tools:web_search_tool"),
];
const WEB_FETCH_BACKENDS: [(&str, &str); 2] = [
    ("FIRECRAWL_API_KEY", "deerflow.community.firecrawl.tools:web_fetch_tool"),
    ("EXA_API_KEY", "deerflow.community.exa.tools:web_fetch_tool"),
];

/// DeerFlow's `_coerce_bool` over pydantic-validated bools.
fn kw_flag(args: &AgentArgs, k: &str, default: bool) -> Result<bool> {
    match args.kwargs.get(k) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::String(s)) if matches!(s.trim().to_lowercase().as_str(), "on" | "off") => Ok(s.trim().eq_ignore_ascii_case("on")),
        Some(_) => {
            args.kw_bool(k).ok_or_else(|| anyhow::anyhow!("Invalid kwargs for agent 'deerflow':\n  {k}: Input should be a valid boolean"))
        }
    }
}

enum ConfigSource {
    None,
    Path(String),
    Inline(Value),
}

pub struct DeerFlow {
    args: AgentArgs,
    version: Option<String>,
    config: ConfigSource,
    repo_url: String,
    repo_ref: Option<String>,
    openrouter_base_url: String,
    workdir_opt: Option<String>,
    subagent: bool,
    thinking: bool,
    plan_mode: bool,
    summarize: bool,
    recursion_limit: i64,
    /// (workdir, uid, gid) resolved during setup.
    runtime: Option<(String, i64, i64)>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(DeerFlow::new(args)?))
}

impl DeerFlow {
    fn new(args: AgentArgs) -> Result<DeerFlow> {
        let config = args.kwargs.get("config").filter(|v| !v.is_null());
        let config_path = args.kwargs.get("config_path").filter(|v| !v.is_null());
        if config.is_some() && config_path.is_some() {
            bail!("'config' and 'config_path' are mutually exclusive");
        }
        let config = match config.or(config_path) {
            None => ConfigSource::None,
            Some(Value::String(p)) => {
                if !std::path::Path::new(p).is_file() {
                    bail!("Agent config file not found: {p}");
                }
                ConfigSource::Path(p.clone())
            }
            Some(v @ Value::Object(_)) => ConfigSource::Inline(v.clone()),
            Some(_) => bail!("Invalid value for 'config': expected a local file path or JSON object"),
        };
        if args.kwargs.contains_key("disable_web_search") && kw_flag(&args, "disable_web_search", false).is_err() {
            bail!("Invalid kwargs for agent 'deerflow':\n  disable_web_search: Input should be a valid boolean");
        }
        Ok(DeerFlow {
            repo_url: args.kw("repo_url").unwrap_or_else(|| DEERFLOW_REPO.into()),
            repo_ref: if args.kwargs.contains_key("repo_ref") { args.kw("repo_ref") } else { Some(DEERFLOW_REF.into()) },
            openrouter_base_url: args.kw("openrouter_base_url").unwrap_or_else(|| "https://openrouter.ai/api/v1".into()),
            workdir_opt: args.kw("workdir"),
            subagent: kw_flag(&args, "subagent", false)?,
            thinking: kw_flag(&args, "thinking", true)?,
            plan_mode: kw_flag(&args, "plan_mode", false)?,
            summarize: kw_flag(&args, "summarize", false)?,
            recursion_limit: kw_int(&args, "recursion_limit")?.unwrap_or(1000),
            version: args.version_kw(),
            config,
            runtime: None,
            args,
        })
    }

    /// (use, model_id, base_url, api-key env var).
    fn model_provider_config(&self) -> (&'static str, String, Option<String>, &'static str) {
        let model = self.args.model_name.clone().unwrap_or_else(|| "openai/gpt-4o".into());
        if let Some((provider, rest)) = model.split_once('/') {
            let direct = match provider {
                "openrouter" => {
                    return ("langchain_openai:ChatOpenAI", rest.into(), Some(self.openrouter_base_url.clone()), "OPENROUTER_API_KEY");
                }
                "openai" => Some(("langchain_openai:ChatOpenAI", "OPENAI_API_KEY")),
                "anthropic" => Some(("langchain_anthropic:ChatAnthropic", "ANTHROPIC_API_KEY")),
                "deepseek" => Some(("langchain_deepseek:ChatDeepSeek", "DEEPSEEK_API_KEY")),
                "gemini" | "google" => Some(("langchain_google_genai:ChatGoogleGenerativeAI", "GEMINI_API_KEY")),
                _ => None,
            };
            if let Some((use_, key)) = direct {
                return (use_, rest.into(), None, key);
            }
        }
        ("langchain_openai:ChatOpenAI", model, None, "OPENAI_API_KEY")
    }

    fn workdir(&self) -> Result<String> {
        match (&self.runtime, &self.workdir_opt) {
            (Some((w, _, _)), _) | (None, Some(w)) => Ok(w.clone()),
            _ => bail!("DeerFlow workdir is unavailable before runtime context initialization"),
        }
    }

    fn select_backend(&self, backends: &[(&str, &'static str)], fallback: &'static str) -> &'static str {
        backends.iter().find(|(k, _)| self.args.get_env(k).is_some()).map(|(_, u)| *u).unwrap_or(fallback)
    }

    fn model_entry(&self) -> Value {
        let (use_, model, base_url, key) = self.model_provider_config();
        let mut e = json!({"name": MODEL_NAME, "use": use_, "model": model});
        if let Some(u) = base_url {
            e["base_url"] = json!(u);
        }
        e["api_key"] = json!(format!("${key}"));
        e["request_timeout"] = json!(600.0);
        e["max_retries"] = json!(2);
        e
    }

    fn default_config(&self) -> Value {
        let tool = |name: &str, group: &str, use_: &str| json!({"name": name, "group": group, "use": use_});
        let groups: Vec<Value> = ["web", "file:read", "file:write", "bash"].iter().map(|g| json!({"name": g})).collect();
        json!({
            "config_version": 15,
            "log_level": "info",
            "tool_groups": groups,
            "tools": [
                tool("web_search", "web", self.select_backend(&WEB_SEARCH_BACKENDS, "deerflow.community.ddg_search.tools:web_search_tool")),
                tool("web_fetch", "web", self.select_backend(&WEB_FETCH_BACKENDS, "deerflow.community.jina_ai.tools:web_fetch_tool")),
                tool("ls", "file:read", "deerflow.sandbox.tools:ls_tool"),
                tool("read_file", "file:read", "deerflow.sandbox.tools:read_file_tool"),
                tool("glob", "file:read", "deerflow.sandbox.tools:glob_tool"),
                tool("grep", "file:read", "deerflow.sandbox.tools:grep_tool"),
                tool("write_file", "file:write", "deerflow.sandbox.tools:write_file_tool"),
                tool("str_replace", "file:write", "deerflow.sandbox.tools:str_replace_tool"),
                tool("bash", "bash", "deerflow.sandbox.tools:bash_tool"),
            ],
        })
    }

    fn build_config(&self) -> Result<Value> {
        let mut config = match &self.config {
            ConfigSource::Path(p) => {
                let v: Value = serde_yaml_ng::from_str(&std::fs::read_to_string(p)?)?;
                if !v.is_object() {
                    bail!("DeerFlow config file must be a YAML mapping: {p}");
                }
                v
            }
            ConfigSource::Inline(v) => v.clone(),
            ConfigSource::None => self.default_config(),
        };
        if kw_flag(&self.args, "disable_web_search", false)? {
            let keep = |k: &str, f: &dyn Fn(&Value) -> bool| -> Value {
                json!(config[k].as_array().into_iter().flatten().filter(|v| f(v)).cloned().collect::<Vec<_>>())
            };
            let groups = keep("tool_groups", &|g| g["name"] != "web");
            let tools = keep("tools", &|t| t["group"] != "web" && t["name"] != "web_search" && t["name"] != "web_fetch");
            config["tool_groups"] = groups;
            config["tools"] = tools;
        }
        // Our model entry is authoritative; other user models (and extra keys on ours) survive.
        // A legacy `hbench-model` entry is adopted as ours.
        let models = match &config["models"] {
            Value::Null => vec![],
            Value::Array(a) if a.iter().all(Value::is_object) => a.clone(),
            _ => bail!("DeerFlow config models must be a list of mappings"),
        };
        let mut preserved = vec![];
        let mut ours: Option<Value> = None;
        for m in models {
            if m["name"] == MODEL_NAME || m["name"] == "hbench-model" {
                ours.get_or_insert(m);
            } else {
                preserved.push(m);
            }
        }
        let mut ours = ours.unwrap_or_else(|| json!({}));
        let h = ours.as_object_mut().unwrap();
        for f in ["name", "use", "model", "api_key", "base_url", "request_timeout", "max_retries"] {
            h.remove(f);
        }
        h.extend(self.model_entry().as_object().unwrap().clone());
        preserved.push(ours);
        config["models"] = json!(preserved);
        // Local sandbox + host bash + identity mount of the workdir.
        let workdir = self.workdir()?;
        let mut sandbox = config["sandbox"].as_object().cloned().unwrap_or_default();
        sandbox.insert("use".into(), json!("deerflow.sandbox.local:LocalSandboxProvider"));
        sandbox.insert("allow_host_bash".into(), json!(true));
        let mut mounts: Vec<Value> = sandbox
            .get("mounts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|m| m.is_object() && m["container_path"] != workdir.as_str())
            .cloned()
            .collect();
        mounts.push(json!({"host_path": workdir, "container_path": workdir, "read_only": false}));
        sandbox.insert("mounts".into(), json!(mounts));
        config["sandbox"] = Value::Object(sandbox);
        if self.summarize {
            config["summarization"] = json!({"enabled": true, "trigger": [{"type": "fraction", "value": 0.8}]});
        }
        Ok(config)
    }

    fn steered_instruction(&self, instruction: &str) -> Result<String> {
        let w = self.workdir()?;
        Ok(format!(
            "You are operating directly on a real Linux machine through the bash and file tools. Act on the actual filesystem at real absolute paths.\n\
             Your working directory is {w}. Create and modify the task's files under {w} (e.g. {w}/<file>). Do NOT write deliverables into \
             /mnt/user-data; the task is graded on the real files under {w}.\n\nTask:\n{instruction}"
        ))
    }

    fn install_command(&self) -> String {
        let checkout = self
            .repo_ref
            .as_ref()
            .map(|r| format!("git -C {REPO_DIR} fetch --depth 1 origin {0}; git -C {REPO_DIR} checkout --quiet FETCH_HEAD; ", quote(r)))
            .unwrap_or_default();
        format!(
            "set -euo pipefail; curl -LsSf https://astral.sh/uv/install.sh | sh; if [ -f \"$HOME/.local/bin/env\" ]; then . \"$HOME/.local/bin/env\"; \
             else export PATH=\"$HOME/.local/bin:$PATH\"; fi; git clone --depth 1 {} {REPO_DIR}; {checkout}uv python install 3.12; \
             uv venv {VENV_DIR} --python 3.12; uv pip install -q --python {VENV_PYTHON} {REPO_DIR}/backend/packages/harness",
            quote(&self.repo_url)
        )
    }

    fn run_env(&self) -> BTreeMap<String, String> {
        let b = |v: bool| if v { "true" } else { "false" }.to_string();
        let mut env = BTreeMap::from([
            ("DEER_FLOW_PROJECT_ROOT".to_string(), PROJECT_DIR.to_string()),
            ("DEER_FLOW_HOME".into(), format!("{PROJECT_DIR}/.deer-flow")),
            ("DEERFLOW_SUBAGENT_ENABLED".into(), b(self.subagent)),
            ("DEERFLOW_THINKING_ENABLED".into(), b(self.thinking)),
            ("DEERFLOW_PLAN_MODE".into(), b(self.plan_mode)),
            ("DEERFLOW_RECURSION_LIMIT".into(), self.recursion_limit.to_string()),
            ("DEERFLOW_MODEL_NAME".into(), MODEL_NAME.into()),
            ("DEERFLOW_SUMMARY_PATH".into(), SUMMARY_PATH.into()),
        ]);
        for k in FORWARDED_ENV_VARS {
            if let Some(v) = self.args.get_env(k) {
                env.entry(k.into()).or_insert(v);
            }
        }
        env
    }
}

/// Parse `pwd -P && id -u && id -g` into (workdir, uid, gid), honoring the `workdir` option.
fn runtime_context(stdout: &str, workdir_opt: Option<&str>) -> Result<(String, i64, i64)> {
    let lines: Vec<&str> = stdout.lines().collect();
    let [observed, uid, gid] = lines[..] else { bail!("Could not resolve DeerFlow runtime workdir and UID:GID") };
    let workdir = workdir_opt.unwrap_or(observed).to_string();
    if !workdir.starts_with('/') || workdir.split('/').any(|p| p == "..") {
        bail!("DeerFlow workdir must be an absolute normalized path: '{workdir}'");
    }
    let (Ok(uid), Ok(gid)) = (uid.trim().parse::<i64>(), gid.trim().parse::<i64>()) else {
        bail!("Could not resolve numeric UID:GID for DeerFlow")
    };
    if uid < 0 || gid < 0 {
        bail!("Could not resolve numeric UID:GID for DeerFlow");
    }
    Ok((workdir, uid, gid))
}

/// Validate the runner's summary (schema 1, non-negative int token counts) -> (input, output).
pub fn parse_summary(text: &str) -> Option<(i64, i64)> {
    if text.len() as u64 > MAX_SUMMARY_BYTES {
        return None;
    }
    let v: Value = serde_json::from_str(text).ok()?;
    if v["schema_version"] != 1 {
        return None;
    }
    let tok = |k: &str| v["usage"][k].as_i64().filter(|n| *n >= 0);
    Some((tok("input_tokens")?, tok("output_tokens")?))
}

#[async_trait::async_trait]
impl Agent for DeerFlow {
    fn name(&self) -> &str {
        "deerflow"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        let none = BTreeMap::new();
        let r = exec_agent(cx, "pwd -P && id -u && id -g", &none).await?;
        let rt = runtime_context(&r.stdout, self.workdir_opt.as_deref())?;
        if cx.exec(&format!("test -d {}", quote(&rt.0)), &none, None).await?.code != 0 {
            bail!("DeerFlow workdir does not exist: {}", rt.0);
        }
        let owner = quote(&format!("{}:{}", rt.1, rt.2));
        self.runtime = Some(rt);
        if cx.exec(&format!("test -x {VENV_DIR}/bin/deerflow"), &none, None).await?.code != 0 {
            ensure_system_dependencies(cx, &["git", "curl"]).await?;
            exec_root(
                cx,
                &format!("rm -rf {REPO_DIR} {VENV_DIR} && mkdir -p {REPO_DIR} {VENV_DIR} && chown -R {owner} {REPO_DIR} {VENV_DIR}"),
                &none,
            )
            .await?;
            let cmd = self.install_command();
            let r = cx.exec(&format!("set -o pipefail; {cmd}"), &none, Some(Duration::from_secs(2400))).await?;
            if r.code != 0 {
                return Err(exec_error(&cmd, &r, None));
            }
        }
        exec_root(cx, &format!("mkdir -p {PROJECT_DIR} && chown -R {owner} {PROJECT_DIR}"), &none).await?;
        let yaml = serde_yaml_ng::to_string(&self.build_config()?)?;
        upload_asset(cx, &yaml, &format!("{PROJECT_DIR}/config.yaml")).await?;
        upload_asset(cx, RUNNER, RUNNER_PATH).await?;
        if self.version.is_none()
            && let Ok(r) = cx
                .exec(&format!("{VENV_PYTHON} -c \"import importlib.metadata as m; print(m.version('deerflow-harness'))\""), &none, None)
                .await
            && r.code == 0
            && !r.stdout.is_empty()
        {
            self.version = Some(r.stdout.trim().to_string());
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let Some((workdir, _, _)) = self.runtime.clone() else { bail!("DeerFlow runtime context was not initialized") };
        let steered = self.steered_instruction(&instruction)?;
        let _ = std::fs::write(cx.logs_dir.join("instruction.txt"), &steered);
        let remote_instruction = format!("{PROJECT_DIR}/instruction.txt");
        upload_asset(cx, &steered, &remote_instruction).await?;
        // Runs with cwd=workdir; stderr and exit code stay intact for classification.
        let cmd = format!("cd {} && {VENV_PYTHON} {RUNNER_PATH} < {remote_instruction} > /logs/agent/deerflow.jsonl", quote(&workdir));
        exec_agent(cx, &cmd, &self.run_env()).await?;

        let mut meta = ctx.metadata.take().filter(Value::is_object).unwrap_or_else(|| json!({}));
        for (k, v) in [
            ("deerflow_model", json!(self.model_provider_config().1)),
            ("deerflow_repo", json!(self.repo_url)),
            ("deerflow_ref", json!(self.repo_ref)),
            ("deerflow_subagent", json!(self.subagent)),
            ("deerflow_thinking", json!(self.thinking)),
            ("deerflow_plan_mode", json!(self.plan_mode)),
            ("deerflow_summarize", json!(self.summarize)),
            ("deerflow_recursion_limit", json!(self.recursion_limit)),
            ("deerflow_workdir", json!(workdir)),
        ] {
            meta[k] = v;
        }
        ctx.metadata = Some(meta);
        // Best-effort, size-capped token summary from the runner.
        let none = BTreeMap::new();
        if let Ok(r) = cx.exec(&format!("wc -c < {SUMMARY_PATH}"), &none, None).await
            && r.code == 0
            && r.stdout.trim().parse::<u64>().is_ok_and(|n| n <= MAX_SUMMARY_BYTES)
            && let Ok(r) = cx.exec(&format!("cat {SUMMARY_PATH}"), &none, None).await
            && r.code == 0
            && let Some((i, o)) = parse_summary(&r.stdout)
        {
            ctx.n_input_tokens = Some(i);
            ctx.n_output_tokens = Some(o);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common::test_args as args;

    fn df(kwargs: Value, env: &[(&str, &str)], model: &str) -> DeerFlow {
        let mut d = DeerFlow::new(args(kwargs, env, Some(model))).unwrap();
        d.runtime = Some(("/testbed".into(), 1000, 1000));
        d
    }

    #[test]
    fn options_and_model() {
        let d = df(json!({"subagent": "true", "thinking": "off", "recursion_limit": "400"}), &[], "openrouter/example/m");
        assert!((d.subagent, d.thinking, d.plan_mode, d.recursion_limit) == (true, false, false, 400));
        assert_eq!(
            d.model_entry(),
            json!({"name": "hbench-model", "use": "langchain_openai:ChatOpenAI", "model": "example/m", "base_url": "https://openrouter.ai/api/v1", "api_key": "$OPENROUTER_API_KEY", "request_timeout": 600.0, "max_retries": 2})
        );
        assert_eq!(df(json!({}), &[], "anthropic/claude").model_provider_config().0, "langchain_anthropic:ChatAnthropic");
        assert_eq!(df(json!({}), &[], "gpt-4o").model_provider_config().1, "gpt-4o");
        assert!(DeerFlow::new(args(json!({"config": {"a": 1}, "config_path": "/x"}), &[], None)).is_err());
        assert!(DeerFlow::new(args(json!({"subagent": "maybe"}), &[], None)).is_err());
        let env = d.run_env();
        assert_eq!(
            (env["DEERFLOW_SUBAGENT_ENABLED"].as_str(), env["DEERFLOW_RECURSION_LIMIT"].as_str(), env["DEERFLOW_MODEL_NAME"].as_str()),
            ("true", "400", "hbench-model")
        );
        assert!(d.install_command().contains(&format!("git -C {REPO_DIR} fetch --depth 1 origin {DEERFLOW_REF}; ")));
    }

    #[test]
    fn config_bridge() {
        let d = df(json!({}), &[("TAVILY_API_KEY", "t"), ("FIRECRAWL_API_KEY", "f")], "openai/gpt");
        let c = d.build_config().unwrap();
        assert_eq!(c["tools"][0]["use"], "deerflow.community.tavily.tools:web_search_tool");
        assert_eq!(c["tools"][1]["use"], "deerflow.community.firecrawl.tools:web_fetch_tool");
        assert_eq!(c["sandbox"]["mounts"], json!([{"host_path": "/testbed", "container_path": "/testbed", "read_only": false}]));
        assert_eq!(c["sandbox"]["use"], "deerflow.sandbox.local:LocalSandboxProvider");
        let user = df(
            json!({"disable_web_search": true, "summarize": true, "config": {"tools": [{"name": "web_search", "group": "web"}, {"name": "x", "group": "g"}],
                "models": [{"name": "hbench-model", "use": "old", "supports_vision": true}, {"name": "other"}],
                "sandbox": {"mounts": [{"container_path": "/testbed", "host_path": "/elsewhere"}, {"container_path": "/data"}]}}}),
            &[],
            "openai/gpt",
        );
        let c = user.build_config().unwrap();
        assert_eq!(c["tools"], json!([{"name": "x", "group": "g"}]));
        assert_eq!(c["models"][0]["name"], "other");
        assert_eq!(
            (c["models"][1]["use"].as_str(), c["models"][1]["supports_vision"].as_bool()),
            (Some("langchain_openai:ChatOpenAI"), Some(true))
        );
        assert_eq!(c["sandbox"]["mounts"].as_array().unwrap().len(), 2);
        assert_eq!(c["summarization"]["trigger"][0]["value"], 0.8);
        assert!(serde_yaml_ng::to_string(&c).unwrap().contains("allow_host_bash: true"));
        assert!(user.steered_instruction("Fix").unwrap().ends_with("graded on the real files under /testbed.\n\nTask:\nFix"));
    }

    #[test]
    fn runtime_and_summary() {
        assert_eq!(runtime_context("/testbed\n1000\n1000\n", None).unwrap(), ("/testbed".into(), 1000, 1000));
        assert_eq!(runtime_context("/w\n0\n0", Some("/srv")).unwrap().0, "/srv");
        assert!(runtime_context("/w\n0\n0", Some("rel")).is_err());
        assert!(runtime_context("/w\n0\n0", Some("/a/../b")).is_err());
        assert!(runtime_context("/w\nx\n0", None).is_err());
        assert_eq!(
            parse_summary(
                r#"{"schema_version":1,"status":"completed","usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18},"error":null}"#
            ),
            Some((11, 7))
        );
        for bad in [
            "not json",
            "[]",
            r#"{"schema_version":2,"usage":{}}"#,
            r#"{"schema_version":1,"usage":{"input_tokens":true,"output_tokens":2}}"#,
            r#"{"schema_version":1,"usage":{"input_tokens":-1,"output_tokens":2}}"#,
        ] {
            assert_eq!(parse_summary(bad), None, "{bad}");
        }
        assert_eq!(parse_summary(&"x".repeat(65_537)), None);
        assert!(RUNNER.contains("DEERFLOW_SUMMARY_PATH"));
    }
}
