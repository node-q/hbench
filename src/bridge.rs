//! Simulated-user trials: a user agent talks to the target agent over ACP via `acpx`.

use crate::agents::installed::{ensure_acp_node_command, pinned_bin_wrapper_command, safe_bin_symlink_command};
use crate::agents::{AgentBox, AgentCtx};
use crate::config::BridgeConfig;
use crate::env::quote;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const ACPX_NPM_VERSION: &str = "0.11.2";
pub const TRAJECTORY_FILENAME: &str = "bridge-trajectory.json";

const DEFAULT_ACP_PROMPT: &str = "## How to talk to the coding agent

A coding agent is connected to this workspace. You talk to it by running the
`acpx` command-line tool in your shell:

- Send it a message with: `acpx prompt \"<your message>\"`
- `acpx prompt` is the only channel to the coding agent. Your first action must
  send an opening message with that command.
- The command blocks until the coding agent finishes its turn; long waits are normal.
- Run it again to continue the same session, sending exactly one message each time.
- Do not edit files or complete the task yourself. You may inspect the workspace
  read-only to review the coding agent's work.
- When you are satisfied, stop sending messages and end your session.";

const DEFAULT_USER_PERSONA: &str = "You are playing the role of a human user who wants an agent to complete a
task. Your goal is given at the end of this prompt. It is private to you; the
agent cannot see it, so convey it through your messages.";

const DEFAULT_USER_PROMPT_TEMPLATE: &str = "{{ persona }}\n\n{{ bridge_instructions }}\n\n{{ instruction }}";

pub fn build_acpx_config(acp_command: &[String], overrides: &serde_json::Map<String, Value>) -> Result<Value> {
    let Some((cmd, args)) = acp_command.split_first() else { bail!("acp_command cannot be empty") };
    validate_overrides(overrides)?;
    let mut c = json!({
        "agents": {"target": {"command": cmd, "args": args}},
        "defaultAgent": "target",
        "defaultPermissions": "approve-all",
        "ttl": 0,
        "timeout": 3600,
        "format": "quiet",
    });
    for (k, v) in overrides {
        c[k] = v.clone();
    }
    Ok(c)
}

fn validate_overrides(o: &serde_json::Map<String, Value>) -> Result<()> {
    let reserved: Vec<&String> = o.keys().filter(|k| *k == "agents" || *k == "defaultAgent").collect();
    if !reserved.is_empty() {
        bail!("Reserved ACPX config keys: {reserved:?}");
    }
    Ok(())
}

/// Last token-usage mapping in an ACP session export.
pub fn extract_target_usage(v: &Value) -> Option<Value> {
    const KEYS: [&str; 8] =
        ["inputTokens", "outputTokens", "totalTokens", "input_tokens", "output_tokens", "total_tokens", "cachedTokens", "cached_tokens"];
    fn walk(v: &Value, last: &mut Option<Value>) {
        match v {
            Value::Object(m) => {
                if KEYS.iter().any(|k| m.contains_key(*k)) {
                    *last = Some(v.clone());
                }
                m.values().for_each(|x| walk(x, last));
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, last)),
            _ => {}
        }
    }
    let mut last = None;
    walk(v, &mut last);
    last
}

/// Render the simulated user's prompt (validates template variables).
pub fn render_user_prompt(instruction: &str, bridge_instructions: &str, template: Option<&Path>, persona: Option<&Path>) -> Result<String> {
    let (tpl, source) = match template {
        Some(p) => (
            std::fs::read_to_string(p).with_context(|| format!("User prompt template not found: {}", p.display()))?,
            p.display().to_string(),
        ),
        None => (DEFAULT_USER_PROMPT_TEMPLATE.to_string(), "built-in default".into()),
    };
    let env = minijinja::Environment::new();
    let t = env.template_from_str(&tpl).map_err(|e| anyhow::anyhow!("User prompt template ({source}) is not valid Jinja2: {e}"))?;
    let vars: BTreeSet<String> = t.undeclared_variables(false).into_iter().collect();
    let missing: Vec<&str> = ["bridge_instructions", "instruction"].into_iter().filter(|v| !vars.contains(*v)).collect();
    if !missing.is_empty() {
        bail!("User prompt template ({source}) is missing required variable(s): {missing:?}");
    }
    let unknown: Vec<&String> = vars.iter().filter(|v| !["bridge_instructions", "instruction", "persona"].contains(&v.as_str())).collect();
    if !unknown.is_empty() {
        bail!("User prompt template ({source}) references unknown variable(s): {unknown:?}");
    }
    if persona.is_some() && !vars.contains("persona") {
        bail!("User prompt template ({source}) has no {{{{ persona }}}} slot, so the persona would be ignored.");
    }
    let persona_text = match persona {
        Some(p) => std::fs::read_to_string(p).with_context(|| format!("User persona not found: {}", p.display()))?,
        None => DEFAULT_USER_PERSONA.to_string(),
    };
    Ok(t.render(minijinja::context! { instruction => instruction, bridge_instructions => bridge_instructions, persona => persona_text })?)
}

pub struct Bridge {
    prompt: String,
    overrides: serde_json::Map<String, Value>,
    pub ready: bool,
}

impl Bridge {
    pub fn new(cfg: &BridgeConfig) -> Result<Bridge> {
        if cfg.kind != "acp" {
            bail!("Unsupported bridge kind: {}", cfg.kind);
        }
        if let Some(k) = cfg.kwargs.keys().find(|k| *k != "acpx_config_path") {
            bail!("unknown ACP bridge kwarg {k:?}");
        }
        let prompt = match &cfg.prompt_path {
            Some(p) => std::fs::read_to_string(p).with_context(|| format!("bridge prompt {}", p.display()))?,
            None => DEFAULT_ACP_PROMPT.to_string(),
        };
        let overrides = match cfg.kwargs.get("acpx_config_path").and_then(Value::as_str) {
            None => serde_json::Map::new(),
            Some(p) => {
                let v: Value = serde_json::from_str(
                    &std::fs::read_to_string(p).with_context(|| format!("ACPX config path given but file not found: {p}"))?,
                )?;
                let Value::Object(m) = v else { bail!("ACPX config must contain a JSON object: {p}") };
                validate_overrides(&m)?;
                m
            }
        };
        Ok(Bridge { prompt, overrides, ready: false })
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// Install acpx, prepare the target for ACP, write .acpxrc.json, open the session.
    pub async fn setup(&mut self, cx: &AgentCtx<'_>, target: &mut AgentBox) -> Result<()> {
        let prep = "_hb_missing=\"\"; for _hb_prerequisite in curl bash; do if ! command -v \"$_hb_prerequisite\" >/dev/null 2>&1; then _hb_missing=\"$_hb_missing $_hb_prerequisite\"; fi; done; \
            if [ -n \"$_hb_missing\" ]; then set -- $_hb_missing; if command -v apt-get >/dev/null 2>&1; then hb_apt_install \"$@\"; \
            elif command -v apk >/dev/null 2>&1; then apk add --no-cache \"$@\"; elif command -v dnf >/dev/null 2>&1; then dnf install -y \"$@\"; fi; fi; \
            for _hb_prerequisite in curl bash; do if ! command -v \"$_hb_prerequisite\" >/dev/null 2>&1; then echo \"Missing ACPX prerequisite: $_hb_prerequisite\" >&2; exit 1; fi; done";
        let r = cx
            .exec_as(
                &format!("{}{prep}", crate::agents::installed::APT_INSTALL_FN),
                Some("root"),
                &BTreeMap::from([("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())]),
                None,
            )
            .await?;
        if r.code != 0 {
            bail!("Failed to prepare container for ACPX install: {}", r.stderr);
        }
        let r = cx
            .exec(
                &format!(
                    "set -euo pipefail; _hb_system_node=\"$(command -v node 2>/dev/null || true)\"; {} && npm install -g acpx@{ACPX_NPM_VERSION} && \
                     echo \"${{_hb_system_node:-none}}:$(command -v node):$(command -v acpx)\"",
                    ensure_acp_node_command()
                ),
                &BTreeMap::new(),
                None,
            )
            .await?;
        if r.code != 0 {
            bail!("Failed to install ACPX: {}", r.stderr);
        }
        let last = r.stdout.trim().lines().last().unwrap_or_default().to_string();
        let parts: Vec<&str> = last.split(':').collect();
        let [system_node, node, acpx] = parts.as_slice() else { bail!("Could not locate node/ACPX binaries after install: {last:?}") };
        let mut link = pinned_bin_wrapper_command(node, acpx, "/usr/local/bin/acpx");
        if *system_node == "none" {
            link += &format!(" && {}", safe_bin_symlink_command(node, "/usr/local/bin/node"));
        }
        let r = cx.exec_as(&link, Some("root"), &BTreeMap::new(), None).await?;
        if r.code != 0 {
            bail!("Failed to surface ACPX: {}", r.stderr);
        }
        target.acp_install(cx).await?;
        let cmd = target.acp_command().context("target agent does not support the 'acp' bridge")?;
        let config = build_acpx_config(&cmd, &self.overrides)?;
        let r = cx
            .exec(
                &format!(
                    "printf '%s' {} > \"$(pwd)/.acpxrc.json\"; echo \"$(pwd)/.acpxrc.json\"",
                    quote(&serde_json::to_string_pretty(&config)?)
                ),
                &BTreeMap::new(),
                None,
            )
            .await?;
        if r.code != 0 {
            bail!("Failed to write .acpxrc.json: {}", r.stderr);
        }
        let r = cx.exec("acpx sessions ensure", &BTreeMap::new(), None).await?;
        if r.code != 0 {
            bail!("Failed to start the ACP session for the target agent: {}", r.stderr);
        }
        self.ready = true;
        Ok(())
    }

    /// Close the session and export the target-side transcript to the trial's agent dir.
    pub async fn export(&self, cx: &AgentCtx<'_>, host_agent_dir: &Path) -> Result<PathBuf> {
        let _ = cx.exec("acpx sessions close", &BTreeMap::new(), None).await;
        let container = format!("/logs/agent/{TRAJECTORY_FILENAME}");
        let r = cx.exec(&format!("acpx sessions export --output {container}"), &BTreeMap::new(), None).await?;
        if r.code != 0 {
            bail!("ACPX session export failed: {}", r.stderr);
        }
        let host = host_agent_dir.join(TRAJECTORY_FILENAME);
        cx.env.download_file(crate::task::MAIN, &container, &host).await?;
        Ok(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acpx_config_and_usage() {
        let c = build_acpx_config(&["env".into(), "claude-code-acp".into()], &serde_json::Map::new()).unwrap();
        assert_eq!(c["agents"]["target"]["command"], "env");
        assert_eq!(c["agents"]["target"]["args"][0], "claude-code-acp");
        assert_eq!((c["defaultPermissions"].as_str(), c["timeout"].as_i64()), (Some("approve-all"), Some(3600)));
        let bad: serde_json::Map<String, Value> = serde_json::from_str(r#"{"agents": {}}"#).unwrap();
        assert!(build_acpx_config(&["x".into()], &bad).is_err());
        let ex = json!({"a": [{"usage": {"inputTokens": 1}}, {"b": {"input_tokens": 9, "output_tokens": 2}}]});
        assert_eq!(extract_target_usage(&ex).unwrap()["input_tokens"], 9);
    }

    #[test]
    fn user_prompt_rendering() {
        let p = render_user_prompt("do X", "use acpx", None, None).unwrap();
        assert!(p.starts_with("You are playing the role") && p.contains("use acpx\n\ndo X"));
        let d = std::env::temp_dir().join(format!("hbench-up-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let t = d.join("t.j2");
        std::fs::write(&t, "{{ instruction }}").unwrap();
        assert!(render_user_prompt("x", "y", Some(&t), None).is_err()); // missing bridge_instructions
        std::fs::write(&t, "{{ instruction }} {{ bridge_instructions }} {{ other }}").unwrap();
        assert!(render_user_prompt("x", "y", Some(&t), None).is_err());
        std::fs::write(&t, "{{ instruction }} {{ bridge_instructions }}").unwrap();
        let persona = d.join("p.md");
        std::fs::write(&persona, "P").unwrap();
        assert!(render_user_prompt("x", "y", Some(&t), Some(&persona)).is_err()); // persona ignored
        assert_eq!(render_user_prompt("x", "y", Some(&t), None).unwrap(), "x y");
    }
}
