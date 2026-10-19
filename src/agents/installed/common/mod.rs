//! Helpers shared by the installed agents: common setup steps, the nvm node install
//! snippet, and small parsing utilities.

pub mod opts;

use super::pins;
use crate::agents::{AgentArgs, AgentCtx};
use anyhow::{Result, anyhow};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Task `skills_dir`, else `/hbench/skills` when `--skill`s were given.
pub fn skills_dir(args: &AgentArgs) -> Option<String> {
    args.task.config.environment.skills_dir.clone().or_else(|| (!args.skills.is_empty()).then(|| "/hbench/skills".into()))
}

/// Shell snippet loading nvm when it is installed (a no-op otherwise). A macro so consts can `concat!` it.
macro_rules! nvm_load {
    () => {
        "if [ -s \"$HOME/.nvm/nvm.sh\" ]; then . \"$HOME/.nvm/nvm.sh\"; fi"
    };
}
pub(crate) use nvm_load;
pub const NVM_LOAD: &str = nvm_load!();

/// Shell command running the pinned uv installer, installing into `dir` when given.
pub fn uv_install(dir: Option<&str>) -> String {
    let url = pins::uv_installer();
    match dir {
        Some(d) => format!("curl -LsSf {url} | env UV_INSTALL_DIR={d} sh"),
        None => format!("curl -LsSf {url} | sh"),
    }
}

/// Shell command running the pinned nvm installer.
pub fn nvm_install_script() -> String {
    format!("curl -o- {} | bash", pins::nvm_installer())
}

/// Shell snippet loading a just-installed nvm (failing if it did not load), then installing Node `node_major`
/// as the default.
pub fn nvm_activate_node(node_major: u32) -> String {
    format!(
        "export NVM_DIR=\"$HOME/.nvm\" && \\. \"$NVM_DIR/nvm.sh\" || true && \
         command -v nvm &>/dev/null || {{ echo 'Error: NVM failed to load' >&2; exit 1; }} && \
         nvm install {node_major} && nvm alias default {node_major} && npm -v"
    )
}

/// Shell snippet installing Node via nvm.
pub fn nvm_node_install_snippet(node_major: u32) -> String {
    format!("curl -o- {} | env -u NODE_VERSION bash && {}", pins::nvm_installer(), nvm_activate_node(node_major))
}

/// Shell condition true on musl (Alpine) images, where nvm's prebuilt Node does not run.
pub const IS_MUSL: &str = "ldd --version 2>&1 | grep -qi musl || [ -f /etc/alpine-release ]";

/// Shell snippet keeping the system Node on musl images, else installing Node `node_major` via nvm.
pub fn system_or_nvm_node(node_major: u32) -> String {
    format!("if {IS_MUSL}; then node --version && npm --version; else {}; fi", nvm_node_install_snippet(node_major))
}

/// Create `/installed-agent`, where agents keep their install payloads.
pub async fn installed_dir(cx: &AgentCtx<'_>) -> Result<()> {
    cx.env.exec_root("[ -d /installed-agent ] || mkdir -p /installed-agent", None).await?;
    Ok(())
}

/// Run `cmd` and parse its output as the installed version; None if it fails or prints nothing.
pub async fn detect_version(cx: &AgentCtx<'_>, cmd: &str, parse: impl Fn(&str) -> String) -> Option<String> {
    let r = cx.exec(cmd, &BTreeMap::new(), None).await.ok()?;
    (r.code == 0 && !r.stdout.is_empty()).then(|| parse(&r.stdout))
}

/// Create `/installed-agent`, run `install`, then detect the version unless the `version` kwarg set it.
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
    // Assets include auth files, so the host copy is private to this user.
    let local = crate::env::private_temp_file("hbench-asset", content.as_bytes())?;
    let r = cx.env.upload_file(&local, path).await;
    let _ = std::fs::remove_file(&local);
    r
}

/// Recursive merge: `over` wins, nested objects merge.
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
