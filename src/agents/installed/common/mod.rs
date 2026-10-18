//! Helpers shared by the installed agents: common setup steps, the nvm node install
//! snippet, and small parsing utilities.

pub mod opts;

use crate::agents::{AgentArgs, AgentCtx};
use anyhow::{Result, anyhow};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Task `skills_dir`, else `/hbench/skills` when `--skill`s were given.
pub fn skills_dir(args: &AgentArgs) -> Option<String> {
    args.task.config.environment.skills_dir.clone().or_else(|| (!args.skills.is_empty()).then(|| "/hbench/skills".into()))
}

/// Shell snippet installing Node via nvm.
pub fn nvm_node_install_snippet(node_major: u32) -> String {
    format!(
        "curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh | env -u NODE_VERSION bash && \
         export NVM_DIR=\"$HOME/.nvm\" && \\. \"$NVM_DIR/nvm.sh\" || true && \
         command -v nvm &>/dev/null || {{ echo 'Error: NVM failed to load' >&2; exit 1; }} && \
         nvm install {node_major} && nvm alias default {node_major} && npm -v"
    )
}

/// BaseInstalledAgent.setup prelude.
pub async fn installed_dir(cx: &AgentCtx<'_>) -> Result<()> {
    cx.env.exec_root("[ -d /installed-agent ] || mkdir -p /installed-agent", None).await?;
    Ok(())
}

/// Best-effort version detection after install (`get_version_command` + `parse_version`).
pub async fn detect_version(cx: &AgentCtx<'_>, cmd: &str, parse: impl Fn(&str) -> String) -> Option<String> {
    let r = cx.exec(cmd, &BTreeMap::new(), None).await.ok()?;
    (r.code == 0 && !r.stdout.is_empty()).then(|| parse(&r.stdout))
}

/// `BaseInstalledAgent.setup` around `install`: mkdir, install, best-effort version detection.
pub async fn installed_setup(
    cx: &AgentCtx<'_>,
    version: &mut Option<String>,
    install: impl std::future::Future<Output = Result<()>>,
    version_cmd: &str,
    parse: fn(&str) -> String,
) -> Result<()> {
    installed_dir(cx).await?;
    install.await?;
    if version.is_none() {
        *version = detect_version(cx, version_cmd, parse).await;
    }
    Ok(())
}

/// Upload a bundled script (root-owned, mode from the temp file).
pub async fn upload_asset(cx: &AgentCtx<'_>, content: &str, path: &str) -> Result<()> {
    let local = std::env::temp_dir().join(format!("hbench-asset-{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&local, content)?;
    let r = cx.env.upload_file(&local, path).await;
    let _ = std::fs::remove_file(&local);
    r
}

/// Recursive dict merge (`_deep_merge`): `over` wins, nested objects merge.
pub fn deep_merge(base: &mut Value, over: &Value) {
    if let (Value::Object(b), Value::Object(o)) = (base, over) {
        deep_merge_maps(b, o);
    }
}

pub fn deep_merge_maps(base: &mut Map<String, Value>, over: &Map<String, Value>) {
    for (k, v) in over {
        match (base.get_mut(k), v) {
            (Some(Value::Object(b)), Value::Object(o)) => deep_merge_maps(b, o),
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
}

/// JSON values of the lines that parse; others are skipped.
pub fn parse_jsonl(text: &str) -> Vec<Value> {
    text.lines().filter_map(|l| serde_json::from_str(l.trim()).ok()).collect()
}

pub fn parse_bool_env_value(v: &str, name: &str) -> Result<bool> {
    opts::lax_bool(&Value::String(v.into()))
        .ok_or_else(|| anyhow!("Invalid value for '{name}': cannot parse '{v}' as bool (expected true/false/1/0/yes/no)"))
}

/// An unset variable is false.
pub fn parse_bool_env(v: Option<String>, name: &str) -> Result<bool> {
    v.map_or(Ok(false), |v| parse_bool_env_value(&v, name))
}

#[cfg(test)]
pub fn test_args(kwargs: Value, env: &[(&str, &str)], model: Option<&str>) -> AgentArgs {
    let dir = std::env::temp_dir().join(format!("hbench-agent-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("environment")).unwrap();
    std::fs::write(dir.join("task.toml"), "[environment]\ndocker_image='x'\n").unwrap();
    std::fs::write(dir.join("instruction.md"), "x").unwrap();
    let task = std::sync::Arc::new(crate::task::Task::load(&dir).unwrap());
    AgentArgs {
        load_trajectory: None,
        model_name: model.map(String::from),
        kwargs: serde_json::from_value(kwargs).unwrap(),
        extra_env: env.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
        mcp_servers: vec![],
        skills: vec![],
        logs_dir: dir,
        task,
    }
}
