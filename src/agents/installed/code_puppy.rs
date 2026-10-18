//! Code Puppy.

use super::common::installed_dir;
use super::{connection, ensure_system_dependencies, exec_agent};
use crate::agents::model_connection::Spec;
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use base64::Engine;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const PATH_EXPORT: &str = "export PATH=\"$HOME/.local/bin:$PATH\"";
const CUSTOM_API_KEY_ENV: &str = "CODE_PUPPY_API_KEY";
const USAGE: &str = "/logs/agent/code-puppy-usage.json";
const CFG_DIR: &str = "$HOME/.code_puppy";
const SPEC: Spec = Spec { default_provider: None, api_key_envs: &[CUSTOM_API_KEY_ENV], base_url_envs: &[], passthrough: true };
const EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

pub struct CodePuppy {
    args: AgentArgs,
    version: Option<String>,
    base_url: Option<String>,
    reasoning_effort: Option<String>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    let reasoning_effort = args.kw("reasoning_effort");
    if let Some(r) = &reasoning_effort
        && !EFFORTS.contains(&r.as_str())
    {
        bail!(
            "Invalid kwargs for agent 'code-puppy':\n  reasoning_effort: Input should be 'minimal', 'low', 'medium', 'high', 'xhigh' or 'max'"
        );
    }
    Ok(Box::new(CodePuppy { version: args.version_kw(), base_url: args.kw("base_url"), reasoning_effort, args }))
}

impl CodePuppy {
    /// (`extra_models.json` entry, env it needs) for `provider/model`.
    fn model_definition(&self) -> Result<(Value, BTreeMap<String, String>)> {
        let Some(name) = self.args.model_name.as_deref() else { bail!("Code Puppy requires a model (provider/model).") };
        let Some((provider, model_id)) = name.split_once('/').filter(|(_, m)| !m.is_empty()) else {
            bail!("Code Puppy model names must be 'provider/model', got '{name}'.");
        };
        let conn = connection(&self.args, SPEC);
        let Some(api_key) = conn.api_key.clone().filter(|k| !k.is_empty()) else {
            bail!(
                "No API key found for provider '{provider}'. Set the provider's credential on the host (or pass it with `--ae`/`--ak`) before running Code Puppy."
            );
        };
        let endpoint = self.base_url.clone().or(conn.configured_base_url.clone());
        let canonical = conn.provider.clone().unwrap_or_else(|| provider.to_string());
        let native = match canonical.as_str() {
            "anthropic" => Some(("anthropic", "ANTHROPIC_API_KEY")),
            "openai" | "openai-responses" => Some(("openai", "OPENAI_API_KEY")),
            "google" => Some(("gemini", "GEMINI_API_KEY")),
            _ => None,
        };
        let mut env = BTreeMap::new();
        let def = match (endpoint.clone(), native) {
            (None, Some((t, key_env))) => {
                env.insert(key_env.to_string(), api_key);
                json!({"type": t, "name": model_id})
            }
            _ => {
                let Some(url) = endpoint.or(conn.base_url.clone()) else {
                    bail!("Provider '{provider}' has no known base URL. Pass one with `--ak base_url=...`.");
                };
                let t = match canonical.as_str() {
                    "anthropic" => "custom_anthropic",
                    "openai-responses" | "openai_responses" => "custom_openai_responses",
                    _ => "custom_openai",
                };
                env.insert(CUSTOM_API_KEY_ENV.to_string(), api_key);
                json!({"type": t, "name": model_id, "custom_endpoint": {"url": url, "api_key": format!("${CUSTOM_API_KEY_ENV}")}})
            }
        };
        // Provider-native envs (base URLs, extra credentials) ride along.
        for (k, v) in conn.env {
            env.entry(k).or_insert(v);
        }
        Ok((def, env))
    }

    fn puppy_cfg(&self, alias: &str) -> String {
        let mut lines =
            vec!["[puppy]".to_string(), "puppy_name = HbenchPup".into(), "owner_name = hbench".into(), "auto_save_session = false".into()];
        if let Some(r) = &self.reasoning_effort {
            let setting = alias.replace(['.', '-', '/'], "_").to_lowercase();
            lines.push(format!("model_settings_{setting}_reasoning_effort = {r}"));
        }
        lines.join("\n") + "\n"
    }

    fn run_command(&self, instruction: &str) -> String {
        // A leading `!`/`/` would be parsed as a shell/slash command.
        let instruction = if instruction.trim_start().starts_with(['!', '/']) {
            format!("Task request:\n{instruction}")
        } else {
            instruction.to_string()
        };
        format!(
            "{PATH_EXPORT}; set -o pipefail; code-puppy --yolo true --disable-ask-user-question --usage-file {USAGE} --model {} --prompt {} \
             2>&1 </dev/null | stdbuf -oL tee /logs/agent/code-puppy.txt; cp_status=$?; if [ ! -s {USAGE} ]; then \
             echo 'code-puppy produced no result: {USAGE} was not written' >&2; exit 1; fi; exit $cp_status",
            quote(self.args.model_name.as_deref().unwrap_or("")),
            quote(&instruction)
        )
    }
}

fn write_file_cmd(path: &str, content: &str) -> String {
    format!("printf '%s' {} | base64 -d > {path}", quote(&base64::engine::general_purpose::STANDARD.encode(content)))
}

/// Map `code-puppy-usage.json` onto the context (only keys that are present).
pub fn apply_usage(text: &str, ctx: &mut AgentContext) {
    let Ok(Value::Object(u)) = serde_json::from_str::<Value>(text) else { return };
    let int = |k: &str| {
        u.get(k)
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
    };
    if let Some(v) = int("input_tokens") {
        ctx.n_input_tokens = Some(v);
    }
    if let Some(v) = int("output_tokens") {
        ctx.n_output_tokens = Some(v);
    }
    if let Some(v) = int("cache_read_tokens") {
        ctx.n_cache_tokens = Some(v);
    }
    if let Some(v) = u.get("cost_usd").and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))) {
        ctx.cost_usd = Some(v);
    }
}

#[async_trait::async_trait]
impl Agent for CodePuppy {
    fn name(&self) -> &str {
        "code-puppy"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl", "ca_certificates", "coreutils", "ripgrep"]).await?;
        let spec = self.version.as_ref().map(|v| format!("=={v}")).unwrap_or_default();
        let none = BTreeMap::new();
        exec_agent(
            cx,
            &format!("set -euo pipefail; curl -LsSf https://astral.sh/uv/install.sh | sh && {PATH_EXPORT} && uv tool install --python 3.13 code-puppy{spec} && code-puppy --version"),
            &none,
        )
        .await?;
        if self.version.is_none() {
            let r = cx.exec(&format!("{PATH_EXPORT}; code-puppy --version"), &none, None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(r.stdout.trim().lines().last().unwrap_or("").trim().to_string());
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let (def, env) = self.model_definition()?;
        let alias = self.args.model_name.clone().unwrap_or_default();
        let models = serde_json::to_string_pretty(&json!({ alias.clone(): def }))? + "\n";
        let setup = [
            format!("mkdir -p {CFG_DIR}"),
            write_file_cmd(&format!("{CFG_DIR}/extra_models.json"), &models),
            write_file_cmd(&format!("{CFG_DIR}/puppy.cfg"), &self.puppy_cfg(&alias)),
            format!("chmod 600 {CFG_DIR}/*.json {CFG_DIR}/puppy.cfg"),
        ];
        exec_agent(cx, &setup.join(" && "), &env).await?;
        exec_agent(cx, &self.run_command(&instruction), &env).await.map(|_| ())
    }

    fn populate_context_post_run(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        if let Ok(t) = std::fs::read_to_string(logs_dir.join("code-puppy-usage.json")) {
            apply_usage(&t, ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<CodePuppy> {
        let args = common::test_args(kwargs, env, model);
        let reasoning_effort = args.kw("reasoning_effort");
        create(args.clone())?;
        Ok(CodePuppy { version: None, base_url: args.kw("base_url"), reasoning_effort, args })
    }

    #[test]
    fn model_definitions() {
        let a = agent(json!({}), &[("ANTHROPIC_API_KEY", "ak")], Some("anthropic/claude-x")).unwrap();
        let (def, env) = a.model_definition().unwrap();
        assert_eq!(def, json!({"type": "anthropic", "name": "claude-x"}));
        assert_eq!(env["ANTHROPIC_API_KEY"], "ak");

        let o = agent(json!({}), &[("OPENROUTER_API_KEY", "ok")], Some("openrouter/openai/gpt-5")).unwrap();
        let (def, env) = o.model_definition().unwrap();
        assert_eq!(
            def,
            json!({"type": "custom_openai", "name": "openai/gpt-5", "custom_endpoint": {"url": "https://openrouter.ai/api/v1", "api_key": "$CODE_PUPPY_API_KEY"}})
        );
        assert_eq!((env["CODE_PUPPY_API_KEY"].as_str(), env["OPENROUTER_API_KEY"].as_str()), ("ok", "ok"));

        let r = agent(json!({"base_url": "http://gw/v1"}), &[("OPENAI_API_KEY", "k")], Some("openai-responses/gpt-5")).unwrap();
        assert_eq!(r.model_definition().unwrap().0["type"], "custom_openai_responses");
        let u = agent(json!({}), &[("CODE_PUPPY_API_KEY", "k")], Some("acme/m1")).unwrap();
        assert!(u.model_definition().unwrap_err().to_string().contains("no known base URL"));
        assert!(agent(json!({}), &[], Some("bare")).unwrap().model_definition().is_err());
        assert!(agent(json!({"reasoning_effort": "huge"}), &[], None).is_err());
    }

    #[test]
    fn config_and_command() {
        let a = agent(json!({"reasoning_effort": "high"}), &[], Some("openai/gpt-5.1-codex")).unwrap();
        assert_eq!(
            a.puppy_cfg("openai/gpt-5.1-codex"),
            "[puppy]\npuppy_name = HbenchPup\nowner_name = hbench\nauto_save_session = false\nmodel_settings_openai_gpt_5_1_codex_reasoning_effort = high\n"
        );
        let cmd = a.run_command("/help me");
        assert!(cmd.contains("--model openai/gpt-5.1-codex --prompt 'Task request:\n/help me' 2>&1 </dev/null | stdbuf -oL tee /logs/agent/code-puppy.txt; cp_status=$?"));
        assert!(cmd.ends_with("exit $cp_status"));
        assert_eq!(write_file_cmd("/x", "hi"), "printf '%s' aGk= | base64 -d > /x");
    }

    #[test]
    fn usage_file() {
        let mut c = AgentContext::default();
        apply_usage(r#"{"input_tokens": 1200, "output_tokens": 340, "cache_read_tokens": 900, "cost_usd": 0.25}"#, &mut c);
        assert_eq!((c.n_input_tokens, c.n_output_tokens, c.n_cache_tokens, c.cost_usd), (Some(1200), Some(340), Some(900), Some(0.25)));
        let mut c = AgentContext::default();
        apply_usage(r#"{"input_tokens": 5}"#, &mut c);
        assert_eq!((c.n_input_tokens, c.n_output_tokens), (Some(5), None));
        apply_usage("[]", &mut c);
    }
}
