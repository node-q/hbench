//! Gemini CLI, incl. ACP target mode.

use super::common::{detect_version, installed_dir, nvm_node_install_snippet, opts, parse_bool_env, skills_dir};
use super::pins;
use super::{connection, ensure_system_dependencies, exec_agent, exec_root, pinned_bin_wrapper_command, safe_bin_symlink_command};
use crate::agents::model_connection::{Resolved, Spec};
use crate::agents::pricing;
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SPEC: Spec = Spec {
    default_provider: Some("google"),
    api_key_envs: &["GEMINI_API_KEY"],
    base_url_envs: &["GOOGLE_GEMINI_BASE_URL"],
    passthrough: true,
};
const SECRETS_DIR: &str = "/tmp/gemini-secrets";

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(GeminiCli::new(args)?))
}

pub struct GeminiCli {
    args: AgentArgs,
    version: Option<String>,
}

impl GeminiCli {
    fn new(args: AgentArgs) -> Result<GeminiCli> {
        let a = GeminiCli { version: args.version_kw(), args };
        a.cli_flags()?;
        a.disable_web_search()?;
        if let (Some(effort), Some(model)) = (a.effort()?, &a.args.model_name) {
            if model.contains("2.5") {
                bail!("Gemini 2.5 models do not support reasoning_effort. Use a Gemini 3 model, or add explicit thinking_budget support.");
            }
            if ["minimal", "medium"].contains(&effort.as_str()) && !model.contains("flash") {
                bail!(
                    "Gemini model '{model}' does not support reasoning_effort='{effort}'. Use 'low' or 'high', or choose a Gemini 3 Flash model."
                );
            }
        }
        Ok(a)
    }

    fn effort(&self) -> Result<Option<String>> {
        opts::choice(&self.args, "reasoning_effort", None, &["minimal", "low", "medium", "high"])
    }
    fn disable_web_search(&self) -> Result<bool> {
        Ok(opts::boolean(&self.args, "disable_web_search", None)?.unwrap_or(false))
    }
    fn cli_flags(&self) -> Result<String> {
        Ok(if opts::boolean(&self.args, "sandbox", None)? == Some(true) { "--sandbox".into() } else { String::new() })
    }

    /// OAuth / Vertex routing has no standard Gemini API endpoint to infer.
    fn connection(&self) -> Resolved {
        let mut c = connection(&self.args, SPEC);
        let opaque = self.args.get_env("GEMINI_OAUTH_CREDS_PATH").is_some()
            || ["GEMINI_FORCE_OAUTH", "GOOGLE_GENAI_USE_VERTEXAI"].iter().any(|n| parse_bool_env(self.args.get_env(n), n).unwrap_or(false));
        if opaque {
            c.provider = None;
            c.base_url = None;
        }
        c
    }

    /// GEMINI_OAUTH_CREDS_PATH=<path> or GEMINI_FORCE_OAUTH=<truthy> (~/.gemini/oauth_creds.json).
    fn oauth_creds_path(&self) -> Result<Option<PathBuf>> {
        if let Some(p) = self.args.get_env("GEMINI_OAUTH_CREDS_PATH") {
            if !Path::new(&p).is_file() {
                bail!("GEMINI_OAUTH_CREDS_PATH points to non-existent file: {p}");
            }
            return Ok(Some(p.into()));
        }
        if parse_bool_env(self.args.get_env("GEMINI_FORCE_OAUTH"), "GEMINI_FORCE_OAUTH")? {
            let d = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".gemini/oauth_creds.json");
            if !d.is_file() {
                bail!("GEMINI_FORCE_OAUTH is set but {} does not exist", d.display());
            }
            return Ok(Some(d));
        }
        Ok(None)
    }

    fn auth_env(&self, oauth: bool) -> BTreeMap<String, String> {
        if oauth {
            // Don't leak an API key alongside OAuth; keep the project for Workspace logins.
            ["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_LOCATION"]
                .iter()
                .filter_map(|k| self.args.get_env(k).map(|v| (k.to_string(), v)))
                .collect()
        } else {
            self.connection().env
        }
    }

    fn env_auth_type(&self) -> Option<&'static str> {
        let c = self.connection();
        if c.provider.is_none() {
            Some("vertex-ai")
        } else if c.api_key.as_deref().is_some_and(|k| !k.is_empty()) {
            Some("gemini-api-key")
        } else {
            None
        }
    }

    /// settings.json content and the `--model` alias carrying the reasoning effort.
    fn settings(&self, model: Option<&str>, auth_type: Option<&str>) -> Result<(Option<Value>, Option<String>)> {
        let effort = self.effort()?;
        let alias = model.filter(|m| !m.is_empty()).zip(effort.as_ref()).map(|(m, e)| format!("hbench-{m}-{e}"));
        let mut c = serde_json::Map::new();
        if let Some(t) = auth_type {
            // Headless mode can't show the auth dialog; pin the method.
            c.insert("security".into(), json!({"auth": {"selectedType": t}}));
        }
        if !self.args.mcp_servers.is_empty() {
            let mut servers = serde_json::Map::new();
            for s in &self.args.mcp_servers {
                let v = match s.transport() {
                    "stdio" => json!({"command": s.command, "args": s.args}),
                    "streamable-http" => json!({"httpUrl": s.url}),
                    _ => json!({"url": s.url}),
                };
                servers.insert(s.name.clone(), v);
            }
            c.insert("mcpServers".into(), Value::Object(servers));
        }
        if let (Some(a), Some(e)) = (&alias, &effort) {
            c.insert(
                "modelConfigs".into(),
                json!({"customAliases": {a: {"modelConfig": {"model": model, "generateContentConfig": {"thinkingConfig": {"includeThoughts": true, "thinkingLevel": e.to_uppercase()}}}}}}),
            );
        }
        if self.disable_web_search()? {
            c.insert("tools".into(), json!({"exclude": ["google_web_search", "web_fetch"]}));
        }
        if c.is_empty() {
            return Ok((None, None));
        }
        c.insert("experimental".into(), json!({"skills": true}));
        Ok((Some(Value::Object(c)), alias))
    }

    fn short_model(&self) -> Option<String> {
        self.args.model_name.as_deref().map(|m| m.rsplit('/').next().unwrap().to_string())
    }

    fn settings_command(&self, model: Option<&str>, auth_type: Option<&str>) -> Result<(Option<String>, Option<String>)> {
        let (s, alias) = self.settings(model, auth_type)?;
        let cmd = s.map(|s| {
            format!("mkdir -p ~/.gemini && printf %s {} > ~/.gemini/settings.json", quote(&serde_json::to_string_pretty(&s).unwrap()))
        });
        Ok((cmd, alias))
    }

    /// Upload oauth_creds.json (lands as root) into ~/.gemini with 0600 perms.
    async fn inject_oauth_creds(&self, cx: &AgentCtx<'_>, creds: &Path, env: &BTreeMap<String, String>) -> Result<()> {
        let tmp = format!("{SECRETS_DIR}/oauth_creds.json");
        exec_agent(cx, &format!("mkdir -p {} ~/.gemini", quote(SECRETS_DIR)), env).await?;
        cx.env.upload_file(creds, &tmp).await?;
        if let Some(u) = &cx.user {
            exec_root(cx, &format!("chown {u} {}", quote(&tmp)), &BTreeMap::new()).await?;
        }
        exec_agent(cx, &format!("cp {} ~/.gemini/oauth_creds.json\nchmod 600 ~/.gemini/oauth_creds.json", quote(&tmp)), env)
            .await
            .map(|_| ())
    }
}

/// Cost: the session has no cost field, so back it out of
/// LiteLLM's table (cache reads fall back to the input rate). None on a pricing miss.
pub fn gemini_cost(model: &str, prompt: i64, completion: i64, cached: i64) -> Option<f64> {
    let p = pricing::model_cost(&pricing::pricing_key(model)?)?;
    let rate = |k: &str| p.get(k).and_then(Value::as_f64).filter(|v| *v != 0.0);
    let input = rate("input_cost_per_token").unwrap_or(0.0);
    let output = rate("output_cost_per_token").unwrap_or(0.0);
    let cache_read = rate("cache_read_input_token_cost").unwrap_or(input);
    Some((prompt - cached).max(0) as f64 * input + cached as f64 * cache_read + completion as f64 * output)
}

/// Gemini CLI session: legacy single JSON blob, or v0.40+ JSONL records normalized to `{sessionId, messages}`.
pub fn load_gemini_session(text: &str) -> Option<Value> {
    if text.trim().is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(text)
        && v.get("messages").is_some()
    {
        return Some(v);
    }
    fn merge_update(m: &mut Value, u: &Value) {
        for (k, v) in u.as_object().into_iter().flatten() {
            if k == "type" || k == "id" {
                continue;
            }
            match (m.get_mut(k), v) {
                (Some(Value::Object(cur)), Value::Object(new)) => cur.extend(new.clone()),
                _ => {
                    m[k] = v.clone();
                }
            }
        }
    }
    let mut meta = serde_json::Map::new();
    let mut ids: Vec<String> = vec![];
    let mut msgs: BTreeMap<String, Value> = BTreeMap::new();
    let mut pending: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for rec in text.lines().filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok()) {
        let Some(o) = rec.as_object() else { continue };
        let mid = rec["id"].as_str().map(String::from);
        if let Some(r) = o.get("$rewindTo") {
            if let Some(i) = ids.iter().position(|x| Some(x.as_str()) == r.as_str()) {
                for x in ids.drain(i..) {
                    msgs.remove(&x);
                    pending.remove(&x);
                }
            } else {
                ids.clear();
                msgs.clear();
                pending.clear();
            }
        } else if let Some(Value::Object(s)) = o.get("$set") {
            meta.extend(s.clone());
        } else if rec["type"] == "user" || rec["type"] == "gemini" {
            let Some(mid) = mid else { continue }; // never listed without an id
            let m = msgs.entry(mid.clone()).or_insert_with(|| {
                ids.push(mid.clone());
                json!({})
            });
            m.as_object_mut().unwrap().extend(o.clone());
            for u in pending.remove(&mid).unwrap_or_default() {
                merge_update(m, &u);
            }
        } else if rec["type"] == "message_update" {
            if let Some(mid) = mid {
                match msgs.get_mut(&mid) {
                    Some(m) => merge_update(m, &rec),
                    None => pending.entry(mid).or_default().push(rec.clone()),
                }
            }
        } else if o.contains_key("sessionId") {
            meta.extend(o.iter().filter(|(k, _)| *k != "messages").map(|(k, v)| (k.clone(), v.clone())));
        }
    }
    if ids.is_empty() && meta.is_empty() {
        return None;
    }
    let mut out = meta;
    out.entry("sessionId").or_insert_with(|| json!("unknown"));
    out.insert("messages".into(), Value::Array(ids.iter().map(|i| msgs[i].clone()).collect()));
    Some(Value::Object(out))
}

/// Token totals over `gemini` messages (cost comes from `gemini_cost`).
pub fn gemini_usage(session: &Value) -> AgentContext {
    let (mut i, mut o, mut c) = (0, 0, 0);
    for m in session["messages"].as_array().into_iter().flatten().filter(|m| m["type"] == "gemini") {
        let t = &m["tokens"];
        let n = |k: &str| t[k].as_i64().unwrap_or(0);
        i += n("input");
        o += n("output") + n("tool") + n("thoughts");
        c += n("cached");
    }
    AgentContext { n_input_tokens: Some(i), n_cache_tokens: Some(c), n_output_tokens: Some(o), ..Default::default() }
}

const VERSION_CMD: &str = "if [ -s ~/.nvm/nvm.sh ]; then . ~/.nvm/nvm.sh; fi; gemini --version";

#[async_trait::async_trait]
impl Agent for GeminiCli {
    fn name(&self) -> &str {
        "gemini-cli"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn supports_resume(&self) -> bool {
        true
    }

    fn acp_command(&self) -> Option<Vec<String>> {
        let mut c: Vec<String> = ["env", "GEMINI_CLI_TRUST_WORKSPACE=true", "gemini", "--acp"].map(String::from).to_vec();
        if let Some(m) = self.short_model() {
            let alias = self.effort().ok().flatten().map(|e| format!("hbench-{m}-{e}"));
            c.push(format!("--model={}", alias.unwrap_or(m)));
        }
        Some(c)
    }

    /// Only the env-var half of auth; acp_install handles the oauth file + settings selectedType.
    fn acp_env(&self) -> BTreeMap<String, String> {
        self.auth_env(self.oauth_creds_path().ok().flatten().is_some())
    }

    async fn acp_install(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        let r = exec_agent(
            cx,
            "_hb_system_node=\"$(command -v node 2>/dev/null || true)\"; if [ -s \"$HOME/.nvm/nvm.sh\" ]; then . \"$HOME/.nvm/nvm.sh\"; fi; \
             echo \"${_hb_system_node:-none}:$(command -v node):$(command -v gemini)\"",
            &BTreeMap::new(),
        )
        .await?;
        let last = r.stdout.trim().lines().last().unwrap_or_default();
        let parts: Vec<&str> = if last.matches(':').count() == 2 { last.split(':').collect() } else { vec!["", "", ""] };
        let (system_node, node, gemini) = (parts[0], parts[1], parts[2]);
        if node.is_empty() || gemini.is_empty() {
            bail!("Could not locate node/gemini binaries for ACP mode (got {:?})", r.stdout);
        }
        // Pin gemini to the node it was installed with; expose node only if the image had none.
        let mut link = pinned_bin_wrapper_command(node, gemini, "/usr/local/bin/gemini");
        if system_node == "none" {
            link += &format!(" && {}", safe_bin_symlink_command(node, "/usr/local/bin/node"));
        }
        exec_root(cx, &link, &BTreeMap::new()).await?;
        let oauth = self.oauth_creds_path()?;
        let auth_type = if oauth.is_some() { Some("oauth-personal") } else { self.env_auth_type() };
        if let Some(p) = &oauth {
            self.inject_oauth_creds(cx, p, &BTreeMap::new()).await?;
        }
        if let (Some(s), _) = self.settings_command(self.short_model().as_deref(), auth_type)? {
            exec_agent(cx, &s, &BTreeMap::new()).await?;
        }
        Ok(())
    }

    /// run()'s cleanup never executes in ACP mode; injected OAuth material must not outlive the session.
    async fn acp_teardown(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        exec_agent(cx, &format!("rm -rf {} ~/.gemini/oauth_creds.json", quote(SECRETS_DIR)), &BTreeMap::new()).await.map(|_| ())
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        ensure_system_dependencies(cx, &["curl"]).await?;
        let none = BTreeMap::new();
        let spec = pins::GEMINI_CLI.npm_spec(self.version.as_deref());
        exec_agent(cx, &format!("set -euo pipefail; {} && npm install -g {spec}", nvm_node_install_snippet(22)), &none).await?;
        exec_agent(cx, "mkdir -p ~/.gemini && cat > ~/.gemini/settings.json << 'SETTINGS'\n{\n  \"experimental\": {\n    \"skills\": true\n  }\n}\nSETTINGS", &none).await?;
        exec_agent(cx, ". ~/.nvm/nvm.sh && gemini --version", &none).await?;
        if self.version.is_none() {
            self.version = detect_version(cx, VERSION_CMD, |s| s.trim().to_string()).await;
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let Some((_, model)) = self.args.model_name.as_deref().and_then(|m| m.split_once('/')) else {
            bail!("Model name must be in the format provider/model_name")
        };
        let model = model.rsplit('/').next().unwrap().to_string();
        let oauth = self.oauth_creds_path()?;
        // Gemini CLI ignores --yolo in an untrusted workspace.
        let mut env = BTreeMap::from([("GEMINI_CLI_TRUST_WORKSPACE".to_string(), "true".to_string())]);
        env.extend(self.auth_env(oauth.is_some()));
        let auth_type = if oauth.is_some() { Some("oauth-personal") } else { self.env_auth_type() };

        if let Some(p) = &oauth {
            self.inject_oauth_creds(cx, p, &env).await?;
        }
        if let Some(d) = skills_dir(&self.args) {
            exec_agent(cx, &format!("mkdir -p ~/.gemini/skills && cp -r {}/* ~/.gemini/skills/ 2>/dev/null || true", quote(&d)), &env)
                .await?;
        }
        let (settings, alias) = self.settings_command(Some(&model), auth_type)?;
        if let Some(s) = settings {
            exec_agent(cx, &s, &env).await?;
        }
        let flags = self.cli_flags()?;
        let extra = if flags.is_empty() { String::new() } else { format!("{flags} ") };
        let cmd = format!(
            "mkdir -p /logs/agent; if [ -s ~/.nvm/nvm.sh ]; then . ~/.nvm/nvm.sh; fi; gemini --yolo {}{extra}--model={} --prompt={} \
             2>&1 </dev/null | stdbuf -oL tee /logs/agent/gemini-cli.txt",
            if resume { "--resume latest " } else { "" },
            quote(alias.as_deref().unwrap_or(&model)),
            quote(&instruction)
        );
        let r = exec_agent(cx, &cmd, &env).await;
        let _ = exec_agent(
            cx,
            "latest=$(find ~/.gemini/tmp -type f \\( -name 'session-*.json' -o -name 'session-*.jsonl' \\) -printf '%T@ %p\\n' 2>/dev/null | sort -nr | head -n 1 | cut -d' ' -f2-); \
             if [ -n \"$latest\" ]; then case \"$latest\" in *.jsonl) cp \"$latest\" /logs/agent/gemini-cli.trajectory.jsonl ;; \
             *) cp \"$latest\" /logs/agent/gemini-cli.trajectory.json ;; esac; fi",
            &BTreeMap::new(),
        )
        .await;
        let _ = exec_agent(cx, &format!("rm -rf {} ~/.gemini/oauth_creds.json", quote(SECRETS_DIR)), &env).await;
        r.map(|_| ())
    }

    fn populate_context_post_run(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        let Some(text) = ["gemini-cli.trajectory.jsonl", "gemini-cli.trajectory.json"]
            .iter()
            .map(|f| logs_dir.join(f))
            .find(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok())
        else {
            return;
        };
        if let Some(s) = load_gemini_session(&text) {
            let c = gemini_usage(&s);
            ctx.n_input_tokens = c.n_input_tokens;
            ctx.n_cache_tokens = c.n_cache_tokens;
            ctx.n_output_tokens = c.n_output_tokens;
            ctx.cost_usd =
                self.args.model_name.as_deref().and_then(|m| {
                    gemini_cost(m, c.n_input_tokens.unwrap_or(0), c.n_output_tokens.unwrap_or(0), c.n_cache_tokens.unwrap_or(0))
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<GeminiCli> {
        GeminiCli::new(common::test_args(kwargs, env, model))
    }

    #[test]
    fn validation_and_settings() {
        assert!(agent(json!({"reasoning_effort": "high"}), &[], Some("google/gemini-2.5-pro")).is_err());
        assert!(agent(json!({"reasoning_effort": "medium"}), &[], Some("google/gemini-3-pro")).is_err());
        assert!(agent(json!({"reasoning_effort": "turbo"}), &[], None).is_err());
        let a = agent(
            json!({"reasoning_effort": "medium", "disable_web_search": true, "sandbox": true}),
            &[("GEMINI_API_KEY", "g")],
            Some("google/gemini-3-flash"),
        )
        .unwrap();
        assert_eq!(a.cli_flags().unwrap(), "--sandbox");
        assert_eq!(a.env_auth_type(), Some("gemini-api-key"));
        assert_eq!(a.auth_env(false)["GEMINI_API_KEY"], "g");
        let (s, alias) = a.settings(Some("gemini-3-flash"), Some("gemini-api-key")).unwrap();
        let s = s.unwrap();
        assert_eq!(alias.as_deref(), Some("hbench-gemini-3-flash-medium"));
        assert_eq!(
            s["modelConfigs"]["customAliases"]["hbench-gemini-3-flash-medium"]["modelConfig"]["generateContentConfig"]["thinkingConfig"]["thinkingLevel"],
            "MEDIUM"
        );
        assert_eq!(s["security"]["auth"]["selectedType"], "gemini-api-key");
        assert_eq!(s["tools"]["exclude"], json!(["google_web_search", "web_fetch"]));
        assert_eq!(s["experimental"]["skills"], true);
        let v = agent(json!({}), &[("GOOGLE_GENAI_USE_VERTEXAI", "true")], Some("google/gemini-3-pro")).unwrap();
        assert_eq!(v.env_auth_type(), Some("vertex-ai"));
        assert_eq!(agent(json!({}), &[], Some("google/x")).unwrap().settings(Some("x"), None).unwrap().0, None);
    }

    #[test]
    fn litellm_cost_and_acp_command() {
        // gemini-2.5-flash: in 3e-7, out 2.5e-6, cache read 3e-8.
        let c = gemini_cost("google/gemini-2.5-flash", 1_000_000, 10_000, 400_000).unwrap();
        assert!((c - (600_000.0 * 3e-7 + 400_000.0 * 3e-8 + 10_000.0 * 2.5e-6)).abs() < 1e-12);
        // gpt-4 has no cache-read rate: cached tokens bill at the input rate (3e-5), out 6e-5.
        let c = gemini_cost("gpt-4", 100, 10, 40).unwrap();
        assert!((c - (100.0 * 3e-5 + 10.0 * 6e-5)).abs() < 1e-12);
        assert!(gemini_cost("google/not-a-model", 1, 1, 0).is_none());

        let a = agent(json!({"reasoning_effort": "high"}), &[("GEMINI_API_KEY", "g")], Some("google/gemini-3-pro")).unwrap();
        assert_eq!(
            a.acp_command().unwrap(),
            vec!["env", "GEMINI_CLI_TRUST_WORKSPACE=true", "gemini", "--acp", "--model=hbench-gemini-3-pro-high"]
        );
        assert_eq!(a.acp_env()["GEMINI_API_KEY"], "g");
        assert_eq!(
            agent(json!({}), &[], None).unwrap().acp_command().unwrap(),
            vec!["env", "GEMINI_CLI_TRUST_WORKSPACE=true", "gemini", "--acp"]
        );
    }

    #[test]
    fn session_usage() {
        let legacy = r#"{"sessionId":"s","messages":[{"type":"user","content":"hi"},{"type":"gemini","content":"ok","tokens":{"input":100,"output":10,"cached":40,"thoughts":5,"tool":1}}]}"#;
        let c = gemini_usage(&load_gemini_session(legacy).unwrap());
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens), (Some(100), Some(40), Some(16)));

        let jsonl = r#"{"sessionId":"s","projectHash":"p"}
{"id":"m1","type":"user","content":"hi"}
{"id":"m2","type":"gemini","content":"","tokens":{"input":999,"output":9}}
{"$rewindTo":"m2"}
{"id":"m3","type":"message_update","tokens":{"input":50,"output":5}}
{"id":"m3","type":"gemini","content":"ok","tokens":{"input":1,"output":1,"cached":2}}
{"$set":{"summary":"x"}}
"#;
        let s = load_gemini_session(jsonl).unwrap();
        assert_eq!(s["sessionId"], "s");
        assert_eq!(s["messages"].as_array().unwrap().len(), 2);
        let c = gemini_usage(&s);
        assert_eq!((c.n_input_tokens, c.n_cache_tokens, c.n_output_tokens), (Some(50), Some(2), Some(5)));
        assert!(load_gemini_session("  \n").is_none());
    }
}
