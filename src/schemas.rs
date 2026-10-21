//! Vendored JSON schemas (task.toml, trial config, an OCI dataset's config blob and source descriptor, each
//! agent's options) under assets/schemas.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

pub const TASK: &str = include_str!("../assets/schemas/task.json");
pub const TRIAL: &str = include_str!("../assets/schemas/trial.json");
pub const DATASET_CONFIG: &str = include_str!("../assets/schemas/dataset-config.v1.json");
pub const DATASET_SOURCE: &str = include_str!("../assets/schemas/dataset-source.v1.json");

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

const ANNOTATION_KEYWORDS: &[&str] = &["$schema", "$id", "$defs", "title", "description"];

/// Where `value` breaks `schema`, one message per violation. Only the keywords hbench's own schemas use are
/// understood; a schema with any other keyword is an error, never a silent pass.
pub fn violations(schema: &Value, value: &Value) -> Result<Vec<String>> {
    let mut out = vec![];
    check(schema, schema, value, "", &mut out)?;
    Ok(out)
}

fn check(root: &Value, schema: &Value, value: &Value, at: &str, out: &mut Vec<String>) -> Result<()> {
    let here = if at.is_empty() { "the document" } else { at };
    let rules = match schema {
        Value::Object(rules) => rules,
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => {
            out.push(format!("{here}: not allowed"));
            return Ok(());
        }
        _ => bail!("schema at {at:?} is not an object"),
    };
    let child = |key: &str| if at.is_empty() { key.to_string() } else { format!("{at}.{key}") };
    for (keyword, rule) in rules {
        match keyword.as_str() {
            k if ANNOTATION_KEYWORDS.contains(&k) => {}
            "$ref" => {
                let target = rule.as_str().and_then(|r| r.strip_prefix("#/$defs/")).and_then(|name| root["$defs"].get(name));
                check(root, target.with_context(|| format!("unresolvable $ref {rule} at {here}"))?, value, at, out)?;
            }
            "type" => {
                let kind = rule.as_str().with_context(|| format!("type at {here} is not a string"))?;
                if !has_type(value, kind) {
                    out.push(format!("{here}: expected {kind}, got {value}"));
                }
            }
            "properties" => {
                let (Value::Object(props), Value::Object(fields)) = (rule, value) else { continue };
                for (name, sub) in props {
                    if let Some(field) = fields.get(name) {
                        check(root, sub, field, &child(name), out)?;
                    }
                }
            }
            "required" => {
                let Value::Object(fields) = value else { continue };
                for name in rule.as_array().into_iter().flatten().filter_map(Value::as_str) {
                    if !fields.contains_key(name) {
                        out.push(format!("{here}: missing {name}"));
                    }
                }
            }
            "additionalProperties" => {
                let (Value::Bool(false), Value::Object(fields)) = (rule, value) else { continue };
                let known = rules.get("properties").and_then(Value::as_object);
                for name in fields.keys().filter(|k| !known.is_some_and(|p| p.contains_key(*k))) {
                    out.push(format!("{here}: unknown key {name}"));
                }
            }
            "items" => {
                let Value::Array(items) = value else { continue };
                for (i, item) in items.iter().enumerate() {
                    check(root, rule, item, &child(&i.to_string()), out)?;
                }
            }
            "pattern" => {
                let (Some(pattern), Value::String(s)) = (rule.as_str(), value) else { continue };
                if !matches_pattern(pattern, s)? {
                    out.push(format!("{here}: {s:?} does not match {pattern}"));
                }
            }
            "minLength" | "maxLength" => {
                let (Some(limit), Value::String(s)) = (rule.as_u64(), value) else { continue };
                let n = s.chars().count() as u64;
                if (keyword == "minLength" && n < limit) || (keyword == "maxLength" && n > limit) {
                    out.push(format!("{here}: {keyword} is {limit}, got {n} characters"));
                }
            }
            "minItems" => {
                let (Some(limit), Value::Array(items)) = (rule.as_u64(), value) else { continue };
                if (items.len() as u64) < limit {
                    out.push(format!("{here}: needs at least {limit} item(s)"));
                }
            }
            "minimum" => {
                let (Some(limit), Some(n)) = (rule.as_f64(), value.as_f64()) else { continue };
                if n < limit {
                    out.push(format!("{here}: {n} is below the minimum {limit}"));
                }
            }
            "const" => {
                if value != rule {
                    out.push(format!("{here}: expected {rule}, got {value}"));
                }
            }
            "enum" => {
                let allowed = rule.as_array().with_context(|| format!("enum at {here} is not an array"))?;
                if !allowed.contains(value) {
                    let allowed: Vec<String> = allowed.iter().map(Value::to_string).collect();
                    out.push(format!("{here}: {value} is not one of {}", allowed.join(", ")));
                }
            }
            "uniqueItems" => {
                let (Value::Bool(true), Value::Array(items)) = (rule, value) else { continue };
                for (i, item) in items.iter().enumerate().filter(|(i, item)| items[..*i].contains(item)) {
                    out.push(format!("{}: {item} is listed twice", child(&i.to_string())));
                }
            }
            "minProperties" => {
                let (Some(limit), Value::Object(fields)) = (rule.as_u64(), value) else { continue };
                if (fields.len() as u64) < limit {
                    out.push(format!("{here}: needs at least {limit} key(s)"));
                }
            }
            "if" => {
                let mut failed = vec![];
                check(root, rule, value, at, &mut failed)?;
                let branch = if failed.is_empty() { "then" } else { "else" };
                if let Some(sub) = rules.get(branch) {
                    check(root, sub, value, at, out)?;
                }
            }
            "then" | "else" => {}
            other => bail!("unsupported schema keyword {other} at {here}"),
        }
    }
    Ok(())
}

fn has_type(v: &Value, kind: &str) -> bool {
    match kind {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => false,
    }
}

fn matches_pattern(pattern: &str, s: &str) -> Result<bool> {
    static COMPILED: OnceLock<Mutex<HashMap<String, regex::Regex>>> = OnceLock::new();
    let mut compiled = COMPILED.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if !compiled.contains_key(pattern) {
        compiled.insert(pattern.to_string(), regex::Regex::new(pattern).with_context(|| format!("schema pattern {pattern}"))?);
    }
    Ok(compiled[pattern].is_match(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_config_schema_checks_what_it_declares() {
        let schema: Value = serde_json::from_str(DATASET_CONFIG).unwrap();
        let digest = format!("sha256:{}", "a".repeat(64));
        let good = serde_json::json!({
            "name": "hello", "version": "1.0", "license": "Apache-2.0",
            "tasks": [{"name": "hello-world", "layer_digest": digest, "package_digest": digest, "timeouts": {"agent_sec": 60.0}}],
        });
        assert_eq!(violations(&schema, &good).unwrap(), Vec::<String>::new());
        let bad = serde_json::json!({
            "name": "Hello", "version": "v1", "license": "", "extra": 1,
            "tasks": [{"name": "../x", "layer_digest": "sha256:abc", "timeouts": {"agent_sec": -1}}],
        });
        assert_eq!(
            violations(&schema, &bad).unwrap(),
            [
                "the document: unknown key extra",
                "name: \"Hello\" does not match ^[a-z0-9]+((\\.|_|__|-+)[a-z0-9]+)*$",
                "version: \"v1\" does not match ^(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)(\\.(0|[1-9][0-9]*))?(-[0-9A-Za-z-]+(\\.[0-9A-Za-z-]+)*)?$",
                "license: minLength is 1, got 0 characters",
                "tasks.0: missing package_digest",
                "tasks.0.name: \"../x\" does not match ^[A-Za-z0-9][A-Za-z0-9._-]*$",
                "tasks.0.layer_digest: \"sha256:abc\" does not match ^sha256:[0-9a-f]{64}$",
                "tasks.0.timeouts.agent_sec: -1 is below the minimum 0",
            ]
        );
        let unknown_keyword = serde_json::json!({"type": "object", "propertyNames": {"maxLength": 3}});
        assert!(violations(&unknown_keyword, &good).unwrap_err().to_string().contains("unsupported schema keyword propertyNames"));
    }

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
