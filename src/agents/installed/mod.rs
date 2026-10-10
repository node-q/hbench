//! Installed (CLI) agents: shared machinery from upstream's BaseInstalledAgent —
//! system-package installs, `set -o pipefail` execs, and error classification.

pub mod claude_code;
pub mod acp;
pub mod aider;
pub mod antigravity_cli;
pub mod antigravity_sdk;
pub mod cline_cli;
pub mod code_puppy;
pub mod codex;
pub mod copilot_cli;
pub mod cortex_code;
pub mod cursor_cli;
pub mod deerflow;
pub mod devin;
pub mod eve;
pub mod fx;
pub mod fx_dev;
pub mod gemini_cli;
pub mod goose;
pub mod grok_build;
pub mod hermes;
pub mod junie;
pub mod kimi_cli;
pub mod kimi_code;
pub mod langgraph;
pub mod mcode;
pub mod mimo;
pub mod mini_swe_agent;
pub mod muse_code;
pub mod nemo_agent;
pub mod openclaw;
pub mod opencode;
pub mod openhands;
pub mod openhands_sdk;
pub mod pi;
pub mod qwen_code;
pub mod rovodev_cli;
pub mod strands;
pub mod swe_agent;
pub mod trae_agent;
pub mod vibe;

use super::model_connection::{self, Resolved, Spec};
use super::{AgentArgs, AgentBox, AgentCtx};
use crate::env::{ExecResult, kind_err, quote, tail};
use anyhow::{Result, bail};
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Names of every installed agent (upstream's AgentName values).
pub const NAMES: &[&str] = &["claude-code", "acp", "cline-cli", "cortex-code", "aider", "codex", "cursor-cli", "gemini-cli", "antigravity-cli", "rovodev-cli", "goose", "grok-build", "hermes", "junie", "mini-swe-agent", "nemo-agent", "swe-agent", "opencode", "openclaw", "openhands", "openhands-sdk", "antigravity-sdk", "kimi-code", "kimi-cli", "langgraph", "deerflow", "mimo", "mcode", "pi", "qwen-coder", "copilot-cli", "code-puppy", "devin", "trae-agent", "eve", "fx", "fx-dev", "vibe", "muse-code", "strands"];

pub fn create(name: &str, args: AgentArgs) -> Result<Option<AgentBox>> {
    Ok(Some(match name {
        "claude-code" => Box::new(claude_code::ClaudeCode::new(args)?),
        "acp" => acp::create(args)?,
        "cline-cli" => cline_cli::create(args)?,
        "cortex-code" => cortex_code::create(args)?,
        "aider" => aider::create(args)?,
        "codex" => codex::create(args)?,
        "cursor-cli" => cursor_cli::create(args)?,
        "gemini-cli" => gemini_cli::create(args)?,
        "antigravity-cli" => antigravity_cli::create(args)?,
        "rovodev-cli" => rovodev_cli::create(args)?,
        "goose" => goose::create(args)?,
        "grok-build" => grok_build::create(args)?,
        "hermes" => hermes::create(args)?,
        "junie" => junie::create(args)?,
        "mini-swe-agent" => mini_swe_agent::create(args)?,
        "nemo-agent" => nemo_agent::create(args)?,
        "swe-agent" => swe_agent::create(args)?,
        "opencode" => opencode::create(args)?,
        "openclaw" => openclaw::create(args)?,
        "openhands" => openhands::create(args)?,
        "openhands-sdk" => openhands_sdk::create(args)?,
        "antigravity-sdk" => antigravity_sdk::create(args)?,
        "kimi-code" => kimi_code::create(args)?,
        "kimi-cli" => kimi_cli::create(args)?,
        "langgraph" => langgraph::create(args)?,
        "deerflow" => deerflow::create(args)?,
        "mimo" => mimo::create(args)?,
        "mcode" => mcode::create(args)?,
        "pi" => pi::create(args)?,
        "qwen-coder" => qwen_code::create(args)?,
        "copilot-cli" => copilot_cli::create(args)?,
        "code-puppy" => code_puppy::create(args)?,
        "devin" => devin::create(args)?,
        "trae-agent" => trae_agent::create(args)?,
        "eve" => eve::create(args)?,
        "fx" => fx::create(args)?,
        "fx-dev" => fx_dev::create(args)?,
        "vibe" => vibe::create(args)?,
        "muse-code" => muse_code::create(args)?,
        "strands" => strands::create(args)?,
        _ => return Ok(None),
    }))
}

/// (commands that prove presence, per-manager packages, always install)
struct PackageSpec {
    commands: &'static [&'static str],
    apt: &'static [&'static str],
    dnf: &'static [&'static str],
    apk: &'static [&'static str],
    always: bool,
}

fn package_spec(name: &str) -> Option<PackageSpec> {
    let std_ = |n: &'static str, cmd: &'static [&'static str], pkg: &'static [&'static str]| {
        let _ = n;
        PackageSpec { commands: cmd, apt: pkg, dnf: pkg, apk: pkg, always: false }
    };
    Some(match name {
        "curl" => std_("curl", &["curl"], &["curl"]),
        "bash" => std_("bash", &["bash"], &["bash"]),
        "git" => std_("git", &["git"], &["git"]),
        "tmux" => std_("tmux", &["tmux"], &["tmux"]),
        "bzip2" => std_("bzip2", &["bzip2"], &["bzip2"]),
        "tar" => std_("tar", &["tar"], &["tar"]),
        "unzip" => std_("unzip", &["unzip"], &["unzip"]),
        "wget" => std_("wget", &["wget"], &["wget"]),
        "python3" => std_("python3", &["python3"], &["python3"]),
        "npm" => std_("npm", &["npm"], &["npm"]),
        "build_tools" => PackageSpec { commands: &["gcc", "make"], apt: &["build-essential"], dnf: &["gcc", "gcc-c++", "make"], apk: &["build-base"], always: false },
        "ripgrep" => PackageSpec { commands: &["rg"], apt: &["ripgrep"], dnf: &["ripgrep"], apk: &["ripgrep"], always: false },
        "xz" => PackageSpec { commands: &["xz"], apt: &["xz-utils"], dnf: &["xz"], apk: &["xz"], always: false },
        "ca_certificates" => PackageSpec { commands: &[], apt: &["ca-certificates"], dnf: &["ca-certificates"], apk: &["ca-certificates"], always: true },
        "procps" => PackageSpec { commands: &["pgrep"], apt: &["procps"], dnf: &["procps-ng"], apk: &["procps"], always: false },
        "coreutils" => PackageSpec { commands: &["stdbuf"], apt: &["coreutils"], dnf: &["coreutils"], apk: &["coreutils"], always: false },
        "gnupg" => PackageSpec { commands: &["gpg"], apt: &["gnupg2"], dnf: &["gnupg2"], apk: &["gnupg"], always: false },
        "python_pip" => PackageSpec { commands: &["pip3"], apt: &["python3-pip"], dnf: &["python3-pip"], apk: &["py3-pip"], always: false },
        "python_venv" => PackageSpec { commands: &[], apt: &["python3-venv"], dnf: &["python3"], apk: &["py3-virtualenv"], always: true },
        "nodejs" => PackageSpec { commands: &["node"], apt: &["nodejs"], dnf: &["nodejs"], apk: &["nodejs"], always: false },
        "libxcb" => PackageSpec { commands: &[], apt: &["libxcb1"], dnf: &["libxcb"], apk: &["libxcb"], always: true },
        "libgomp" => PackageSpec { commands: &[], apt: &["libgomp1"], dnf: &["libgomp"], apk: &["libgomp"], always: true },
        _ => return None,
    })
}

/// Install missing system packages with the image's package manager (upstream semantics).
pub async fn ensure_system_dependencies(cx: &AgentCtx<'_>, deps: &[&str]) -> Result<()> {
    let mut specs = vec![];
    for d in deps {
        let Some(s) = package_spec(d) else { bail!("Unknown system dependency: {d}") };
        if !s.always {
            let check = s.commands.iter().map(|c| format!("command -v {} >/dev/null 2>&1", quote(c))).collect::<Vec<_>>().join(" && ");
            if cx.env.exec_root(&check, None).await?.code == 0 {
                continue;
            }
        }
        specs.push(s);
    }
    if specs.is_empty() {
        return Ok(());
    }
    let mgr = cx
        .env
        .exec_root("for manager in apt-get dnf yum apk; do if command -v \"$manager\" >/dev/null 2>&1; then printf \"%s\" \"$manager\"; break; fi; done", None)
        .await?
        .stdout
        .trim()
        .to_string();
    let mut pkgs: Vec<&str> = vec![];
    for s in &specs {
        let list = match mgr.as_str() {
            "apt-get" => s.apt,
            "dnf" | "yum" => s.dnf,
            "apk" => s.apk,
            _ => return Ok(()), // no supported package manager: upstream only warns
        };
        for p in list {
            if !pkgs.contains(p) {
                pkgs.push(p);
            }
        }
    }
    let args = pkgs.iter().map(|p| quote(p)).collect::<Vec<_>>().join(" ");
    let (cmd, env) = match mgr.as_str() {
        "apt-get" => (format!("apt-get update && apt-get install -y {args}"), BTreeMap::from([("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())])),
        "dnf" => (format!("dnf install -y {args}"), BTreeMap::new()),
        "yum" => (format!("yum install -y {args}"), BTreeMap::new()),
        _ => (format!("apk add --no-cache {args}"), BTreeMap::new()),
    };
    exec_root(cx, &cmd, &env).await.map(|_| ())
}

/// upstream's ERROR_PATTERNS: the match furthest toward the end of the output wins.
fn error_patterns() -> &'static [(Regex, &'static str)] {
    static P: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    P.get_or_init(|| {
        let raw: &[(&str, &str)] = &[
            (r"rate.?limit", "ApiRateLimitError"),
            (r"too many requests", "ApiRateLimitError"),
            (r"specified API usage limits", "ApiUsageLimitError"),
            (r"You've hit your usage limit", "ApiUsageLimitError"),
            (r"You have an unpaid invoice", "ApiUsageLimitError"),
            (r"Quota exceeded.", "ApiUsageLimitError"),
            (r"You have no credits remaining", "ApiUsageLimitError"),
            (r"Free tier users do not have access|Upgrade to paid credits", "ApiUsageLimitError"),
            (r"Budget has been exceeded![\s\S]{0,300}?Max\s+budget:", "ApiUsageLimitError"),
            (r"API Error: 500 Internal server error", "ApiInternalServerError"),
            (r"API Error: Server error mid-response", "ApiInternalServerError"),
            (r"RetriableError: \[internal\] Error", "ApiInternalServerError"),
            (r"API Error: Overloaded", "ApiOverloadedError"),
            (r"ServiceUnavailableError", "ApiOverloadedError"),
            (r"Selected model is at capacity\. Please try a different model\.", "ApiOverloadedError"),
            (r"API Error: (?:Connection closed|Connection lost) mid-response", "ApiConnectionClosedError"),
            (r"API Error: Connection to the API was lost", "ApiConnectionClosedError"),
            (r"API Error: stream closed before completion", "ApiConnectionClosedError"),
            (r"API Error: (?:Response stalled mid-stream|Stream idle timeout)", "ApiResponseStalledError"),
            (r"response exceeded .+ output token maximum", "OutputTokenExceededError"),
            (r"input token count exceeds the maximum number of tokens|prompt is too long: \d+ tokens > \d+ maximum", "ContextWindowExceededError"),
            (r"Not logged in", "AgentAuthenticationError"),
            (
                r"\binvalid_api_key\b|Invalid API key\b|API key is invalid\b|Incorrect API key provided\b|unexpected status 401 Unauthorized\b",
                "AgentAuthenticationError",
            ),
            (r#"could not read Username for ['"]https://github\.com"#, "AgentAuthenticationError"),
            (r"Cannot use this model", "ModelNotFoundError"),
            (
                r"\blitellm\.UnsupportedParamsError\b|does not support parameters:\s*\[[^\]]*\],\s*for\s+model=|Unsupported parameter: '[\w.]+'[\s\S]{0,300}?\bunsupported_parameter\b|_map_reasoning_effort_to_thinking_level\b[\s\S]{0,300}?Invalid reasoning effort",
                "UnsupportedModelParametersError",
            ),
            (r"\blitellm\.NotFoundError\b", "ModelNotFoundError"),
            (r#"Invalid proxy server token passed|\btoken_not_found_in_db\b|"turn\.failed"[^\n]{0,60}?unexpected status 403\b"#, "AgentAuthenticationError"),
            (r#""turn\.failed"[^\n]{0,60}?unexpected status 500\b"#, "ApiInternalServerError"),
            (r#""turn\.failed"[^\n]{0,60}?unexpected status 50[234]\b"#, "ApiOverloadedError"),
            (r"Provider Error We.re having trouble finding the resource you requested", "ApiProviderResourceNotFoundError"),
            (
                r#"safety measures that flagged|Cyber Verification Program|flagged for possible cybersecurity risk|Trusted Access for Cyber|chatgpt\.com/cyber|This request was blocked by our safety systems\.|API Error: [^\r\n]{1,80} can.t help with this\. Start a new session to continue\.|Output blocked by content filtering policy|violate our Usage Policy|https://www\.anthropic\.com/legal/aup|triggered cyber-related safeguards|model_refusal_no_fallback|ContentFilterError|blocked by the provider.s content filter|"reason"\s*:\s*"content-filter""#,
                "AgentSafetyRefusalError",
            ),
            (r"API Error", "UnknownApiError"),
            (r"Error when talking to Gemini API\s+Full report available at:", "UnknownApiError"),
            (r"SSL_ERROR_SYSCALL", "NetworkConnectionError"),
            (r"SSL_connect", "NetworkConnectionError"),
            (r"Could not resolve host", "NetworkConnectionError"),
            (r"Fatal: Could not connect to the release server to download the manifest", "NetworkConnectionError"),
            (r"Connection refused", "NetworkConnectionError"),
            (r"Connection timed out", "NetworkConnectionError"),
            (r"Request timed out", "NetworkConnectionError"),
            (r"No route to host", "NetworkConnectionError"),
            (r"curl: \(\d+\)", "NetworkConnectionError"),
        ];
        raw.iter().map(|(p, k)| (Regex::new(&format!("(?i){p}")).unwrap(), *k)).collect()
    })
}

/// Classify failed command output into a upstream error type name.
pub fn classify(output: &str) -> &'static str {
    // A soft Claude divert continues the run; don't call it a safety refusal.
    let skip_asr = output.contains("model_refusal_fallback") && !output.contains("model_refusal_no_fallback");
    let mut best: Option<(usize, &'static str)> = None;
    for (re, kind) in error_patterns() {
        if skip_asr && *kind == "AgentSafetyRefusalError" {
            continue;
        }
        for m in re.find_iter(output) {
            if best.is_none_or(|(end, _)| m.end() > end) {
                best = Some((m.end(), kind));
            }
        }
    }
    best.map(|(_, k)| k).unwrap_or("NonZeroAgentExitCodeError")
}

/// Keep only JSONL events `keep` accepts (non-JSON lines kept), so tool output can't match patterns.
pub fn filter_jsonl_events(output: &str, keep: impl Fn(&serde_json::Value) -> bool) -> String {
    output
        .lines()
        .filter(|l| {
            let t = l.trim();
            if !t.starts_with('{') {
                return true;
            }
            serde_json::from_str::<serde_json::Value>(t).is_ok_and(|v| v.is_object() && keep(&v))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max / 4).collect();
    format!("{head} ... [truncated] ... {}", tail(s, max - max / 4))
}

pub fn exec_error(cmd: &str, r: &ExecResult, classify_on: Option<&str>) -> anyhow::Error {
    let detail = format!(
        "Command failed (exit {}): {}\nstdout: {}\nstderr: {}",
        r.code,
        truncate(cmd, 2000),
        truncate(&r.stdout, 1000),
        truncate(&r.stderr, 1000)
    );
    let output = classify_on.map(String::from).unwrap_or_else(|| format!("{}\n{}", r.stdout, r.stderr));
    kind_err(classify(&output), detail)
}

/// `set -o pipefail; cmd` as the agent user; non-zero exit -> classified error.
pub async fn exec_agent(cx: &AgentCtx<'_>, cmd: &str, env: &BTreeMap<String, String>) -> Result<ExecResult> {
    let r = cx.exec(&format!("set -o pipefail; {cmd}"), env, None).await?;
    if r.code != 0 {
        return Err(exec_error(cmd, &r, None));
    }
    Ok(r)
}

pub async fn exec_root(cx: &AgentCtx<'_>, cmd: &str, env: &BTreeMap<String, String>) -> Result<ExecResult> {
    let r = cx.exec_as(&format!("set -o pipefail; {cmd}"), Some("root"), env, None).await?;
    if r.code != 0 {
        return Err(exec_error(cmd, &r, None));
    }
    Ok(r)
}

/// Resolve a model connection with agent env (> host env) precedence.
pub fn connection(args: &AgentArgs, spec: Spec) -> Resolved {
    let get = |names: &[&str]| -> Option<(String, String)> {
        for source in [Some(&args.extra_env), None] {
            for n in names {
                let v = match source {
                    Some(m) => m.get(*n).cloned(),
                    None => std::env::var(n).ok(),
                };
                if let Some(v) = v {
                    return Some((n.to_string(), v));
                }
            }
        }
        None
    };
    model_connection::resolve(args.model_name.as_deref(), spec, &get)
}

/// Write `content` to `path` in the container with mode 600, owned by the agent user.
pub async fn upload_text(cx: &AgentCtx<'_>, path: &str, content: &str) -> Result<()> {
    let local = std::env::temp_dir().join(format!("hbench-cfg-{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&local, content)?;
    let r = cx.env.upload_file(&local, path).await;
    let _ = std::fs::remove_file(&local);
    r?;
    let q = quote(path);
    let chown = cx.user.as_ref().map(|u| format!("chown {} {q} && ", quote(u))).unwrap_or_default();
    exec_root(cx, &format!("{chown}chmod 600 {q}"), &BTreeMap::new()).await.map(|_| ())
}

/// Shell snippet ensuring a Node >= 20 toolchain is active (nvm-installs Node 22 if needed).
pub fn ensure_acp_node_command() -> String {
    "if [ -s \"$HOME/.nvm/nvm.sh\" ]; then . \"$HOME/.nvm/nvm.sh\"; fi; \
     _hb_node_major=\"$(node --version 2>/dev/null | sed \"s/^v//\" | cut -d. -f1 || true)\"; \
     if [ \"${_hb_node_major:-0}\" -lt 20 ]; then \
       curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh | bash && \
       export NVM_DIR=\"$HOME/.nvm\" && . \"$NVM_DIR/nvm.sh\" && nvm install 22; \
     fi"
    .to_string()
}

/// Executable wrapper at `dest` running `target` with `interpreter` (keeps npm shims on their node).
pub fn pinned_bin_wrapper_command(interpreter: &str, target: &str, dest: &str) -> String {
    let (i, t, d) = (quote(interpreter), quote(target), quote(dest));
    format!(
        "_hb_tgt=\"$(readlink -f {t} 2>/dev/null || echo {t})\"; _hb_dst=\"$(readlink -f {d} 2>/dev/null || echo {d})\"; \
         if [ \"$_hb_tgt\" != \"$_hb_dst\" ]; then rm -f {d} && printf '#!/bin/sh\\nexec \"%s\" \"%s\" \"$@\"\\n' {i} {t} > {d} && chmod 755 {d}; fi"
    )
}

/// Symlink `source` -> `dest` without ever creating a cycle.
pub fn safe_bin_symlink_command(source: &str, dest: &str) -> String {
    let (s, d) = (quote(source), quote(dest));
    format!("_hb_real=\"$(readlink -f {s} 2>/dev/null || echo {s})\" && if [ \"$_hb_real\" != {d} ]; then ln -sf \"$_hb_real\" {d}; fi")
}

/// First x.y.z in `stdout`, else the trimmed text (upstream's parse_version).
pub fn parse_version(stdout: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(\d+\.\d+\.\d+)").unwrap());
    re.captures(stdout).map(|c| c[1].to_string()).unwrap_or_else(|| stdout.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_last_match_wins() {
        assert_eq!(classify("boom"), "NonZeroAgentExitCodeError");
        assert_eq!(classify("429 Too Many Requests"), "ApiRateLimitError");
        assert_eq!(classify("rate limit hit\n... later: Invalid API key"), "AgentAuthenticationError");
        assert_eq!(classify("API Error: Overloaded"), "ApiOverloadedError");
        assert_eq!(classify("API Error: something odd"), "UnknownApiError");
        assert_eq!(classify("curl: (6) Could not resolve host: x"), "NetworkConnectionError");
        assert_eq!(classify("violate our Usage Policy"), "AgentSafetyRefusalError");
        assert_eq!(classify("model_refusal_fallback violate our Usage Policy"), "NonZeroAgentExitCodeError");
        assert_eq!(classify("prompt is too long: 300000 tokens > 200000 maximum"), "ContextWindowExceededError");
    }

    #[test]
    fn jsonl_filter_and_version() {
        let out = "plain\n{\"type\":\"assistant\",\"text\":\"rate limit\"}\n{\"type\":\"result\",\"is_error\":true}\nnot{json";
        let f = filter_jsonl_events(out, |e| e["type"] == "result");
        assert_eq!(f, "plain\n{\"type\":\"result\",\"is_error\":true}\nnot{json");
        assert_eq!(parse_version("2.1.3 (Claude Code)"), "2.1.3");
        assert_eq!(parse_version("weird"), "weird");
    }
}
