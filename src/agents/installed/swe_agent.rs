//! SWE-agent (port of upstream's agents/installed/swe_agent.py). Runs as root only.

use super::mini_swe_agent::{opt_str, reject_unknown};
use super::{connection, ensure_system_dependencies, exec_agent, exec_error, exec_root};
use crate::agents::model_connection::Spec;
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const N: &str = "swe-agent";
const SPEC: Spec = Spec { default_provider: None, api_key_envs: &[], base_url_envs: &[], passthrough: true };
const REPO_URL: &str = "https://github.com/SWE-agent/SWE-agent.git";
const REPO_PATH: &str = "/opt/sweagent-repo";
/// Options with a `Cli` annotation, in model field order.
const CLI: &[(&str, &str)] = &[
    ("per_instance_cost_limit", "--agent.model.per_instance_cost_limit"),
    ("total_cost_limit", "--agent.model.total_cost_limit"),
    ("max_input_tokens", "--agent.model.max_input_tokens"),
    ("temperature", "--agent.model.temperature"),
    ("top_p", "--agent.model.top_p"),
];
const OPTIONS: &[&str] = &[
    "version",
    "prompt_template_path",
    "config",
    "per_instance_cost_limit",
    "total_cost_limit",
    "max_input_tokens",
    "temperature",
    "top_p",
    "completion_kwargs",
    "reasoning_effort",
];

pub struct SweAgent {
    args: AgentArgs,
    version: Option<String>,
    opts: BTreeMap<&'static str, String>,
    completion_kwargs: Option<String>,
    reasoning_effort: Option<String>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(SweAgent::new(args)?))
}

/// Python `json.dumps` (default separators, ensure_ascii).
fn py_json(v: &Value) -> String {
    match v {
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter().map(|(k, v)| format!("{}: {}", py_json(&Value::String(k.clone())), py_json(v))).collect::<Vec<_>>().join(", ")
        ),
        Value::Array(a) => format!("[{}]", a.iter().map(py_json).collect::<Vec<_>>().join(", ")),
        Value::String(s) => serde_json::to_string(s).unwrap().chars().fold(String::new(), |mut o, c| {
            if c.is_ascii() {
                o.push(c);
            } else {
                for u in c.encode_utf16(&mut [0; 2]) {
                    o += &format!("\\u{u:04x}");
                }
            }
            o
        }),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        v => v.to_string(),
    }
}

fn clone_command(version: Option<&str>) -> String {
    let (repo, dest) = (quote(REPO_URL), quote(REPO_PATH));
    match version.filter(|v| !v.trim().is_empty()) {
        None => format!("git clone --depth 1 {repo} {dest}"),
        Some(v) => format!(
            "git init {dest} && git -C {dest} remote add origin {repo} && revision={} && \
             (git -C {dest} fetch --depth 1 origin -- \"refs/heads/$revision\" || git -C {dest} fetch --depth 1 origin -- \"refs/tags/$revision\" || \
             git -C {dest} fetch --depth 1 origin -- \"$revision\") && git -C {dest} checkout --detach FETCH_HEAD",
            quote(v)
        ),
    }
}

impl SweAgent {
    fn new(args: AgentArgs) -> Result<Self> {
        reject_unknown(&args, N, OPTIONS)?;
        if args.kwargs.get("config").is_some_and(|v| !v.is_null()) {
            bail!("Agent '{N}' does not support config. You can implement it.");
        }
        let mut opts = BTreeMap::new();
        for (k, _) in CLI {
            if let Some(v) = opt_str(&args, N, k)? {
                opts.insert(*k, v);
            }
        }
        // completion_kwargs: dict/list -> json.dumps, blank -> None, env fallback when omitted.
        let completion_kwargs = match args.kwargs.get("completion_kwargs") {
            None => args.extra_env.get("SWEAGENT_COMPLETION_KWARGS").cloned().or_else(|| std::env::var("SWEAGENT_COMPLETION_KWARGS").ok()),
            Some(v @ (Value::Object(_) | Value::Array(_))) => Some(py_json(v)),
            Some(_) => opt_str(&args, N, "completion_kwargs")?,
        }
        .filter(|s| !s.trim().is_empty());
        Ok(SweAgent { version: args.version_kw(), reasoning_effort: opt_str(&args, N, "reasoning_effort")?, args, opts, completion_kwargs })
    }

    fn cli_flags(&self, completion_kwargs: Option<&str>) -> String {
        let mut f: Vec<String> = CLI.iter().filter_map(|(k, flag)| self.opts.get(k).map(|v| format!("{flag} {}", quote(v)))).collect();
        if let Some(c) = completion_kwargs {
            f.push(format!("--agent.model.completion_kwargs {}", quote(c)));
        }
        f.join(" ")
    }

    /// upstream's `_apply_reasoning_options`: (completion_kwargs, inject_reasoning, active_openai_reasoning).
    fn reasoning_options(&self) -> Result<(Option<String>, bool, bool)> {
        let Some(effort) = &self.reasoning_effort else { return Ok((self.completion_kwargs.clone(), false, false)) };
        let mut kwargs = self.completion_kwargs.clone();
        if let Some(raw) = &self.completion_kwargs {
            let mut v: Value = serde_json::from_str(raw)?;
            let Some(o) = v.as_object_mut() else {
                bail!("SWE-agent completion_kwargs must be a JSON object when reasoning_effort is configured")
            };
            o.insert("reasoning_effort".into(), Value::String(effort.clone()));
            kwargs = Some(py_json(&v));
        }
        let active = self.args.model_name.as_deref().is_some_and(|m| m.starts_with("openai/")) && effort.to_lowercase() != "none";
        if active {
            if self.opts.get("top_p").is_some_and(|v| v != "null") {
                bail!("OpenAI models with active reasoning require top_p=null");
            }
            if self.opts.get("temperature").is_some_and(|v| !["1", "1.0"].contains(&v.as_str())) {
                bail!("OpenAI models with active reasoning require temperature=1");
            }
        }
        Ok((kwargs, self.completion_kwargs.is_none(), active))
    }

    fn reasoning_config_command(&self, config_path: &str, inject: bool, active: bool) -> (String, String) {
        if !inject && !active {
            return (config_path.into(), String::new());
        }
        let effective = "/opt/sweagent-configs/hbench-effective.yaml";
        let cmd = format!(
            "cp {} {effective}\n/opt/sweagent-venv/bin/python - {effective} {} {} {} <<'PY'\n\
import sys\n\
import yaml\n\
\n\
path, effort, inject_reasoning, active_openai = sys.argv[1:]\n\
with open(path) as f:\n    config = yaml.safe_load(f) or {{}}\n\
model = config.setdefault('agent', {{}}).setdefault('model', {{}})\n\
if inject_reasoning == '1':\n    kwargs = model.setdefault('completion_kwargs', {{}})\n    if not isinstance(kwargs, dict):\n        raise ValueError('SWE-agent YAML completion_kwargs must be a mapping')\n    kwargs['reasoning_effort'] = effort\n\
if active_openai == '1':\n    if model.get('top_p') is not None:\n        raise ValueError('OpenAI active reasoning requires YAML top_p: null')\n    if 'temperature' in model and model['temperature'] not in (1, 1.0):\n        raise ValueError('OpenAI active reasoning requires YAML temperature: 1')\n    model['top_p'] = None\n    model['temperature'] = 1\n\
with open(path, 'w') as f:\n    yaml.safe_dump(config, f, sort_keys=False)\n\
PY\n",
            quote(config_path),
            quote(self.reasoning_effort.as_deref().unwrap_or("")),
            inject as u8,
            active as u8
        );
        (effective.into(), cmd)
    }

    /// `_get_env(a, b)`: per source (agent env, then host), first present name.
    fn env_any(&self, names: &[&str]) -> Option<String> {
        names.iter().find_map(|n| self.args.extra_env.get(*n).cloned()).or_else(|| names.iter().find_map(|n| std::env::var(n).ok()))
    }

    /// (full command, redacted command, env).
    fn command(&self, instruction: &str) -> Result<(String, String, BTreeMap<String, String>)> {
        let Some(model) = self.args.model_name.as_deref() else { bail!("Model name must be specified for SWE-agent") };
        let mut instruction = instruction.to_string();
        if !self.args.mcp_servers.is_empty() {
            instruction += "\n\nMCP Servers:\nThe following MCP servers are available for this task.\n";
            for s in &self.args.mcp_servers {
                if s.transport() == "stdio" {
                    let cmd = std::iter::once(s.command.clone().unwrap_or_default())
                        .chain(s.args.iter().cloned())
                        .map(|a| quote(&a))
                        .collect::<Vec<_>>()
                        .join(" ");
                    instruction += &format!("- {}: stdio transport, command: {cmd}\n", s.name);
                } else {
                    instruction += &format!("- {}: {} transport, url: {}\n", s.name, s.transport(), s.url.as_deref().unwrap_or("None"));
                }
            }
        }
        let mut env = connection(&self.args, SPEC).env;
        if let Some(c) = self.env_any(&["SWEAGENT_CONFIG"]) {
            env.insert("SWEAGENT_CONFIG".into(), c);
        }
        let hosted_vllm = model.starts_with("hosted_vllm/");
        if hosted_vllm {
            if let Some(k) = self.env_any(&["OPENAI_API_KEY"]).filter(|k| !k.is_empty()) {
                env.insert("OPENAI_API_KEY".into(), k);
            }
            if let Some(b) = self.env_any(&["OPENAI_BASE_URL", "OPENAI_API_BASE"]).filter(|b| !b.is_empty()) {
                env.insert("OPENAI_BASE_URL".into(), b.clone());
                env.insert("OPENAI_API_BASE".into(), b);
            }
        }
        let instruction_path = "/logs/agent/problem_statement.md";
        let heredoc = format!("HBENCH_INSTRUCTION_{}", uuid::Uuid::new_v4().simple());
        let write_instruction = format!("mkdir -p /logs/agent\ncat > '{instruction_path}' << '{heredoc}'\n{instruction}\n{heredoc}\n");
        let mut parts = vec![
            "sweagent run".to_string(),
            format!("--agent.model.name={model}"),
            format!("--problem_statement.path={instruction_path}"),
            "--env.deployment.type=local".into(),
            "--output_dir=/logs/agent/swe-agent-output".into(),
            "$(if [ -d /testbed ]; then echo '--env.repo.type=preexisting --env.repo.repo_name=/testbed'; else echo '--env.repo.path=$(pwd)'; fi)".into(),
        ];
        let mut download = String::new();
        let mut config_path = "/opt/sweagent-configs/default.yaml".to_string();
        if let Some(src) = env.get("SWEAGENT_CONFIG") {
            if src.starts_with("http://") || src.starts_with("https://") {
                config_path = "/opt/sweagent-configs/swesmith_infer.yaml".into();
                download = format!("curl -sSL '{src}' -o '{config_path}'\n");
            } else {
                config_path = src.clone();
            }
        }
        let (completion_kwargs, inject, active) = self.reasoning_options()?;
        let (config_path, reasoning_cmd) = self.reasoning_config_command(&config_path, inject, active);
        parts.push(format!("--config={}", quote(&config_path)));
        if hosted_vllm {
            // 0 = unlimited, unless overridden by kwargs.
            for (k, flag) in &CLI[..3] {
                if !self.opts.contains_key(k) {
                    parts.push(format!("{flag}=0"));
                }
            }
        }
        let flags = self.cli_flags(completion_kwargs.as_deref());
        if !flags.is_empty() {
            parts.push(flags);
        }
        if let Some(b) = env.get("OPENAI_BASE_URL") {
            parts.push(format!("--agent.model.api_base={b}"));
        }
        let cmd = format!(
            "set -euo pipefail\nset +u\n. /etc/profile.d/testbed-conda.sh || true\nset -u\n{download}{reasoning_cmd}{write_instruction}{} 2>&1 | stdbuf -oL tee /logs/agent/swe-agent.txt\n",
            parts.join(" ")
        );
        // Cli(sensitive=True): completion_kwargs must not leak into logged commands.
        let redacted = completion_kwargs.iter().filter(|s| !s.is_empty()).fold(cmd.clone(), |c, s| c.replace(s.as_str(), "****"));
        Ok((cmd, redacted, env))
    }
}

fn find_traj(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            find_traj(&p, out);
        } else if p.extension().is_some_and(|x| x == "traj") {
            out.push(p);
        }
    }
}

/// Totals from SWE-agent's `.traj` info block (upstream's populate_context_post_run).
pub fn parse_trajectory(text: &str) -> Option<AgentContext> {
    let t: Value = serde_json::from_str(text).ok()?;
    let info = &t["info"];
    let nz = |k: &str| info[k].as_f64().filter(|v| *v != 0.0);
    Some(AgentContext {
        cost_usd: Some(nz("total_cost").or(nz("cost")).unwrap_or(0.0)),
        n_input_tokens: Some(info["input_tokens"].as_i64().unwrap_or(0)),
        n_output_tokens: Some(info["output_tokens"].as_i64().unwrap_or(0)),
        ..Default::default()
    })
}

#[async_trait::async_trait]
impl Agent for SweAgent {
    fn name(&self) -> &str {
        N
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        if let Some(u) = cx.user.as_deref().filter(|u| *u != "root" && *u != "0") {
            bail!("SWE-agent requires root access and cannot run as a non-root user ({u}). Consider using mini-swe-agent instead.");
        }
        cx.env.exec_root("[ -d /installed-agent ] || mkdir -p /installed-agent", None).await?;
        ensure_system_dependencies(cx, &["curl", "build_tools", "git", "tmux", "coreutils"]).await?;
        let none = BTreeMap::new();
        exec_root(cx, "if ! command -v uv >/dev/null 2>&1; then  curl -LsSf https://astral.sh/uv/install.sh | sh; fi && if [ -f \"$HOME/.local/bin/env\" ]; then source \"$HOME/.local/bin/env\"; fi", &none).await?;
        exec_root(
            cx,
            &format!(
                "set -euo pipefail; if [ -f \"$HOME/.local/bin/env\" ]; then source \"$HOME/.local/bin/env\"; fi && uv python install 3.12 && mkdir -p /opt && \
                 uv venv /opt/sweagent-venv --python 3.12 && source /opt/sweagent-venv/bin/activate && rm -rf /opt/sweagent-repo && {} && \
                 uv pip install /opt/sweagent-repo && SITE_PACKAGES=$(python -c \"import site; print(site.getsitepackages()[0])\") && \
                 cp -r /opt/sweagent-repo/config \"$SITE_PACKAGES/config\" && cp -r /opt/sweagent-repo/tools \"$SITE_PACKAGES/tools\" && \
                 mkdir -p /opt/sweagent-configs && cp /opt/sweagent-repo/config/default.yaml /opt/sweagent-configs/default.yaml && \
                 cp /opt/sweagent-repo/config/default_backticks.yaml /opt/sweagent-configs/default_backticks.yaml 2>/dev/null || true && mkdir -p \"$SITE_PACKAGES/trajectories\"",
                clone_command(self.version.as_deref())
            ),
            &none,
        )
        .await?;
        exec_root(
            cx,
            "cat > /usr/local/bin/sweagent << 'WRAPPER'\n#!/bin/bash\nsource /opt/sweagent-venv/bin/activate\nexec python -m sweagent.run.run \"$@\"\nWRAPPER\n\
             chmod +x /usr/local/bin/sweagent && cat > /etc/profile.d/testbed-conda.sh << 'EOF'\n\
             if [ -z \"${CONDA_DEFAULT_ENV:-}\" ] && [ -d \"/opt/miniconda3/envs/testbed\" ]; then\n    if [ -f \"/opt/miniconda3/etc/profile.d/conda.sh\" ]; then\n        . \"/opt/miniconda3/etc/profile.d/conda.sh\"\n        conda activate testbed 2>/dev/null || true\n    fi\nfi\nEOF\n\
             cat /etc/profile.d/testbed-conda.sh >> /root/.bashrc",
            &none,
        )
        .await?;
        if self.version.is_none()
            && let Ok(r) = cx.exec("/opt/sweagent-venv/bin/pip show swe-agent | grep ^Version:", &none, None).await
            && r.code == 0
            && !r.stdout.is_empty()
        {
            let t = r.stdout.trim();
            self.version = Some(t.strip_prefix("Version:").map(str::trim).unwrap_or(t).to_string());
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let (cmd, redacted, env) = self.command(&instruction)?;
        let r = cx.exec(&format!("set -o pipefail; {cmd}"), &env, None).await;
        // Best-effort: copy the .traj next to the logs.
        let _ = exec_agent(
            cx,
            "TRAJ_FILE=$(find /logs/agent/swe-agent-output -name '*.traj' -print -quit); if [ -n \"$TRAJ_FILE\" ]; then cp \"$TRAJ_FILE\" /logs/agent/swe-agent.trajectory.json; fi",
            &BTreeMap::new(),
        )
        .await;
        let r = r?;
        if r.code != 0 {
            return Err(exec_error(&redacted, &r, None));
        }
        Ok(())
    }

    fn populate_context_post_run(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        let mut found = vec![];
        find_traj(&logs_dir.join("swe-agent-output"), &mut found);
        // ponytail: upstream asserts at most one .traj; with several we skip usage rather than panic.
        let path = match found.len() {
            0 => logs_dir.join("swe-agent.trajectory.json"),
            1 => found.remove(0),
            _ => return,
        };
        if let Some(c) = std::fs::read_to_string(path).ok().and_then(|t| parse_trajectory(&t)) {
            ctx.cost_usd = c.cost_usd;
            ctx.n_input_tokens = c.n_input_tokens;
            ctx.n_output_tokens = c.n_output_tokens;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::mini_swe_agent::tests::args;
    use serde_json::json;

    fn agent(kwargs: Value, env: &[(&str, &str)], model: &str) -> Result<SweAgent> {
        SweAgent::new(args(kwargs, env, Some(model)))
    }

    fn split_flag(flags: &str, flag: &str) -> String {
        let rest = &flags[flags.find(flag).unwrap() + flag.len() + 1..];
        // Single-quoted shell token.
        let end = rest[1..].find('\'').unwrap();
        rest[1..=end].to_string()
    }

    #[test]
    fn flags_and_completion_kwargs() {
        let a = agent(json!({"temperature": 0.7, "per_instance_cost_limit": 3}), &[], "openai/gpt-5").unwrap();
        assert_eq!(a.cli_flags(a.completion_kwargs.as_deref()), "--agent.model.per_instance_cost_limit 3 --agent.model.temperature 0.7");
        let payload = json!({"extra_headers": {"CF-Access-Client-Id": "abc.def", "x": "a b"}});
        let a = agent(json!({"completion_kwargs": payload}), &[], "openai/gpt-5").unwrap();
        let v = split_flag(&a.cli_flags(a.completion_kwargs.as_deref()), "--agent.model.completion_kwargs");
        assert_eq!(v, r#"{"extra_headers": {"CF-Access-Client-Id": "abc.def", "x": "a b"}}"#);
        assert_eq!(serde_json::from_str::<Value>(&v).unwrap(), payload);
        let a = agent(json!({}), &[("SWEAGENT_COMPLETION_KWARGS", r#"{"a": 1}"#)], "openai/gpt-5").unwrap();
        assert_eq!(a.completion_kwargs.as_deref(), Some(r#"{"a": 1}"#));
        assert!(agent(json!({"completion_kwargs": ""}), &[], "openai/gpt-5").unwrap().completion_kwargs.is_none());
        assert!(agent(json!({"bogus": 1}), &[], "openai/gpt-5").is_err());
        assert!(agent(json!({"config": {}}), &[], "openai/gpt-5").err().unwrap().to_string().contains("does not support config"));
    }

    #[test]
    fn reasoning() {
        let a = agent(
            json!({"reasoning_effort": "none", "completion_kwargs": {"parallel_tool_calls": false, "timeout": 300}}),
            &[],
            "openai/gpt-5",
        )
        .unwrap();
        let (k, inject, active) = a.reasoning_options().unwrap();
        assert_eq!(k.as_deref(), Some(r#"{"parallel_tool_calls": false, "timeout": 300, "reasoning_effort": "none"}"#));
        assert!(!inject && !active);
        let a = agent(json!({"reasoning_effort": "medium"}), &[], "openai/gpt-5").unwrap();
        assert_eq!(a.reasoning_options().unwrap(), (None, true, true));
        let a = agent(json!({"reasoning_effort": "medium"}), &[], "anthropic/claude-sonnet-4-5").unwrap();
        assert_eq!(a.reasoning_options().unwrap(), (None, true, false));
        let a = agent(json!({"reasoning_effort": "none", "completion_kwargs": ["not", "an", "object"]}), &[], "openai/gpt-5").unwrap();
        assert!(a.reasoning_options().unwrap_err().to_string().contains("must be a JSON object"));
        let a = agent(json!({"reasoning_effort": "medium", "temperature": "0.7"}), &[], "openai/gpt-5").unwrap();
        assert!(a.reasoning_options().unwrap_err().to_string().contains("temperature=1"));
        let a = agent(json!({"reasoning_effort": "none"}), &[], "openai/gpt-5").unwrap();
        let (p, cmd) = a.reasoning_config_command("/opt/custom config.yaml", true, false);
        assert_eq!(p, "/opt/sweagent-configs/hbench-effective.yaml");
        assert!(
            cmd.contains("cp '/opt/custom config.yaml'")
                && cmd.contains("kwargs['reasoning_effort'] = effort")
                && cmd.contains(" none 1 0 <<'PY'\n")
        );
    }

    #[test]
    fn run_command() {
        let mut a = agent(json!({"completion_kwargs": {"h": "CF-SECRET-abc123"}}), &[], "openai/gpt-5").unwrap();
        a.args.mcp_servers = serde_json::from_value(json!([
            {"name": "remote-tools", "transport": "streamable-http", "url": "http://remote-tools:8000/mcp"},
            {"name": "local-tools", "transport": "stdio", "command": "python", "args": ["-c", "print('hello world')"]}
        ]))
        .unwrap();
        let (cmd, redacted, _) = a.command("Solve the task.").unwrap();
        assert!(
            cmd.contains("Solve the task.\n\nMCP Servers:")
                && cmd.contains("- remote-tools: streamable-http transport, url: http://remote-tools:8000/mcp\n")
        );
        assert!(cmd.contains("- local-tools: stdio transport, command: python -c 'print('\\''hello world'\\'')'\n"));
        assert!(cmd.contains("sweagent run --agent.model.name=openai/gpt-5 --problem_statement.path=/logs/agent/problem_statement.md --env.deployment.type=local"));
        assert!(
            cmd.contains("--config=/opt/sweagent-configs/default.yaml")
                && cmd.ends_with(" 2>&1 | stdbuf -oL tee /logs/agent/swe-agent.txt\n")
        );
        assert!(cmd.contains("CF-SECRET-abc123") && !redacted.contains("CF-SECRET-abc123") && redacted.contains("****"));

        let a = agent(
            json!({}),
            &[("OPENAI_API_KEY", "vllm-key"), ("OPENAI_API_BASE", "https://vllm.example.test/v1")],
            "hosted_vllm/local-model",
        )
        .unwrap();
        let (cmd, _, env) = a.command("task").unwrap();
        assert_eq!(
            (env["OPENAI_API_KEY"].as_str(), env["OPENAI_BASE_URL"].as_str(), env["OPENAI_API_BASE"].as_str()),
            ("vllm-key", "https://vllm.example.test/v1", "https://vllm.example.test/v1")
        );
        assert!(cmd.contains("--agent.model.per_instance_cost_limit=0 --agent.model.total_cost_limit=0 --agent.model.max_input_tokens=0 --agent.model.api_base=https://vllm.example.test/v1"));
    }

    #[test]
    fn clone_and_usage() {
        assert_eq!(clone_command(Some("  ")), "git clone --depth 1 https://github.com/SWE-agent/SWE-agent.git /opt/sweagent-repo");
        let c = clone_command(Some("abc123"));
        assert!(
            c.contains("revision=abc123")
                && c.contains("fetch --depth 1 origin -- \"refs/tags/$revision\"")
                && c.contains("checkout --detach FETCH_HEAD")
        );
        assert!(py_json(&json!({"k": "é"})) == r#"{"k": "\u00e9"}"#);

        let dir = std::env::temp_dir().join(format!("hbench-swe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("swe-agent-output/inst")).unwrap();
        std::fs::write(dir.join("swe-agent-output/inst/inst.traj"), r#"{"environment":"swe_main","trajectory":[{"action":"ls"}],"info":{"model_name":"m","total_cost":0.0,"cost":1.5,"input_tokens":1000,"output_tokens":200}}"#).unwrap();
        let a = agent(json!({}), &[], "openai/gpt-5").unwrap();
        let mut ctx = AgentContext::default();
        a.populate_context_post_run(&dir, &mut ctx);
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens, ctx.cost_usd, ctx.n_cache_tokens), (Some(1000), Some(200), Some(1.5), None));
    }
}
