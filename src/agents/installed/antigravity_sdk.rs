//! antigravity-sdk: Google's Antigravity SDK driven by an uploaded uv script (assets/agents/antigravity-sdk).

use super::antigravity_cli::validate_base_url;
use super::common::{installed_dir, skills_dir, uv_install};
use super::{ensure_system_dependencies, exec_agent, exec_root_checked};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const RUNNER: &str = include_str!("../../../assets/agents/antigravity-sdk/antigravity_sdk_runner.py");
const RUNNER_LOCK: &str = include_str!("../../../assets/agents/antigravity-sdk/antigravity_sdk_runner.py.lock");
const OUTPUT_FILENAME: &str = "antigravity_sdk.txt";
const TRAJECTORY_FILENAME: &str = "trajectory.json";
const WHEEL_ENV: &str = "ANTIGRAVITY_SDK_WHEEL_PATH";
const DEFAULT_SKILL_PATHS: &[&str] = &[
    "~/.openhands-sdk/skills",
    "~/.claude/skills",
    "~/.codex/skills",
    "~/.agents/skills",
    "~/.goose/skills",
    "~/.gemini/skills",
    "~/.factory/skills",
    "~/.opencode/skill",
];

/// USD per million tokens: (input, output, cache_read).
fn pricing(model: &str) -> Option<(f64, f64, f64)> {
    Some(match model {
        "gemini-3.5-flash" => (1.50, 9.00, 0.15),
        "gemini-3.1-flash-lite" => (0.25, 1.50, 0.025),
        "gemini-3.1-pro-preview" => (2.00, 12.00, 0.20),
        "gemini-2.5-pro" => (1.25, 10.00, 0.125),
        "gemini-2.5-flash" => (0.30, 2.50, 0.03),
        "gemini-2.5-flash-lite" => (0.10, 0.40, 0.01),
        _ => return None,
    })
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(AntigravitySdk::new(args)?))
}

pub struct AntigravitySdk {
    args: AgentArgs,
    version: Option<String>,
    load_skills: bool,
    skill_paths: Option<Vec<String>>,
}

/// Cost from the built-in price table, for trajectories that report none.
fn cost_from_pricing(model_name: Option<&str>, prompt: i64, completion: i64, cached: i64) -> Option<f64> {
    let (input, output, cache_read) = pricing(model_name?.rsplit('/').next()?)?;
    let uncached = (prompt - cached).max(0) as f64;
    Some((uncached * input + cached as f64 * cache_read + completion as f64 * output) / 1_000_000.0)
}

/// Usage from the runner's ATIF `trajectory.json` `final_metrics`.
pub fn parse_trajectory(text: &str, model_name: Option<&str>) -> Option<AgentContext> {
    let v: Value = serde_json::from_str(text).ok()?;
    let m = &v["final_metrics"];
    let n = |k: &str| m[k].as_i64().unwrap_or(0);
    let (input, output, cache) = (n("total_prompt_tokens"), n("total_completion_tokens"), n("total_cached_tokens"));
    let cost = m["total_cost_usd"].as_f64().filter(|c| *c != 0.0).or_else(|| cost_from_pricing(model_name, input, output, cache));
    Some(AgentContext {
        n_input_tokens: Some(input),
        n_cache_tokens: Some(cache),
        n_output_tokens: Some(output),
        cost_usd: cost,
        ..Default::default()
    })
}

impl AntigravitySdk {
    pub fn new(args: AgentArgs) -> Result<AntigravitySdk> {
        let load_skills = match args.kwargs.get("load_skills") {
            None => true,
            Some(_) => args.kw_bool("load_skills").ok_or_else(|| anyhow!("Invalid value for 'load_skills': expected a boolean"))?,
        };
        let skill_paths = match args.kwargs.get("skill_paths") {
            None | Some(Value::Null) => None,
            Some(Value::Array(a)) => Some(
                a.iter()
                    .map(|p| {
                        p.as_str().map(String::from).ok_or_else(|| anyhow!("Invalid value for 'skill_paths': expected a list of strings"))
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            Some(_) => bail!("Invalid value for 'skill_paths': expected a list of strings"),
        };
        Ok(AntigravitySdk { version: args.version_kw(), args, load_skills, skill_paths })
    }

    /// Optional host path to a local SDK wheel (ANTIGRAVITY_SDK_WHEEL_PATH).
    fn wheel_path(&self) -> Result<Option<PathBuf>> {
        let Some(raw) = self.args.get_env(WHEEL_ENV).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else { return Ok(None) };
        let expanded = match raw.strip_prefix("~/") {
            Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
            None => PathBuf::from(&raw),
        };
        let path = std::fs::canonicalize(&expanded).unwrap_or(expanded);
        if !path.is_file() {
            bail!("{WHEEL_ENV} points to a non-existent file: {}", path.display());
        }
        if path.extension().is_none_or(|e| e.to_string_lossy().to_lowercase() != "whl") {
            bail!("{WHEEL_ENV} must point to a .whl file: {}", path.display());
        }
        Ok(Some(path))
    }

    fn parse_version(&self, stdout: &str) -> String {
        if matches!(self.wheel_path(), Ok(Some(_))) {
            return "custom".into();
        }
        let t = stdout.trim();
        t.strip_prefix("Version:").map(|v| v.trim().to_string()).unwrap_or_else(|| t.to_string())
    }

    fn run_env(&self) -> Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::new();
        let Some(key) = self.args.get_env("GEMINI_API_KEY") else { bail!("GEMINI_API_KEY environment variable must be set") };
        env.insert("GEMINI_API_KEY".into(), key);
        if let Some(url) = self.args.get_env("GOOGLE_GEMINI_BASE_URL") {
            validate_base_url(&url, &|k| self.args.get_env(k))?;
            env.insert("GOOGLE_GEMINI_BASE_URL".into(), url);
        }
        let model = match &self.args.model_name {
            Some(m) => m.clone(),
            None => self.args.get_env("MODEL_NAME").ok_or_else(|| anyhow!("No LLM model specified"))?,
        };
        env.insert("MODEL_NAME".into(), model);
        if let Some(r) = self.args.kw("reasoning_effort") {
            env.insert("REASONING_EFFORT".into(), r);
        }
        env.insert("AGENT_LOGS_DIR".into(), "/logs/agent".into());
        env.insert("TRAJECTORY_PATH".into(), format!("/logs/agent/{TRAJECTORY_FILENAME}"));
        if let Some(w) = self.wheel_path()? {
            env.insert(WHEEL_ENV.into(), format!("/installed-agent/{}", w.file_name().unwrap().to_string_lossy()));
        }
        let mut skills: Vec<String> = vec![];
        if self.load_skills {
            skills.extend(skills_dir(&self.args));
            let defaults: Vec<String> = DEFAULT_SKILL_PATHS.iter().map(|s| s.to_string()).collect();
            for p in self.skill_paths.as_ref().filter(|p| !p.is_empty()).unwrap_or(&defaults) {
                if !skills.contains(p) {
                    skills.push(p.clone());
                }
            }
        }
        env.insert("SKILLS_PATHS_JSON".into(), serde_json::to_string(&skills)?);
        if !self.args.mcp_servers.is_empty() {
            let list: Vec<Value> = self
                .args
                .mcp_servers
                .iter()
                .map(|s| {
                    let mut e = json!({"name": s.name, "transport": s.transport()});
                    if s.transport() == "stdio" {
                        if let Some(c) = &s.command {
                            e["command"] = json!(c);
                        }
                        if !s.args.is_empty() {
                            e["args"] = json!(s.args);
                        }
                    } else if let Some(u) = &s.url {
                        e["url"] = json!(u);
                    }
                    e
                })
                .collect();
            env.insert("MCP_SERVERS_JSON".into(), serde_json::to_string(&list)?);
        }
        Ok(env)
    }

    fn run_command(instruction: &str) -> String {
        format!(
            "/installed-agent/run_agent.py     --instruction={}     --logs-dir=\"$AGENT_LOGS_DIR\"     --trajectory-path=\"$TRAJECTORY_PATH\"     2>&1 | stdbuf -oL tee /logs/agent/{OUTPUT_FILENAME}",
            quote(instruction)
        )
    }

    /// The runner with its `google-antigravity` dependency pinned to an uploaded wheel, if any.
    /// Pinned to Python 3.12. With only `requires-python = ">=3.12"`, uv now downloads CPython 3.15,
    /// for which cffi/pydantic-core have no wheels, so the install needs a C compiler most task images lack.
    fn runner_script(remote_wheel: Option<&str>) -> String {
        let runner = RUNNER.replacen("#!/usr/bin/env -S uv run --script", "#!/usr/bin/env -S uv run --python 3.12 --script", 1);
        match remote_wheel {
            None => runner,
            Some(w) => regex::Regex::new(r#""google-antigravity[^"]*""#)
                .unwrap()
                .replace(&runner, regex::NoExpand(&format!("\"google-antigravity @ file://{w}\"")))
                .into_owned(),
        }
    }

    async fn install(&self, cx: &AgentCtx<'_>) -> Result<()> {
        let wheel = self.wheel_path()?;
        if cx.exec("command -v uv >/dev/null 2>&1", &BTreeMap::new(), None).await?.code != 0 {
            ensure_system_dependencies(cx, &["curl", "ca_certificates", "python3"]).await?;
            exec_root_checked(cx, &uv_install(Some("/usr/local/bin")), &BTreeMap::new()).await?;
        }
        let remote_wheel = wheel.as_ref().map(|w| format!("/installed-agent/{}", w.file_name().unwrap().to_string_lossy()));
        if let (Some(w), Some(r)) = (&wheel, &remote_wheel) {
            cx.env.upload_file(w, r).await?;
        }
        let local = cx.logs_dir.join("run_agent.py");
        std::fs::write(&local, Self::runner_script(remote_wheel.as_deref()))?;
        cx.env.upload_file(&local, "/installed-agent/run_agent.py").await?;
        if wheel.is_none() {
            let tmp = std::env::temp_dir().join(format!("hbench-agy-sdk-{}.lock", uuid::Uuid::new_v4().simple()));
            std::fs::write(&tmp, RUNNER_LOCK)?;
            let r = cx.env.upload_file(&tmp, "/installed-agent/run_agent.py.lock").await;
            let _ = std::fs::remove_file(&tmp);
            r?;
        }
        cx.env.exec_root("chmod -R 777 /installed-agent && chmod +x /installed-agent/run_agent.py", None).await?;
        // Pre-warm the uv script environment so downloads don't count against run().
        exec_agent(cx, "/installed-agent/run_agent.py --version", &BTreeMap::new()).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Agent for AntigravitySdk {
    fn name(&self) -> &str {
        "antigravity-sdk"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        // Exec failures pass through unchanged; anything else is wrapped as an install failure.
        self.install(cx)
            .await
            .map_err(|e| if e.downcast_ref::<crate::env::KindError>().is_some() { e } else { anyhow!("Agent install failed: {e:#}") })?;
        if self.version.is_none() {
            let r = cx.exec("/installed-agent/run_agent.py --version", &BTreeMap::new(), None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(self.parse_version(&r.stdout));
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let env = self.run_env()?;
        exec_agent(cx, &Self::run_command(&instruction), &env).await?;
        Ok(())
    }

    fn collect_usage(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        if let Some(c) = std::fs::read_to_string(logs_dir.join(TRAJECTORY_FILENAME))
            .ok()
            .and_then(|t| parse_trajectory(&t, self.args.model_name.as_deref()))
        {
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
    use crate::agents::installed::fx::tests::args;

    fn agent(kwargs: Value, env: &[(&str, &str)]) -> AntigravitySdk {
        AntigravitySdk::new(args(kwargs, env, Some("google/gemini-3.5-flash"), vec![])).unwrap()
    }

    #[test]
    fn run_env_and_command() {
        let a =
            agent(json!({"reasoning_effort": "high"}), &[("GEMINI_API_KEY", "k"), ("GOOGLE_GEMINI_BASE_URL", "https://via-ae.test/genai")]);
        let env = a.run_env().unwrap();
        assert_eq!(env["GOOGLE_GEMINI_BASE_URL"], "https://via-ae.test/genai");
        assert_eq!(env["MODEL_NAME"], "google/gemini-3.5-flash");
        assert_eq!(env["REASONING_EFFORT"], "high");
        assert_eq!(env["TRAJECTORY_PATH"], "/logs/agent/trajectory.json");
        let skills: Vec<String> = serde_json::from_str(&env["SKILLS_PATHS_JSON"]).unwrap();
        assert_eq!(skills.len(), DEFAULT_SKILL_PATHS.len());
        assert!(!env.contains_key("MCP_SERVERS_JSON"));
        let c = AntigravitySdk::run_command("do it");
        assert!(c.starts_with("/installed-agent/run_agent.py     --instruction='do it'     --logs-dir=\"$AGENT_LOGS_DIR\""));
        assert!(c.ends_with("2>&1 | stdbuf -oL tee /logs/agent/antigravity_sdk.txt"));

        let custom = agent(json!({"skill_paths": ["/a", "/a", "/b"]}), &[("GEMINI_API_KEY", "k")]);
        assert_eq!(custom.run_env().unwrap()["SKILLS_PATHS_JSON"], r#"["/a","/b"]"#);
        let off = agent(json!({"load_skills": false}), &[("GEMINI_API_KEY", "k")]);
        assert_eq!(off.run_env().unwrap()["SKILLS_PATHS_JSON"], "[]");
        assert!(AntigravitySdk::new(args(json!({"skill_paths": "x"}), &[], None, vec![])).is_err());
    }

    #[test]
    fn base_url_policy() {
        let none = |_: &str| None;
        let allow = |_: &str| Some("true".to_string());
        assert!(validate_base_url("https://proxy.test/genai", &none).is_ok());
        assert!(validate_base_url("https://[::1]:8443/x", &none).is_ok());
        let e = validate_base_url("not-a-url-secret", &none).unwrap_err().to_string();
        assert!(e.contains("GOOGLE_GEMINI_BASE_URL") && !e.contains("not-a-url-secret"));
        assert!(validate_base_url("https://user:pass@proxy.test/genai", &none).unwrap_err().to_string().contains("credentials"));
        assert!(validate_base_url("https://x.test#frag", &none).is_err());
        assert!(validate_base_url("https://x.test:0", &none).is_err());
        assert!(validate_base_url("https://x.test:99999", &none).is_err());
        assert!(validate_base_url("https://a b", &none).unwrap_err().to_string().contains("whitespace"));
        assert!(
            validate_base_url("http://host.docker.internal:8080", &none).unwrap_err().to_string().contains("ALLOW_INSECURE_MODEL_BASE_URL")
        );
        assert!(validate_base_url("http://host.docker.internal:8080", &allow).is_ok());
        assert!(validate_base_url("ftp://x.test", &none).is_err());
    }

    #[test]
    fn trajectory_usage_and_pricing() {
        let t = r#"{"schema_version":"ATIF-v1.6","final_metrics":{"total_prompt_tokens":1000000,"total_completion_tokens":100000,"total_cached_tokens":200000,"total_steps":4}}"#;
        let c = parse_trajectory(t, Some("google/gemini-3.5-flash")).unwrap();
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens), (Some(1_000_000), Some(200_000), Some(100_000)));
        // 800k * 1.50 + 200k * 0.15 + 100k * 9.00, per million
        assert!((c.cost_usd.unwrap() - (1.2 + 0.03 + 0.9)).abs() < 1e-9);
        let explicit = r#"{"final_metrics":{"total_prompt_tokens":5,"total_cost_usd":0.5}}"#;
        assert_eq!(parse_trajectory(explicit, Some("google/unknown")).unwrap().cost_usd, Some(0.5));
        assert_eq!(parse_trajectory(r#"{"final_metrics":{}}"#, Some("google/unknown")).unwrap().cost_usd, None);
    }

    #[test]
    fn runner_wheel_pin_and_version() {
        assert!(RUNNER.contains("\"google-antigravity>=0.1.20\""));
        let s = AntigravitySdk::runner_script(Some("/installed-agent/g.whl"));
        assert!(s.contains("\"google-antigravity @ file:///installed-agent/g.whl\"") && !s.contains("google-antigravity>=0.1.20"));
        assert!(RUNNER_LOCK.len() > 1000);
        let a = agent(json!({}), &[]);
        assert_eq!(a.parse_version("Version: 0.1.22\n"), "0.1.22");
    }
}
