//! MiniMax Code CLI installed agent.

use super::common::opts::kw_int;
use super::common::{installed_setup, nvm_node_install_snippet, parse_jsonl, skills_dir};
use super::pins;
use super::{connection, ensure_system_dependencies, exec_agent};
use crate::agents::model_connection::{Spec, provider as provider_access};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use crate::fmt::title_case;
use anyhow::{Result, anyhow, bail};
use serde_json::{Map, json};
use std::collections::BTreeMap;

const SPEC: Spec = Spec { default_provider: None, api_key_envs: &[], base_url_envs: &[], passthrough: true };
const DATA_DIR: &str = "/tmp/hbench-mcode";
const PERMISSION_MODES: [&str; 4] = ["ask", "full", "off", "smart"];
const API_FORMATS: [&str; 2] = ["anthropic-messages", "openai-completions"];
const AWK_PROGRAM: &str = r#"$0 == "custom_provider:" {
    in_custom = 1
    in_provider = 0
    in_models = 0
}
in_custom && $0 ~ /^[^ ]/ && $0 != "custom_provider:" {
    in_custom = 0
    in_provider = 0
    in_models = 0
}
in_custom && $0 ~ /^  [^ ]/ {
    in_provider = ($0 == "  " provider ":")
    in_models = 0
}
in_provider && $0 ~ /^    [^ ]/ {
    in_models = ($0 == "    models:")
}
in_models && $0 == "      " model ":" && !patched {
    print
    print "        limit:"
    if (context_line != "") print context_line
    if (output_line != "") print output_line
    patched = 1
    next
}
{ print }
END { if (!patched) exit 1 }"#;

pub struct MCode {
    args: AgentArgs,
    version: Option<String>,
    max_steps: Option<i64>,
    api_format: Option<String>,
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
    permission_mode: String,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(MCode::new(args)?))
}

impl MCode {
    fn new(args: AgentArgs) -> Result<MCode> {
        let positive = |k: &str| -> Result<Option<i64>> {
            let v = kw_int(&args, k).map_err(|_| anyhow!("Invalid value for '{k}': expected a positive int"))?;
            if v.is_some_and(|n| n <= 0) {
                bail!("Invalid value for '{k}': expected a positive int");
            }
            Ok(v)
        };
        let context_window = positive("context_window")?;
        let max_output_tokens = positive("max_output_tokens")?;
        let permission_mode = args.kw("permission_mode").unwrap_or_else(|| "full".into());
        if !PERMISSION_MODES.contains(&permission_mode.as_str()) {
            bail!("Invalid value for 'permission_mode': '{permission_mode}'. Valid values: {}", PERMISSION_MODES.join(", "));
        }
        let api_format = args.kw("api_format");
        if let Some(f) = &api_format
            && !API_FORMATS.contains(&f.as_str())
        {
            bail!("Invalid value for 'api_format': '{f}'. Valid values: {}", API_FORMATS.join(", "));
        }
        Ok(MCode {
            max_steps: kw_int(&args, "max_steps")?,
            version: args.version_kw(),
            args,
            api_format,
            context_window,
            max_output_tokens,
            permission_mode,
        })
    }

    fn model_parts(&self) -> Result<(String, String)> {
        match self.args.model_name.as_deref().and_then(|m| m.split_once('/')) {
            Some((p, m)) if !p.is_empty() && !m.is_empty() => Ok((p.into(), m.into())),
            _ => bail!("Model name must use <provider>/<model>."),
        }
    }

    fn provider_name(provider: &str) -> String {
        let display = match provider {
            "deepseek" => "DeepSeek".into(),
            "minimax" => "MiniMax".into(),
            "openai" => "OpenAI".into(),
            p => title_case(&p.replace(['-', '_'], " ")),
        };
        format!("hbench {display}")
    }

    fn api_format(&self, provider: &str) -> Result<String> {
        if let Some(f) = &self.api_format {
            return Ok(f.clone());
        }
        Ok(match provider {
            "anthropic" | "minimax" => "anthropic-messages",
            "deepseek" | "openai" => "openai-completions",
            _ => bail!("MCode does not know the API format for provider '{provider}'. Pass api_format=anthropic-messages or api_format=openai-completions."),
        }
        .into())
    }

    fn mcp_command(&self) -> Option<String> {
        if self.args.mcp_servers.is_empty() {
            return None;
        }
        let mut servers = Map::new();
        for s in &self.args.mcp_servers {
            let v = if s.transport() == "stdio" {
                json!({"type": "stdio", "command": s.command, "args": s.args})
            } else {
                let t = if s.transport() == "streamable-http" { "http" } else { s.transport() };
                json!({"type": t, "url": s.url})
            };
            servers.insert(s.name.clone(), v);
        }
        let config = serde_json::to_string_pretty(&json!({"mcpServers": servers})).unwrap();
        Some(format!("printf %s {} > {}", quote(&config), quote(&format!("{DATA_DIR}/mcp.json"))))
    }

    fn skills_command(&self) -> Option<String> {
        let d = skills_dir(&self.args)?;
        Some(format!(
            "if [ -d {} ]; then mkdir -p {DATA_DIR}/skills && cp -R {} {DATA_DIR}/skills/; fi",
            quote(&d),
            quote(&format!("{}/.", d.trim_end_matches('/')))
        ))
    }

    fn limits_command(&self, provider_key: &str, requested_provider: &str, model_id: &str) -> Option<String> {
        let known = (requested_provider == "minimax" && model_id == "MiniMax-M3").then_some((512_000, 128_000));
        let context = self.context_window.or(known.map(|k| k.0));
        let output = self.max_output_tokens.or(known.map(|k| k.1));
        if context.is_none() && output.is_none() {
            return None;
        }
        let config = format!("{DATA_DIR}/config.yaml");
        let temp = format!("{config}.tmp");
        let context_line = context.map(|c| format!("          context: {c}")).unwrap_or_default();
        let output_line = output.map(|o| format!("          output: {o}")).unwrap_or_default();
        let (c, t) = (quote(&config), quote(&temp));
        let patch = format!(
            "cp {c} {t} && awk -v provider={} -v model={} -v context_line={} -v output_line={} {} {c} > {t} && mv {t} {c}",
            quote(provider_key),
            quote(model_id),
            quote(&context_line),
            quote(&output_line),
            quote(AWK_PROGRAM)
        );
        if self.context_window.is_some() || self.max_output_tokens.is_some() {
            let err = "Error: Could not apply explicit MCode model limits. MCode generated an unsupported config layout; pin a compatible MCode version or omit context_window/max_output_tokens.";
            return Some(format!("{{ {{ {patch}; }} || {{ rm -f {t}; printf '%s\\n' {} >&2; exit 1; }}; }}", quote(err)));
        }
        let warn = "Warning: Could not apply known MCode model limits; continuing with MCode defaults.";
        Some(format!("{{ {{ {patch}; }} || {{ rm -f {t}; printf '%s\\n' {} >&2; }}; }}", quote(warn)))
    }

    /// (setup command, run command, env).
    fn commands(&self, instruction: &str, resume: bool) -> Result<(String, String, BTreeMap<String, String>)> {
        let (requested, model_id) = self.model_parts()?;
        let access = connection(&self.args, SPEC);
        let provider = access.provider.clone().unwrap_or(requested.clone());
        let pa = provider_access(&provider);
        let provider_key = format!("hbench-{}", provider.replace('_', "-"));
        let execution_model = format!("custom_provider:{provider_key}/{model_id}");
        if access.api_key.is_none() {
            let names = pa
                .as_ref()
                .map(|p| p.api_key_envs.join(" or "))
                .unwrap_or_else(|| format!("{}_API_KEY", provider.to_uppercase().replace('-', "_")));
            bail!("{names} is required. Set it on the host or pass it with --ae.");
        }
        let mut env = access.env.clone();
        env.insert("MINIMAX_DATA_DIR".into(), DATA_DIR.into());
        let Some(key_env) = pa.and_then(|p| p.api_key_envs.iter().find(|n| env.contains_key(**n)).map(|n| n.to_string())) else {
            bail!("Could not resolve the API key environment for '{provider}'.")
        };
        let Some(base_url) = access.base_url.clone() else { bail!("A base URL is required for provider '{provider}'.") };
        let api_format = self.api_format(&provider)?;
        let config = format!("{DATA_DIR}/config.yaml");
        let runtime = format!("{DATA_DIR}/hbench-config.yaml");
        let web_policy = "\nagents:\n  default:\n    features:\n      webSearch: false\n";
        let opt = |c: Option<String>| c.map(|c| format!(" && {c}")).unwrap_or_default();
        let setup = format!(
            "set -euo pipefail; . ~/.nvm/nvm.sh; mcode provider remove --yes {} < /dev/null > /dev/null 2>&1 || true; \
             mcode provider add --name {} --base-url {} --api-format {} --model {} --api-key-env {} < /dev/null{} \
             && grep -q '^defaultModel:' {} && sed -i {} {} && cp {} {} && printf %s {} >> {}{}{}",
            quote(&format!("custom_provider:{provider_key}")),
            quote(&Self::provider_name(&provider)),
            quote(&base_url),
            quote(&api_format),
            quote(&model_id),
            quote(&key_env),
            opt(self.limits_command(&provider_key, &requested, &model_id)),
            quote(&config),
            quote(&format!("s|^defaultModel:.*|defaultModel: {execution_model}|")),
            quote(&config),
            quote(&config),
            quote(&runtime),
            quote(web_policy),
            quote(&runtime),
            opt(self.mcp_command()),
            opt(self.skills_command()),
        );
        let flags = self.max_steps.map(|n| format!(" --max-steps {n}")).unwrap_or_default();
        let resume_arg = if resume { " --continue" } else { "" };
        let run = format!(
            ". ~/.nvm/nvm.sh; mcode exec --model {} --config {} --permission {}{flags}{resume_arg} --output-format stream-json {} \
             2> >(tee /logs/agent/mcode.stderr >&2) < /dev/null | tee /logs/agent/mcode.jsonl",
            quote(&execution_model),
            quote(&runtime),
            quote(&self.permission_mode),
            quote(instruction)
        );
        Ok((setup, run, env))
    }
}

/// Sum assistant `usage` from mcode stream-json.
pub fn parse_usage(text: &str) -> Option<AgentContext> {
    let (mut input, mut cache, mut output, mut saw) = (0, 0, 0, false);
    for e in parse_jsonl(text) {
        let m = &e["message"];
        if e["type"] != "message" || m["role"] != "assistant" || !m["usage"].is_object() {
            continue;
        }
        saw = true;
        let u = &m["usage"];
        input += u["inputTokens"].as_i64().unwrap_or(0);
        cache += u["cacheReadTokens"].as_i64().unwrap_or(0);
        output += u["outputTokens"].as_i64().unwrap_or(0);
    }
    saw.then(|| AgentContext {
        n_input_tokens: Some(input + cache),
        n_cache_tokens: Some(cache),
        n_output_tokens: Some(output),
        ..Default::default()
    })
}

#[async_trait::async_trait]
impl Agent for MCode {
    fn name(&self) -> &str {
        "mcode"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        let spec = pins::MCODE.npm_spec(self.version.as_deref());
        let install = async {
            ensure_system_dependencies(cx, &["curl", "coreutils"]).await?;
            let cmd = format!("set -euo pipefail; {} && npm install -g {spec} && mcode --version", nvm_node_install_snippet(22));
            exec_agent(cx, &cmd, &BTreeMap::new()).await.map(|_| ())
        };
        installed_setup(cx, &mut self.version, install, ". ~/.nvm/nvm.sh; mcode --version", |s| s.trim().to_string()).await
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let (setup, run, env) = self.commands(&instruction, resume)?;
        exec_agent(cx, &setup, &env).await?;
        exec_agent(cx, &run, &env).await?;
        Ok(())
    }

    fn collect_usage(&self, logs_dir: &std::path::Path, ctx: &mut AgentContext) {
        if let Some(c) = std::fs::read_to_string(logs_dir.join("mcode.jsonl")).ok().and_then(|t| parse_usage(&t)) {
            ctx.n_input_tokens = c.n_input_tokens;
            ctx.n_cache_tokens = c.n_cache_tokens;
            ctx.n_output_tokens = c.n_output_tokens;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common::test_args as args;

    #[test]
    fn options_validation() {
        assert!(MCode::new(args(json!({"permission_mode": "yolo"}), &[], None)).is_err());
        assert!(MCode::new(args(json!({"api_format": "x"}), &[], None)).is_err());
        assert!(MCode::new(args(json!({"context_window": 0}), &[], None)).is_err());
        assert!(MCode::new(args(json!({"max_output_tokens": true}), &[], None)).is_err());
        assert!(MCode::new(args(json!({"max_steps": "abc"}), &[], None)).is_err());
        assert_eq!(title_case("my provider2x"), "My Provider2X");
    }

    #[test]
    fn commands() {
        let a = MCode::new(args(json!({"max_steps": 7}), &[("MINIMAX_API_KEY", "k")], Some("minimax/MiniMax-M3"))).unwrap();
        let (setup, run, env) = a.commands("do it", true).unwrap();
        assert_eq!(env["MINIMAX_API_KEY"], "k");
        assert_eq!(env["MINIMAX_DATA_DIR"], DATA_DIR);
        assert!(setup.contains("--name 'hbench MiniMax' --base-url https://api.minimax.io/anthropic --api-format anthropic-messages --model MiniMax-M3 --api-key-env MINIMAX_API_KEY"));
        assert!(setup.contains("context: 512000") && setup.contains("Warning: Could not apply known"));
        assert!(run.contains("--model custom_provider:hbench-minimax/MiniMax-M3 --config /tmp/hbench-mcode/hbench-config.yaml --permission full --max-steps 7 --continue --output-format stream-json 'do it'"));
        let no_key = MCode::new(args(json!({}), &[], Some("zzprov/m"))).unwrap();
        assert!(no_key.commands("x", false).unwrap_err().to_string().contains("ZZPROV_API_KEY is required"));
        assert!(MCode::new(args(json!({}), &[], Some("/MiniMax-M3"))).unwrap().commands("x", false).is_err());
    }

    #[test]
    fn usage() {
        let text = r#"{"type":"message","message":{"role":"user","usage":{"inputTokens":999}}}
{"type":"message","message":{"id":"assistant-1","role":"assistant","usage":{"totalTokens":127,"inputTokens":100,"outputTokens":7,"cacheReadTokens":20}}}
not-json
{"type":"message","message":{"id":"assistant-2","role":"assistant","usage":{"totalTokens":93,"inputTokens":10,"outputTokens":3,"cacheReadTokens":80}}}"#;
        let c = parse_usage(text).unwrap();
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens, c.cost_usd), (Some(210), Some(100), Some(10), None));
        assert!(parse_usage("not-json\n{\"type\":\"message\",\"message\":{\"role\":\"user\"}}").is_none());
    }
}
