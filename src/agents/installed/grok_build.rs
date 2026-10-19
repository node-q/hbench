//! grok-build.

use super::common::{deep_merge, installed_dir, parse_bool_env_value, skills_dir};
use super::pins;
use super::{ensure_system_dependencies, exec_agent, exec_error};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx, pricing};
use crate::env::{kind_err, quote};
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::OnceLock;

const OUTPUT: &str = "grok-build.txt";
const PATH_EXPORT: &str = "export PATH=\"$HOME/.grok/bin:$PATH\"";
const POLL: u64 = 5;
const INSURANCE: i64 = 60;
const CHILD_TERM_GRACE: u64 = 10;
const DEFAULT_BG_WAIT: i64 = 600;
const USD_TICKS_PER_DOLLAR: f64 = 10_000_000_000.0;
const REASONING: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Non-xAI providers: (base_url, env_key, api_backend).
fn provider(p: &str) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match p {
        "openrouter" => ("https://openrouter.ai/api/v1", "OPENROUTER_API_KEY", "chat_completions"),
        "openai" => ("https://api.openai.com/v1", "OPENAI_API_KEY", "responses"),
        "anthropic" => ("https://api.anthropic.com/v1", "ANTHROPIC_API_KEY", "messages"),
        _ => return None,
    })
}

pub struct GrokBuild {
    args: AgentArgs,
    version: Option<String>,
    session_id: String,
    provider: Option<String>,
    model_slug: Option<String>,
    flags: String,
    grok_config: Option<Map<String, Value>>,
    disable_web_search: bool,
    background_wait_sec: Option<i64>,
    kill_leftover_processes: bool,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(GrokBuild::new(args)?))
}

/// Parse a boolean env value.
fn parse_bool(v: &Value, name: &str) -> Result<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::String(s) => parse_bool_env_value(s, name),
        _ => bail!("Invalid value for '{name}': expected a boolean"),
    }
}

impl GrokBuild {
    fn new(args: AgentArgs) -> Result<GrokBuild> {
        let grok_config = match args.kwargs.get("grok_config") {
            None | Some(Value::Null) => None,
            Some(Value::Object(o)) => Some(o.clone()),
            Some(_) => bail!("Invalid grok_config: expected an object of config.toml sections"),
        };
        let disable_web_search =
            args.kwargs.get("disable_web_search").map(|v| parse_bool(v, "disable_web_search")).transpose()?.unwrap_or(true);
        let kill_leftover_processes =
            args.kwargs.get("kill_leftover_processes").map(|v| parse_bool(v, "kill_leftover_processes")).transpose()?.unwrap_or(true);
        let background_wait_sec = match args.kwargs.get("background_wait_sec") {
            None => Some(DEFAULT_BG_WAIT),
            Some(Value::Null) => None,
            Some(v) => {
                let n = match v {
                    Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
                    Value::String(s) => s.trim().parse().ok(),
                    Value::Bool(b) => Some(*b as i64),
                    _ => None,
                };
                let n = n.ok_or_else(|| anyhow!("Invalid background_wait_sec: expected integer seconds or null, got {v}"))?;
                if n < 1 {
                    bail!("Invalid background_wait_sec: must be >= 1 or null, got {v}");
                }
                Some(n)
            }
        };
        let flags = cli_flags(&args)?;
        let (provider, model_slug) = match args.model_name.as_deref() {
            None | Some("") => (None, None),
            Some(m) => {
                let (p, rest) = m.split_once('/').unwrap_or((m, ""));
                if provider(p).is_some() && !rest.is_empty() {
                    (Some(p.to_string()), Some(rest.to_string()))
                } else {
                    (None, Some(if rest.is_empty() { p } else { rest }.to_string()))
                }
            }
        };
        Ok(GrokBuild {
            version: args.version_kw(),
            args,
            session_id: uuid::Uuid::new_v4().to_string(),
            provider,
            model_slug,
            flags,
            grok_config,
            disable_web_search,
            background_wait_sec,
            kill_leftover_processes,
        })
    }

    fn config_toml(&self) -> Result<String> {
        let mut config = json!({
            "cli": {"auto_update": false},
            "features": {"telemetry": false},
            "harness": {"disable_codebase_upload": true},
        });
        if self.disable_web_search {
            config["disable_web_search"] = json!(true);
        }
        if let (Some(p), Some(slug)) = (&self.provider, &self.model_slug) {
            let (base_url, env_key, api_backend) = provider(p).unwrap();
            config["models"] = json!({"default": slug, "session_summary": slug, "image_description": slug, "web_search": slug});
            config["model"] =
                json!({slug: {"name": slug, "model": slug, "base_url": base_url, "env_key": env_key, "api_backend": api_backend}});
        }
        if !self.args.mcp_servers.is_empty() {
            let mut servers = Map::new();
            for s in &self.args.mcp_servers {
                let v = if s.transport() == "stdio" { json!({"command": s.command, "args": s.args}) } else { json!({"url": s.url}) };
                servers.insert(s.name.clone(), v);
            }
            config["mcp_servers"] = Value::Object(servers);
        }
        if let Some(o) = &self.grok_config {
            deep_merge(&mut config, &Value::Object(o.clone()));
        }
        Ok(toml::to_string(&config)?)
    }

    fn watchdog_grace_sec(&self) -> i64 {
        self.background_wait_sec.unwrap_or(DEFAULT_BG_WAIT) + INSURANCE
    }

    fn run_script(&self, escaped_instruction: &str) -> String {
        let mut parts = vec![
            "grok --no-auto-update".to_string(),
            format!("--single {escaped_instruction}"),
            "--always-approve".into(),
            "--output-format streaming-json".into(),
            format!("--session-id {}", self.session_id),
        ];
        if let Some(m) = &self.model_slug {
            parts.push(format!("--model {}", quote(m)));
        }
        if let Some(w) = self.background_wait_sec {
            parts.push(format!("--background-wait-timeout {w}"));
        }
        if !self.flags.is_empty() {
            parts.push(self.flags.clone());
        }
        let grok_command = parts.join(" ");
        let output_path = format!("/logs/agent/{OUTPUT}");
        let watchdog_log = "/logs/agent/grok-build-watchdog.log";
        let sid = &self.session_id;
        let pid_file = format!("/tmp/.grok-build-{sid}.pid");
        let exit_file = format!("/tmp/.grok-build-{sid}.exit");
        let snapshot_file = format!("/tmp/.grok-build-{sid}.children");
        let events_glob = format!("\"$HOME\"/.grok/sessions/*/{sid}/events.jsonl");
        let grace = self.watchdog_grace_sec();

        let mut watchdog = String::new();
        let mut sweep = String::new();
        if self.kill_leftover_processes {
            watchdog = format!(
                r#"
{{
  # Grok spawns background tool tasks as setsid'd session leaders, so killing
  # each child's process group also reaps grandchildren (npm -> node etc.).
  # Children sharing the script's own group (never grok's task style) get a
  # plain kill so grok and this script are never signalled.
  OWN_PGID=$(ps -o pgid= -p $$ 2>/dev/null | tr -d ' ')
  kill_pids() {{
    SIG=$1; shift
    for PID in "$@"; do
      PGID=$(ps -o pgid= -p "$PID" 2>/dev/null | tr -d ' ')
      if [ -n "$PGID" ] && [ "$PGID" != "$OWN_PGID" ]; then
        kill -"$SIG" -- "-$PGID" 2>/dev/null || kill -"$SIG" "$PID" 2>/dev/null
      else
        kill -"$SIG" "$PID" 2>/dev/null
      fi
    done
  }}
  # Sleep in poll-sized slices so we react promptly once grok exits.
  wait_for_exit() {{
    REMAIN=$1
    while [ "$REMAIN" -gt 0 ]; do
      [ -f {exit_file} ] && return 0
      sleep {POLL}
      REMAIN=$((REMAIN - {POLL}))
    done
    [ -f {exit_file} ]
  }}
  while [ ! -f {exit_file} ]; do
    GROK_PID=$(cat {pid_file} 2>/dev/null)
    # Snapshot live children so orphans can be swept after grok exits.
    [ -n "$GROK_PID" ] && ps -o pid= --ppid "$GROK_PID" 2>/dev/null > {snapshot_file}
    sleep {POLL}
    grep -qs '"type":"turn_ended"' {events_glob} || continue
    wait_for_exit {grace} && break
    [ -n "$GROK_PID" ] || continue
    CHILDREN=$(ps -o pid= --ppid "$GROK_PID" 2>/dev/null)
    [ -n "$CHILDREN" ] || continue
    echo "$(date -u +%FT%TZ) grok-build watchdog: grok still running \
{grace}s after turn end; terminating leftover child \
process groups: $CHILDREN" | tee -a {watchdog_log} >&2
    kill_pids TERM $CHILDREN
    wait_for_exit {CHILD_TERM_GRACE} && break
    CHILDREN=$(ps -o pid= --ppid "$GROK_PID" 2>/dev/null)
    [ -n "$CHILDREN" ] && kill_pids KILL $CHILDREN
  done
}} &
"#
            );
            sweep = format!(
                r#"
# Pre-reap grok versions exit their background wait leaving tasks orphaned
# (reparented to init); sweep the last child snapshot so nothing outlives the
# agent phase or holds ports during verification.
if [ -s {snapshot_file} ]; then
  SWEPT=""
  for PID in $(cat {snapshot_file}); do
    kill -0 "$PID" 2>/dev/null && SWEPT="$SWEPT $PID"
  done
  if [ -n "$SWEPT" ]; then
    echo "$(date -u +%FT%TZ) grok-build watchdog: sweeping orphaned \
processes:$SWEPT" | tee -a {watchdog_log} >&2
    OWN_PGID=$(ps -o pgid= -p $$ 2>/dev/null | tr -d ' ')
    for PID in $SWEPT; do
      PGID=$(ps -o pgid= -p "$PID" 2>/dev/null | tr -d ' ')
      if [ -n "$PGID" ] && [ "$PGID" != "$OWN_PGID" ]; then
        kill -TERM -- "-$PGID" 2>/dev/null || kill -TERM "$PID" 2>/dev/null
      else
        kill -TERM "$PID" 2>/dev/null
      fi
    done
  fi
fi
"#
            );
        }
        format!(
            r#"{PATH_EXPORT}
rm -f {pid_file} {exit_file} {snapshot_file}

{{
  {grok_command} </dev/null &
  GROK_PID=$!
  echo "$GROK_PID" > {pid_file}
  wait "$GROK_PID"
  echo $? > {exit_file}
}} 2>&1 | stdbuf -oL tee {output_path} &
{watchdog}
# Key on grok's own exit signal, not pipeline completion: an orphan holding
# the stdout pipe would otherwise stall tee (and this script) until it dies.
while [ ! -f {exit_file} ]; do
  sleep {POLL}
done
{sweep}
# Orphans are gone, so tee gets EOF promptly and the watchdog sees the exit
# file; both finish here.
wait
STATUS=$(cat {exit_file} 2>/dev/null)
rm -f {pid_file} {exit_file} {snapshot_file}
exit "${{STATUS:-1}}"
"#
        )
    }

    fn run_env(&self) -> Result<BTreeMap<String, String>> {
        let key_name = self.provider.as_deref().and_then(provider).map(|p| p.1).unwrap_or("XAI_API_KEY");
        let Some(key) = self.args.get_env(key_name) else {
            bail!(
                "{key_name} environment variable is required for model '{}'. Set it on the host or via --ae {key_name}=...",
                self.args.model_name.as_deref().unwrap_or("None")
            );
        };
        Ok(BTreeMap::from([
            (key_name.to_string(), key),
            ("GROK_DISABLE_AUTOUPDATER".into(), "1".into()),
            ("GROK_LOG_FILE".into(), "/logs/agent/grok-build-cli.log".into()),
            ("RUST_LOG".into(), self.args.get_env("RUST_LOG").unwrap_or_else(|| "warn".into())),
        ]))
    }
}

/// grok-build options as CLI flags (with env fallbacks).
fn cli_flags(args: &AgentArgs) -> Result<String> {
    let opt = |k: &str, env: &str| if args.kwargs.contains_key(k) { args.kw(k) } else { args.get_env(env) };
    let mut f = vec![];
    if let Some(v) = opt("max_turns", "GROK_BUILD_MAX_TURNS") {
        let n: i64 = v.trim().parse().map_err(|_| anyhow!("Invalid value for 'max_turns': cannot parse '{v}' as an integer"))?;
        f.push(format!("--max-turns {n}"));
    }
    if let Some(v) = opt("reasoning_effort", "GROK_BUILD_REASONING_EFFORT") {
        if !REASONING.contains(&v.as_str()) {
            bail!("Invalid value for 'reasoning_effort': '{v}'");
        }
        f.push(format!("--reasoning-effort {v}"));
    }
    Ok(f.join(" "))
}

/// Prepends `resource-exhausted|currently at capacity` (ApiOverloadedError) to ERROR_PATTERNS.
fn classify_grok(output: &str) -> &'static str {
    static P: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let p = P.get_or_init(|| vec![(Regex::new(r"(?i)resource-exhausted|currently at capacity").unwrap(), "ApiOverloadedError")]);
    super::classify_ordered(output, p, &[])
}

/// grok usage buckets: (input, output, cache_read, cache_creation, reasoning). None if malformed.
type Usage = [i64; 5];

fn parse_usage(v: &Value) -> Option<Usage> {
    let o = v.as_object()?;
    let mut u = [0i64; 5];
    for (i, k) in
        ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens", "reasoning_tokens"].iter().enumerate()
    {
        if let Some(x) = o.get(*k) {
            let n = x.as_i64().or_else(|| x.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))?;
            if n < 0 {
                return None;
            }
            u[i] = n;
        }
    }
    Some(u)
}

/// Total cost: price-table cost per call (so each is tiered by its own prompt size),
/// with xAI's at-boundary tiering and cache-write-at-input-rate adjustments. None if unpriced.
fn total_cost_usd(model: &str, calls: &[Usage]) -> Option<f64> {
    let key = pricing::pricing_key(model)?;
    let p = pricing::model_cost(&key)?;
    let xai = p.get("litellm_provider").and_then(Value::as_str) == Some("xai");
    let tier_re = Regex::new(r"^input_cost_per_token_above_(\d+)k_tokens$").unwrap();
    let mut total = 0.0;
    for u in calls {
        let prompt = u[0] + u[2] + u[3];
        // Input tier: the highest `above_Nk` threshold the prompt reaches.
        let (mut rate_key, mut threshold) = ("input_cost_per_token", None::<i64>);
        for (k, v) in p {
            let Some(n) = tier_re.captures(k).and_then(|c| c[1].parse::<i64>().ok()).filter(|_| v.is_number()) else { continue };
            if prompt >= n * 1000 && threshold.is_none_or(|t| n * 1000 > t) {
                (rate_key, threshold) = (k.as_str(), Some(n * 1000));
            }
        }
        let boundary_rate = p.get(rate_key).and_then(Value::as_f64);
        // The price table tiers strictly above the boundary, xAI at it: price one synthetic token, then remove it.
        let at_boundary = threshold == Some(prompt) && xai && boundary_rate.is_some();
        let (mut input, output) = pricing::cost_per_token(&key, prompt + at_boundary as i64, u[1], u[3], u[2])?;
        if at_boundary {
            input -= boundary_rate.unwrap();
        }
        if xai && p.get("cache_creation_input_token_cost").is_none_or(Value::is_null) {
            let rate = boundary_rate.filter(|r| *r != 0.0).or_else(|| p.get("input_cost_per_token").and_then(Value::as_f64))?;
            input += u[3] as f64 * rate;
        }
        total += input + output;
    }
    Some(total)
}

/// Usage report parsing, incl. the price-table estimate when grok reports no cost.
pub fn usage_from_output(text: &str, model: Option<&str>) -> Option<AgentContext> {
    let mut calls: Vec<Usage> = vec![];
    let mut total: Option<Usage> = None;
    let mut cost: Option<f64> = None;
    let mut can_estimate = true;
    for line in text.lines().map(str::trim).filter(|l| l.starts_with('{')) {
        let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
        let t = e["type"].as_str().unwrap_or("");
        if !["usage", "end", "error"].contains(&t) {
            continue;
        }
        let usage = e.get("usage").filter(|u| u.is_object()).and_then(parse_usage);
        if t == "usage" {
            calls.extend(usage);
            continue;
        }
        total = usage;
        cost = None;
        can_estimate = !(e["usage_is_incomplete"] == true || e["cost_is_partial"] == true);
        if e["cost_is_partial"] != true {
            if let Some(ticks) = e["total_cost_usd_ticks"].as_i64().filter(|t| *t >= 0) {
                cost = Some(ticks as f64 / USD_TICKS_PER_DOLLAR);
            } else if let Some(c) = e["total_cost_usd"].as_f64().filter(|c| c.is_finite() && *c >= 0.0) {
                cost = Some(c);
            }
        }
    }
    let summed = (!calls.is_empty()).then(|| {
        calls.iter().fold([0i64; 5], |mut a, u| {
            for i in 0..5 {
                a[i] += u[i];
            }
            a
        })
    });
    let u = total.or(summed);
    if u.is_none() && cost.is_none() {
        return None;
    }
    let mut ctx = AgentContext::default();
    if let Some(u) = u {
        ctx.n_input_tokens = Some(u[0] + u[2] + u[3]);
        ctx.n_output_tokens = Some(u[1]);
        ctx.n_cache_tokens = Some(u[2]);
    }
    ctx.cost_usd = cost.or_else(|| {
        let model = model.filter(|_| can_estimate && summed.is_some() && total.is_none_or(|t| Some(t) == summed))?;
        total_cost_usd(model, &calls)
    });
    Some(ctx)
}

#[async_trait::async_trait]
impl Agent for GrokBuild {
    fn name(&self) -> &str {
        "grok-build"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "bash", "ca_certificates", "coreutils", "procps"]).await?;
        let version_arg = match pins::GROK_BUILD.or(self.version.as_deref()) {
            "latest" => String::new(),
            v => format!(" -s {}", quote(v)),
        };
        let none = BTreeMap::new();
        exec_agent(
            cx,
            &format!("set -euo pipefail; curl -fsSL https://x.ai/cli/install.sh | bash{version_arg} && {PATH_EXPORT} && grok --version"),
            &none,
        )
        .await?;
        if let Some(d) = skills_dir(&self.args) {
            let d = quote(&d);
            exec_agent(
                cx,
                &format!("if [ -d {d} ]; then mkdir -p ~/.grok/skills && cp -r {d}/* ~/.grok/skills/ 2>/dev/null || true; fi"),
                &none,
            )
            .await?;
        }
        if self.version.is_none() {
            let r = cx.exec(&format!("{PATH_EXPORT}; grok --version"), &none, None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                static RE: OnceLock<Regex> = OnceLock::new();
                let re = RE.get_or_init(|| Regex::new(r"(\d+\.\d+\.\d+(?:-[\w.]+)?)").unwrap());
                self.version = Some(re.captures(&r.stdout).map(|c| c[1].to_string()).unwrap_or_else(|| r.stdout.trim().to_string()));
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let env = self.run_env()?;
        exec_agent(cx, &format!("mkdir -p ~/.grok && printf '%s' {} > ~/.grok/config.toml", quote(&self.config_toml()?)), &BTreeMap::new())
            .await?;
        let cmd = self.run_script(&quote(&instruction));
        let r = cx.exec(&format!("set -o pipefail; {cmd}"), &env, None).await;
        // Copy session data (best effort) even on failure.
        let _ = cx
            .exec(
                "set -o pipefail; mkdir -p /logs/agent\nif [ -d \"$HOME/.grok/sessions\" ]; then\n  rm -rf /logs/agent/sessions\n  cp -R \"$HOME/.grok/sessions\" /logs/agent/sessions\nfi",
                &BTreeMap::new(),
                None,
            )
            .await;
        let r = r?;
        if r.code != 0 {
            let e = exec_error(&cmd, &r, None);
            let kind = classify_grok(&format!("{}\n{}", r.stdout, r.stderr));
            return Err(kind_err(kind, e.to_string()));
        }
        Ok(())
    }

    fn collect_usage(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        if let Some(c) =
            std::fs::read_to_string(logs_dir.join(OUTPUT)).ok().and_then(|t| usage_from_output(&t, self.args.model_name.as_deref()))
        {
            *ctx = c;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<GrokBuild> {
        GrokBuild::new(common::test_args(kwargs, env, model))
    }

    #[test]
    fn options_and_flags() {
        let a = agent(json!({"max_turns": 7, "reasoning_effort": "high"}), &[("XAI_API_KEY", "xk")], Some("xai/grok-4")).unwrap();
        assert_eq!(a.flags, "--max-turns 7 --reasoning-effort high");
        assert_eq!(a.model_slug.as_deref(), Some("grok-4"));
        assert!(a.provider.is_none());
        let s = a.run_script("'do it'");
        assert!(s.contains("grok --no-auto-update --single 'do it' --always-approve --output-format streaming-json"));
        assert!(s.contains("--model grok-4 --background-wait-timeout 600 --max-turns 7 --reasoning-effort high </dev/null &"));
        assert!(s.contains("wait_for_exit 660 && break"));
        assert_eq!(a.run_env().unwrap()["XAI_API_KEY"], "xk");
        assert_eq!(a.run_env().unwrap()["RUST_LOG"], "warn");

        let b = agent(
            json!({"background_wait_sec": null, "kill_leftover_processes": "false"}),
            &[("GROK_BUILD_MAX_TURNS", "3")],
            Some("grok-4"),
        )
        .unwrap();
        assert_eq!(b.flags, "--max-turns 3");
        let s = b.run_script("x");
        assert!(!s.contains("--background-wait-timeout") && !s.contains("kill_pids"));
        assert!(b.run_env().unwrap_err().to_string().contains("XAI_API_KEY environment variable is required"));

        assert!(agent(json!({"reasoning_effort": "huge"}), &[], None).is_err());
        assert!(agent(json!({"background_wait_sec": 0}), &[], None).is_err());
        assert!(agent(json!({"disable_web_search": "maybe"}), &[], None).is_err());
        assert!(agent(json!({"grok_config": "x"}), &[], None).is_err());
    }

    #[test]
    fn provider_routing_config() {
        let a = agent(
            json!({"grok_config": {"cli": {"extra": 1}, "disable_web_search": false}}),
            &[("OPENROUTER_API_KEY", "ok")],
            Some("openrouter/openai/gpt-4o-mini"),
        )
        .unwrap();
        assert_eq!((a.provider.as_deref(), a.model_slug.as_deref()), (Some("openrouter"), Some("openai/gpt-4o-mini")));
        assert_eq!(a.run_env().unwrap()["OPENROUTER_API_KEY"], "ok");
        let t: toml::Value = toml::from_str(&a.config_toml().unwrap()).unwrap();
        assert_eq!(t["cli"]["auto_update"].as_bool(), Some(false));
        assert_eq!(t["cli"]["extra"].as_integer(), Some(1));
        assert_eq!(t["disable_web_search"].as_bool(), Some(false));
        assert_eq!(t["models"]["default"].as_str(), Some("openai/gpt-4o-mini"));
        assert_eq!(t["model"]["openai/gpt-4o-mini"]["api_backend"].as_str(), Some("chat_completions"));
        assert_eq!(t["harness"]["disable_codebase_upload"].as_bool(), Some(true));
        let b = agent(json!({}), &[], Some("myorg/team/model")).unwrap();
        assert_eq!((b.provider.as_deref(), b.model_slug.as_deref()), (None, Some("team/model")));
    }

    #[test]
    fn usage_parsing() {
        let rec = |u: Value| json!({"type": "usage", "usage": u}).to_string();
        let out = [
            json!({"type": "assistant", "content": "hi"}).to_string(),
            rec(json!({"input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 10, "cache_creation_input_tokens": 5, "reasoning_tokens": 3})),
            json!({"type": "end", "stopReason": "end_turn", "usage": {"input_tokens": 200, "output_tokens": 40, "cache_read_input_tokens": 20, "cache_creation_input_tokens": 10, "reasoning_tokens": 6}, "total_cost_usd": 0.0}).to_string(),
        ]
        .join("\n");
        let c = usage_from_output(&out, None).unwrap();
        assert_eq!((c.n_input_tokens, c.n_output_tokens, c.n_cache_tokens, c.cost_usd), (Some(230), Some(40), Some(20), Some(0.0)));

        let ticks = [rec(json!({"input_tokens": 100, "output_tokens": 20})), json!({"type": "end", "usage": {"input_tokens": 100, "output_tokens": 20}, "total_cost_usd": 9.99, "total_cost_usd_ticks": 261_300_000}).to_string()].join("\n");
        assert_eq!(usage_from_output(&ticks, None).unwrap().cost_usd, Some(0.02613));
        let partial = [
            rec(json!({"input_tokens": 100})),
            json!({"type": "end", "usage": {"input_tokens": 100}, "cost_is_partial": true, "total_cost_usd": 9.99}).to_string(),
        ]
        .join("\n");
        assert_eq!(usage_from_output(&partial, None).unwrap().cost_usd, None);

        // Malformed records are skipped; no terminal event -> per-call sum.
        let calls = [
            rec(json!({"input_tokens": null})),
            rec(json!({"output_tokens": 12.5})),
            rec(json!({"input_tokens": -1})),
            rec(json!({"input_tokens": 100, "output_tokens": 10})),
        ]
        .join("\n");
        let c = usage_from_output(&calls, None).unwrap();
        assert_eq!((c.n_input_tokens, c.n_output_tokens, c.cost_usd), (Some(100), Some(10), None));
        assert!(usage_from_output("", None).is_none());
    }

    #[test]
    fn litellm_estimate() {
        // xai/grok-4-0709: in 3e-6 (6e-6 above 128k), out 1.5e-5, no cache rates; cache writes billed at input rate.
        let rec = |u: Value| json!({"type": "usage", "usage": u}).to_string();
        let a = json!({"input_tokens": 1000, "output_tokens": 100, "cache_read_input_tokens": 200, "cache_creation_input_tokens": 50});
        let b = json!({"input_tokens": 128_000});
        let want = 1000.0 * 3e-6 + 100.0 * 1.5e-5 + 50.0 * 3e-6 + 128_000.0 * 6e-6; // second call tiers at the xAI boundary
        let end = |u: Value| json!({"type": "end", "usage": u}).to_string();
        let out = [
            rec(a.clone()),
            rec(b.clone()),
            end(json!({"input_tokens": 129_000, "output_tokens": 100, "cache_read_input_tokens": 200, "cache_creation_input_tokens": 50})),
        ]
        .join("\n");
        let c = usage_from_output(&out, Some("xai/grok-4-0709")).unwrap().cost_usd.unwrap();
        assert!((c - want).abs() < 1e-9, "{c} vs {want}");
        // Terminal total differs from the per-call sum (subagents): no estimate. Unpriced model: None.
        let sub = [rec(a.clone()), end(json!({"input_tokens": 5000}))].join("\n");
        assert_eq!(usage_from_output(&sub, Some("xai/grok-4-0709")).unwrap().cost_usd, None);
        assert_eq!(usage_from_output(&rec(a.clone()), Some("xai/not-a-model")).unwrap().cost_usd, None);
        let incomplete = [rec(a), json!({"type": "end", "usage_is_incomplete": true}).to_string()].join("\n");
        assert_eq!(usage_from_output(&incomplete, Some("xai/grok-4-0709")).unwrap().cost_usd, None);
    }

    #[test]
    fn error_classification() {
        assert_eq!(classify_grok("rate limit\n{\"code\": \"resource-exhausted\"}"), "ApiOverloadedError");
        assert_eq!(classify_grok("resource-exhausted\nInvalid API key"), "AgentAuthenticationError");
        assert_eq!(classify_grok("boom"), "NonZeroAgentExitCodeError");
    }
}
