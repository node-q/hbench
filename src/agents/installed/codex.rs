//! Codex CLI (port of upstream's agents/installed/codex.py).
//! ACP bridge mode and load_trajectory seeding have no hbench counterpart and are not ported.

use super::aider::opts;
use super::{connection, ensure_system_dependencies, exec_agent, exec_error, exec_root, filter_jsonl_events, upload_text};
use crate::agents::model_connection::Spec;
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SPEC: Spec = Spec { default_provider: Some("openai"), api_key_envs: &[], base_url_envs: &[], passthrough: false };
const CODEX_HOME: &str = "/installed-agent/codex-home";
const SECRETS_DIR: &str = "/tmp/codex-secrets";
const AUTH_PATH: &str = "/tmp/codex-secrets/auth.json";
const CONFIG_PATH: &str = "/installed-agent/codex-home/config.toml";
const VERSION_CMD: &str = "if [ -s ~/.nvm/nvm.sh ]; then . ~/.nvm/nvm.sh; fi; codex --version";
const KNOWN: &[&str] = &["reasoning_effort", "reasoning_summary", "web_search", "disable_web_search", "dangerously_bypass_hook_trust"];

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(Codex::new(args)?))
}

pub struct Codex {
    args: AgentArgs,
    version: Option<String>,
    base_config: serde_json::Map<String, Value>,
}

impl Codex {
    fn new(args: AgentArgs) -> Result<Codex> {
        opts::check_known(&args, "codex", KNOWN, true)?;
        let base_config = load_base_config(args.kwargs.get("config"))?;
        let a = Codex { version: args.version_kw(), args, base_config };
        a.cli_flags()?;
        Ok(a)
    }

    /// CodexOptions -> CLI flags.
    fn cli_flags(&self) -> Result<String> {
        let a = &self.args;
        let mut f = vec![];
        if let Some(v) = opts::choice(a, "reasoning_effort", None, &["none", "minimal", "low", "medium", "high", "xhigh", "max"])? {
            f.push(format!("-c model_reasoning_effort={v}"));
        }
        if let Some(v) = opts::choice(a, "reasoning_summary", None, &["auto", "concise", "detailed", "none"])? {
            f.push(format!("-c model_reasoning_summary={v}"));
        }
        let mut web = opts::choice(a, "web_search", None, &["disabled", "cached", "live"])?;
        if opts::boolean(a, "disable_web_search", None)?.unwrap_or(false) {
            web = Some("disabled".into());
        }
        if let Some(v) = web {
            f.push(format!("-c web_search={v}"));
        }
        if opts::boolean(a, "dangerously_bypass_hook_trust", None)?.unwrap_or(false) {
            f.push("--dangerously-bypass-hook-trust".into());
        }
        Ok(f.join(" "))
    }

    /// upstream's `_build_effective_config`: base config + runtime base URL + task MCP servers.
    fn effective_config(&self, base_url: Option<&str>) -> Result<serde_json::Map<String, Value>> {
        let mut c = self.base_config.clone();
        if let Some(u) = base_url {
            c.insert("openai_base_url".into(), json!(u));
        }
        if self.args.mcp_servers.is_empty() {
            return Ok(c);
        }
        let Some(servers) = c.entry("mcp_servers").or_insert_with(|| json!({})).as_object_mut() else {
            bail!("Invalid Codex config: mcp_servers must be a TOML table when upstream task MCP servers are configured")
        };
        for s in &self.args.mcp_servers {
            let v = if s.transport() == "stdio" { json!({"command": s.command, "args": s.args}) } else { json!({"url": s.url}) };
            servers.insert(s.name.clone(), v);
        }
        Ok(c)
    }

    /// CODEX_AUTH_JSON_PATH=<path> or CODEX_FORCE_AUTH_JSON=<truthy> (~/.codex/auth.json); default OPENAI_API_KEY.
    fn auth_json_path(&self) -> Result<Option<PathBuf>> {
        if let Some(p) = self.args.get_env("CODEX_AUTH_JSON_PATH") {
            if !Path::new(&p).is_file() {
                bail!("CODEX_AUTH_JSON_PATH points to non-existent file: {p}");
            }
            return Ok(Some(p.into()));
        }
        if parse_bool_env(self.args.get_env("CODEX_FORCE_AUTH_JSON"), "CODEX_FORCE_AUTH_JSON")? {
            let d = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex/auth.json");
            if !d.is_file() {
                bail!("CODEX_FORCE_AUTH_JSON is set but {} does not exist", d.display());
            }
            return Ok(Some(d));
        }
        Ok(None)
    }

    /// `_ensure_agent_owned_dir`: private dir under root-owned /installed-agent.
    async fn ensure_agent_owned_dir(cx: &AgentCtx<'_>, path: &str) -> Result<()> {
        let r = exec_agent(cx, "printf \"\\nHBENCH_CODEX_ID %s %s\\n\" \"$(id -u)\" \"$(id -g)\"", &BTreeMap::new()).await?;
        let re = regex::Regex::new(r"(?m)^HBENCH_CODEX_ID ([0-9]+) ([0-9]+)$").unwrap();
        let c = re.captures(&r.stdout).ok_or_else(|| anyhow!("Could not resolve the agent uid/gid to create {path}"))?;
        let q = quote(path);
        exec_root(cx, &format!("mkdir -p {q} && chown -R {}:{} {q} && chmod 700 {q}", &c[1], &c[2]), &BTreeMap::new()).await?;
        Ok(())
    }
}

/// upstream's `parse_bool_env_value(..., default=False)`.
pub(super) fn parse_bool_env(v: Option<String>, name: &str) -> Result<bool> {
    match v.as_deref().map(|s| s.trim().to_lowercase()) {
        None => Ok(false),
        Some(s) if ["true", "1", "yes"].contains(&s.as_str()) => Ok(true),
        Some(s) if ["false", "0", "no"].contains(&s.as_str()) => Ok(false),
        Some(_) => bail!("Invalid value for '{name}': cannot parse '{}' as bool (expected true/false/1/0/yes/no)", v.unwrap()),
    }
}

/// `config`: a TOML file path, or an inline JSON object that must round-trip through TOML.
fn load_base_config(config: Option<&Value>) -> Result<serde_json::Map<String, Value>> {
    match config {
        None | Some(Value::Null) => Ok(Default::default()),
        Some(Value::Object(o)) => {
            let back = toml::Value::try_from(o)
                .and_then(|t| toml::to_string(&t))
                .map_err(|e| anyhow!("Invalid inline Codex config for TOML conversion: {e}"))
                .and_then(|s| toml::from_str::<Value>(&s).map_err(|e| anyhow!("Invalid inline Codex config for TOML conversion: {e}")))?;
            if back != Value::Object(o.clone()) {
                bail!("Invalid inline Codex config: JSON object cannot be represented losslessly as TOML");
            }
            Ok(o.clone())
        }
        Some(Value::String(p)) => {
            if !Path::new(p).is_file() {
                bail!("Agent config file not found: {p}");
            }
            let text = std::fs::read_to_string(p).map_err(|e| anyhow!("Invalid Codex config file {p}: {e}"))?;
            match toml::from_str::<Value>(&text).map_err(|e| anyhow!("Invalid Codex config file {p}: {e}"))? {
                Value::Object(o) => Ok(o),
                _ => bail!("Invalid Codex config file {p}: expected a TOML table"),
            }
        }
        Some(_) => bail!("Invalid value for 'config': expected a local file path or JSON object"),
    }
}

fn parse_codex_version(stdout: &str) -> String {
    let text = stdout.trim();
    text.lines().map(str::trim).find(|l| !l.is_empty()).map(|l| l.strip_prefix("codex-cli").unwrap_or(l).trim().to_string()).unwrap_or_else(|| text.to_string())
}

/// Deepest, lexicographically last session date dir under `logs_dir/sessions`, then its last rollout file.
fn latest_session_file(logs_dir: &Path) -> Option<PathBuf> {
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.push(p.clone());
                walk(&p, out);
            }
        }
    }
    let mut dirs = vec![];
    walk(&logs_dir.join("sessions"), &mut dirs);
    let depth = dirs.iter().map(|d| d.components().count()).max()?;
    let dir = dirs.into_iter().filter(|d| d.components().count() == depth).max()?;
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).max()
}

/// Totals from the last `token_count` event, as upstream's rollout -> ATIF final_metrics.
/// Cost: Codex's own `total_cost`/`cost_usd` when present; upstream's LiteLLM price estimate is not available here.
pub fn parse_rollout_usage(text: &str) -> Option<AgentContext> {
    let events: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l.trim()).ok()).collect();
    let step_types = ["message", "web_search_call", "function_call", "custom_tool_call", "function_call_output", "custom_tool_call_output"];
    if !events.iter().any(|e| e["type"] == "response_item" && e["payload"]["type"].as_str().is_some_and(|t| step_types.contains(&t))) {
        return None; // no ATIF steps -> upstream writes no trajectory
    }
    let info = events.iter().rev().filter(|e| e["type"] == "event_msg" && e["payload"]["type"] == "token_count").map(|e| &e["payload"]["info"]).find(|i| i["total_token_usage"].is_object())?;
    let t = &info["total_token_usage"];
    let n = |k: &str| t[k].as_i64().unwrap_or(0);
    let cost = info["total_cost"].as_f64().or_else(|| info["cost_usd"].as_f64());
    Some(AgentContext { n_input_tokens: Some(n("input_tokens")), n_cache_tokens: Some(n("cached_input_tokens")), n_output_tokens: Some(n("output_tokens")), cost_usd: cost, ..Default::default() })
}

#[async_trait::async_trait]
impl Agent for Codex {
    fn name(&self) -> &str {
        "codex"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        opts::installed_dir(cx).await?;
        let none = BTreeMap::new();
        let satisfied = match &self.version {
            None => cx.exec("if [ -s ~/.nvm/nvm.sh ]; then . ~/.nvm/nvm.sh; fi; command -v codex >/dev/null 2>&1", &none, None).await?.code == 0,
            Some(v) => {
                let r = cx.exec(VERSION_CMD, &none, None).await?;
                r.code == 0 && parse_codex_version(&r.stdout) == *v
            }
        };
        if !satisfied {
            ensure_system_dependencies(cx, &["curl", "bash", "nodejs", "npm", "ripgrep"]).await?;
            let spec = format!("@{}", self.version.as_deref().unwrap_or("latest"));
            exec_agent(
                cx,
                &format!(
                    "set -euo pipefail; if ldd --version 2>&1 | grep -qi musl || [ -f /etc/alpine-release ]; then  npm install -g @openai/codex{spec}; \
                     else  curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh | bash &&  export NVM_DIR=\"$HOME/.nvm\" &&  \
                     \\. \"$NVM_DIR/nvm.sh\" || true &&  command -v nvm &>/dev/null || {{ echo 'Error: NVM failed to load' >&2; exit 1; }} &&  \
                     nvm install 22 && nvm alias default 22 && npm -v &&  npm install -g @openai/codex{spec}; fi && codex --version"
                ),
                &BTreeMap::from([("NVM_NODEJS_ORG_MIRROR".to_string(), "https://nodejs.org/dist".to_string())]),
            )
            .await?;
            exec_root(
                cx,
                "for bin in node codex; do  BIN_PATH=\"$(which \"$bin\" 2>/dev/null || true)\";  \
                 if [ -n \"$BIN_PATH\" ] && [ \"$BIN_PATH\" != \"/usr/local/bin/$bin\" ]; then    ln -sf \"$BIN_PATH\" \"/usr/local/bin/$bin\";  fi; done",
                &none,
            )
            .await?;
        }
        if self.version.is_none() {
            self.version = opts::detect_version(cx, VERSION_CMD, parse_codex_version).await;
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let Some(model_name) = &self.args.model_name else { bail!("Model name is required") };
        let model = model_name.rsplit('/').next().unwrap().to_string();
        let flags = self.cli_flags()?;
        let flags_arg = if flags.is_empty() { String::new() } else { format!("{flags} ") };
        let auth_json = self.auth_json_path()?;
        let conn = connection(&self.args, SPEC);

        let mut env = BTreeMap::from([("CODEX_HOME".to_string(), CODEX_HOME.to_string())]);
        Self::ensure_agent_owned_dir(cx, CODEX_HOME).await?;
        exec_agent(cx, &format!("mkdir -p \"$CODEX_HOME\" {} /logs/agent", quote(SECRETS_DIR)), &env).await?;

        let mut setup = if let Some(p) = &auth_json {
            cx.env.upload_file(p, AUTH_PATH).await?;
            if let Some(u) = &cx.user {
                exec_root(cx, &format!("chown {u} {AUTH_PATH}"), &BTreeMap::new()).await?;
            }
            format!("ln -sf {} \"$CODEX_HOME/auth.json\"\n", quote(AUTH_PATH))
        } else {
            env.insert("OPENAI_API_KEY".into(), conn.api_key.clone().unwrap_or_default());
            format!(
                "cat >{q} <<EOF\n{{\n  \"OPENAI_API_KEY\": \"${{OPENAI_API_KEY}}\"\n}}\nEOF\nln -sf {q} \"$CODEX_HOME/auth.json\"\n",
                q = quote(AUTH_PATH)
            )
        };
        if let Some(u) = &conn.configured_base_url {
            env.insert("OPENAI_BASE_URL".into(), u.clone());
        }
        // codex only honors openai_base_url from config.toml, not the env var.
        let config = self.effective_config(conn.configured_base_url.as_deref())?;
        if !config.is_empty() {
            upload_text(cx, CONFIG_PATH, &toml::to_string(&toml::Value::try_from(&config)?)?).await?;
        }
        if let Some(d) = opts::skills_dir(&self.args) {
            setup += &format!("\nmkdir -p $HOME/.agents/skills && cp -r {}/* $HOME/.agents/skills/ 2>/dev/null || true", quote(&d));
        }
        if resume {
            setup += "\nif [ ! -d /logs/agent/sessions ]; then\n  echo \"Cannot resume Codex: no previous session logs found\" >&2\n  exit 1\nfi\n\
                      rm -rf \"$CODEX_HOME/sessions\"\ncp -R /logs/agent/sessions \"$CODEX_HOME/sessions\"";
        }
        exec_agent(cx, &setup, &env).await?;

        let cmd = format!(
            "if [ -s ~/.nvm/nvm.sh ]; then . ~/.nvm/nvm.sh; fi; codex exec {}--dangerously-bypass-approvals-and-sandbox --skip-git-repo-check \
             --model {model} --json --enable unified_exec {flags_arg}-- {} 2>&1 </dev/null | tee /logs/agent/codex.txt",
            if resume { "resume --last " } else { "" },
            quote(&instruction)
        );
        let r = cx.exec(&format!("set -o pipefail; {cmd}"), &env, None).await;
        // Always persist sessions and drop secrets, even on failure.
        let _ = cx
            .exec(
                "set -o pipefail; mkdir -p /logs/agent\nif [ -d \"$CODEX_HOME/sessions\" ]; then\n  rm -rf /logs/agent/sessions\n  cp -R \"$CODEX_HOME/sessions\" /logs/agent/sessions\nfi",
                &env,
                None,
            )
            .await;
        let _ = cx.exec_root(&format!("set -o pipefail; rm -rf {} {}", quote(SECRETS_DIR), quote(CODEX_HOME)), &BTreeMap::new()).await;
        let r = r?;
        if r.code != 0 {
            // Classify from harness-emitted events only; item.* events carry tool output and model text.
            let stdout = filter_jsonl_events(&r.stdout, |e| e["type"] == "turn.failed" || e["type"] == "error");
            return Err(exec_error(&cmd, &r, Some(&format!("{stdout}\n{}", r.stderr))));
        }
        Ok(())
    }

    fn populate_context_post_run(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        if let Some(c) = latest_session_file(logs_dir).and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| parse_rollout_usage(&t)) {
            ctx.n_input_tokens = c.n_input_tokens;
            ctx.n_cache_tokens = c.n_cache_tokens;
            ctx.n_output_tokens = c.n_output_tokens;
            ctx.cost_usd = c.cost_usd;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<Codex> {
        Codex::new(opts::test_args(kwargs, env, model))
    }

    #[test]
    fn flags_config_and_validation() {
        let a = agent(json!({"reasoning_effort": "high", "reasoning_summary": "auto", "web_search": "live", "disable_web_search": true, "dangerously_bypass_hook_trust": true}), &[], Some("openai/gpt-5")).unwrap();
        assert_eq!(a.cli_flags().unwrap(), "-c model_reasoning_effort=high -c model_reasoning_summary=auto -c web_search=disabled --dangerously-bypass-hook-trust");
        assert!(agent(json!({"reasoning_effort": "huge"}), &[], None).is_err());
        assert!(agent(json!({"nope": 1}), &[], None).is_err());

        let mut args = opts::test_args(json!({"config": {"model_verbosity": "low", "mcp_servers": {"keep": {"url": "http://k"}}}}), &[], Some("openai/gpt-5"));
        args.mcp_servers = vec![serde_json::from_value(json!({"name": "fs", "transport": "stdio", "command": "npx", "args": ["fs"]})).unwrap()];
        let a = Codex::new(args).unwrap();
        let c = a.effective_config(Some("http://proxy/v1")).unwrap();
        assert_eq!(c["openai_base_url"], "http://proxy/v1");
        assert_eq!(c["mcp_servers"]["fs"], json!({"command": "npx", "args": ["fs"]}));
        assert_eq!(c["mcp_servers"]["keep"]["url"], "http://k");
        assert!(toml::to_string(&toml::Value::try_from(&c).unwrap()).unwrap().contains("[mcp_servers.fs]"));
        assert!(agent(json!({"config": {"a": null}}), &[], None).is_err());
        assert!(parse_bool_env(Some("maybe".into()), "X").is_err());
        assert_eq!(parse_codex_version("codex-cli 0.118.0\n"), "0.118.0");
    }

    #[test]
    fn rollout_usage() {
        let text = r#"{"timestamp":"t","type":"session_meta","payload":{"id":"s1","cli_version":"0.118.0"}}
{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}
{"timestamp":"t","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":7,"reasoning_output_tokens":3,"total_tokens":107},"last_token_usage":{"input_tokens":100}}}}
{"timestamp":"t","type":"event_msg","payload":{"type":"token_count","info":null}}
not json
"#;
        let c = parse_rollout_usage(text).unwrap();
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens, c.cost_usd), (Some(100), Some(40), Some(7), None));
        assert!(parse_rollout_usage("{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{}}}}").is_none());

        let dir = std::env::temp_dir().join(format!("hbench-codex-{}", uuid::Uuid::new_v4()));
        let day = dir.join("sessions/2026/10/10");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::create_dir_all(dir.join("sessions/2026/09/01")).unwrap();
        std::fs::write(day.join("rollout-2026-10-10T00-00-00-a.jsonl"), text).unwrap();
        assert_eq!(latest_session_file(&dir).unwrap(), day.join("rollout-2026-10-10T00-00-00-a.jsonl"));
    }
}
