//! Default versions of everything the installed agents fetch, so that an upstream release can neither change a
//! benchmark's results nor break its install. An agent's `version` kwarg overrides its own pin; `version: latest`
//! asks for the newest release where the installer understands it. Dependabot cannot see these:
//! `scripts/dev/check-pins.py` lists the ones with a newer release.

/// Where a pin is published, which is where `check-pins.py` looks for newer releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Npm,
    PyPi,
    /// A GitHub release or tag (a leading `v` on the tag is ignored).
    GitHub,
    /// A commit on the repository's default branch.
    Commit,
    /// A vendor URL that answers with the current version (plain text, or JSON with a `version` field).
    Url(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    /// Package name, or `owner/repo` for GitHub sources.
    pub name: &'static str,
    pub version: &'static str,
    pub source: Source,
}

impl Pin {
    const fn npm(name: &'static str, version: &'static str) -> Pin {
        Pin { name, version, source: Source::Npm }
    }
    const fn pypi(name: &'static str, version: &'static str) -> Pin {
        Pin { name, version, source: Source::PyPi }
    }
    const fn github(name: &'static str, version: &'static str) -> Pin {
        Pin { name, version, source: Source::GitHub }
    }
    const fn commit(name: &'static str, version: &'static str) -> Pin {
        Pin { name, version, source: Source::Commit }
    }
    const fn url(name: &'static str, version: &'static str, latest: &'static str) -> Pin {
        Pin { name, version, source: Source::Url(latest) }
    }

    /// The `version` kwarg if given, else the pin.
    pub fn or<'a>(&'a self, version: Option<&'a str>) -> &'a str {
        version.unwrap_or(self.version)
    }

    /// `name@version` for npm.
    pub fn npm_spec(&self, version: Option<&str>) -> String {
        format!("{}@{}", self.name, self.or(version))
    }

    /// `==version` for pip and uv; empty (unconstrained) for `latest`.
    pub fn pip_constraint(&self, version: Option<&str>) -> String {
        match self.or(version) {
            "latest" => String::new(),
            v => format!("=={v}"),
        }
    }

    /// `name==version`, the form for a pinned dependency.
    pub fn requirement(&self) -> String {
        format!("{}=={}", self.name, self.version)
    }
}

pub const AIDER: Pin = Pin::pypi("aider-chat", "0.86.2");
pub const CLAUDE_CODE: Pin = Pin::npm("@anthropic-ai/claude-code", "2.1.302");
pub const CLAUDE_CODE_ACP: Pin = Pin::npm("@zed-industries/claude-code-acp", "0.16.2");
pub const CLINE: Pin = Pin::npm("cline", "3.0.73-nightly.1792239493");
pub const CODE_PUPPY: Pin = Pin::pypi("code-puppy", "0.0.933");
pub const CODEX: Pin = Pin::npm("@openai/codex", "0.163.0");
pub const CODEX_ACP: Pin = Pin::npm("@agentclientprotocol/codex-acp", "1.12.0");
pub const COPILOT_CLI: Pin = Pin::github("github/copilot-cli", "1.0.98");
pub const DEERFLOW: Pin = Pin::commit("bytedance/deer-flow", "7a6c4a994a86583d2a3c056ee9d0f157d4f030c2");
pub const DEEPSEEK_HARNESS: Pin = Pin::npm("@deepseek-ai/dsh", "0.2.0-rc.2");
pub const DEVIN: Pin = Pin::url("devin", "3000.11.92", "https://static.devin.ai/cli/current/manifest.json");
pub const FX: Pin = Pin::url("fx", "0.0.14", "https://releases.fx.sh/latest.txt");
pub const GEMINI_CLI: Pin = Pin::npm("@google/gemini-cli", "0.64.0");
pub const GOOSE: Pin = Pin::github("block/goose", "1.55.0");
pub const GROK_BUILD: Pin = Pin::url("grok", "1.0.62", "https://x.ai/cli/stable");
/// Release v0.22.2.
pub const HERMES: Pin = Pin::commit("NousResearch/hermes-agent", "d923a5eb487a17c0a68801bba9ef0f56c81040a8");
pub const KIMI_CLI: Pin = Pin::pypi("kimi-cli", "1.52.0");
pub const KIMI_CODE: Pin = Pin::npm("@moonshot-ai/kimi-code", "2.2.0");
pub const LANGGRAPH: Pin = Pin::pypi("langgraph", "1.2.15");
pub const LANGSMITH: Pin = Pin::npm("langsmith", "0.3.68");
pub const MCODE: Pin = Pin::npm("@minimax-ai/code", "0.6.12");
pub const MINI_SWE_AGENT: Pin = Pin::pypi("mini-swe-agent", "2.4.6");
pub const NEMO_AGENT: Pin = Pin::pypi("nvidia-nat", "1.9.0");
pub const OPENCLAW: Pin = Pin::npm("openclaw", "2026.10.4");
pub const OPENCODE: Pin = Pin::npm("opencode-ai", "1.19.3");
pub const OPENHANDS: Pin = Pin::pypi("openhands-ai", "1.11.0");
/// openhands-sdk and openhands-tools are released together, under one version.
pub const OPENHANDS_SDK: Pin = Pin::pypi("openhands-sdk", "1.55.1");
pub const PI: Pin = Pin::npm("@earendil-works/pi-coding-agent", "1.3.0");
pub const PI_MCP_ADAPTER: Pin = Pin::npm("pi-mcp-adapter", "2.34.0");
pub const QWEN_CODE: Pin = Pin::npm("@qwen-code/qwen-code", "0.25.2");
pub const STRANDS: Pin = Pin::pypi("strands-agents", "1.54.0");
pub const SWE_AGENT: Pin = Pin::commit("SWE-agent/SWE-agent", "3ea751c087f32b16e039a2233dd6eefecef325d5");
pub const TRAE_AGENT: Pin = Pin::commit("bytedance/trae-agent", "e839e559ac61bdd0e057c375dd1dee391fee797d");
pub const TSX: Pin = Pin::npm("tsx", "4.19.3");
pub const VIBE: Pin = Pin::pypi("mistral-vibe", "2.26.1");

pub const ACP_PROTOCOL: Pin = Pin::pypi("agent-client-protocol", "0.12.1");
pub const ASCIINEMA: Pin = Pin::pypi("asciinema", "2.4.0");
pub const BUN: Pin = Pin::npm("bun", "1.4.3");
pub const FASTAPI: Pin = Pin::pypi("fastapi", "0.143.2");
/// litellm 1.106.0 imports `mcp` without depending on it, so every model call fails.
pub const LITELLM: Pin = Pin::pypi("litellm", "1.105.1");
pub const NVM: Pin = Pin::github("nvm-sh/nvm", "0.40.2");
pub const ORJSON: Pin = Pin::pypi("orjson", "3.13.0");
pub const PYTHON_DOTENV: Pin = Pin::pypi("python-dotenv", "1.2.4");
pub const TMUX: Pin = Pin::github("tmux/tmux", "3.4");
pub const UV: Pin = Pin::github("astral-sh/uv", "0.13.1");

/// uv's installer for the pinned release.
pub fn uv_installer() -> String {
    format!("https://astral.sh/uv/{}/install.sh", UV.version)
}

/// nvm's installer for the pinned release.
pub fn nvm_installer() -> String {
    format!("https://raw.githubusercontent.com/nvm-sh/nvm/v{}/install.sh", NVM.version)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How each installed agent's default install is pinned.
    #[derive(Debug)]
    enum Install {
        Pinned(&'static [Pin]),
        /// The vendor's installer always fetches its newest release and takes no version.
        Vendor,
    }

    const AGENTS: &[(&str, Install)] = &[
        ("acp", Install::Pinned(&[ACP_PROTOCOL, UV])),
        ("aider", Install::Pinned(&[AIDER, UV])),
        ("antigravity-cli", Install::Vendor),
        ("antigravity-sdk", Install::Pinned(&[UV])),
        ("claude-code", Install::Pinned(&[CLAUDE_CODE, CLAUDE_CODE_ACP, NVM])),
        ("cline-cli", Install::Pinned(&[CLINE, NVM])),
        ("code-puppy", Install::Pinned(&[CODE_PUPPY, UV])),
        ("codex", Install::Pinned(&[CODEX, CODEX_ACP, NVM])),
        ("copilot-cli", Install::Pinned(&[COPILOT_CLI])),
        ("cortex-code", Install::Vendor),
        ("cursor-cli", Install::Vendor),
        ("deepseek-harness", Install::Pinned(&[DEEPSEEK_HARNESS, NVM])),
        ("deerflow", Install::Pinned(&[DEERFLOW, UV])),
        ("devin", Install::Pinned(&[DEVIN])),
        ("eve", Install::Pinned(&[BUN, NVM])),
        ("fx", Install::Pinned(&[FX])),
        ("fx-dev", Install::Pinned(&[FX])),
        ("gemini-cli", Install::Pinned(&[GEMINI_CLI, NVM])),
        ("goose", Install::Pinned(&[GOOSE])),
        ("grok-build", Install::Pinned(&[GROK_BUILD])),
        ("hermes", Install::Pinned(&[HERMES])),
        ("junie", Install::Vendor),
        ("kimi-cli", Install::Pinned(&[KIMI_CLI, UV])),
        ("kimi-code", Install::Pinned(&[KIMI_CODE])),
        ("langgraph", Install::Pinned(&[LANGGRAPH, PYTHON_DOTENV, TSX, LANGSMITH, BUN, UV, NVM])),
        ("mcode", Install::Pinned(&[MCODE, NVM])),
        ("mimo", Install::Vendor),
        ("mini-swe-agent", Install::Pinned(&[MINI_SWE_AGENT, LITELLM, ORJSON, FASTAPI, UV])),
        ("muse-code", Install::Vendor),
        ("nemo-agent", Install::Pinned(&[NEMO_AGENT, UV])),
        ("openclaw", Install::Pinned(&[OPENCLAW, NVM])),
        ("opencode", Install::Pinned(&[OPENCODE, NVM])),
        ("openhands", Install::Pinned(&[OPENHANDS, UV])),
        ("openhands-sdk", Install::Pinned(&[OPENHANDS_SDK, FASTAPI, UV])),
        ("pi", Install::Pinned(&[PI, PI_MCP_ADAPTER, NVM])),
        ("qwen-coder", Install::Pinned(&[QWEN_CODE, NVM])),
        ("rovodev-cli", Install::Vendor),
        ("strands", Install::Pinned(&[STRANDS, UV])),
        ("swe-agent", Install::Pinned(&[SWE_AGENT, UV])),
        ("trae-agent", Install::Pinned(&[TRAE_AGENT, UV])),
        ("vibe", Install::Pinned(&[VIBE, UV])),
    ];

    #[test]
    fn every_installed_agent_says_how_it_is_pinned() {
        let names: Vec<&str> = super::super::names().collect();
        for n in &names {
            assert!(AGENTS.iter().any(|(a, _)| a == n), "{n} is missing from pins::AGENTS");
        }
        for (a, install) in AGENTS {
            assert!(names.contains(a), "pins::AGENTS lists {a}, which is not registered");
            if let Install::Pinned(pins) = install {
                assert!(!pins.is_empty() && pins.iter().all(|p| !p.version.is_empty() && p.version != "latest"), "{a}");
            }
        }
    }

    /// The agents' sources may not fall back to a moving target; their pins live here.
    #[test]
    fn no_default_install_floats() {
        let floating = [
            "version.as_deref().unwrap_or(\"latest\")",
            "@latest",
            "astral.sh/uv/install.sh",
            "nvm-sh/nvm/v",
            "releases/download/stable",
            "install asciinema\"",
            "install -g bun ",
            "install -g bun\"",
            "nvidia-nat[eval,langchain]>=",
        ];
        let mut stack = vec![std::path::PathBuf::from("src/agents")];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.file_name().is_some_and(|n| n == "pins.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&p).unwrap();
                let code = text.split("\n#[cfg(test)]").next().unwrap();
                for f in floating {
                    assert!(!code.contains(f), "{} installs a floating version ({f})", p.display());
                }
            }
        }
    }

    #[test]
    fn specs() {
        assert_eq!(CODEX.npm_spec(None), "@openai/codex@0.163.0");
        assert_eq!(CODEX.npm_spec(Some("latest")), "@openai/codex@latest");
        assert_eq!(KIMI_CLI.pip_constraint(None), "==1.52.0");
        assert_eq!(KIMI_CLI.pip_constraint(Some("latest")), "");
        assert_eq!(LITELLM.requirement(), "litellm==1.105.1");
        assert_eq!(uv_installer(), "https://astral.sh/uv/0.13.1/install.sh");
    }
}
