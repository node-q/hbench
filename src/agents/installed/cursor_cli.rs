//! Cursor CLI (port of upstream's agents/installed/cursor_cli.py).

use super::aider::opts;
use super::{ensure_system_dependencies, exec_agent};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Per-million-token USD rates (input, output, cache_read, cache_write) from cursor.com/docs/models-and-pricing.
fn builtin_pricing(slug: &str) -> Option<[f64; 4]> {
    Some(match slug {
        "composer-2.5" | "composer-2" => [0.5, 2.5, 0.2, 0.5],
        "composer-2-fast" => [3.0, 15.0, 0.6, 3.0],
        "composer-1.5" => [3.5, 17.5, 0.35, 3.5],
        "composer-1" => [1.25, 10.0, 0.125, 1.25],
        "auto" => [1.25, 6.0, 0.25, 1.25],
        "grok-4.5" => [2.0, 6.0, 0.5, 2.0],
        _ => return None,
    })
}

/// LiteLLM `model_cost` rates for `model` (then without its `provider/` prefix), per million tokens.
fn litellm_rates(model: &str) -> Option<[f64; 4]> {
    let p = crate::agents::pricing::model_cost(model).or_else(|| crate::agents::pricing::model_cost(model.split_once('/').map_or(model, |(_, m)| m)))?;
    let rate = |k: &str| p.get(k).and_then(Value::as_f64);
    let input = rate("input_cost_per_token").unwrap_or(0.0);
    let output = rate("output_cost_per_token").unwrap_or(0.0);
    Some([input, output, rate("cache_read_input_token_cost").unwrap_or(input), rate("cache_creation_input_token_cost").unwrap_or(input)].map(|r| r * 1_000_000.0))
}

fn model_slug(model: &str) -> String {
    let s = model.split_once('/').map_or(model, |(_, m)| m).to_lowercase();
    match s.as_str() {
        "composer-2-5" | "composer2.5" => "composer-2.5".into(),
        "composer2" => "composer-2".into(),
        "composer-2-fast-mode" => "composer-2-fast".into(),
        "grok-4-5" | "grok4.5" => "grok-4.5".into(),
        _ => s,
    }
}

/// The `pricing` kwarg: flat per-million rates, or a model-slug -> rates map.
#[derive(Debug, Clone, PartialEq)]
enum Pricing {
    Flat([f64; 4]),
    Map(BTreeMap<String, [f64; 4]>),
}

fn parse_rates(v: &Value) -> Result<[f64; 4]> {
    let Some(o) = v.as_object() else { bail!("expected an object of rates") };
    if let Some(k) = o.keys().find(|k| !["input", "output", "cache_read", "cache_write"].contains(&k.as_str())) {
        bail!("unexpected rate key '{k}'");
    }
    let rate = |k: &str, required: bool| -> Result<Option<f64>> {
        match &o.get(k) {
            None | Some(Value::Null) if !required => Ok(None),
            Some(Value::Number(n)) if n.as_f64().is_some_and(|f| f >= 0.0) => Ok(n.as_f64()),
            _ => bail!("'{k}' must be a number >= 0"),
        }
    };
    let input = rate("input", true)?.unwrap();
    Ok([input, rate("output", true)?.unwrap(), rate("cache_read", false)?.unwrap_or(input), rate("cache_write", false)?.unwrap_or(input)])
}

fn parse_pricing(v: Option<&Value>) -> Result<Option<Pricing>> {
    let Some(v) = v.filter(|v| !v.is_null()) else { return Ok(None) };
    let Some(o) = v.as_object() else { bail!("Invalid pricing: expected per-million USD rates or a model → rates map") };
    // Any known rate key means flat rates, so `{"input": 1}` fails as incomplete instead of a model map.
    if o.keys().any(|k| ["input", "output", "cache_read", "cache_write"].contains(&k.as_str())) {
        return parse_rates(v)
            .map(|r| Some(Pricing::Flat(r)))
            .map_err(|e| anyhow::anyhow!("Invalid pricing: expected per-million USD rates with 'input' and 'output' (optional cache_read/cache_write). {e}"));
    }
    if o.is_empty() {
        bail!("Invalid pricing: expected a non-empty model → rates map");
    }
    let mut m = BTreeMap::new();
    for (k, r) in o {
        let r = parse_rates(r).map_err(|e| anyhow::anyhow!("Invalid pricing: expected per-million USD rates ({{'input': ..., 'output': ..., ...}}) or a model → rates map. {k}: {e}"))?;
        m.insert(model_slug(k), r);
    }
    Ok(Some(Pricing::Map(m)))
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(CursorCli::new(args)?))
}

pub struct CursorCli {
    args: AgentArgs,
    version: Option<String>,
    pricing: Option<Pricing>,
}

impl CursorCli {
    fn new(args: AgentArgs) -> Result<CursorCli> {
        opts::check_known(&args, "cursor-cli", &["mode", "pricing", "reasoning_effort"], false)?;
        let pricing = parse_pricing(args.kwargs.get("pricing"))?;
        let a = CursorCli { version: args.version_kw(), args, pricing };
        a.cli_flags()?;
        a.effort()?;
        Ok(a)
    }

    fn cli_flags(&self) -> Result<String> {
        Ok(opts::choice(&self.args, "mode", None, &["plan", "ask"])?.map(|m| format!("--mode {}", quote(&m))).unwrap_or_default())
    }

    fn effort(&self) -> Result<Option<String>> {
        opts::choice(&self.args, "reasoning_effort", None, EFFORTS)
    }

    /// `--model` value; effort goes in Cursor's parameterized form `base[k=v,...,effort=X]`.
    fn model_arg(&self) -> Result<String> {
        let Some((_, model)) = self.args.model_name.as_deref().and_then(|m| m.split_once('/')) else {
            bail!("Model name must be in the format provider/model_name")
        };
        let Some(effort) = self.effort()? else { return Ok(model.to_string()) };
        let re = regex::Regex::new(r"^(?s)(?P<base>.+)\[(?P<params>[^\[\]]*)\]$").unwrap();
        let Some(c) = re.captures(model) else { return Ok(format!("{model}[effort={effort}]")) };
        let params = &c["params"];
        if regex::Regex::new(r"(?:^|,)\s*effort=").unwrap().is_match(params) {
            bail!("Model '{model}' already sets effort=...; omit reasoning_effort or remove the effort override from the model name");
        }
        let merged = if params.trim().is_empty() { format!("effort={effort}") } else { format!("{params},effort={effort}") };
        Ok(format!("{}[{merged}]", &c["base"]))
    }

    /// Per-million rates: user `pricing` override, then built-in Cursor rates, then LiteLLM.
    fn rates(&self) -> Option<[f64; 4]> {
        let model = self.args.model_name.as_deref()?;
        let user = match &self.pricing {
            Some(Pricing::Flat(r)) => Some(*r),
            Some(Pricing::Map(m)) => m.get(&model_slug(model)).copied(),
            None => None,
        };
        user.or_else(|| builtin_pricing(&model_slug(model))).or_else(|| litellm_rates(model))
    }

    fn mcp_command(&self) -> Option<String> {
        if self.args.mcp_servers.is_empty() {
            return None;
        }
        let mut servers = serde_json::Map::new();
        for s in &self.args.mcp_servers {
            let v = if s.transport() == "stdio" { json!({"command": s.command, "args": s.args}) } else { json!({"url": s.url}) };
            servers.insert(s.name.clone(), v);
        }
        let config = serde_json::to_string_pretty(&json!({"mcpServers": servers})).unwrap();
        Some(format!("mkdir -p ~/.cursor && echo {} > ~/.cursor/mcp.json", quote(&config)))
    }

    /// Usage totals from stream-json `result` events (one per session turn).
    fn parse_usage(&self, text: &str) -> Option<AgentContext> {
        let events: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l.trim()).ok()).collect();
        if events.is_empty() {
            return None;
        }
        let (mut tot, mut reported, mut any_usage) = ([0i64; 4], None::<f64>, false); // input, output, cache_read, cache_write
        for e in &events {
            let int = |v: &Value, k: &str| v[k].as_i64();
            // upstream validates result events strictly; malformed ones are skipped.
            let valid = e["type"] == "result"
                && e["subtype"].is_string()
                && int(e, "duration_ms").is_some()
                && int(e, "duration_api_ms").is_some()
                && e["is_error"].is_boolean()
                && e["result"].is_string()
                && e["session_id"].is_string();
            let u = &e["usage"];
            let usage = if u.is_null() {
                None
            } else {
                match ["inputTokens", "outputTokens", "cacheReadTokens", "cacheWriteTokens"].map(|k| int(u, k)) {
                    [Some(a), Some(b), Some(c), Some(d)] => Some([a, b, c, d]),
                    _ => continue,
                }
            };
            if !valid {
                continue;
            }
            if let Some(n) = usage {
                any_usage = true;
                (0..4).for_each(|i| tot[i] += n[i]);
                if let Some(c) = u["totalCost"].as_f64().or_else(|| u["cost"].as_f64()) {
                    reported = Some(reported.unwrap_or(0.0) + c);
                }
            }
        }
        let cost = reported.or_else(|| {
            let r = self.rates().filter(|_| any_usage)?;
            Some((tot[0] as f64 * r[0] + tot[2] as f64 * r[2] + tot[3] as f64 * r[3] + tot[1] as f64 * r[1]) / 1_000_000.0)
        });
        Some(AgentContext { n_input_tokens: Some(tot[0] + tot[2] + tot[3]), n_cache_tokens: Some(tot[2]), n_output_tokens: Some(tot[1]), cost_usd: cost, ..Default::default() })
    }
}

#[async_trait::async_trait]
impl Agent for CursorCli {
    fn name(&self) -> &str {
        "cursor-cli"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        opts::installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "bash"]).await?;
        let none = BTreeMap::new();
        exec_agent(cx, "set -euo pipefail; curl https://cursor.com/install -fsS | bash && export PATH=\"$HOME/.local/bin:$PATH\" && cursor-agent --version", &none).await?;
        if let Some(d) = opts::skills_dir(&self.args) {
            let d = quote(&d);
            exec_agent(cx, &format!("if [ -d {d} ]; then mkdir -p ~/.cursor/skills && cp -r {d}/* ~/.cursor/skills/ 2>/dev/null || true; fi"), &none).await?;
        }
        if self.version.is_none() {
            self.version = opts::detect_version(cx, "export PATH=\"$HOME/.local/bin:$PATH\"; cursor-agent --version", |s| s.trim().to_string()).await;
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let model = quote(&self.model_arg()?);
        // upstream reads the key from the host environment only.
        let Ok(key) = std::env::var("CURSOR_API_KEY") else { bail!("CURSOR_API_KEY environment variable is required. Please set your Cursor API key.") };
        let env = BTreeMap::from([("CURSOR_API_KEY".to_string(), key)]);
        if let Some(c) = self.mcp_command() {
            exec_agent(cx, &c, &env).await?;
        }
        let flags = self.cli_flags()?;
        let extra = if flags.is_empty() { String::new() } else { format!("{flags} ") };
        let cmd = format!(
            "export PATH=\"$HOME/.local/bin:$PATH\"; cursor-agent --yolo --print --output-format=stream-json {extra}--model={model} -- {} \
             2>&1 | stdbuf -oL tee /logs/agent/cursor-cli.txt",
            quote(&instruction)
        );
        exec_agent(cx, &cmd, &env).await?;
        Ok(())
    }

    fn populate_context_post_run(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        if let Some(c) = std::fs::read_to_string(logs_dir.join("cursor-cli.txt")).ok().and_then(|t| self.parse_usage(&t)) {
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

    fn agent(kwargs: Value, model: &str) -> Result<CursorCli> {
        CursorCli::new(opts::test_args(kwargs, &[], Some(model)))
    }

    #[test]
    fn model_effort_and_validation() {
        assert_eq!(agent(json!({"reasoning_effort": "high"}), "cursor/gpt-5").unwrap().model_arg().unwrap(), "gpt-5[effort=high]");
        assert_eq!(agent(json!({"reasoning_effort": "low"}), "cursor/gpt-5[context=1m]").unwrap().model_arg().unwrap(), "gpt-5[context=1m,effort=low]");
        assert_eq!(agent(json!({"reasoning_effort": "low"}), "cursor/gpt-5[]").unwrap().model_arg().unwrap(), "gpt-5[effort=low]");
        assert!(agent(json!({"reasoning_effort": "low"}), "cursor/gpt-5[effort=high]").unwrap().model_arg().is_err());
        assert!(agent(json!({"reasoning_effort": "extreme"}), "cursor/x").is_err());
        assert_eq!(agent(json!({"mode": "plan"}), "cursor/x").unwrap().cli_flags().unwrap(), "--mode plan");
        assert!(agent(json!({"mode": "yolo"}), "cursor/x").is_err());
        assert!(agent(json!({"pricing": {"input": 1}}), "cursor/x").is_err());
        assert!(agent(json!({"pricing": {}}), "cursor/x").is_err());
        assert!(agent(json!({"pricing": {"input": -1, "output": 1}}), "cursor/x").is_err());
        let p = agent(json!({"pricing": {"Grok4.5": {"input": 2, "output": 6}}}), "cursor/x").unwrap().pricing.unwrap();
        assert_eq!(p, Pricing::Map(BTreeMap::from([("grok-4.5".to_string(), [2.0, 6.0, 2.0, 2.0])])));
    }

    #[test]
    fn usage_and_cost() {
        let log = "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\"}\n\
            {\"type\":\"result\",\"subtype\":\"success\",\"duration_ms\":10,\"duration_api_ms\":8,\"is_error\":false,\"result\":\"ok\",\"session_id\":\"s\",\
            \"usage\":{\"inputTokens\":1000,\"outputTokens\":200,\"cacheReadTokens\":3000,\"cacheWriteTokens\":500}}\n\
            {\"type\":\"result\",\"subtype\":\"success\",\"duration_ms\":10,\"is_error\":false,\"result\":\"bad: no duration_api_ms\",\"session_id\":\"s\",\
            \"usage\":{\"inputTokens\":9,\"outputTokens\":9,\"cacheReadTokens\":9,\"cacheWriteTokens\":9}}\n";
        let c = agent(json!({}), "cursor/composer-2").unwrap().parse_usage(log).unwrap();
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens), (Some(4500), Some(3000), Some(200)));
        let want = (1000.0 * 0.5 + 3000.0 * 0.2 + 500.0 * 0.5 + 200.0 * 2.5) / 1e6;
        assert!((c.cost_usd.unwrap() - want).abs() < 1e-12);
        let flat = agent(json!({"pricing": {"input": 1, "output": 2}}), "cursor/unknown").unwrap().parse_usage(log).unwrap();
        assert!((flat.cost_usd.unwrap() - (4500.0 + 400.0) / 1e6).abs() < 1e-12);
        assert_eq!(agent(json!({}), "cursor/unknown").unwrap().parse_usage(log).unwrap().cost_usd, None);
        // LiteLLM fallback: gpt-5 in 1.25e-6, out 1e-5, cache read 1.25e-7, no cache write (-> input rate).
        let l = agent(json!({}), "cursor/gpt-5").unwrap().parse_usage(log).unwrap().cost_usd.unwrap();
        assert!((l - (1000.0 * 1.25e-6 + 3000.0 * 1.25e-7 + 500.0 * 1.25e-6 + 200.0 * 1e-5)).abs() < 1e-12);
        let reported = log.replace("\"cacheWriteTokens\":500}", "\"cacheWriteTokens\":500,\"totalCost\":0.42}");
        assert_eq!(agent(json!({}), "cursor/composer-2").unwrap().parse_usage(&reported).unwrap().cost_usd, Some(0.42));
    }
}
