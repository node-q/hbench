//! Vendored JSON schemas (task.toml, trial config, each agent's options) under assets/schemas.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

pub const TASK: &str = include_str!("../assets/schemas/task.json");
pub const TRIAL: &str = include_str!("../assets/schemas/trial.json");

const AGENTS: &[(&str, &str)] = &[
    ("acp", include_str!("../assets/schemas/agents/acp.json")),
    ("aider", include_str!("../assets/schemas/agents/aider.json")),
    ("antigravity-cli", include_str!("../assets/schemas/agents/antigravity-cli.json")),
    ("antigravity-sdk", include_str!("../assets/schemas/agents/antigravity-sdk.json")),
    ("claude-code", include_str!("../assets/schemas/agents/claude-code.json")),
    ("cline-cli", include_str!("../assets/schemas/agents/cline-cli.json")),
    ("code-puppy", include_str!("../assets/schemas/agents/code-puppy.json")),
    ("codex", include_str!("../assets/schemas/agents/codex.json")),
    ("computer-1", include_str!("../assets/schemas/agents/computer-1.json")),
    ("copilot-cli", include_str!("../assets/schemas/agents/copilot-cli.json")),
    ("cortex-code", include_str!("../assets/schemas/agents/cortex-code.json")),
    ("cursor-cli", include_str!("../assets/schemas/agents/cursor-cli.json")),
    ("deepseek-harness", include_str!("../assets/schemas/agents/deepseek-harness.json")),
    ("deerflow", include_str!("../assets/schemas/agents/deerflow.json")),
    ("devin", include_str!("../assets/schemas/agents/devin.json")),
    ("dspy-rlm", include_str!("../assets/schemas/agents/dspy-rlm.json")),
    ("eve", include_str!("../assets/schemas/agents/eve.json")),
    ("fx", include_str!("../assets/schemas/agents/fx.json")),
    ("fx-dev", include_str!("../assets/schemas/agents/fx-dev.json")),
    ("gemini-cli", include_str!("../assets/schemas/agents/gemini-cli.json")),
    ("goose", include_str!("../assets/schemas/agents/goose.json")),
    ("grok-build", include_str!("../assets/schemas/agents/grok-build.json")),
    ("hermes", include_str!("../assets/schemas/agents/hermes.json")),
    ("junie", include_str!("../assets/schemas/agents/junie.json")),
    ("kimi-cli", include_str!("../assets/schemas/agents/kimi-cli.json")),
    ("kimi-code", include_str!("../assets/schemas/agents/kimi-code.json")),
    ("langgraph", include_str!("../assets/schemas/agents/langgraph.json")),
    ("mcode", include_str!("../assets/schemas/agents/mcode.json")),
    ("mimo", include_str!("../assets/schemas/agents/mimo.json")),
    ("mini-swe-agent", include_str!("../assets/schemas/agents/mini-swe-agent.json")),
    ("muse-code", include_str!("../assets/schemas/agents/muse-code.json")),
    ("nemo-agent", include_str!("../assets/schemas/agents/nemo-agent.json")),
    ("openclaw", include_str!("../assets/schemas/agents/openclaw.json")),
    ("opencode", include_str!("../assets/schemas/agents/opencode.json")),
    ("openhands", include_str!("../assets/schemas/agents/openhands.json")),
    ("openhands-sdk", include_str!("../assets/schemas/agents/openhands-sdk.json")),
    ("pi", include_str!("../assets/schemas/agents/pi.json")),
    ("qwen-coder", include_str!("../assets/schemas/agents/qwen-coder.json")),
    ("rovodev-cli", include_str!("../assets/schemas/agents/rovodev-cli.json")),
    ("strands", include_str!("../assets/schemas/agents/strands.json")),
    ("swe-agent", include_str!("../assets/schemas/agents/swe-agent.json")),
    ("terminus-2", include_str!("../assets/schemas/agents/terminus-2.json")),
    ("trae-agent", include_str!("../assets/schemas/agents/trae-agent.json")),
    ("vibe", include_str!("../assets/schemas/agents/vibe.json")),
];

fn canonical(name: &str) -> &str {
    match name {
        "terminus" | "terminus-1" => "terminus-2",
        n if n.starts_with("acp:") => "acp",
        n => n,
    }
}

/// An agent's options schema (`terminus`/`terminus-1` share terminus-2's, every `acp:<id>` shares acp's).
pub fn agent(name: &str) -> Option<&'static str> {
    let name = canonical(name);
    AGENTS.iter().find(|(n, _)| *n == name).map(|(_, schema)| *schema)
}

/// What an agent's options schema accepts as `kwargs`.
pub struct Options {
    /// Property names and their `x-aliases`.
    pub keys: BTreeSet<String>,
    /// `x-native-config`: the agent passes `config` on to its own configuration.
    pub native_config: bool,
}

pub fn options(name: &str) -> Option<&'static Options> {
    static ALL: OnceLock<BTreeMap<&str, Options>> = OnceLock::new();
    let all = ALL.get_or_init(|| {
        AGENTS
            .iter()
            .map(|(name, text)| {
                let s: Value = serde_json::from_str(text).expect("vendored agent schema");
                let keys = s["properties"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .flat_map(|(k, p)| std::iter::once(k.clone()).chain(aliases(p).map(String::from)))
                    .collect();
                (*name, Options { keys, native_config: s["x-native-config"] == true })
            })
            .collect()
    });
    all.get(canonical(name))
}

fn aliases(property: &Value) -> impl Iterator<Item = &str> {
    property["x-aliases"].as_array().into_iter().flatten().filter_map(Value::as_str)
}

fn schema_type(p: &Value) -> String {
    if let Some(a) = p["anyOf"].as_array() {
        return a.iter().map(schema_type).collect::<Vec<_>>().join(" | ");
    }
    if let Some(e) = p["enum"].as_array() {
        return e.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" | ");
    }
    if let Some(c) = p.get("const") {
        return c.to_string();
    }
    if let Some(r) = p["$ref"].as_str() {
        return r.rsplit('/').next().unwrap_or(r).to_string();
    }
    p["type"].as_str().map(String::from).unwrap_or_else(|| "any".into())
}

/// The `agent schema` table: name, type, default, description.
pub fn options_table(agent: &str, schema: &Value) -> String {
    let required: Vec<&str> = schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let mut rows = vec![("Name".to_string(), "Type".to_string(), "Default".to_string(), "Description".to_string())];
    for (name, p) in schema["properties"].as_object().into_iter().flatten() {
        let default = match p.get("default") {
            Some(d) => d.to_string(),
            None if required.contains(&name.as_str()) => "required".into(),
            None => "—".into(),
        };
        let aliases: Vec<&str> = aliases(p).collect();
        let name = if aliases.is_empty() { name.clone() } else { format!("{name} ({})", aliases.join(", ")) };
        rows.push((name, schema_type(p), default, p["description"].as_str().unwrap_or("").to_string()));
    }
    let w = |f: fn(&(String, String, String, String)) -> &String| rows.iter().map(|r| f(r).chars().count()).max().unwrap_or(0);
    let (a, b, c) = (w(|r| &r.0), w(|r| &r.1).min(48), w(|r| &r.2).min(24));
    let mut out = format!("{agent} options\n");
    for r in &rows {
        out += &format!("{:<a$}  {:<b$}  {:<c$}  {}\n", r.0, r.1, r.2, r.3);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_agent_has_options() {
        for n in crate::agents::agent_names() {
            if matches!(n, "oracle" | "nop") {
                continue;
            }
            let o = options(n).unwrap_or_else(|| panic!("no schema for {n}"));
            assert!(o.keys.contains("config") || !o.native_config, "{n}");
        }
        let _: Value = serde_json::from_str(TASK).unwrap();
        let _: Value = serde_json::from_str(TRIAL).unwrap();
        let t = options_table("claude-code", &serde_json::from_str(agent("claude-code").unwrap()).unwrap());
        assert!(t.contains("max_turns") && t.contains("bypassPermissions"));
        let t = options_table("terminus-2", &serde_json::from_str(agent("terminus").unwrap()).unwrap());
        assert!(t.contains("max_turns (max_episodes, episodes)"), "{t}");
        assert!(options("acp:goose").is_some_and(|o| !o.native_config) && options("codex").is_some_and(|o| o.native_config));
        assert!(options("cline-cli").unwrap().keys.contains("cline-timeout"));
    }
}
