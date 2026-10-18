//! trae-agent installed agent.

use super::common::{installed_dir, uv_install};
use super::pins;
use super::{connection, ensure_system_dependencies, exec_agent};
use crate::agents::model_connection::{Resolved, Spec, with_canonical_provider_envs};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const SPEC: Spec = Spec { default_provider: None, api_key_envs: &[], base_url_envs: &[], passthrough: true };
const CONFIG_PATH: &str = "/logs/agent/trae_config.yaml";
const TRAJECTORY_FILE: &str = "trae-trajectory.json";

/// Provider -> trae-agent LLMProvider.
fn trae_provider(p: &str) -> Option<&'static str> {
    Some(match p {
        "anthropic" => "anthropic",
        "openai" => "openai",
        "google" | "gemini" => "google",
        "openrouter" => "openrouter",
        "doubao" => "doubao",
        "azure" => "azure",
        "ollama" => "ollama",
        _ => return None,
    })
}

/// Pydantic lax `int` coercion.
pub(crate) fn opt_int(k: &str, v: &Value) -> Result<i64> {
    let bad = || anyhow!("Invalid kwargs for agent: {k}: Input should be a valid integer");
    match v {
        Value::Bool(b) => Ok(*b as i64),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)).ok_or_else(bad),
        Value::String(s) => s.trim().parse().map_err(|_| bad()),
        _ => Err(bad()),
    }
}

/// Pydantic `str` with coerce_numbers_to_str.
pub(crate) fn opt_str(k: &str, v: &Value) -> Result<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        _ => bail!("Invalid kwargs for agent: {k}: Input should be a valid string"),
    }
}

pub struct TraeAgent {
    args: AgentArgs,
    version: Option<String>,
    max_steps: i64,
    temperature: String,
    max_tokens: i64,
    top_p: String,
    top_k: i64,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(TraeAgent::new(args)?))
}

impl TraeAgent {
    fn new(args: AgentArgs) -> Result<TraeAgent> {
        // kwarg > fallback env (extra_env > host) > default.
        let raw =
            |k: &str, env: &str| args.kwargs.get(k).filter(|v| !v.is_null()).cloned().or_else(|| args.get_env(env).map(Value::String));
        let int = |k: &str, env: &str, d: i64| raw(k, env).map(|v| opt_int(k, &v)).unwrap_or(Ok(d));
        let st = |k: &str, env: &str, d: &str| raw(k, env).map(|v| opt_str(k, &v)).unwrap_or(Ok(d.to_string()));
        Ok(TraeAgent {
            max_steps: int("max_steps", "TRAE_MAX_STEPS", 200)?,
            temperature: st("temperature", "TRAE_TEMPERATURE", "0.7")?,
            max_tokens: int("max_tokens", "TRAE_MAX_TOKENS", 16384)?,
            top_p: st("top_p", "TRAE_TOP_P", "0.95")?,
            top_k: int("top_k", "TRAE_TOP_K", 20)?,
            version: args.version_kw(),
            args,
        })
    }

    fn connection(&self) -> Resolved {
        with_canonical_provider_envs(connection(&self.args, SPEC))
    }

    fn config_yaml(&self, provider: &str, model: &str, api_key: &str, base_url: Option<&str>) -> Result<String> {
        let float = |k: &str, v: &str| v.trim().parse::<f64>().map_err(|_| anyhow!("could not convert string to float: '{v}' ({k})"));
        let mut prov = json!({"api_key": api_key, "provider": provider});
        if let Some(u) = base_url.filter(|u| !u.is_empty()) {
            prov["base_url"] = json!(u);
        }
        let cfg = json!({
            "agents": {"trae_agent": {
                "enable_lakeview": false,
                "model": "hbench_model",
                "max_steps": self.max_steps,
                "tools": ["bash", "str_replace_based_edit_tool", "sequentialthinking", "task_done"],
            }},
            "model_providers": {provider: prov},
            "models": {"hbench_model": {
                "model_provider": provider,
                "model": model,
                "max_tokens": self.max_tokens,
                "temperature": float("temperature", &self.temperature)?,
                "top_p": float("top_p", &self.top_p)?,
                "top_k": self.top_k,
                "max_retries": 10,
                "parallel_tool_calls": true,
            }},
        });
        Ok(serde_yaml_ng::to_string(&cfg)?)
    }

    /// Validated (provider, model, env, config yaml) for a run.
    fn prepare(&self) -> Result<(BTreeMap<String, String>, String)> {
        let Some((hp, model)) = self.args.model_name.as_deref().and_then(|m| m.split_once('/')) else {
            bail!("Model name must be in the format provider/model_name");
        };
        let access = self.connection();
        let tp = trae_provider(hp).filter(|tp| access.provider.as_deref() == Some(*tp));
        let Some(tp) = tp else {
            bail!("Unsupported provider: {hp}. Supported: anthropic, azure, doubao, gemini, google, ollama, openai, openrouter");
        };
        let Some(key) = access.api_key.clone().filter(|k| !k.is_empty()) else { bail!("No API key found for provider: {tp}") };
        let yaml = self.config_yaml(tp, model, &key, access.configured_base_url.as_deref())?;
        Ok((access.env, yaml))
    }
}

/// ATIF Step timestamp check: Python's `datetime.fromisoformat(v.replace("Z", "+00:00"))`.
// Covers the RFC 3339 / naive date-time / date forms agents emit, not every fromisoformat variant.
pub(crate) fn iso_timestamp_ok(v: &Value) -> bool {
    use chrono::{DateTime, NaiveDate, NaiveDateTime};
    let Some(s) = v.as_str() else { return v.is_null() };
    let s = s.replace('Z', "+00:00");
    DateTime::parse_from_rfc3339(&s).is_ok()
        || ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%:z"].iter().any(|f| DateTime::parse_from_str(&s, f).is_ok())
        || ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M"].iter().any(|f| NaiveDateTime::parse_from_str(&s, f).is_ok())
        || NaiveDate::parse_from_str(&s, "%Y-%m-%d").is_ok()
}

/// Totals from trae-agent's trajectory JSON, as ATIF final_metrics:
/// (input, output, cache) or None when the converter would produce no steps.
pub fn trajectory_usage(raw: &Value) -> Option<(i64, i64, i64)> {
    let empty = vec![];
    let inter = raw["llm_interactions"].as_array().unwrap_or(&empty);
    let steps = raw["agent_steps"].as_array().unwrap_or(&empty);
    let truthy = |v: &Value| {
        !matches!(v, Value::Null | Value::Bool(false)) && *v != json!("") && *v != json!(0) && *v != json!([]) && *v != json!({})
    };
    if inter.is_empty() && !steps.iter().any(|s| truthy(&s["error"])) {
        return None;
    }
    // Step timestamps that fail ATIF validation make conversion fail (no usage recorded).
    let stepped: Vec<&Value> =
        if inter.is_empty() { steps.iter().filter(|s| truthy(&s["error"])).collect() } else { inter.iter().collect() };
    if !stepped.iter().all(|s| iso_timestamp_ok(&s["timestamp"])) {
        return None;
    }
    let (mut i, mut o, mut c) = (0, 0, 0);
    for x in inter {
        let u = &x["response"]["usage"];
        i += u["input_tokens"].as_i64().unwrap_or(0);
        o += u["output_tokens"].as_i64().unwrap_or(0);
        c += u["cache_read_input_tokens"].as_i64().unwrap_or(0);
    }
    Some((i, o, c))
}

/// trae-cli version parsing: first non-empty line, strip "trae-cli" / ",", take up to ",".
pub fn parse_trae_version(stdout: &str) -> String {
    let text = stdout.trim();
    if let Some(line) = text.lines().map(str::trim).find(|l| !l.is_empty()) {
        let l = line.strip_prefix("trae-cli").unwrap_or(line);
        let l = l.strip_prefix(',').unwrap_or(l);
        return l.trim().split(',').next().unwrap_or("").trim().to_string();
    }
    text.to_string()
}

#[async_trait::async_trait]
impl Agent for TraeAgent {
    fn name(&self) -> &str {
        "trae-agent"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "git"]).await?;
        let vs = match pins::TRAE_AGENT.or(self.version.as_deref()) {
            "latest" => String::new(),
            v => format!("@{v}"),
        };
        exec_agent(
            cx,
            &format!(
                "set -euo pipefail; if ! command -v uv &>/dev/null; then  {uv_install} &&  \
                 export PATH=\"$HOME/.local/bin:$PATH\"; fi && uv tool install --python 3.12 \"trae-agent[test,evaluation] @ \
                 git+https://github.com/bytedance/trae-agent.git{vs}\" && export PATH=\"$HOME/.local/bin:$PATH\" && trae-cli --help",
                uv_install = uv_install(None)
            ),
            &BTreeMap::new(),
        )
        .await?;
        if self.version.is_none() {
            let r = cx.exec("export PATH=\"$HOME/.local/bin:$PATH\"; trae-cli --version", &BTreeMap::new(), None).await;
            if let Ok(r) = r
                && r.code == 0
                && !r.stdout.is_empty()
            {
                self.version = Some(parse_trae_version(&r.stdout));
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let (env, yaml) = self.prepare()?;
        exec_agent(cx, &format!("cat > {CONFIG_PATH} <<'HBENCH_CONFIG_EOF'\n{yaml}HBENCH_CONFIG_EOF"), &env).await?;
        let cmd = format!(
            "export PATH=\"$HOME/.local/bin:$PATH\"; trae-cli run --config-file {CONFIG_PATH} --working-dir . \
             --trajectory-file /logs/agent/{TRAJECTORY_FILE} --patch-path /logs/agent/trae-agent.patch --console-type simple \
             {} 2>&1 | stdbuf -oL tee /logs/agent/trae-agent.txt",
            quote(&instruction)
        );
        let r = exec_agent(cx, &cmd, &env).await;
        // Mask the API key left in the config, whatever the outcome.
        let _ = exec_agent(
            cx,
            &format!(r"sed -i 's/\(api_key:\s*\)\(.\{{4\}}\)[^ ]*/\1\2********/g' {CONFIG_PATH} 2>/dev/null || true"),
            &BTreeMap::new(),
        )
        .await;
        r.map(|_| ())
    }

    fn collect_usage(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        let Some(raw) = std::fs::read_to_string(logs_dir.join(TRAJECTORY_FILE)).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            return;
        };
        if let Some((i, o, c)) = trajectory_usage(&raw) {
            ctx.cost_usd = None;
            ctx.n_input_tokens = Some(i);
            ctx.n_output_tokens = Some(o);
            ctx.n_cache_tokens = Some(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common::test_args as args;

    #[test]
    fn options_and_config() {
        let a =
            TraeAgent::new(args(json!({}), &[("ANTHROPIC_API_KEY", "sk-test-123")], Some("anthropic/claude-sonnet-4-20250514"))).unwrap();
        let (env, yaml) = a.prepare().unwrap();
        assert_eq!(env["ANTHROPIC_API_KEY"], "sk-test-123");
        let cfg: Value = serde_yaml_ng::from_str(&yaml).unwrap();
        let ag = &cfg["agents"]["trae_agent"];
        assert_eq!(
            (ag["enable_lakeview"].clone(), ag["model"].clone(), ag["max_steps"].clone()),
            (json!(false), json!("hbench_model"), json!(200))
        );
        assert_eq!(cfg["model_providers"]["anthropic"], json!({"api_key": "sk-test-123", "provider": "anthropic"}));
        let m = &cfg["models"]["hbench_model"];
        assert_eq!(
            (m["temperature"].as_f64(), m["top_p"].as_f64(), m["top_k"].as_i64(), m["max_tokens"].as_i64()),
            (Some(0.7), Some(0.95), Some(20), Some(16384))
        );
        assert_eq!(
            (m["model"].as_str(), m["max_retries"].as_i64(), m["parallel_tool_calls"].as_bool()),
            (Some("claude-sonnet-4-20250514"), Some(10), Some(true))
        );

        let b = TraeAgent::new(args(
            json!({"max_steps": "50", "temperature": 0.2}),
            &[("TRAE_TOP_K", "5"), ("OPENAI_API_KEY", "k"), ("OPENAI_BASE_URL", "http://x")],
            Some("openai/gpt-5"),
        ))
        .unwrap();
        let cfg: Value = serde_yaml_ng::from_str(&b.prepare().unwrap().1).unwrap();
        assert_eq!(cfg["agents"]["trae_agent"]["max_steps"], 50);
        assert_eq!(cfg["models"]["hbench_model"]["top_k"], 5);
        assert_eq!(cfg["models"]["hbench_model"]["temperature"], 0.2);
        assert_eq!(cfg["model_providers"]["openai"]["base_url"], "http://x");

        assert!(TraeAgent::new(args(json!({"max_steps": "many"}), &[], None)).is_err());
        // Run-time validation.
        assert!(TraeAgent::new(args(json!({}), &[], Some("nomodel"))).unwrap().prepare().is_err());
        assert!(TraeAgent::new(args(json!({}), &[("XAI_API_KEY", "k")], Some("xai/grok"))).unwrap().prepare().is_err());
        assert!(
            TraeAgent::new(args(json!({"temperature": "hot"}), &[("OPENAI_API_KEY", "k")], Some("openai/x"))).unwrap().prepare().is_err()
        );
        // Provider alias canonicalized.
        let g = TraeAgent::new(args(json!({}), &[("GEMINI_API_KEY", "gemini-key")], Some("google/gemini-2.5-pro"))).unwrap();
        assert_eq!(g.connection().env, BTreeMap::from([("GOOGLE_API_KEY".to_string(), "gemini-key".to_string())]));
    }

    #[test]
    fn usage_and_version() {
        let inter = |i: i64, o: i64, c: i64| json!({"timestamp": "2026-01-01T00:00:00Z", "provider": "anthropic", "response": {"content": "", "usage": {"input_tokens": i, "output_tokens": o, "cache_creation_input_tokens": 0, "cache_read_input_tokens": c, "reasoning_tokens": 0}, "tool_calls": null}});
        let raw = json!({"model": "m", "llm_interactions": [inter(100, 50, 0), inter(200, 25, 10), inter(300, 25, 5)], "agent_steps": []});
        assert_eq!(trajectory_usage(&raw), Some((600, 100, 15)));
        assert_eq!(trajectory_usage(&json!({"llm_interactions": [], "agent_steps": []})), None);
        let err = json!({"llm_interactions": [], "agent_steps": [{"step_number": 1, "error": "Expecting ',' delimiter"}]});
        assert_eq!(trajectory_usage(&err), Some((0, 0, 0)));

        let a = TraeAgent::new(args(json!({}), &[], Some("anthropic/x"))).unwrap();
        let dir = a.args.logs_dir.clone();
        let mut ctx = AgentContext::default();
        a.collect_usage(&dir, &mut ctx);
        assert!(ctx.n_input_tokens.is_none());
        std::fs::write(dir.join(TRAJECTORY_FILE), "{not json").unwrap();
        a.collect_usage(&dir, &mut ctx);
        assert!(ctx.n_input_tokens.is_none());
        std::fs::write(dir.join(TRAJECTORY_FILE), json!({"llm_interactions": [inter(1000, 200, 50)]}).to_string()).unwrap();
        a.collect_usage(&dir, &mut ctx);
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens, ctx.n_cache_tokens), (Some(1000), Some(200), Some(50)));

        assert_eq!(trajectory_usage(&json!({"llm_interactions": [{"timestamp": "bad"}]})), None);
        assert!(iso_timestamp_ok(&json!("2026-03-31T16:11:59.203754")) && iso_timestamp_ok(&json!("2026-01-01T00:00:00Z")));
        assert_eq!(parse_trae_version("trae-cli, version 0.1.0\n"), "version 0.1.0");
        assert_eq!(parse_trae_version("\n0.2.1, build x"), "0.2.1");
    }
}
