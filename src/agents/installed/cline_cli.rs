//! Cline CLI.

use super::common::{installed_dir, skills_dir};
use super::pins;
use super::{exec_agent, exec_root};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::{ExecResult, quote};
use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const THINKING: &[&str] = &["none", "low", "medium", "high", "xhigh"];
const NVM_LOAD: &str = "export NVM_DIR=\"$HOME/.nvm\" && { [ -s \"$NVM_DIR/nvm.sh\" ] && . \"$NVM_DIR/nvm.sh\" || true; } && ";

pub struct ClineCli {
    args: AgentArgs,
    version: Option<String>,
    flags: String,
    tarball_path: Option<PathBuf>,
    tarball_url: Option<String>,
    github_user: Option<String>,
    commit_hash: String,
    cline_version: Option<String>,
    plugin_source: Option<String>,
    plugin_tarball_url: Option<String>,
    plugin_tarball_path: Option<PathBuf>,
    remote_plugin_tarball: Option<&'static str>,
    cline_timeout_sec: Option<i64>,
    setup_retries: i64,
    setup_retry_delay_sec: f64,
    setup_command_timeout_sec: Option<f64>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(ClineCli::new(args)?))
}

fn pystr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// Timeout seconds: positive integer from int/integral float/str.
fn parse_positive_int(v: &Value, field: &str) -> Result<i64> {
    let err = || anyhow!("Invalid {field} value: '{}'. Must be a positive integer.", pystr(v));
    let n = match v {
        Value::Number(n) => match n.as_i64() {
            Some(i) => i,
            None => n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64).ok_or_else(err)?,
        },
        Value::String(s) => {
            let t = s.trim();
            match t.parse::<i64>() {
                Ok(i) => i,
                Err(_) => t.parse::<f64>().ok().filter(|f| f.fract() == 0.0).map(|f| f as i64).ok_or_else(err)?,
            }
        }
        _ => return Err(err()),
    };
    if n <= 0 {
        bail!("Invalid {field} value: '{}'. Must be > 0 seconds.", pystr(v));
    }
    Ok(n)
}

/// Non-negative float seconds from int/float/str.
fn parse_nonneg_f64(v: &Value, field: &str, rule: &str) -> Result<f64> {
    let err = || anyhow!("Invalid {field} value: '{}'. {rule}", pystr(v));
    let f = match v {
        Value::Number(n) => n.as_f64().ok_or_else(err)?,
        Value::String(s) => s.trim().parse::<f64>().map_err(|_| err())?,
        _ => return Err(err()),
    };
    if f < 0.0 {
        return Err(err());
    }
    Ok(f)
}

impl ClineCli {
    fn new(args: AgentArgs) -> Result<ClineCli> {
        // First non-null value among a kwarg and its kebab-case aliases.
        let pick = |names: &[&str]| -> Option<Value> { names.iter().find_map(|n| args.kwargs.get(*n).filter(|v| !v.is_null()).cloned()) };
        let pick_s = |names: &[&str]| pick(names).map(|v| pystr(&v));
        if pick(&["double_check_completion", "double-check-completion", "double_check_completions", "double-check-completions"]).is_some() {
            bail!(
                "double_check_completion is not supported by cline-cli; the new Cline CLI does not expose a --double-check-completion flag."
            );
        }
        let mut thinking = pick(&["thinking"]);
        let reasoning = pick(&["reasoning_effort", "reasoning-effort"]);
        if thinking.is_some() && reasoning.is_some() {
            bail!("Pass only one of 'thinking' or 'reasoning_effort' for cline-cli.");
        }
        if let Some(r) = reasoning {
            let Value::String(s) = &r else { bail!("Invalid value for 'reasoning_effort': expected str for enum") };
            let n = s.trim().to_lowercase();
            if !THINKING.contains(&n.as_str()) {
                bail!("Invalid value for 'reasoning_effort': '{s}'. Valid values: high, low, medium, none, xhigh");
            }
            thinking = Some(Value::String(n));
        }
        let mut f = vec![];
        if let Some(t) = thinking {
            match t.as_str() {
                Some(s) if THINKING.contains(&s) => f.push(format!("--thinking {s}")),
                _ => bail!("Invalid kwargs for agent 'cline-cli':\n  thinking: Input should be 'none', 'low', 'medium', 'high' or 'xhigh'"),
            }
        }
        if let Some(v) = pick(&["max_consecutive_mistakes", "max-consecutive-mistakes"]) {
            let n = match &v {
                Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
                Value::String(s) => s.trim().parse().ok(),
                _ => None,
            }
            .ok_or_else(|| anyhow!("Invalid kwargs for agent 'cline-cli':\n  max_consecutive_mistakes: Input should be a valid integer"))?;
            if n < 0 {
                bail!("Invalid value for 'max_consecutive_mistakes': {n}. Must be >= 0.");
            }
            f.push(format!("--max-consecutive-mistakes {n}"));
        }

        let agent_timeout = pick(&["agent_timeout_sec"]).map(|v| parse_positive_int(&v, "agent_timeout_sec")).transpose()?;
        let explicit = [
            ("cline_timeout_sec", &["cline_timeout_sec", "cline-timeout-sec", "cline-timeout"][..]),
            ("timeout_sec", &["timeout_sec", "timeout-sec"]),
            ("timeout", &["timeout", "timeout-seconds"]),
        ]
        .into_iter()
        .find_map(|(name, aliases)| pick(aliases).map(|v| (name, v)));
        let cline_timeout_sec = match explicit {
            Some((name, v)) => Some(parse_positive_int(&v, name)?),
            None => agent_timeout,
        };
        let setup_retries =
            pick(&["setup_retries", "setup-retries"]).map(|v| parse_positive_int(&v, "setup_retries")).transpose()?.unwrap_or(2);
        let setup_retry_delay_sec = pick(&["setup_retry_delay_sec", "setup-retry-delay-sec", "setup-retry-delay-seconds"])
            .map(|v| parse_nonneg_f64(&v, "setup_retry_delay_sec", "Must be >= 0 seconds."))
            .transpose()?
            .unwrap_or(2.0);
        let setup_command_timeout_sec =
            match pick(&["setup_command_timeout_sec", "setup-command-timeout-sec", "setup-command-timeout-seconds"]) {
                None => Some(240.0),
                Some(v) => {
                    Some(parse_nonneg_f64(&v, "setup_command_timeout_sec", "Must be >= 0 seconds, or 0 to disable.")?).filter(|t| *t > 0.0)
                }
            };
        let expand = |p: String| {
            PathBuf::from(match p.strip_prefix("~/") {
                Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
                None => p,
            })
        };
        let commit_hash = pick_s(&["commit_hash", "commit-hash"]);
        let github_user =
            pick_s(&["github_user", "github-user"]).or_else(|| commit_hash.as_ref().filter(|c| !c.is_empty()).map(|_| "cline".to_string()));
        Ok(ClineCli {
            version: args.version_kw(),
            flags: f.join(" "),
            tarball_path: pick_s(&["tarball_path", "tarball-path", "local-tarball-path"]).filter(|s| !s.is_empty()).map(expand),
            tarball_url: pick_s(&["tarball_url", "tarball-url"]).filter(|s| !s.is_empty()),
            github_user,
            commit_hash: commit_hash.filter(|c| !c.is_empty()).unwrap_or_else(|| "main".into()),
            cline_version: pick_s(&["cline_version", "cline-version"]).filter(|s| !s.is_empty()),
            plugin_source: pick_s(&["plugin_source", "plugin-source"]).filter(|s| !s.is_empty()),
            plugin_tarball_url: pick_s(&["plugin_tarball_url", "plugin-tarball-url"]).filter(|s| !s.is_empty()),
            plugin_tarball_path: pick_s(&["plugin_tarball_path", "plugin-tarball-path", "local-plugin-tarball-path"])
                .filter(|s| !s.is_empty())
                .map(expand),
            remote_plugin_tarball: None,
            cline_timeout_sec,
            setup_retries,
            setup_retry_delay_sec,
            setup_command_timeout_sec,
            args,
        })
    }

    fn source_install_command(&self, reason: &str) -> String {
        let repo = quote(&format!("https://github.com/{}/cline.git", self.github_user.as_deref().unwrap_or("cline")));
        let r#ref = quote(&self.commit_hash);
        let bun = pins::BUN.version;
        format!(
            "echo {} && export BUN_INSTALL=\"$HOME/.bun\" && export PATH=\"$BUN_INSTALL/bin:$PATH\" && \
             if ! command -v bun >/dev/null 2>&1; then curl -fsSL https://bun.sh/install | bash -s bun-v{bun} && export PATH=\"$BUN_INSTALL/bin:$PATH\"; fi && \
             CLONE_DIR=\"$HOME/.cache/hbench-cline-source\" && REPO_URL={repo} && REF={ref} && rm -rf \"$CLONE_DIR\" && mkdir -p \"$(dirname \"$CLONE_DIR\")\" && \
             (git clone --branch \"$REF\" --depth 1 \"$REPO_URL\" \"$CLONE_DIR\" || (git clone \"$REPO_URL\" \"$CLONE_DIR\" && cd \"$CLONE_DIR\" && git checkout \"$REF\")) && \
             cd \"$CLONE_DIR/sdk\" && bun install && bun run build:sdk && CLINE_SHIM=\"$(dirname \"$(command -v node)\")/cline\" && \
             cat > \"$CLINE_SHIM\" <<'__HBENCH_CLINE_SHIM__'\n#!/usr/bin/env bash\nset -euo pipefail\nexport BUN_INSTALL=\"${{BUN_INSTALL:-$HOME/.bun}}\"\n\
             export PATH=\"$BUN_INSTALL/bin:$PATH\"\nexport CLINE_BUILD_ENV=\"${{CLINE_BUILD_ENV:-development}}\"\n\
             exec \"$BUN_INSTALL/bin/bun\" --conditions=development \"$HOME/.cache/hbench-cline-source/sdk/apps/cli/src/index.ts\" \"$@\"\n__HBENCH_CLINE_SHIM__\n\
             chmod +x \"$CLINE_SHIM\" && hash -r && cline --version",
            quote(reason),
            ref = r#ref
        )
    }

    fn npm_install_command(&self, spec: &str) -> String {
        let fallback =
            self.source_install_command("Cline npm binary smoke test failed; installing Cline from source with Bun as a fallback.");
        format!(
            "npm install -g {spec} && sleep 0.5 && if cline --version || cline version; then echo 'Cline npm binary smoke test passed.'; \
             else status=\"$?\"; echo \"Cline npm binary smoke test failed with exit ${{status}}.\"; {fallback}; fi"
        )
    }

    /// Exec with per-attempt timeout + exponential-backoff retries, logging each attempt to logs_dir/setup.
    async fn exec_with_retries(
        &self,
        cx: &AgentCtx<'_>,
        cmd: &str,
        label: &str,
        as_root: bool,
        env: &BTreeMap<String, String>,
        timeout: Option<f64>,
    ) -> Result<()> {
        for attempt in 1..=self.setup_retries {
            let started = chrono::Utc::now();
            let label = if attempt == 1 { label.to_string() } else { format!("{label}.attempt-{attempt}") };
            let fut = async { if as_root { exec_root(cx, cmd, env).await } else { exec_agent(cx, cmd, env).await } };
            let res = match timeout {
                Some(t) => {
                    tokio::time::timeout(Duration::from_secs_f64(t), fut).await.unwrap_or_else(|_| Err(anyhow!("timed out after {t}s")))
                }
                None => fut.await,
            };
            let err = match res {
                Ok(r) => {
                    self.setup_log(&label, &r.stdout, &r.stderr, started);
                    return Ok(());
                }
                Err(e) => e,
            };
            let suffix = if err.to_string().starts_with("timed out after") { "timeout" } else { "failed" };
            self.setup_log(&format!("{label}.{suffix}"), "", &err.to_string(), started);
            if attempt >= self.setup_retries {
                return Err(err);
            }
            let delay = self.setup_retry_delay_sec * 2f64.powi(attempt as i32 - 1);
            if delay > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            }
        }
        Ok(())
    }

    fn setup_log(&self, label: &str, stdout: &str, stderr: &str, started: chrono::DateTime<chrono::Utc>) {
        let ended = chrono::Utc::now();
        let dir = self.args.logs_dir.join("setup");
        let mut body = format!(
            "=== {label} ===\nstart:    {}\nend:      {}\nduration: {:.2}s\n--- STDOUT ---\n{stdout}",
            started.to_rfc3339(),
            ended.to_rfc3339(),
            (ended - started).num_milliseconds() as f64 / 1000.0
        );
        if !stderr.is_empty() {
            body += &format!("\n--- STDERR ---\n{stderr}");
        }
        let _ = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(format!("{label}.log")), body));
    }

    fn mcp_command(&self) -> Option<String> {
        if self.args.mcp_servers.is_empty() {
            return None;
        }
        let mut servers = Map::new();
        for s in &self.args.mcp_servers {
            let v = match s.transport() {
                "stdio" => json!({"command": s.command, "args": s.args, "disabled": false}),
                "streamable-http" => json!({"url": s.url, "type": "streamableHttp", "disabled": false}),
                _ => json!({"url": s.url, "disabled": false}),
            };
            servers.insert(s.name.clone(), v);
        }
        let cfg = serde_json::to_string_pretty(&json!({"mcpServers": servers})).unwrap();
        Some(format!("mkdir -p ~/.cline/data/settings && echo {} > ~/.cline/data/settings/cline_mcp_settings.json", quote(&cfg)))
    }

    /// (setup command, run command, env) for the run.
    fn run_commands(&self, instruction: &str) -> Result<(String, String, BTreeMap<String, String>)> {
        let raw = instruction.trim();
        if raw.is_empty() {
            bail!("Instruction is empty before invoking cline");
        }
        let Some((provider, model)) = self.args.model_name.as_deref().and_then(|m| m.split_once(':')) else {
            bail!("model_name must be in format 'provider:model-id', got: '{}'", self.args.model_name.as_deref().unwrap_or("None"));
        };
        let provider_env = match provider {
            "anthropic" => Some("ANTHROPIC_API_KEY"),
            "gemini" => Some("GEMINI_API_KEY"),
            "google" => Some("GOOGLE_API_KEY"),
            "openai" => Some("OPENAI_API_KEY"),
            "openrouter" => Some("OPENROUTER_API_KEY"),
            "cline" => Some("CLINE_API_KEY"),
            "xai" => Some("XAI_API_KEY"),
            _ => None,
        };
        let Some(api_key) = provider_env.into_iter().chain(["API_KEY"]).find_map(|e| self.args.get_env(e)) else {
            let expected = provider_env.map(|p| format!("{p} or API_KEY")).unwrap_or_else(|| "API_KEY".into());
            bail!("{expected} environment variable is required");
        };
        let cline_provider = if provider == "vercel" { "vercel-ai-gateway" } else { provider };
        let env = BTreeMap::from([
            ("PROVIDER".to_string(), provider.to_string()),
            ("API_KEY".into(), api_key),
            ("MODELID".into(), model.to_string()),
            ("CLINE_WRITE_PROMPT_ARTIFACTS".into(), "1".into()),
            ("CLINE_PROMPT_ARTIFACT_DIR".into(), "/logs/agent".into()),
        ]);
        let mut setup = format!(
            "mkdir -p /logs/agent ~/.cline/data && echo {} > ~/.cline/data/globalState.json",
            quote(r#"{"welcomeViewCompleted": true, "isNewUser": false}"#)
        );
        if let Some(d) = skills_dir(&self.args) {
            setup += &format!(" && mkdir -p ~/.cline/skills && (cp -r {}/* ~/.cline/skills/ 2>/dev/null || true)", quote(&d));
        }
        if let Some(m) = self.mcp_command() {
            setup += &format!(" && {m}");
        }
        let unpack = "rm -rf /tmp/cline-plugin-install && mkdir -p /tmp/cline-plugin-install && ";
        if let Some(p) = self.remote_plugin_tarball {
            setup += &format!(
                " && {unpack}tar -xzf {} -C /tmp/cline-plugin-install --strip-components=1 && {NVM_LOAD}cline plugin install /tmp/cline-plugin-install --force",
                quote(p)
            );
        } else if let Some(u) = &self.plugin_tarball_url {
            setup += &format!(
                " && {unpack}curl -fsSL {} -o /tmp/cline-plugin-install/plugin.tar.gz && tar -xzf /tmp/cline-plugin-install/plugin.tar.gz \
                 -C /tmp/cline-plugin-install --strip-components=1 && {NVM_LOAD}cline plugin install /tmp/cline-plugin-install --force",
                quote(u)
            );
        } else if let Some(s) = &self.plugin_source {
            setup += &format!(" && {NVM_LOAD}cline plugin install {} --git --force", quote(s));
        }
        let mut flags = vec![
            "-P".to_string(),
            cline_provider.into(),
            "-k".into(),
            "$API_KEY".into(),
            "-m".into(),
            "$MODELID".into(),
            "--json".into(),
            "--yolo".into(),
        ];
        if let Some(t) = self.cline_timeout_sec {
            flags.extend(["-t".into(), t.to_string()]);
        }
        if !self.flags.is_empty() {
            flags.push(self.flags.clone());
        }
        let run = format!(
            "export NVM_DIR=\"$HOME/.nvm\"; if [ -s \"$NVM_DIR/nvm.sh\" ]; then . \"$NVM_DIR/nvm.sh\"; nvm use 22 >/dev/null 2>&1 || true; fi; \
             set -o pipefail; cline {} -- {} < /dev/null 2>&1 | stdbuf -oL tee /logs/agent/cline.txt; status=${{PIPESTATUS[0]}}; \
             echo \"__CLINE_EXIT=${{status}}\" | tee -a /logs/agent/cline.txt; exit \"${{status}}\"",
            flags.join(" "),
            quote(raw)
        );
        Ok((setup, run, env))
    }
}

/// Sum assistant-message metrics from a Cline `*.messages.json` session.
pub fn usage_from_messages(text: &str) -> Option<AgentContext> {
    let doc: Value = serde_json::from_str(text).ok()?;
    let msgs = doc.get("messages")?.as_array()?;
    let mut c = AgentContext::default();
    let mut saw = false;
    let int = |v: &Value| v.as_i64().filter(|_| v.is_i64() || v.is_u64()).unwrap_or(0);
    for m in msgs.iter().filter(|m| m["role"] == "assistant") {
        let Some(x) = m.get("metrics").filter(|x| x.is_object()) else { continue };
        saw = true;
        c.add_usage(int(&x["inputTokens"]), int(&x["cacheReadTokens"]), int(&x["outputTokens"]), Some(x["cost"].as_f64().unwrap_or(0.0)));
    }
    saw.then_some(c)
}

/// Newest `sessions/*/*.messages.json` by mtime.
fn find_session(logs_dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(logs_dir.join("sessions"))
        .ok()?
        .flatten()
        .filter(|d| d.path().is_dir())
        .filter_map(|d| std::fs::read_dir(d.path()).ok())
        .flat_map(|r| r.flatten())
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".messages.json")))
        .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())
}

#[async_trait::async_trait]
impl Agent for ClineCli {
    fn name(&self) -> &str {
        "cline-cli"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        self.exec_with_retries(
            cx,
            &format!("{}{}", super::APT_INSTALL_FN, "if command -v git &> /dev/null && command -v curl &> /dev/null && command -v unzip &> /dev/null; then  echo 'git, curl, and unzip already installed, skipping apt-get...'; \
             else  echo 'Killing background apt processes to release lock...';  pkill -9 -x unattended-upgrades 2>/dev/null || true;  pkill -9 -x apt-get 2>/dev/null || true;  \
             pkill -9 -x dpkg 2>/dev/null || true;  sleep 1;  rm -f /var/lib/apt/lists/lock /var/lib/dpkg/lock /var/lib/dpkg/lock-frontend /var/cache/apt/archives/lock 2>/dev/null || true;  \
             dpkg --configure -a 2>/dev/null || true;  echo 'Trying apt-get install without update first...';  if apt-get install -y curl ca-certificates git unzip 2>/dev/null; then    \
             echo 'Install succeeded without update.';  else    echo 'Falling back to apt-get update + install...';    hb_apt_install curl ca-certificates git unzip;  fi; fi"),
            "install-root-prereqs",
            true,
            &BTreeMap::from([("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())]),
            None,
        )
        .await?;

        let mut parts = vec![
            format!(
                "if command -v node &> /dev/null && node --version | grep -qE '^v2[2-9]|^v[3-9]'; then  echo 'Node.js already installed, skipping nvm setup...'; \
                 else  curl -o- {} | bash &&  export NVM_DIR=\"$HOME/.nvm\" &&  \
                 [ -s \"$NVM_DIR/nvm.sh\" ] && \\. \"$NVM_DIR/nvm.sh\" &&  nvm install 22 && nvm use 22 && nvm alias default 22; fi",
                pins::nvm_installer()
            ),
            "export NVM_DIR=\"$HOME/.nvm\" && { [ -s \"$NVM_DIR/nvm.sh\" ] && \\. \"$NVM_DIR/nvm.sh\" || true; }".into(),
        ];
        if let Some(p) = &self.tarball_path {
            if !p.is_file() {
                bail!("Cline CLI tarball not found: {}", p.display());
            }
            cx.env.upload_file(p, "/tmp/hbench-cline-cli.tgz").await?;
            parts.push("npm install -g --ignore-scripts -- /tmp/hbench-cline-cli.tgz && (cline --version || cline version)".into());
        } else if let Some(u) = &self.tarball_url {
            parts.push(format!("npm install -g --ignore-scripts -- \"{u}\" && (cline --version || cline version)"));
        } else if let Some(g) = &self.github_user {
            parts.push(self.source_install_command(&format!("Installing Cline from source: {g}/cline @ {}", self.commit_hash)));
        } else {
            parts.push(self.npm_install_command(&pins::CLINE.npm_spec(self.cline_version.as_deref())));
        }
        if let Some(p) = &self.plugin_tarball_path {
            if !p.is_file() {
                bail!("Cline plugin tarball not found: {}", p.display());
            }
            cx.env.upload_file(p, "/tmp/hbench-cline-plugin.tgz").await?;
            self.remote_plugin_tarball = Some("/tmp/hbench-cline-plugin.tgz");
        }
        // Host-only GitHub tokens (read from the host environment, not --ae).
        let install_env: BTreeMap<String, String> = ["GITHUB_TOKEN", "GH_TOKEN"]
            .iter()
            .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| (k.to_string(), v)))
            .collect();
        self.exec_with_retries(
            cx,
            &format!("set -e; {}", parts.join(" && ")),
            "install-agent-runtime",
            false,
            &install_env,
            self.setup_command_timeout_sec,
        )
        .await?;

        if self.version.is_none() {
            let r: ExecResult = cx.exec(". ~/.nvm/nvm.sh 2>/dev/null; cline --version || cline version", &BTreeMap::new(), None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(r.stdout.trim().to_string());
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let (setup, run, env) = self.run_commands(&instruction)?;
        let mut r = exec_agent(cx, &setup, &env).await;
        if r.is_ok() {
            r = exec_agent(cx, &run, &env).await;
        }
        let _ = exec_agent(
            cx,
            "if [ -d ~/.cline/data/sessions ]; then mkdir -p /logs/agent/sessions && LATEST_SESSION=\"$(ls -1td ~/.cline/data/sessions/*/ 2>/dev/null | head -n 1)\" && \
             if [ -n \"$LATEST_SESSION\" ]; then cp -r \"$LATEST_SESSION\" /logs/agent/sessions/; fi; fi",
            &BTreeMap::new(),
        )
        .await;
        r.map(|_| ())
    }

    fn populate_context_post_run(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        if let Some(c) = find_session(logs_dir).and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| usage_from_messages(&t)) {
            *ctx = c;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;

    fn agent(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> Result<ClineCli> {
        ClineCli::new(common::test_args(kwargs, env, model))
    }

    #[test]
    fn options_and_validation() {
        let a = agent(
            json!({"reasoning-effort": " HIGH ", "max-consecutive-mistakes": "3", "timeout-sec": "120", "agent_timeout_sec": 900}),
            &[],
            None,
        )
        .unwrap();
        assert_eq!(a.flags, "--thinking high --max-consecutive-mistakes 3");
        assert_eq!(a.cline_timeout_sec, Some(120));
        assert_eq!((a.setup_retries, a.setup_retry_delay_sec, a.setup_command_timeout_sec), (2, 2.0, Some(240.0)));
        let b = agent(json!({"agent_timeout_sec": "900.0", "setup-command-timeout-sec": 0, "commit-hash": "abc"}), &[], None).unwrap();
        assert_eq!((b.cline_timeout_sec, b.setup_command_timeout_sec), (Some(900), None));
        assert_eq!((b.github_user.as_deref(), b.commit_hash.as_str()), (Some("cline"), "abc"));
        assert!(agent(json!({"thinking": "low", "reasoning_effort": "low"}), &[], None).is_err());
        assert!(agent(json!({"thinking": "ultra"}), &[], None).is_err());
        assert!(agent(json!({"double-check-completion": true}), &[], None).is_err());
        assert!(agent(json!({"max_consecutive_mistakes": -1}), &[], None).is_err());
        assert!(agent(json!({"timeout": 1.5}), &[], None).is_err());
        assert!(agent(json!({"timeout": 0}), &[], None).is_err());
        assert!(agent(json!({"setup_retry_delay_sec": -1}), &[], None).is_err());
    }

    #[test]
    fn run_commands_and_auth() {
        let a = agent(
            json!({"thinking": "medium", "timeout": 600}),
            &[("OPENROUTER_API_KEY", "or"), ("API_KEY", "generic")],
            Some("openrouter:anthropic/claude-opus-4.5"),
        )
        .unwrap();
        let (setup, run, env) = a.run_commands("  Fix `foo` $(x)  ").unwrap();
        assert_eq!(
            (env["API_KEY"].as_str(), env["PROVIDER"].as_str(), env["MODELID"].as_str()),
            ("or", "openrouter", "anthropic/claude-opus-4.5")
        );
        assert!(setup.starts_with("mkdir -p /logs/agent ~/.cline/data && echo '{\"welcomeViewCompleted\": true, \"isNewUser\": false}' > ~/.cline/data/globalState.json"));
        assert!(run.contains(
            "cline -P openrouter -k $API_KEY -m $MODELID --json --yolo -t 600 --thinking medium -- 'Fix `foo` $(x)' < /dev/null 2>&1"
        ));
        let v = agent(json!({}), &[("API_KEY", "g")], Some("vercel:m")).unwrap();
        assert!(v.run_commands("x").unwrap().1.contains("-P vercel-ai-gateway"));
        let n = agent(json!({}), &[], Some("openrouter:m")).unwrap();
        // Host env may define API_KEY; only assert the message when it fails.
        if let Err(e) = n.run_commands("x") {
            assert!(e.to_string().contains("OPENROUTER_API_KEY or API_KEY environment variable"));
        }
        assert!(agent(json!({}), &[], Some("openrouter/m")).unwrap().run_commands("x").is_err());
        assert!(a.run_commands("   ").is_err());
    }

    #[test]
    fn usage_from_session_messages() {
        let doc = json!({"sessionId": "sess-1", "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [{"type": "text", "text": "hi"}], "metrics": {"inputTokens": 100, "outputTokens": 20, "cacheReadTokens": 10, "cost": 0.03}},
            {"role": "assistant", "metrics": {"inputTokens": 5, "outputTokens": 1}}
        ]});
        let c = usage_from_messages(&doc.to_string()).unwrap();
        assert_eq!((c.n_input_tokens, c.n_output_tokens, c.n_cache_tokens, c.cost_usd), (Some(105), Some(21), Some(10), Some(0.03)));
        assert!(usage_from_messages("[]").is_none());
        assert!(usage_from_messages(&json!({"messages": [{"role": "user"}]}).to_string()).is_none());

        let a = agent(json!({}), &[], None).unwrap();
        let d = a.args.logs_dir.join("sessions/sess-1");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("sess-1.messages.json"), doc.to_string()).unwrap();
        let mut ctx = AgentContext::default();
        a.populate_context_post_run(&a.args.logs_dir, &mut ctx);
        assert_eq!(ctx.n_input_tokens, Some(105));
    }
}
