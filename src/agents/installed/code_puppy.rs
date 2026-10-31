//! Code Puppy.

use super::common::{installed_dir, pydantic_ai_steps, uv_install};
use super::pins;
use super::{connection, ensure_system_dependencies, exec_agent};
use crate::agents::model_connection::Spec;
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::atif;
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
        bail!("Invalid kwargs for agent 'code-puppy':\n  reasoning_effort: must be 'minimal', 'low', 'medium', 'high', 'xhigh' or 'max'");
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
             2>&1 </dev/null | stdbuf -oL tee /logs/agent/code-puppy.txt; cp_status=$?; \
             mkdir -p /logs/agent/code-puppy-sessions; find \"$HOME\" -path '*/autosaves/*.json' -not -name '*_meta.json' \
             -exec cp {{}} /logs/agent/code-puppy-sessions/ \\; 2>/dev/null; if [ ! -s {USAGE} ]; then \
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

/// An autosaved session, a JSON envelope around pydantic-ai messages, -> ATIF.
pub fn atif_trajectory(text: &str, version: Option<&str>, model: Option<&str>) -> Option<atif::Trajectory> {
    let session: Value = serde_json::from_str(text).ok()?;
    let steps = pydantic_ai_steps(session["messages"].as_array()?, &|_| true);
    let agent = atif::Agent {
        name: "code-puppy".into(),
        version: session["code_puppy"].as_str().or(version).unwrap_or("unknown").into(),
        model_name: model.map(String::from),
        ..Default::default()
    };
    atif::assemble(agent, None, steps)
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
        let spec = pins::CODE_PUPPY.pip_constraint(self.version.as_deref());
        let none = BTreeMap::new();
        exec_agent(
            cx,
            &format!("set -euo pipefail; {uv_install} && {PATH_EXPORT} && uv tool install --python 3.13 code-puppy{spec} && code-puppy --version", uv_install = uv_install(None)),
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

    fn collect_usage(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        if let Ok(t) = std::fs::read_to_string(logs_dir.join("code-puppy-usage.json")) {
            apply_usage(&t, ctx);
        }
        let mut sessions: Vec<_> =
            std::fs::read_dir(logs_dir.join("code-puppy-sessions")).into_iter().flatten().flatten().map(|e| e.path()).collect();
        sessions.sort();
        let text = sessions.last().and_then(|p| std::fs::read_to_string(p).ok());
        if let Some(mut t) = text.and_then(|t| atif_trajectory(&t, self.version.as_deref(), self.args.model_name.as_deref())) {
            atif::set_run_totals(&mut t, ctx.n_input_tokens, ctx.n_output_tokens, ctx.n_cache_tokens, ctx.cost_usd);
            atif::write_trajectory(logs_dir, &t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;

    #[test]
    fn session_to_atif() {
        // A real run's autosave, trimmed.
        let session = json!({"format": 2, "code_puppy": "0.0.933", "pydantic_ai": "2.51.0", "encoding": "pydantic-ai-messages", "messages": [
            {"kind": "request", "timestamp": "2026-10-31T10:53:33.016426Z", "instructions": "You are HbenchPup",
             "parts": [{"content": "Write the line.", "timestamp": "2026-10-31T10:53:33.016061Z", "part_kind": "user-prompt"}]},
            {"kind": "response", "model_name": "qwen3.5:9b", "timestamp": "2026-10-31T10:53:46.422962Z",
             "usage": {"input_tokens": 3732, "cache_write_tokens": 0, "cache_read_tokens": 0, "output_tokens": 127},
             "parts": [{"content": "Use create_file.", "id": "reasoning", "part_kind": "thinking"},
                       {"tool_name": "create_file", "args": "{\"file_path\": \"/app/hello.txt\"}", "tool_call_id": "call_1", "part_kind": "tool-call"}]},
            {"kind": "request", "timestamp": "2026-10-31T10:53:54.812295Z", "instructions": "You are HbenchPup",
             "parts": [{"tool_name": "create_file", "content": {"success": true}, "tool_call_id": "call_1", "outcome": "success", "part_kind": "tool-return"}]},
            {"kind": "response", "model_name": "qwen3.5:9b", "timestamp": "2026-10-31T10:53:56.539863Z",
             "usage": {"input_tokens": 3914, "cache_read_tokens": 3728, "output_tokens": 56},
             "parts": [{"content": "Done.", "part_kind": "text"}]}
        ]});
        let a = atif_trajectory(&session.to_string(), None, Some("openai/m")).unwrap();
        assert_eq!(a.agent.version, "0.0.933");
        assert_eq!(a.steps.iter().map(|s| s.source.as_str()).collect::<Vec<_>>(), ["system", "user", "agent", "agent"]);
        let call = &a.steps[2];
        assert_eq!((call.reasoning_content.as_deref(), call.model_name.as_deref()), (Some("Use create_file."), Some("qwen3.5:9b")));
        assert_eq!(call.tool_calls.as_ref().unwrap()[0].arguments["file_path"], "/app/hello.txt");
        let result = &call.observation.as_ref().unwrap().results[0];
        assert_eq!((result.source_call_id.as_deref(), result.content.clone()), (Some("call_1"), Some(json!({"success": true}))));
        let f = a.final_metrics.unwrap();
        assert_eq!((f.total_prompt_tokens, f.total_completion_tokens, f.total_cached_tokens), (Some(7646), Some(183), Some(3728)));
        assert!(atif_trajectory("{}", None, None).is_none());
    }

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
