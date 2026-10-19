//! Strands: runs a user factory (`module:symbol`) that returns a `strands.Agent`, via assets/agents/strands/strands_runner.py.

use super::common::{installed_dir, upload_asset, uv_install};
use super::pins;
use super::{connection, ensure_system_dependencies, exec_agent, exec_root, upload_text};
use crate::agents::model_connection::{Resolved, Spec, with_canonical_provider_envs};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const RUNNER: &str = include_str!("../../../assets/agents/strands/strands_runner.py");
const INSTALL_DIR: &str = "/installed-agent/strands";
const FACTORY_DIR: &str = "/installed-agent/strands/factory";
const RUNNER_PATH: &str = "/installed-agent/strands/strands_runner.py";
const VENV_DIR: &str = "/installed-agent/strands/venv";
const PYTHON: &str = "/installed-agent/strands/venv/bin/python";
const INSTRUCTION_PATH: &str = "/installed-agent/strands/instruction.txt";
const CONFIG_PATH: &str = "/installed-agent/strands/config.json";
const RESULT_PATH: &str = "/logs/agent/strands-result.json";
const EVENTS_PATH: &str = "/logs/agent/strands-events.jsonl";
const ANTHROPIC_MODEL_PROVIDER: &str = "strands.models.anthropic.AnthropicModel";
const IGNORE: [&str; 6] = [".git", ".venv", "__pycache__", ".pytest_cache", ".ruff_cache", ".env"];

/// PEP 440 subset normalization (`str(Version(v))` for common release/pre/post/dev forms).
fn normalize_version(v: &str) -> Result<String> {
    let re =
        Regex::new(r"^v?(\d+(?:\.\d+)*)(?:[-_.]?(a|b|rc|alpha|beta|c)[-_.]?(\d*))?(?:[-_.]?post[-_.]?(\d*))?(?:[-_.]?dev[-_.]?(\d*))?$")
            .unwrap();
    let lower = v.trim().to_lowercase();
    let Some(c) = re.captures(&lower) else { bail!("version must be an exact PEP 440 version, for example '1.54.0'") };
    let release: Vec<String> = c[1].split('.').map(|p| p.parse::<u64>().unwrap_or(0).to_string()).collect();
    let mut out = release.join(".");
    if let Some(pre) = c.get(2) {
        let tag = match pre.as_str() {
            "alpha" => "a",
            "beta" => "b",
            "c" => "rc",
            t => t,
        };
        out += &format!("{tag}{}", c.get(3).and_then(|n| n.as_str().parse::<u64>().ok()).unwrap_or(0));
    }
    if let Some(p) = c.get(4) {
        out += &format!(".post{}", p.as_str().parse::<u64>().unwrap_or(0));
    }
    if let Some(d) = c.get(5) {
        out += &format!(".dev{}", d.as_str().parse::<u64>().unwrap_or(0));
    }
    Ok(out)
}

/// Kwarg dicts must be plain JSON (numbers are finite by construction).
fn validate_json(v: &Value, path: &str) -> Result<()> {
    match v {
        Value::Object(o) => o.iter().try_for_each(|(k, c)| validate_json(c, &format!("{path}.{k}"))),
        Value::Array(a) => a.iter().enumerate().try_for_each(|(i, c)| validate_json(c, &format!("{path}[{i}]"))),
        _ => Ok(()), // serde_json numbers are always finite
    }
}

fn ignored(name: &str) -> bool {
    IGNORE.contains(&name) || name.starts_with(".env.")
}

/// Copy `src` into `dst` minus VCS/venv/cache/.env entries; symlinks are rejected.
fn copy_tree(src: &Path, dst: &Path, root: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name();
        if ignored(&name.to_string_lossy()) {
            continue;
        }
        let p = e.path();
        let ft = std::fs::symlink_metadata(&p)?.file_type();
        if ft.is_symlink() {
            bail!("Strands factory_path must not contain symbolic links: {}", p.strip_prefix(root).unwrap_or(&p).display());
        }
        if ft.is_dir() {
            copy_tree(&p, &dst.join(&name), root)?;
        } else {
            std::fs::copy(&p, dst.join(&name))?;
        }
    }
    Ok(())
}

pub struct Strands {
    args: AgentArgs,
    version: Option<String>,
    factory: String,
    factory_path: Option<PathBuf>,
    factory_kwargs: Value,
    model_kwargs: Value,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(Strands::new(args)?))
}

impl Strands {
    fn new(args: AgentArgs) -> Result<Strands> {
        let version = args.version_kw().map(|v| normalize_version(&v)).transpose()?;
        let json_opt = |k: &str| -> Result<Value> {
            match args.kwargs.get(k) {
                None | Some(Value::Null) => Ok(json!({})),
                Some(v @ Value::Object(_)) => validate_json(v, k).map(|_| v.clone()),
                Some(_) => bail!("Invalid kwargs for agent 'strands':\n  {k}: Input should be a valid dictionary"),
            }
        };
        let factory_path = match args.kw("factory_path") {
            None => None,
            Some(p) => {
                let expanded = match p.strip_prefix("~/") {
                    Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
                    None => PathBuf::from(&p),
                };
                let Ok(abs) = std::fs::canonicalize(&expanded) else {
                    bail!("Strands factory_path does not exist: {}", expanded.display())
                };
                Some(abs)
            }
        };
        Ok(Strands {
            factory: args.kw("factory").unwrap_or_else(|| "agent:create_agent".into()),
            factory_kwargs: json_opt("factory_kwargs")?,
            model_kwargs: json_opt("model_kwargs")?,
            factory_path,
            version,
            args,
        })
    }

    fn connection(&self) -> Resolved {
        let spec =
            Spec { default_provider: self.args.model_name.is_none().then_some("amazon-bedrock"), passthrough: true, ..Default::default() };
        with_canonical_provider_envs(connection(&self.args, spec))
    }

    fn model_id(&self) -> Option<String> {
        self.args.provider_model().1
    }

    fn mcp_config(&self) -> Value {
        let mut m = Map::new();
        for s in &self.args.mcp_servers {
            let mut e = Map::from_iter([("transport".to_string(), json!(s.transport()))]);
            if let Some(u) = &s.url {
                e.insert("url".into(), json!(u));
            }
            if let Some(c) = &s.command {
                e.insert("command".into(), json!(c));
            }
            if !s.args.is_empty() {
                e.insert("args".into(), json!(s.args));
            }
            m.insert(s.name.clone(), Value::Object(e));
        }
        Value::Object(m)
    }

    fn runner_config(&self, c: &Resolved) -> Value {
        json!({
            "factory": self.factory,
            "factory_kwargs": self.factory_kwargs,
            "model_kwargs": self.model_kwargs,
            "mcp_servers": self.mcp_config(),
            "model_name": self.args.model_name,
            "model_id": self.model_id(),
            "model_provider": c.provider,
            "model_base_url": c.base_url,
            "workdir": self.args.task.config.environment.workdir,
        })
    }

    fn runner_env(&self, c: &Resolved) -> BTreeMap<String, String> {
        let mut env = c.env.clone();
        env.insert("PYTHONPATH".into(), FACTORY_DIR.into());
        env.insert("HBENCH_MODEL".into(), self.args.model_name.clone().unwrap_or_default());
        env.insert("HBENCH_MODEL_ID".into(), self.model_id().unwrap_or_default());
        env.insert("HBENCH_MODEL_PROVIDER".into(), c.provider.clone().unwrap_or_default());
        env
    }

    fn install_command(&self, editable_factory: bool) -> String {
        let mut deps = vec![format!("strands-agents[all]{}", pins::STRANDS.pip_constraint(self.version.as_deref()))];
        if editable_factory {
            deps.extend(["-e".into(), FACTORY_DIR.into()]);
        }
        let deps = deps.iter().map(|d| quote(d)).collect::<Vec<_>>().join(" ");
        format!(
            "set -euo pipefail; {uv_install}; if [ -f \"$HOME/.local/bin/env\" ]; then . \"$HOME/.local/bin/env\"; fi; \
             uv python install 3.12; uv venv {VENV_DIR} --clear --python 3.12; uv pip install --python {PYTHON} {deps}",
            uv_install = uv_install(None)
        )
    }
}

/// Strands usage -> (prompt, completion, cached, cache_write); None when no usage keys are present.
fn usage_counts(u: &Value, provider: Option<&str>) -> Option<(i64, Option<i64>, i64, i64)> {
    if !["inputTokens", "cacheReadInputTokens", "cacheWriteInputTokens", "outputTokens"].iter().any(|k| !u[*k].is_null()) {
        return None;
    }
    let n = |k: &str| u[k].as_i64().unwrap_or(0);
    let (input, output, cr, cw) = (n("inputTokens"), n("outputTokens"), n("cacheReadInputTokens"), n("cacheWriteInputTokens"));
    let separate = provider == Some(ANTHROPIC_MODEL_PROVIDER)
        || u["totalTokens"].is_null()
        || u["totalTokens"].as_i64() == Some(input + output + cr + cw);
    Some((input + if separate { cr + cw } else { 0 }, u["outputTokens"].as_i64(), cr, cw))
}

/// Strands result + events -> AgentContext (final_metrics totals + metadata).
pub fn parse_context(result: Option<&str>, events: Option<&str>) -> AgentContext {
    let result = result.and_then(|r| serde_json::from_str::<Value>(r).ok()).filter(Value::is_object).unwrap_or_else(
        || json!({"output": "", "usage": {}, "error": {"type": "MissingResult", "message": "Strands result was unavailable"}}),
    );
    let provider = result["observed_model"]["provider"].as_str();
    let mut totals: (Option<i64>, Option<i64>, Option<i64>) = (None, None, None);
    if let Some((p, c, cr, _)) = usage_counts(&result["usage"], provider) {
        totals = (Some(p), c, Some(cr));
    } else {
        // Sum assistant message usage from the valid prefix of the event log (non-preexisting only).
        let add = |acc: &mut Option<i64>, v: Option<i64>| {
            if let Some(v) = v {
                *acc = Some(acc.unwrap_or(0) + v);
            }
        };
        for line in events.unwrap_or_default().lines().filter(|l| !l.trim().is_empty()) {
            let Ok(e) = serde_json::from_str::<Value>(line) else { break };
            let m = &e["message"];
            if e["kind"] != "message" || m["role"] != "assistant" || !m["content"].is_array() || e["is_preexisting_context"] == true {
                continue;
            }
            if let Some((p, c, cr, _)) =
                m["metadata"]["usage"].is_object().then(|| usage_counts(&m["metadata"]["usage"], provider)).flatten()
            {
                add(&mut totals.0, Some(p));
                add(&mut totals.1, c);
                add(&mut totals.2, Some(cr));
            }
        }
    }
    AgentContext {
        n_input_tokens: totals.0,
        n_output_tokens: totals.1,
        n_cache_tokens: totals.2,
        metadata: Some(json!({
            "output": if result["output"].is_null() || result["output"] == "" { json!("") } else { result["output"].clone() },
            "observed_model": result["observed_model"],
            "stop_reason": result["stop_reason"],
            "error": result["error"],
        })),
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl Agent for Strands {
    fn name(&self) -> &str {
        "strands"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        let none = BTreeMap::new();
        ensure_system_dependencies(cx, &["curl", "git"]).await?;
        exec_root(cx, &format!("mkdir -p {INSTALL_DIR}"), &none).await?;
        upload_asset(cx, RUNNER, RUNNER_PATH).await?;
        // Stage the factory under logs/source (kept with the trial for reference), then upload it.
        let mut editable = false;
        if let Some(fp) = &self.factory_path {
            let target = self.args.logs_dir.join("source");
            let _ = std::fs::remove_dir_all(&target);
            let _ = std::fs::remove_file(&target);
            if fp.is_dir() {
                if let Err(e) = copy_tree(fp, &target, fp) {
                    let _ = std::fs::remove_dir_all(&target);
                    return Err(e);
                }
                editable = fp.join("pyproject.toml").is_file();
            } else {
                std::fs::create_dir_all(&target)?;
                std::fs::copy(fp, target.join(fp.file_name().unwrap_or_default()))?;
            }
            cx.env.upload_dir(&target, FACTORY_DIR).await?;
        }
        if let Some(u) = &cx.user {
            exec_root(cx, &format!("chown -R {} {INSTALL_DIR}", quote(u)), &none).await?;
        }
        exec_agent(cx, &self.install_command(editable), &none).await?;
        if self.version.is_none()
            && let Ok(r) = cx
                .exec(
                    &format!("{PYTHON} -c \"import importlib.metadata; print(importlib.metadata.version('strands-agents'))\""),
                    &none,
                    None,
                )
                .await
            && r.code == 0
            && !r.stdout.is_empty()
        {
            self.version = Some(r.stdout.trim().to_string());
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let _ = std::fs::write(cx.logs_dir.join("instruction.txt"), &instruction);
        upload_asset(cx, &instruction, INSTRUCTION_PATH).await?;
        if let Some(u) = &cx.user {
            exec_root(cx, &format!("chown {} {INSTRUCTION_PATH}", quote(u)), &BTreeMap::new()).await?;
        }
        let conn = self.connection();
        upload_text(cx, CONFIG_PATH, &(serde_json::to_string_pretty(&self.runner_config(&conn))? + "\n")).await?;
        let cmd = format!(
            "rm -f {RESULT_PATH} {EVENTS_PATH}; exec {PYTHON} {RUNNER_PATH} --config {CONFIG_PATH} --instruction {INSTRUCTION_PATH} --result {RESULT_PATH} --events {EVENTS_PATH}"
        );
        let r = exec_agent(cx, &cmd, &self.runner_env(&conn)).await;
        // Always leave a result artifact, even when the runner died early.
        let fallback = r#"{"output":"","usage":{},"error":{"type":"IncompleteInvocation","message":"Strands runner stopped before writing a result"}}"#;
        let _ = cx
            .exec(
                &format!("test -f {RESULT_PATH} || printf %s {} > {RESULT_PATH}", quote(fallback)),
                &BTreeMap::new(),
                Some(Duration::from_secs(5)),
            )
            .await;
        r.map(|_| ())
    }

    fn collect_usage(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        let result = std::fs::read_to_string(logs_dir.join("strands-result.json")).ok();
        let events = std::fs::read_to_string(logs_dir.join("strands-events.jsonl")).ok();
        let c = parse_context(result.as_deref(), events.as_deref());
        ctx.n_input_tokens = c.n_input_tokens;
        ctx.n_cache_tokens = c.n_cache_tokens;
        ctx.n_output_tokens = c.n_output_tokens;
        ctx.metadata = c.metadata;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common::test_args as args;

    #[test]
    fn options_and_install() {
        assert_eq!(normalize_version(" v1.54 ").unwrap(), "1.54");
        assert_eq!(normalize_version("1.0RC1").unwrap(), "1.0rc1");
        assert!(normalize_version("latest").is_err());
        assert!(Strands::new(args(json!({"version": "nope"}), &[], None)).is_err());
        assert!(Strands::new(args(json!({"factory_path": "/definitely/missing/path"}), &[], None)).is_err());
        assert!(Strands::new(args(json!({"factory_kwargs": [1]}), &[], None)).is_err());
        let s = Strands::new(args(
            json!({"version": "1.55.0", "factory_kwargs": {"a": [1, {"b": null}]}}),
            &[("AWS_ACCESS_KEY_ID", "ak"), ("AWS_REGION", "us-west-2")],
            None,
        ))
        .unwrap();
        assert!(s.install_command(true).ends_with("uv pip install --python /installed-agent/strands/venv/bin/python 'strands-agents[all]==1.55.0' -e /installed-agent/strands/factory"));
        let conn = s.connection();
        assert_eq!(conn.provider.as_deref(), Some("amazon-bedrock"));
        assert_eq!(conn.env["AWS_REGION"], "us-west-2");
        let cfg = s.runner_config(&conn);
        assert_eq!(cfg["factory"], "agent:create_agent");
        assert_eq!(cfg["model_provider"], "amazon-bedrock");
        assert_eq!(s.runner_env(&conn)["PYTHONPATH"], FACTORY_DIR);
    }

    #[test]
    fn factory_staging_rejects_symlinks_and_ignores() {
        let src = std::env::temp_dir().join(format!("hbench-strands-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(src.join("__pycache__")).unwrap();
        std::fs::write(src.join("agent.py"), "x").unwrap();
        std::fs::write(src.join(".env.local"), "secret").unwrap();
        let dst = src.with_extension("out");
        copy_tree(&src, &dst, &src).unwrap();
        assert!(dst.join("agent.py").exists() && !dst.join(".env.local").exists() && !dst.join("__pycache__").exists());
        std::os::unix::fs::symlink("/etc/hosts", src.join("link")).unwrap();
        assert!(copy_tree(&src, &src.with_extension("out2"), &src).is_err());
    }

    #[test]
    fn usage() {
        let u = |p: Option<&str>, total: Value| {
            let r = json!({"usage": {"inputTokens": 10, "cacheReadInputTokens": 2, "cacheWriteInputTokens": 1, "outputTokens": 4, "totalTokens": total}, "observed_model": {"provider": p}});
            let c = parse_context(Some(&r.to_string()), None);
            (c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens)
        };
        assert_eq!(u(Some("strands.models.openai.OpenAIModel"), json!(14)), (Some(10), Some(2), Some(4)));
        assert_eq!(u(Some(ANTHROPIC_MODEL_PROVIDER), json!(14)), (Some(13), Some(2), Some(4)));
        assert_eq!(u(None, json!(17)), (Some(13), Some(2), Some(4)));
        assert_eq!(u(None, Value::Null), (Some(13), Some(2), Some(4)));

        let events = concat!(
            r#"{"kind":"message","timestamp":"t","message":{"role":"assistant","content":[{"text":"Partial answer"}],"metadata":{"usage":{"inputTokens":3,"cacheReadInputTokens":1,"outputTokens":2}}}}"#,
            "\n{\"kind\":\"message\"\n",
            r#"{"kind":"message","message":{"role":"assistant","content":[{"text":"x"}],"metadata":{"usage":{"inputTokens":100,"outputTokens":100}}}}"#
        );
        let c = parse_context(Some(r#"{"output":"","usage":{},"error":{"type":"Timeout"}}"#), Some(events));
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens), (Some(4), Some(1), Some(2)));
        assert_eq!(c.metadata.unwrap()["error"]["type"], "Timeout");
        let missing = parse_context(None, None);
        assert_eq!(missing.n_input_tokens, None);
        assert_eq!(missing.metadata.unwrap()["error"]["type"], "MissingResult");
        assert!(RUNNER.contains("def main()"));
    }
}
