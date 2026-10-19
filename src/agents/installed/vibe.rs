//! Mistral Vibe installed agent.

use super::common::{installed_dir, skills_dir, uv_install};
use super::pins;
use super::{ensure_system_dependencies, exec_agent, exec_error};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::{kind_err, quote};
use crate::fmt::{float_with_point, truthy};
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const VIBE_HOME: &str = "/logs/agent/vibe-home";
const PROVIDER_NAME: &str = "hbench-openai";
const DEFAULT_API_BASE: &str = "https://api.mistral.ai/v1";
pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(Vibe::new(args)?))
}

#[derive(Debug, Default)]
struct Options {
    max_turns: Option<i64>,
    max_price: Option<String>,
    max_tokens: Option<i64>,
    thinking: Option<String>,
    temperature: f64,
    backend: Option<String>,
}

pub struct Vibe {
    args: AgentArgs,
    version: Option<String>,
    opts: Options,
}

fn toml_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One `[[key]]` table: its `(field, pre-rendered value)` pairs.
type TomlTable<'a> = Vec<(&'a str, String)>;

/// TOML rendering: top-level scalars, then `[[key]]` table arrays; values are pre-rendered.
fn to_toml(scalars: &[(&str, String)], tables: &[(&str, Vec<TomlTable>)]) -> String {
    let mut lines: Vec<String> = scalars.iter().map(|(k, v)| format!("{k} = {v}")).collect();
    let mut blocks = vec![];
    for (key, entries) in tables {
        for e in entries {
            blocks.push(format!("[[{key}]]"));
            blocks.extend(e.iter().map(|(k, v)| format!("{k} = {v}")));
            blocks.push(String::new());
        }
    }
    if !lines.is_empty() && !blocks.is_empty() {
        lines.push(String::new());
    }
    lines.extend(blocks);
    lines.join("\n") + "\n"
}

fn vibe_patterns() -> &'static [(Regex, &'static str)] {
    static P: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    P.get_or_init(|| {
        [
            (r"price limit exceeded: \$", "ApiUsageLimitError"),
            (r"turn limit of \d+ reached", "ApiUsageLimitError"),
            (r"token limit exceeded: [\d,]+ > [\d,]+", "ApiUsageLimitError"),
            (r"read ?timeout", "ApiConnectionError"),
            (r"connect(?:ion)? ?(?:error|timeout|reset|aborted)", "ApiConnectionError"),
            (r"network error", "ApiConnectionError"),
        ]
        .iter()
        .map(|(p, k)| (Regex::new(&format!("(?i){p}")).unwrap(), *k))
        .collect()
    })
}

/// Base ERROR_PATTERNS + Vibe's, last match wins.
fn vibe_classify(output: &str) -> &'static str {
    super::classify_with(output, vibe_patterns())
}

impl Vibe {
    pub fn new(args: AgentArgs) -> Result<Vibe> {
        let bad = |k: &str, why: &str| {
            anyhow!("Invalid kwargs for agent 'vibe':\n  {k}: {why}\nSee `hbench agents schema vibe` for available options.")
        };
        let int = |k: &str, env: &str| -> Result<Option<i64>> {
            let v = match args.kwargs.get(k) {
                Some(Value::Null) => return Ok(None), // explicit None opts out of the env fallback
                Some(v) => v.clone(),
                None => match args.get_env(env) {
                    Some(s) => Value::String(s),
                    None => return Ok(None),
                },
            };
            match &v {
                Value::Number(n) if n.as_i64().is_some() => Ok(n.as_i64()),
                Value::Number(n) if n.as_f64().is_some_and(|f| f.fract() == 0.0) => Ok(n.as_f64().map(|f| f as i64)),
                Value::String(s) => s.trim().parse().map(Some).map_err(|_| bad(k, "Input should be a valid integer")),
                _ => Err(bad(k, "Input should be a valid integer")),
            }
        };
        let string = |k: &str| -> Result<Option<String>> {
            match args.kwargs.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(Value::Number(n)) => Ok(Some(n.to_string())),
                Some(_) => Err(bad(k, "Input should be a valid string")),
            }
        };
        let temperature = match args.kwargs.get("temperature") {
            None => 0.2,
            Some(Value::Number(n)) => n.as_f64().unwrap(),
            Some(Value::String(s)) => s.trim().parse().map_err(|_| bad("temperature", "Input should be a valid number"))?,
            Some(_) => bail!(bad("temperature", "Input should be a valid number")),
        };
        let max_price = match args.kwargs.contains_key("max_price") {
            true => string("max_price")?,
            false => args.get_env("VIBE_MAX_PRICE"),
        };
        let opts = Options {
            max_turns: int("max_turns", "VIBE_MAX_TURNS")?,
            max_price,
            max_tokens: int("max_tokens", "VIBE_MAX_TOKENS")?,
            // `thinking` wins over its `reasoning_effort` alias.
            thinking: match string("thinking")? {
                Some(t) => Some(t),
                None => string("reasoning_effort")?,
            },
            temperature,
            backend: string("backend")?,
        };
        Ok(Vibe { version: args.version_kw(), args, opts })
    }

    fn model_id(&self) -> Result<String> {
        let m = self.args.model_name.as_deref().ok_or_else(|| anyhow!("Model name is required"))?;
        Ok(m.split_once('/').map(|(_, r)| r).unwrap_or(m).to_string())
    }

    /// Agent env > host env without dropping empty values (an empty key is deliberate keyless auth).
    fn raw_env(&self, k: &str) -> Option<String> {
        self.args.extra_env.get(k).cloned().or_else(|| std::env::var(k).ok())
    }

    fn cli_flags(&self) -> String {
        let mut f = vec![];
        if let Some(v) = self.opts.max_turns {
            f.push(format!("--max-turns {v}"));
        }
        if let Some(v) = &self.opts.max_price {
            f.push(format!("--max-price {}", quote(v)));
        }
        if let Some(v) = self.opts.max_tokens {
            f.push(format!("--max-tokens {v}"));
        }
        f.join(" ")
    }

    fn backend_and_key_env(&self) -> Result<(String, String)> {
        let backend = self
            .args
            .get_env("VIBE_BACKEND")
            .or_else(|| self.opts.backend.clone().filter(|b| !b.is_empty()))
            .unwrap_or_else(|| "mistral".into())
            .to_lowercase();
        if backend != "mistral" && backend != "generic" {
            bail!(
                "Unknown Vibe backend '{backend}'; valid backends are 'mistral' and 'generic' (use 'generic' for any OpenAI-compatible endpoint)."
            );
        }
        let default_key = if backend == "generic" { "OPENAI_API_KEY" } else { "MISTRAL_API_KEY" };
        let key_env = self.args.get_env("VIBE_API_KEY_ENV").unwrap_or_else(|| default_key.into());
        Ok((backend, key_env))
    }

    /// Dollars per million (input, output) tokens: explicit VIBE_INPUT_PRICE / VIBE_OUTPUT_PRICE,
    /// else the bundled price table (full model name, then the provider-stripped id; all-zero entries skipped).
    fn pricing(&self) -> Result<Option<(f64, f64)>> {
        let (i, o) = (self.args.get_env("VIBE_INPUT_PRICE"), self.args.get_env("VIBE_OUTPUT_PRICE"));
        match (&i, &o) {
            (None, None) => {
                let Some(m) = self.args.model_name.as_deref() else { return Ok(None) };
                Ok([m, m.split_once('/').map_or(m, |(_, r)| r)].into_iter().find_map(|k| {
                    let e = crate::agents::pricing::model_cost(k)?;
                    let rate = |f: &str| e.get(f).and_then(Value::as_f64).unwrap_or(0.0);
                    let (i, o) = (rate("input_cost_per_token"), rate("output_cost_per_token"));
                    (i > 0.0 || o > 0.0).then_some((i * 1_000_000.0, o * 1_000_000.0))
                }))
            }
            (Some(i), Some(o)) => match (i.trim().parse::<f64>(), o.trim().parse::<f64>()) {
                (Ok(a), Ok(b)) => Ok(Some((a, b))),
                _ => bail!("VIBE_INPUT_PRICE and VIBE_OUTPUT_PRICE must be numbers (dollars per million tokens); got '{i}' / '{o}'."),
            },
            _ => bail!("Set both VIBE_INPUT_PRICE and VIBE_OUTPUT_PRICE (dollars per million tokens); only one was provided."),
        }
    }

    fn config_toml(&self) -> Result<String> {
        let (backend, key_env) = self.backend_and_key_env()?;
        let api_base = if backend == "mistral" {
            self.args.get_env("VIBE_API_BASE").unwrap_or_else(|| DEFAULT_API_BASE.into())
        } else {
            self.args.get_env("VIBE_API_BASE").or_else(|| self.args.get_env("OPENAI_BASE_URL")).ok_or_else(|| {
                anyhow!("The generic backend requires an explicit endpoint; set VIBE_API_BASE or OPENAI_BASE_URL (e.g. VIBE_API_BASE=https://api.mistral.ai/v1).")
            })?
        };
        let provider_name = if backend == "mistral" { "mistral" } else { PROVIDER_NAME };
        let alias = self.model_id()?;
        let mut provider = vec![
            ("name", toml_str(provider_name)),
            ("api_base", toml_str(&api_base)),
            ("api_key_env_var", toml_str(&key_env)),
            ("backend", toml_str(&backend)),
        ];
        if backend != "mistral" {
            provider.push(("api_style", toml_str("openai")));
        }
        let mut model = vec![
            ("name", toml_str(&alias)),
            ("provider", toml_str(provider_name)),
            ("alias", toml_str(&alias)),
            ("temperature", float_with_point(self.opts.temperature)),
        ];
        if let Some(t) = &self.opts.thinking {
            model.push(("thinking", toml_str(t)));
        }
        let pricing = self.pricing()?;
        if let Some((i, o)) = pricing {
            model.push(("input_price", float_with_point(i)));
            model.push(("output_price", float_with_point(o)));
        }
        if self.opts.max_price.is_some() && pricing.is_none_or(|(i, o)| i <= 0.0 && o <= 0.0) {
            bail!(
                "--max-price cannot be enforced: Vibe computes session cost from the model's configured prices, and no usable (non-zero) pricing is known for '{}'. \
                 Set VIBE_INPUT_PRICE and VIBE_OUTPUT_PRICE (dollars per million tokens) or use a model in LiteLLM's pricing table.",
                self.args.model_name.as_deref().unwrap_or_default()
            );
        }
        let mut scalars = vec![
            ("active_model", toml_str(&alias)),
            ("enable_telemetry", "false".into()),
            ("enable_update_checks", "false".into()),
            ("enable_auto_update", "false".into()),
            ("enable_notifications", "false".into()),
        ];
        if let Some(d) = skills_dir(&self.args) {
            scalars.push(("skill_paths", format!("[{}]", toml_str(&d))));
        }
        Ok(to_toml(&scalars, &[("providers", vec![provider]), ("models", vec![model])]))
    }

    fn mcp_command(&self, config_path: &str) -> Result<Option<String>> {
        if self.args.mcp_servers.is_empty() {
            return Ok(None);
        }
        let mut servers = vec![];
        for s in &self.args.mcp_servers {
            let t = s.transport();
            let none = || "None".to_string(); // Python str(None)
            servers.push(match t {
                "stdio" => vec![
                    ("name", toml_str(&s.name)),
                    ("transport", toml_str("stdio")),
                    ("command", toml_str(&s.command.clone().unwrap_or_else(none))),
                    ("args", format!("[{}]", s.args.iter().map(|a| toml_str(a)).collect::<Vec<_>>().join(", "))),
                ],
                "streamable-http" => vec![("name", toml_str(&s.name)), ("transport", toml_str("streamable-http")), ("url", toml_str(&s.url.clone().unwrap_or_else(none)))],
                _ => bail!(
                    "Vibe does not support MCP transport '{t}' (server '{}'). Set transport = 'streamable-http' or 'stdio' explicitly in the task's MCP server config (the task format defaults to 'sse' when omitted).",
                    s.name
                ),
            });
        }
        let block = to_toml(&[], &[("mcp_servers", servers)]);
        Ok(Some(format!("cat >> {} <<'VIBE_MCP_EOF'\n{block}VIBE_MCP_EOF", quote(config_path))))
    }

    async fn exec(&self, cx: &AgentCtx<'_>, cmd: &str, env: &BTreeMap<String, String>) -> Result<()> {
        let r = cx.exec(&format!("set -o pipefail; {cmd}"), env, None).await?;
        if r.code != 0 {
            let e = exec_error(cmd, &r, None);
            return Err(kind_err(vibe_classify(&format!("{}\n{}", r.stdout, r.stderr)), e.to_string()));
        }
        Ok(())
    }
}

fn read_meta(dir: &Path) -> Map<String, Value> {
    std::fs::read_to_string(dir.join("meta.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn read_messages(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("messages.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter(Value::is_object)
        .collect()
}

fn str_field<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Session dirs for this run, root→latest, following `parent_session_id` from the newest leaf.
fn session_chain(logs_dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(logs_dir.join("vibe-home/logs/session")) else { return vec![] };
    let dirs: Vec<PathBuf> =
        rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|d| d.is_dir() && d.join("messages.jsonl").is_file()).collect();
    if dirs.is_empty() {
        return vec![];
    }
    let metas: Vec<Map<String, Value>> = dirs.iter().map(|d| read_meta(d)).collect();
    let parents: Vec<&str> = metas.iter().filter_map(|m| str_field(m, "parent_session_id")).collect();
    let mut leaves: Vec<usize> =
        (0..dirs.len()).filter(|&i| str_field(&metas[i], "session_id").is_none_or(|id| !parents.contains(&id))).collect();
    if leaves.is_empty() {
        leaves = (0..dirs.len()).collect();
    }
    let key = |i: usize| {
        let start = metas[i].get("start_time").and_then(Value::as_str).unwrap_or("").to_string();
        let mtime = std::fs::metadata(&dirs[i]).and_then(|m| m.modified()).ok();
        (start, mtime)
    };
    let mut latest = leaves[0];
    for &i in &leaves[1..] {
        if key(i) > key(latest) {
            latest = i;
        }
    }
    let mut chain = vec![latest];
    let mut parent = str_field(&metas[latest], "parent_session_id");
    while let Some(p) = parent {
        let Some(i) = (0..dirs.len()).find(|&i| str_field(&metas[i], "session_id") == Some(p)) else { break };
        if chain.contains(&i) {
            break;
        }
        chain.push(i);
        parent = str_field(&metas[i], "parent_session_id");
    }
    chain.iter().rev().map(|&i| dirs[i].clone()).collect()
}

/// Trajectory `final_metrics` mapping, computed straight from the session chain.
fn session_usage(chain: &[PathBuf], has_pricing: bool) -> Option<(Option<i64>, Option<i64>, Option<f64>)> {
    let first = chain.first()?;
    let first_msgs = read_messages(first);
    // The trajectory exists iff it has at least one step.
    let sys = read_meta(first).get("system_prompt").cloned().unwrap_or(Value::Null);
    let sys_step = truthy(&sys) && !first_msgs.iter().any(|m| m["role"] == "system") && (!sys.is_object() || truthy(&sys["content"]));
    let mut has_steps = sys_step || first_msgs.iter().any(|m| m["role"] != "tool");
    for d in &chain[1..] {
        has_steps |= !read_messages(d).is_empty();
    }
    if !has_steps {
        return None;
    }
    let meta = read_meta(chain.last()?);
    let Some(stats) = meta.get("stats").and_then(Value::as_object) else { return Some((None, None, None)) };
    let mut cost = stats.get("session_cost").and_then(Value::as_f64);
    if cost.is_none_or(|c| c == 0.0) && !has_pricing {
        cost = None;
    }
    Some((stats.get("session_prompt_tokens").and_then(Value::as_i64), stats.get("session_completion_tokens").and_then(Value::as_i64), cost))
}

#[async_trait::async_trait]
impl Agent for Vibe {
    fn name(&self) -> &str {
        "vibe"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "bash"]).await?;
        let spec = pins::VIBE.pip_constraint(self.version.as_deref());
        exec_agent(
            cx,
            &format!(
                "set -euo pipefail; if ! command -v uv &> /dev/null; then  {uv_install}; fi && \
                 export PATH=\"$HOME/.local/bin:$PATH\" && uv tool install mistral-vibe{spec} && vibe --version",
                uv_install = uv_install(None)
            ),
            &BTreeMap::new(),
        )
        .await?;
        if self.version.is_none() {
            let r = cx.exec("export PATH=\"$HOME/.local/bin:$PATH\"; vibe --version", &BTreeMap::new(), None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(parse_vibe_version(&r.stdout));
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        if self.args.model_name.is_none() {
            bail!("Model name is required");
        }
        let config_path = format!("{VIBE_HOME}/config.toml");
        let (backend, key_env) = self.backend_and_key_env()?;
        let Some(key) = self.raw_env(&key_env) else {
            bail!(
                "The Vibe '{backend}' backend reads its API key from '{key_env}', which is not set. Set it (via --ae {key_env}=...), \
                 point VIBE_API_KEY_ENV at the variable holding the key, or set {key_env} to an empty value for endpoints that require no key."
            );
        };
        let env = BTreeMap::from([("VIBE_HOME".to_string(), VIBE_HOME.to_string()), (key_env, key)]);
        self.exec(cx, &format!("mkdir -p {}", quote(VIBE_HOME)), &env).await?;
        let mut setup = format!("cat > {} <<'VIBE_CONFIG_EOF'\n{}VIBE_CONFIG_EOF", quote(&config_path), self.config_toml()?);
        if let Some(m) = self.mcp_command(&config_path)? {
            setup += &format!("\n{m}");
        }
        self.exec(cx, &setup, &env).await?;
        let flags = self.cli_flags();
        let flags = if flags.is_empty() { flags } else { flags + " " };
        let cmd = format!(
            "export PATH=\"$HOME/.local/bin:$PATH\"; vibe --auto-approve --trust --output text {flags}--prompt={} 2>&1 </dev/null | tee /logs/agent/vibe.txt",
            quote(&instruction)
        );
        self.exec(cx, &cmd, &env).await
    }

    fn collect_usage(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        let has_pricing = self.pricing().is_ok_and(|p| p.is_some());
        if let Some((input, output, cost)) = session_usage(&session_chain(logs_dir), has_pricing) {
            ctx.cost_usd = cost;
            ctx.n_input_tokens = input;
            ctx.n_output_tokens = output;
        }
    }
}

/// "vibe 2.17.1" -> "2.17.1" (first non-empty line).
fn parse_vibe_version(stdout: &str) -> String {
    let text = stdout.trim();
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.strip_prefix("vibe").unwrap_or(l).trim().to_string())
        .unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;
    use serde_json::json;

    fn agent_with(kwargs: Value, env: &[(&str, &str)], model: Option<&str>, mcp: Vec<crate::task::McpServer>) -> Result<Vibe> {
        Vibe::new(AgentArgs { mcp_servers: mcp, ..common::test_args(kwargs, env, model) })
    }
    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Vibe {
        agent_with(kwargs, env, model, vec![]).unwrap()
    }

    #[test]
    fn config_and_flags() {
        let a = agent(json!({}), &[], Some("mistral/mistral-medium-3-5"));
        let c = a.config_toml().unwrap();
        for s in [
            "active_model = \"mistral-medium-3-5\"",
            "api_base = \"https://api.mistral.ai/v1\"",
            "backend = \"mistral\"",
            "name = \"mistral\"",
            "api_key_env_var = \"MISTRAL_API_KEY\"",
            "temperature = 0.2",
        ] {
            assert!(c.contains(s), "{s} missing in {c}");
        }
        assert!(!c.contains("api_style") && !c.contains("thinking"));
        // Price table, mistral/mistral-medium-3-5: 1.5e-6 / 7.5e-6 per token.
        assert!(c.contains("input_price = 1.5") && c.contains("output_price = 7.5"), "{c}");
        assert!(c.starts_with("active_model = \"mistral-medium-3-5\"\nenable_telemetry = false\n"));
        assert_eq!(agent(json!({}), &[], Some("together_ai/openai/gpt-oss-120b")).model_id().unwrap(), "openai/gpt-oss-120b");

        let g = agent(
            json!({"backend": "generic", "reasoning_effort": "low", "temperature": 1}),
            &[("VIBE_API_BASE", "https://x/v1"), ("VIBE_INPUT_PRICE", "1.5"), ("VIBE_OUTPUT_PRICE", "7")],
            Some("m"),
        );
        let c = g.config_toml().unwrap();
        for s in [
            "api_style = \"openai\"",
            "name = \"hbench-openai\"",
            "api_key_env_var = \"OPENAI_API_KEY\"",
            "thinking = \"low\"",
            "temperature = 1.0",
            "input_price = 1.5",
            "output_price = 7.0",
        ] {
            assert!(c.contains(s), "{s} missing in {c}");
        }
        let e = agent(json!({"backend": "generic"}), &[("OPENAI_BASE_URL", "")], Some("m")).config_toml();
        if std::env::var("OPENAI_BASE_URL").is_err() && std::env::var("VIBE_API_BASE").is_err() {
            assert!(e.unwrap_err().to_string().contains("requires an explicit endpoint"));
        }
        assert!(
            agent(json!({}), &[("VIBE_BACKEND", "openai")], Some("m"))
                .config_toml()
                .unwrap_err()
                .to_string()
                .contains("Unknown Vibe backend 'openai'")
        );

        // pricing validation
        assert!(
            agent(json!({"max_price": "2.50"}), &[], Some("m"))
                .config_toml()
                .unwrap_err()
                .to_string()
                .contains("max-price cannot be enforced")
        );
        assert!(
            agent(json!({"max_price": "2.50"}), &[("VIBE_INPUT_PRICE", "0"), ("VIBE_OUTPUT_PRICE", "0")], Some("m")).config_toml().is_err()
        );
        // Price-table fallback (mistral/devstral-medium-2507: in 4e-7, out 2e-6 per token) makes --max-price enforceable.
        let p = agent(json!({"max_price": "2.50"}), &[], Some("mistral/devstral-medium-2507"));
        let (i, o) = p.pricing().unwrap().unwrap();
        assert!((i - 0.4).abs() < 1e-9 && (o - 2.0).abs() < 1e-9);
        assert!(p.config_toml().unwrap().contains("input_price = "));
        assert!(
            agent(json!({}), &[("VIBE_INPUT_PRICE", "1.5")], Some("m"))
                .config_toml()
                .unwrap_err()
                .to_string()
                .contains("both VIBE_INPUT_PRICE")
        );
        assert!(
            agent(json!({}), &[("VIBE_INPUT_PRICE", "$1.5"), ("VIBE_OUTPUT_PRICE", "7")], Some("m"))
                .config_toml()
                .unwrap_err()
                .to_string()
                .contains("must be numbers")
        );

        // flags + env fallbacks + option validation
        let f = agent(json!({"max_turns": 5, "max_tokens": "100"}), &[("VIBE_MAX_PRICE", "2.50")], Some("m"));
        assert_eq!(f.cli_flags(), "--max-turns 5 --max-price 2.50 --max-tokens 100");
        assert_eq!(agent(json!({"max_turns": null}), &[("VIBE_MAX_TURNS", "9")], Some("m")).cli_flags(), "");
        assert!(agent_with(json!({"max_turns": "x"}), &[], Some("m"), vec![]).is_err());
        assert_eq!(parse_vibe_version("\nvibe 2.17.1\n"), "2.17.1");
    }

    #[test]
    fn toml_and_mcp() {
        let t = to_toml(&[("api_base", toml_str("https://host/v1\n")), ("name", toml_str("a\tb\u{1}c"))], &[]);
        assert!(t.contains("api_base = \"https://host/v1\\n\"") && t.contains("name = \"a\\tb\\u0001c\""));
        let srv = |name: &str, transport: &str| crate::task::McpServer {
            name: name.into(),
            transport: transport.into(),
            url: Some("http://m:8000/mcp".into()),
            command: Some("run-server".into()),
            args: vec!["--x".into()],
        };
        let a = agent_with(json!({}), &[], Some("m"), vec![srv("h", "http"), srv("l", "stdio")]).unwrap();
        let c = a.mcp_command("/cfg/config.toml").unwrap().unwrap();
        assert!(c.starts_with("cat >> /cfg/config.toml <<'VIBE_MCP_EOF'\n[[mcp_servers]]\nname = \"h\"\ntransport = \"streamable-http\"\nurl = \"http://m:8000/mcp\"\n"));
        assert!(c.contains("command = \"run-server\"\nargs = [\"--x\"]\n\nVIBE_MCP_EOF"));
        let s = agent_with(json!({}), &[], Some("m"), vec![srv("legacy", "sse")]).unwrap();
        assert!(s.mcp_command("/c").unwrap_err().to_string().contains("does not support MCP transport 'sse'"));
    }

    #[test]
    fn error_classification() {
        assert_eq!(vibe_classify("LLM backend error [mistral]\n  reason: ReadTimeout('')"), "ApiConnectionError");
        assert_eq!(vibe_classify("provider_message: Network error"), "ApiConnectionError");
        assert_eq!(vibe_classify("HTTP 429: rate limit exceeded"), "ApiRateLimitError");
        assert_eq!(vibe_classify("some unrelated failure"), "NonZeroAgentExitCodeError");
        assert_eq!(vibe_classify("reason: ReadTimeout('')\n... retried ...\nError: You've hit your usage limit"), "ApiUsageLimitError");
        assert_eq!(vibe_classify("provider_message: Network error\nHTTP 429: rate limit exceeded"), "ApiRateLimitError");
        for o in ["Price limit exceeded: $2.5100 > $2.50", "Turn limit of 5 reached", "Token limit exceeded: 120,001 > 120,000"] {
            assert_eq!(vibe_classify(o), "ApiUsageLimitError");
        }
        assert_ne!(vibe_classify("discussing how a token limit exceeded error should be handled"), "ApiUsageLimitError");
        assert_eq!(vibe_classify("curl: (6) Could not resolve host: astral.sh\nconnection error"), "ApiConnectionError");
    }

    fn write_session(root: &Path, name: &str, msgs: &[Value], meta: Option<Value>) {
        let d = root.join("vibe-home/logs/session").join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("messages.jsonl"), msgs.iter().map(|m| m.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        if let Some(m) = meta {
            std::fs::write(d.join("meta.json"), m.to_string()).unwrap();
        }
    }

    #[test]
    fn usage_from_session_chain() {
        let a = agent(json!({}), &[], Some("mistral-medium-3-5-external"));
        let root = std::env::temp_dir().join(format!("hbench-vibe-logs-{}", uuid::Uuid::new_v4()));
        let mut ctx = AgentContext::default();
        a.collect_usage(&root, &mut ctx);
        assert!(ctx.is_empty());

        // Compaction: s2 continues s1; an unrelated newer-looking old session is ignored.
        write_session(
            &root,
            "s1",
            &[json!({"role": "user", "content": "hi"}), json!("stray")],
            Some(json!({"session_id": "a", "start_time": "2026-07-13T10:00:00+00:00", "stats": {"session_prompt_tokens": 1}})),
        );
        write_session(
            &root,
            "s2",
            &[json!({"role": "user", "content": "summary"}), json!({"role": "assistant", "content": "done"})],
            Some(
                json!({"session_id": "b", "parent_session_id": "a", "start_time": "2026-07-13T11:00:00+00:00", "stats": {"session_prompt_tokens": 100, "session_completion_tokens": 50, "session_cost": 0.0012}}),
            ),
        );
        write_session(
            &root,
            "s0",
            &[json!({"role": "user", "content": "old"})],
            Some(json!({"session_id": "z", "start_time": "2026-07-12T00:00:00+00:00"})),
        );
        let chain = session_chain(&root);
        assert_eq!(chain.iter().map(|p| p.file_name().unwrap().to_str().unwrap()).collect::<Vec<_>>(), ["s1", "s2"]);
        a.collect_usage(&root, &mut ctx);
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens, ctx.cost_usd), (Some(100), Some(50), Some(0.0012)));

        // $0 cost without configured pricing is unknown; with explicit zero prices it's real.
        let root2 = std::env::temp_dir().join(format!("hbench-vibe-logs-{}", uuid::Uuid::new_v4()));
        write_session(
            &root2,
            "s",
            &[json!({"role": "user", "content": "hi"})],
            Some(json!({"stats": {"session_prompt_tokens": 10, "session_completion_tokens": 0, "session_cost": 0.0}})),
        );
        let mut c2 = AgentContext::default();
        a.collect_usage(&root2, &mut c2);
        assert_eq!((c2.n_input_tokens, c2.n_output_tokens, c2.cost_usd), (Some(10), Some(0), None));
        let free = agent(json!({}), &[("VIBE_INPUT_PRICE", "0"), ("VIBE_OUTPUT_PRICE", "0")], Some("m"));
        free.collect_usage(&root2, &mut c2);
        assert_eq!(c2.cost_usd, Some(0.0));
    }
}
