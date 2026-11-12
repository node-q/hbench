//! Apps opened from Finder or the Dock get launchd's bare environment, not the user's shell profile, so hbench
//! would find neither `docker` nor the model API keys exported in ~/.zshrc. Like VS Code, read the login shell's
//! environment once at startup and hand it to hbench.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Where docker usually lives on macOS and Linux, appended to PATH in case the profile doesn't add it.
const EXTRA_PATH: &[&str] = &["/usr/local/bin", "/opt/homebrew/bin", "/Applications/Docker.app/Contents/Resources/bin"];

fn login_shell_env() -> Option<HashMap<String, String>> {
    if cfg!(windows) {
        return None;
    }
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let out = Command::new(&shell).args(["-ilc", "env -0"]).stdin(Stdio::null()).stderr(Stdio::null()).output();
        let _ = tx.send(out);
    });
    // A profile that hangs (prompts, slow plugins) must not hold the app up.
    let out = rx.recv_timeout(Duration::from_secs(8)).ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let env: HashMap<String, String> = text
        .split('\0')
        .filter_map(|kv| kv.split_once('='))
        .filter(|(k, _)| !k.is_empty() && !k.contains(char::is_whitespace))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    (!env.is_empty()).then_some(env)
}

/// The environment hbench runs with: the login shell's (when it can be read) over the app's own, with
/// docker's usual locations on PATH.
pub fn resolve() -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    if let Some(shell) = login_shell_env() {
        for (k, v) in shell {
            // Shell-session noise that shouldn't follow hbench around.
            if matches!(k.as_str(), "SHLVL" | "PWD" | "OLDPWD" | "_") {
                continue;
            }
            env.insert(k, v);
        }
    }
    let sep = if cfg!(windows) { ';' } else { ':' };
    let mut path: Vec<String> = env.get("PATH").map(|p| p.split(sep).map(String::from).collect()).unwrap_or_default();
    if let Some(home) = env.get("HOME") {
        let docker_home = format!("{home}/.docker/bin");
        if !path.contains(&docker_home) {
            path.push(docker_home);
        }
    }
    for p in EXTRA_PATH {
        if !path.iter().any(|q| q == p) {
            path.push(p.to_string());
        }
    }
    env.insert("PATH".into(), path.join(&sep.to_string()));
    env
}
