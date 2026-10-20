//! Installed (CLI) agents: shared machinery —
//! system-package installs, `set -o pipefail` execs, and error classification.

pub mod acp;
pub mod aider;
pub mod antigravity_cli;
pub mod antigravity_sdk;
pub mod claude_code;
pub mod cline_cli;
pub mod code_puppy;
pub mod codex;
pub mod common;
pub mod copilot_cli;
pub mod cortex_code;
pub mod cursor_cli;
pub mod deepseek_harness;
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
pub mod pins;
pub mod qwen_code;
pub mod rovodev_cli;
pub mod strands;
pub mod swe_agent;
pub mod trae_agent;
pub mod vibe;

use super::model_connection::{self, Resolved, Spec};
use super::{AgentArgs, AgentBox, AgentCtx};
use crate::env::{ErrorKind, ExecResult, kind_err, quote, tail};
use anyhow::{Result, bail};
use common::NVM_LOAD;
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::OnceLock;

type Constructor = fn(AgentArgs) -> Result<AgentBox>;

/// Every installed agent name and its constructor, in listing order.
const REGISTRY: &[(&str, Constructor)] = &[
    ("claude-code", |args| Ok(Box::new(claude_code::ClaudeCode::new(args)?))),
    ("acp", acp::create),
    ("cline-cli", cline_cli::create),
    ("cortex-code", cortex_code::create),
    ("aider", aider::create),
    ("codex", codex::create),
    ("cursor-cli", cursor_cli::create),
    ("gemini-cli", gemini_cli::create),
    ("antigravity-cli", antigravity_cli::create),
    ("rovodev-cli", rovodev_cli::create),
    ("goose", goose::create),
    ("grok-build", grok_build::create),
    ("hermes", hermes::create),
    ("junie", junie::create),
    ("mini-swe-agent", mini_swe_agent::create),
    ("nemo-agent", nemo_agent::create),
    ("swe-agent", swe_agent::create),
    ("opencode", opencode::create),
    ("openclaw", openclaw::create),
    ("openhands", openhands::create),
    ("openhands-sdk", openhands_sdk::create),
    ("antigravity-sdk", antigravity_sdk::create),
    ("kimi-code", kimi_code::create),
    ("kimi-cli", kimi_cli::create),
    ("langgraph", langgraph::create),
    ("deerflow", deerflow::create),
    ("mimo", mimo::create),
    ("mcode", mcode::create),
    ("pi", pi::create),
    ("qwen-coder", qwen_code::create),
    ("copilot-cli", copilot_cli::create),
    ("code-puppy", code_puppy::create),
    ("devin", devin::create),
    ("trae-agent", trae_agent::create),
    ("eve", eve::create),
    ("fx", fx::create),
    ("fx-dev", fx_dev::create),
    ("vibe", vibe::create),
    ("muse-code", muse_code::create),
    ("strands", strands::create),
    ("deepseek-harness", deepseek_harness::create),
];

pub fn names() -> impl Iterator<Item = &'static str> {
    REGISTRY.iter().map(|(name, _)| *name)
}

pub fn create(name: &str, args: AgentArgs) -> Result<Option<AgentBox>> {
    REGISTRY.iter().find(|(n, _)| *n == name).map(|(_, construct)| construct(args)).transpose()
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
    let std_ = |cmd: &'static [&'static str], pkg: &'static [&'static str]| PackageSpec {
        commands: cmd,
        apt: pkg,
        dnf: pkg,
        apk: pkg,
        always: false,
    };
    Some(match name {
        "curl" => std_(&["curl"], &["curl"]),
        "bash" => std_(&["bash"], &["bash"]),
        "git" => std_(&["git"], &["git"]),
        "tmux" => std_(&["tmux"], &["tmux"]),
        "bzip2" => std_(&["bzip2"], &["bzip2"]),
        "tar" => std_(&["tar"], &["tar"]),
        "unzip" => std_(&["unzip"], &["unzip"]),
        "wget" => std_(&["wget"], &["wget"]),
        "python3" => std_(&["python3"], &["python3"]),
        "npm" => std_(&["npm"], &["npm"]),
        "build_tools" => PackageSpec {
            commands: &["gcc", "make"],
            apt: &["build-essential"],
            dnf: &["gcc", "gcc-c++", "make"],
            apk: &["build-base"],
            always: false,
        },
        "ripgrep" => PackageSpec { commands: &["rg"], apt: &["ripgrep"], dnf: &["ripgrep"], apk: &["ripgrep"], always: false },
        "xz" => PackageSpec { commands: &["xz"], apt: &["xz-utils"], dnf: &["xz"], apk: &["xz"], always: false },
        "ca_certificates" => {
            PackageSpec { commands: &[], apt: &["ca-certificates"], dnf: &["ca-certificates"], apk: &["ca-certificates"], always: true }
        }
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

/// Shell function `hb_apt_install PKG...`: `apt-get update && apt-get install -y PKG...`, retried against the archive
/// mirrors an EOL release moves to while its index stays up. Stage 1 points only the security suites at the archive,
/// stage 2 every Debian/Ubuntu source. The rewritten sources go in a temp dir used via `-o Dir::Etc::*`, so the image's
/// apt config is left as is.
pub const APT_INSTALL_FN: &str = r#"hb_apt_install() {
  if apt-get update && apt-get install -y "$@"; then return 0; fi
  _hb_rc=1
  for _hb_stage in 1 2; do
    echo "hbench: apt-get install failed; retrying with archived mirrors (stage $_hb_stage)" >&2
    _hb_s='s#https\?://\(deb\|security\)\.debian\.org/debian-security#http://archive.debian.org/debian-security#g'
    if [ "$_hb_stage" = 2 ]; then
      _hb_s="$_hb_s;"'s#https\?://security\.debian\.org\([ \t]\)#http://archive.debian.org/debian-security\1#g;s#https\?://deb\.debian\.org/debian\([/ \t]\|$\)#http://archive.debian.org/debian\1#g;s#https\?://\([a-z]*\.\)\?\(archive\|security\)\.ubuntu\.com/ubuntu#http://old-releases.ubuntu.com/ubuntu#g'
    fi
    _hb_d=/tmp/hbench-apt.$$; rm -rf "$_hb_d"; mkdir -p "$_hb_d/parts" "$_hb_d/conf"; : > "$_hb_d/sources.list"
    # Same apt config minus docker-clean, whose update hook would delete the .debs the failed attempt downloaded.
    # (APT_CONFIG is read before apt.conf.d; a command-line -o Dir::Etc::Parts would come too late.)
    cp -R /etc/apt/apt.conf.d/. "$_hb_d/conf/" 2>/dev/null || true; rm -f "$_hb_d/conf/docker-clean"
    { [ -f /etc/apt/apt.conf ] && cat /etc/apt/apt.conf; echo "Dir::Etc::Parts \"$_hb_d/conf\";"; } > "$_hb_d/apt.conf" || true
    if [ -f /etc/apt/sources.list ]; then sed "$_hb_s" /etc/apt/sources.list > "$_hb_d/sources.list"; fi
    for _hb_f in /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
      if [ -f "$_hb_f" ]; then sed "$_hb_s" "$_hb_f" > "$_hb_d/parts/${_hb_f##*/}"; fi
    done
    _hb_o="-o Dir::Etc::SourceList=$_hb_d/sources.list -o Dir::Etc::SourceParts=$_hb_d/parts -o Acquire::Check-Valid-Until=false"
    _hb_rc=0; { APT_CONFIG="$_hb_d/apt.conf" apt-get $_hb_o update && APT_CONFIG="$_hb_d/apt.conf" apt-get $_hb_o install -y "$@"; } || _hb_rc=$?
    rm -rf "$_hb_d"
    if [ "$_hb_rc" = 0 ]; then break; fi
  done
  apt-get update >/dev/null 2>&1 || true
  return $_hb_rc
}
"#;

/// Install missing system packages with the image's package manager.
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
            _ => return Ok(()), // no supported package manager: skip
        };
        for p in list {
            if !pkgs.contains(p) {
                pkgs.push(p);
            }
        }
    }
    let args = pkgs.iter().map(|p| quote(p)).collect::<Vec<_>>().join(" ");
    let (cmd, env) = match mgr.as_str() {
        "apt-get" => (
            format!("{APT_INSTALL_FN}hb_apt_install {args}"),
            BTreeMap::from([("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())]),
        ),
        "dnf" => (format!("dnf install -y {args}"), BTreeMap::new()),
        "yum" => (format!("yum install -y {args}"), BTreeMap::new()),
        _ => (format!("apk add --no-cache {args}"), BTreeMap::new()),
    };
    if mgr == "apt-get" {
        // Report the plain command, not the helper's body.
        let r = cx.exec_as(&format!("set -o pipefail; {cmd}"), Some("root"), &env, None).await?;
        if r.code != 0 {
            return Err(exec_error(&format!("apt-get update && apt-get install -y {args} (also tried archived mirrors)"), &r, None));
        }
        return Ok(());
    }
    exec_root_checked(cx, &cmd, &env).await.map(|_| ())
}

/// Error patterns: the match furthest toward the end of the output wins.
fn error_patterns() -> &'static [(Regex, ErrorKind)] {
    static P: OnceLock<Vec<(Regex, ErrorKind)>> = OnceLock::new();
    P.get_or_init(|| {
        let raw: &[(&str, ErrorKind)] = &[
            (r"rate.?limit", ErrorKind::ApiRateLimit),
            (r"too many requests", ErrorKind::ApiRateLimit),
            (r"specified API usage limits", ErrorKind::ApiUsageLimit),
            (r"You've hit your usage limit", ErrorKind::ApiUsageLimit),
            (r"You have an unpaid invoice", ErrorKind::ApiUsageLimit),
            (r"Quota exceeded.", ErrorKind::ApiUsageLimit),
            (r"You have no credits remaining", ErrorKind::ApiUsageLimit),
            (r"Free tier users do not have access|Upgrade to paid credits", ErrorKind::ApiUsageLimit),
            (r"Budget has been exceeded![\s\S]{0,300}?Max\s+budget:", ErrorKind::ApiUsageLimit),
            (r"API Error: 500 Internal server error", ErrorKind::ApiInternalServer),
            (r"API Error: Server error mid-response", ErrorKind::ApiInternalServer),
            (r"RetriableError: \[internal\] Error", ErrorKind::ApiInternalServer),
            (r"API Error: Overloaded", ErrorKind::ApiOverloaded),
            (r"ServiceUnavailableError", ErrorKind::ApiOverloaded),
            (r"Selected model is at capacity\. Please try a different model\.", ErrorKind::ApiOverloaded),
            (r"API Error: (?:Connection closed|Connection lost) mid-response", ErrorKind::ApiConnectionClosed),
            (r"API Error: Connection to the API was lost", ErrorKind::ApiConnectionClosed),
            (r"API Error: stream closed before completion", ErrorKind::ApiConnectionClosed),
            (r"API Error: (?:Response stalled mid-stream|Stream idle timeout)", ErrorKind::ApiResponseStalled),
            (r"response exceeded .+ output token maximum", ErrorKind::OutputTokenExceeded),
            (r"input token count exceeds the maximum number of tokens|prompt is too long: \d+ tokens > \d+ maximum", ErrorKind::ContextWindowExceeded),
            (r"Not logged in", ErrorKind::AgentAuthentication),
            (
                r"\binvalid_api_key\b|Invalid API key\b|API key is invalid\b|Incorrect API key provided\b|unexpected status 401 Unauthorized\b",
                ErrorKind::AgentAuthentication,
            ),
            (r#"could not read Username for ['"]https://github\.com"#, ErrorKind::AgentAuthentication),
            (r"Cannot use this model", ErrorKind::ModelNotFound),
            (
                r"\blitellm\.UnsupportedParamsError\b|does not support parameters:\s*\[[^\]]*\],\s*for\s+model=|Unsupported parameter: '[\w.]+'[\s\S]{0,300}?\bunsupported_parameter\b|_map_reasoning_effort_to_thinking_level\b[\s\S]{0,300}?Invalid reasoning effort",
                ErrorKind::UnsupportedModelParameters,
            ),
            (r"\blitellm\.NotFoundError\b", ErrorKind::ModelNotFound),
            (r#"Invalid proxy server token passed|\btoken_not_found_in_db\b|"turn\.failed"[^\n]{0,60}?unexpected status 403\b"#, ErrorKind::AgentAuthentication),
            (r#""turn\.failed"[^\n]{0,60}?unexpected status 500\b"#, ErrorKind::ApiInternalServer),
            (r#""turn\.failed"[^\n]{0,60}?unexpected status 50[234]\b"#, ErrorKind::ApiOverloaded),
            (r"Provider Error We.re having trouble finding the resource you requested", ErrorKind::ApiProviderResourceNotFound),
            (
                r#"safety measures that flagged|Cyber Verification Program|flagged for possible cybersecurity risk|Trusted Access for Cyber|chatgpt\.com/cyber|This request was blocked by our safety systems\.|API Error: [^\r\n]{1,80} can.t help with this\. Start a new session to continue\.|Output blocked by content filtering policy|violate our Usage Policy|https://www\.anthropic\.com/legal/aup|triggered cyber-related safeguards|model_refusal_no_fallback|ContentFilterError|blocked by the provider.s content filter|"reason"\s*:\s*"content-filter""#,
                ErrorKind::AgentSafetyRefusal,
            ),
            (r"API Error", ErrorKind::UnknownApi),
            (r"Error when talking to Gemini API\s+Full report available at:", ErrorKind::UnknownApi),
            (r"SSL_ERROR_SYSCALL", ErrorKind::NetworkConnection),
            (r"SSL_connect", ErrorKind::NetworkConnection),
            (r"Could not resolve host", ErrorKind::NetworkConnection),
            (r"Fatal: Could not connect to the release server to download the manifest", ErrorKind::NetworkConnection),
            (r"Connection refused", ErrorKind::NetworkConnection),
            (r"Connection timed out", ErrorKind::NetworkConnection),
            (r"Request timed out", ErrorKind::NetworkConnection),
            (r"No route to host", ErrorKind::NetworkConnection),
            (r"curl: \(\d+\)", ErrorKind::NetworkConnection),
        ];
        raw.iter().map(|(p, k)| (Regex::new(&format!("(?i){p}")).unwrap(), k.clone())).collect()
    })
}

/// The error type of a failed command's output.
pub fn classify(output: &str) -> ErrorKind {
    classify_with(output, &[])
}

/// `classify` with agent-specific patterns appended after the base list:
/// last match end wins, earlier pattern wins ties.
pub fn classify_with(output: &str, extra: &[(Regex, ErrorKind)]) -> ErrorKind {
    classify_ordered(output, &[], extra)
}

/// Agent patterns placed `before` and/or `after` the base list; same last-match/tie rules.
pub fn classify_ordered(output: &str, before: &[(Regex, ErrorKind)], after: &[(Regex, ErrorKind)]) -> ErrorKind {
    // A soft Claude divert continues the run; don't call it a safety refusal.
    let skip_asr = output.contains("model_refusal_fallback") && !output.contains("model_refusal_no_fallback");
    let mut best: Option<(usize, &ErrorKind)> = None;
    for (re, kind) in before.iter().chain(error_patterns().iter()).chain(after.iter()) {
        if skip_asr && *kind == ErrorKind::AgentSafetyRefusal {
            continue;
        }
        for m in re.find_iter(output) {
            if best.is_none_or(|(end, _)| m.end() > end) {
                best = Some((m.end(), kind));
            }
        }
    }
    best.map_or(ErrorKind::NonZeroAgentExitCode, |(_, k)| k.clone())
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

pub async fn exec_root_checked(cx: &AgentCtx<'_>, cmd: &str, env: &BTreeMap<String, String>) -> Result<ExecResult> {
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
    common::upload_asset(cx, content, path).await?;
    let q = quote(path);
    let chown = cx.user.as_ref().map(|u| format!("chown {} {q} && ", quote(u))).unwrap_or_default();
    exec_root_checked(cx, &format!("{chown}chmod 600 {q}"), &BTreeMap::new()).await.map(|_| ())
}

/// Shell snippet ensuring a Node >= 20 toolchain is active (nvm-installs Node 22 if needed).
pub fn ensure_acp_node_command() -> String {
    format!(
        "{NVM_LOAD}; \
         _hb_node_major=\"$(node --version 2>/dev/null | sed \"s/^v//\" | cut -d. -f1 || true)\"; \
         if [ \"${{_hb_node_major:-0}}\" -lt 20 ]; then \
           {} && \
           export NVM_DIR=\"$HOME/.nvm\" && . \"$NVM_DIR/nvm.sh\" && nvm install 22; \
         fi",
        common::nvm_install_script()
    )
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

/// First x.y.z in `stdout`, else the trimmed text.
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
    fn registry_is_consistent() {
        let mut seen = std::collections::BTreeSet::new();
        for name in names() {
            assert!(seen.insert(name), "{name} registered twice");
            assert!(crate::schemas::options(name).is_some(), "{name} has no options schema");
            let created = create(name, common::test_args(serde_json::json!({}), &[], None));
            assert!(!matches!(created, Ok(None)), "{name} not constructed by create");
        }
        assert!(matches!(create("no-such-agent", common::test_args(serde_json::json!({}), &[], None)), Ok(None)));
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
