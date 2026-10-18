//! Cortex Code.

use super::common::installed_dir;
use super::{ensure_system_dependencies, exec_agent, parse_version};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;

const OUTPUT: &str = "cortex-code.txt";
const PATH_EXPORT: &str = "export PATH=\"$HOME/.local/bin:$PATH\"";

pub struct CortexCode {
    args: AgentArgs,
    version: Option<String>,
    disallowed_tools: Vec<String>,
    allowed_tools: Vec<String>,
    cli_mode: Option<String>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(CortexCode::new(args)?))
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
        Value::Null => "NoneType",
    }
}

/// Lists verbatim; strings split on commas when present, else whitespace.
fn tool_patterns(v: Option<&Value>, kwarg: &str) -> Result<Vec<String>> {
    let raw: Vec<&Value> = match v {
        None | Some(Value::Null) => return Ok(vec![]),
        Some(Value::String(s)) => {
            let parts: Vec<String> =
                if s.contains(',') { s.split(',').map(String::from).collect() } else { s.split_whitespace().map(String::from).collect() };
            return Ok(parts.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect());
        }
        Some(Value::Array(a)) => a.iter().collect(),
        Some(o) => bail!("cortex-code {kwarg} must be a string or list of strings, got {}.", type_name(o)),
    };
    let mut out = vec![];
    for item in raw {
        let Value::String(s) = item else { bail!("cortex-code {kwarg} entries must be strings, got {}.", type_name(item)) };
        if !s.trim().is_empty() {
            out.push(s.trim().to_string());
        }
    }
    Ok(out)
}

fn cli_mode(v: Option<&Value>) -> Result<Option<String>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let n = match s.trim().to_lowercase().as_str() {
                "default" => "standard".to_string(),
                n => n.to_string(),
            };
            if n != "standard" && n != "code" {
                bail!("Invalid cortex-code cli_mode '{s}'. Expected one of: code, standard.");
            }
            Ok(Some(n))
        }
        Some(o) => bail!("cortex-code cli_mode must be a string, got {}. Expected one of: code, standard.", type_name(o)),
    }
}

impl CortexCode {
    fn new(args: AgentArgs) -> Result<CortexCode> {
        let mut disallowed_tools = tool_patterns(args.kwargs.get("disallowed_tools"), "disallowed_tools")?;
        let allowed_tools = tool_patterns(args.kwargs.get("allowed_tools"), "allowed_tools")?;
        let cli_mode = cli_mode(args.kwargs.get("cli_mode"))?;
        let dws = match args.kwargs.get("disable_web_search") {
            None | Some(Value::Null) => false,
            Some(_) => args.kw_bool("disable_web_search").ok_or_else(|| {
                anyhow::anyhow!("Invalid kwargs for agent 'cortex-code':\n  disable_web_search: Input should be a valid boolean")
            })?,
        };
        if dws {
            disallowed_tools.extend(["web_search".into(), "web_fetch".into()]);
        }
        Ok(CortexCode { version: args.version_kw(), args, disallowed_tools, allowed_tools, cli_mode })
    }

    /// `--model ... --disallowed-tools ... --allowed-tools ... --mode ...` (each part has a trailing space).
    fn flags(&self) -> String {
        let mut s = String::new();
        if let Some(m) = self.args.model_name.as_deref().filter(|m| !m.is_empty()) {
            s += &format!("--model {} ", quote(m.rsplit('/').next().unwrap()));
        }
        let mut parts = vec![];
        for (flag, pats) in [("--disallowed-tools", &self.disallowed_tools), ("--allowed-tools", &self.allowed_tools)] {
            if !pats.is_empty() {
                parts.push(format!("{flag} {}", pats.iter().map(|p| quote(p)).collect::<Vec<_>>().join(" ")));
            }
        }
        if !parts.is_empty() {
            s += &format!("{} ", parts.join(" "));
        }
        if let Some(m) = &self.cli_mode {
            s += &format!("--mode {} ", quote(m));
        }
        s
    }

    /// Writes ~/.snowflake/config.toml from SNOWFLAKE_* vars expanded in-container (values never inlined).
    fn connection_setup(&self) -> Result<String> {
        // Only --ae values reach the container, so validate against those.
        let cenv = |k: &str| self.args.extra_env.get(k).filter(|v| !v.is_empty());
        if cenv("SNOWFLAKE_ACCOUNT").is_none() || cenv("SNOWFLAKE_USER").is_none() {
            bail!(
                "Cortex Code requires SNOWFLAKE_ACCOUNT and SNOWFLAKE_USER. Forward them into the environment, e.g. --ae SNOWFLAKE_ACCOUNT=... --ae SNOWFLAKE_USER=..."
            );
        }
        enum Spec {
            Line(&'static str),
            Ref(&'static str, &'static str),
        }
        use Spec::*;
        let mut specs = vec![
            Line("default_connection_name = \"default\""),
            Line(""),
            Line("[connections.default]"),
            Ref("SNOWFLAKE_ACCOUNT", "account"),
            Ref("SNOWFLAKE_USER", "user"),
        ];
        if cenv("SNOWFLAKE_HOST").is_some() {
            specs.push(Ref("SNOWFLAKE_HOST", "host"));
        }
        let authenticator = cenv("SNOWFLAKE_AUTHENTICATOR");
        if cenv("SNOWFLAKE_PAT").is_some() {
            specs.extend([Line("authenticator = \"programmatic_access_token\""), Ref("SNOWFLAKE_PAT", "token")]);
        } else if cenv("SNOWFLAKE_PASSWORD").is_some() {
            if authenticator.is_some_and(|a| a.to_lowercase() == "oauth") {
                specs.extend([Line("authenticator = \"oauth\""), Ref("SNOWFLAKE_PASSWORD", "token")]);
            } else {
                specs.push(if authenticator.is_some() {
                    Ref("SNOWFLAKE_AUTHENTICATOR", "authenticator")
                } else {
                    Line("authenticator = \"snowflake\"")
                });
                specs.push(Ref("SNOWFLAKE_PASSWORD", "password"));
            }
        } else {
            bail!("Cortex Code requires SNOWFLAKE_PAT (recommended) or SNOWFLAKE_PASSWORD for authentication.");
        }
        for (var, key) in [
            ("SNOWFLAKE_ROLE", "role"),
            ("SNOWFLAKE_WAREHOUSE", "warehouse"),
            ("SNOWFLAKE_DATABASE", "database"),
            ("SNOWFLAKE_SCHEMA", "schema"),
        ] {
            if cenv(var).is_some() {
                specs.push(Ref(var, key));
            }
        }
        let body = specs.iter().map(|s| match s {
            Line(l) => l.to_string(),
            Ref(_, k) => format!("{k} = \"$_cc_{k}\""),
        });
        let assigns = specs.iter().filter_map(|s| match s {
            Ref(env, k) => Some(format!("_cc_{k}=\"$(_cc_esc \"${env}\")\"")),
            Line(_) => None,
        });
        Ok(format!(
            "mkdir -p \"$HOME/.snowflake\"\n{}\n{}\ncat > \"$HOME/.snowflake/config.toml\" <<EOF\n{}\nEOF\nchmod 600 \"$HOME/.snowflake/config.toml\"",
            r#"_cc_esc() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }"#,
            assigns.collect::<Vec<_>>().join("\n"),
            body.collect::<Vec<_>>().join("\n")
        ))
    }
}

/// Python-style `a or b or ... or 0` over usage keys.
fn first_truthy(u: &Value, keys: &[&str]) -> i64 {
    keys.iter().map(|k| u[*k].as_i64().unwrap_or(0)).find(|n| *n != 0).unwrap_or(0)
}

/// (input incl. cache, output, cache) summed over stream-json `usage` records (Anthropic or OpenAI keys).
pub fn usage_from_output(text: &str) -> (i64, i64, i64) {
    let (mut input, mut output, mut cache) = (0, 0, 0);
    for line in text.lines().map(str::trim).filter(|l| l.starts_with('{')) {
        let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
        let u = &e["usage"];
        if !u.is_object() {
            continue;
        }
        let c = first_truthy(u, &["cache_read_input_tokens", "cached_input_tokens", "cached_tokens"])
            + first_truthy(u, &["cache_creation_input_tokens"]);
        input += if u.get("input_tokens").is_some() { first_truthy(u, &["input_tokens"]) + c } else { first_truthy(u, &["prompt_tokens"]) };
        output += first_truthy(u, &["output_tokens", "completion_tokens"]);
        cache += c;
    }
    (input, output, cache)
}

#[async_trait::async_trait]
impl Agent for CortexCode {
    fn name(&self) -> &str {
        "cortex-code"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        let none = BTreeMap::new();
        if cx.exec(&format!("{PATH_EXPORT}; command -v cortex >/dev/null 2>&1"), &none, None).await?.code != 0 {
            ensure_system_dependencies(cx, &["curl", "bash", "tar", "coreutils", "ca_certificates"]).await?;
            exec_agent(
                cx,
                &format!(
                    "set -euo pipefail\ncurl -LsS https://ai.snowflake.com/static/cc-scripts/install.sh -o /tmp/install-cortex-code.sh\n\
                     NON_INTERACTIVE=1 sh /tmp/install-cortex-code.sh\nrm -f /tmp/install-cortex-code.sh\n\
                     echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> \"$HOME/.bashrc\"\n{PATH_EXPORT}\ncortex --version"
                ),
                &none,
            )
            .await?;
        }
        if self.version.is_none() {
            let r = cx.exec(&format!("{PATH_EXPORT}; cortex --version"), &none, None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(parse_version(r.stdout.trim()));
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let none = BTreeMap::new();
        exec_agent(cx, &self.connection_setup()?, &none).await?;
        let resume_part = if resume { "--resume last " } else { "" };
        let cmd = format!(
            "{PATH_EXPORT}; cortex {resume_part}--print {} --dangerously-allow-all-tool-calls --auto-accept-plans --no-auto-update \
             --output-format stream-json {}</dev/null 2>&1 | tee /logs/agent/{OUTPUT}",
            quote(&instruction),
            self.flags()
        );
        let env = BTreeMap::from([("SF_SKIP_WARNING_FOR_READ_PERMISSIONS_ON_CONFIG_FILE".to_string(), "true".to_string())]);
        let r = exec_agent(cx, &cmd, &env).await;
        // Snapshot only the conversations dir, then remove the credential file.
        let dst = "/logs/agent/cortex-code-snapshot/conversations";
        let src = "$HOME/.snowflake/cortex/conversations";
        let _ = exec_agent(
            cx,
            &format!("mkdir -p {dst} && (rsync -a \"{src}/\" {dst}/ 2>/dev/null || cp -r \"{src}/.\" {dst}/ 2>/dev/null) || true"),
            &none,
        )
        .await;
        let _ = exec_agent(cx, "rm -f \"$HOME/.snowflake/config.toml\"", &none).await;
        r.map(|_| ())
    }

    fn collect_usage(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        let Ok(text) = std::fs::read_to_string(logs_dir.join(OUTPUT)) else { return };
        let (i, o, c) = usage_from_output(&text);
        if i != 0 || o != 0 || c != 0 {
            ctx.n_input_tokens = Some(i);
            ctx.n_output_tokens = Some(o);
            ctx.n_cache_tokens = Some(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;
    use serde_json::json;

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<CortexCode> {
        CortexCode::new(common::test_args(kwargs, env, model))
    }

    #[test]
    fn flags_and_validation() {
        let a = agent(
            json!({"disallowed_tools": "web_search, web_fetch", "allowed_tools": ["Bash(rm *)"], "cli_mode": " Default "}),
            &[],
            Some("snowflake/claude-4-sonnet"),
        )
        .unwrap();
        assert_eq!(
            a.flags(),
            "--model claude-4-sonnet --disallowed-tools web_search web_fetch --allowed-tools 'Bash(rm *)' --mode standard "
        );
        let b = agent(json!({"disallowed_tools": "a b", "disable_web_search": true}), &[], None).unwrap();
        assert_eq!(b.disallowed_tools, ["a", "b", "web_search", "web_fetch"]);
        assert_eq!(agent(json!({}), &[], None).unwrap().flags(), "");
        assert!(agent(json!({"cli_mode": "turbo"}), &[], None).is_err());
        assert!(agent(json!({"cli_mode": true}), &[], None).is_err());
        assert!(agent(json!({"allowed_tools": [1]}), &[], None).is_err());
        assert!(agent(json!({"allowed_tools": 5}), &[], None).is_err());
    }

    #[test]
    fn connection_config() {
        let a = agent(
            json!({}),
            &[("SNOWFLAKE_ACCOUNT", "acct"), ("SNOWFLAKE_USER", "u"), ("SNOWFLAKE_PAT", "secret"), ("SNOWFLAKE_ROLE", "r")],
            None,
        )
        .unwrap();
        let s = a.connection_setup().unwrap();
        assert!(!s.contains("secret") && !s.contains("acct"));
        assert!(s.contains("authenticator = \"programmatic_access_token\"\ntoken = \"$_cc_token\"\nrole = \"$_cc_role\"\nEOF"));
        assert!(s.contains("_cc_token=\"$(_cc_esc \"$SNOWFLAKE_PAT\")\""));
        let o = agent(
            json!({}),
            &[("SNOWFLAKE_ACCOUNT", "a"), ("SNOWFLAKE_USER", "u"), ("SNOWFLAKE_PASSWORD", "p"), ("SNOWFLAKE_AUTHENTICATOR", "OAuth")],
            None,
        )
        .unwrap();
        assert!(o.connection_setup().unwrap().contains("authenticator = \"oauth\"\ntoken = \"$_cc_token\""));
        let p = agent(json!({}), &[("SNOWFLAKE_ACCOUNT", "a"), ("SNOWFLAKE_USER", "u"), ("SNOWFLAKE_PASSWORD", "p")], None).unwrap();
        assert!(p.connection_setup().unwrap().contains("authenticator = \"snowflake\"\npassword = \"$_cc_password\""));
        assert!(agent(json!({}), &[("SNOWFLAKE_ACCOUNT", "a"), ("SNOWFLAKE_USER", "u")], None).unwrap().connection_setup().is_err());
        assert!(agent(json!({}), &[], None).unwrap().connection_setup().is_err());
    }

    #[test]
    fn usage_parsing() {
        let lines = |v: Vec<Value>| v.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        let t = lines(vec![
            json!({"type": "system", "subtype": "init", "session_id": "sess-9"}),
            json!({"type": "assistant", "usage": {"input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 10}}),
            json!({"type": "result", "usage": {"prompt_tokens": 5, "completion_tokens": 3, "cached_tokens": 2}}),
        ]);
        assert_eq!(usage_from_output(&t), (115, 23, 12));
        assert_eq!(
            usage_from_output(&lines(vec![
                json!({"usage": {"input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 10, "cache_creation_input_tokens": 5}})
            ])),
            (115, 20, 15)
        );
        assert_eq!(
            usage_from_output(&lines(vec![
                json!({"usage": {"input_tokens": 100, "prompt_tokens": 100, "output_tokens": 20, "completion_tokens": 20}})
            ])),
            (100, 20, 0)
        );
        assert_eq!(
            usage_from_output(&lines(vec![
                json!({"usage": {"input_tokens": null, "output_tokens": null}}),
                json!({"usage": {"input_tokens": 10, "output_tokens": 2}})
            ])),
            (10, 2, 0)
        );
        assert_eq!(usage_from_output("42\nnot json"), (0, 0, 0));
    }
}
