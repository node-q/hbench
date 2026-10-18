//! kwargs parsing for the installed agents (pydantic lax-mode style coercion of option
//! fields).

use crate::agents::AgentArgs;
use anyhow::{Result, anyhow, bail};
use serde_json::Value;

/// kwargs value (explicit null = opt-out), else the `fallback` env var.
fn raw(a: &AgentArgs, k: &str, fallback: Option<&str>) -> Option<Value> {
    match a.kwargs.get(k) {
        Some(Value::Null) => None,
        Some(v) => Some(v.clone()),
        None => fallback.and_then(|e| a.get_env(e)).map(Value::String),
    }
}

pub fn string(a: &AgentArgs, k: &str, fallback: Option<&str>) -> Result<Option<String>> {
    Ok(match raw(a, k, fallback) {
        None => None,
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(v) => bail!("Invalid value for '{k}': expected a string, got {v}"),
    })
}

pub fn int(a: &AgentArgs, k: &str, fallback: Option<&str>) -> Result<Option<i64>> {
    let err = |v: &Value| {
        anyhow!("Invalid value for '{k}': cannot parse '{}' as int", v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))
    };
    Ok(match raw(a, k, fallback) {
        None => None,
        Some(v @ Value::Number(_)) => {
            Some(v.as_i64().or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)).ok_or_else(|| err(&v))?)
        }
        Some(v @ Value::String(_)) => Some(v.as_str().unwrap().trim().parse().map_err(|_| err(&v))?),
        Some(v) => return Err(err(&v)),
    })
}

pub fn float(a: &AgentArgs, k: &str, fallback: Option<&str>) -> Result<Option<f64>> {
    Ok(match raw(a, k, fallback) {
        None => None,
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => Some(s.trim().parse().map_err(|_| anyhow!("Invalid value for '{k}': cannot parse '{s}' as float"))?),
        Some(v) => bail!("Invalid value for '{k}': expected a number, got {v}"),
    })
}

pub fn boolean(a: &AgentArgs, k: &str, fallback: Option<&str>) -> Result<Option<bool>> {
    let Some(v) = raw(a, k, fallback) else { return Ok(None) };
    lax_bool(&v).map(Some).ok_or_else(|| anyhow!("Invalid value for '{k}': {v} is not a valid boolean"))
}

/// Pydantic lax `bool`: true/false, 0/1, and the usual yes/no strings.
pub fn lax_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => match n.as_f64() {
            Some(0.0) => Some(false),
            Some(1.0) => Some(true),
            _ => None,
        },
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "1" | "on" | "t" | "true" | "y" | "yes" => Some(true),
            "0" | "off" | "f" | "false" | "n" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `Literal[...]` field: exact string match.
pub fn choice(a: &AgentArgs, k: &str, fallback: Option<&str>, choices: &[&str]) -> Result<Option<String>> {
    match raw(a, k, fallback) {
        None => Ok(None),
        Some(Value::String(s)) if choices.contains(&s.as_str()) => Ok(Some(s)),
        Some(v) => {
            let mut c = choices.to_vec();
            c.sort();
            bail!(
                "Invalid value for '{k}': '{}'. Valid values: {}",
                v.as_str().map(String::from).unwrap_or_else(|| v.to_string()),
                c.join(", ")
            )
        }
    }
}

/// Options forbid unknown kwargs (reported one at a time); `config` needs `native_config`.
pub fn check_known(a: &AgentArgs, agent: &str, known: &[&str], native_config: bool) -> Result<()> {
    for k in a.kwargs.keys() {
        if !known.contains(&k.as_str()) && !["version", "prompt_template_path", "config"].contains(&k.as_str()) {
            bail!(
                "Invalid kwargs for agent '{agent}':\n  Unknown option '{k}'.\nSee `hbench agents schema {agent}` for available options."
            );
        }
    }
    if !native_config && a.kw("config").is_some() {
        bail!("Agent '{agent}' does not support config. You can implement it.");
    }
    Ok(())
}

/// Pydantic `extra="forbid"` + unsupported native config: every unknown
/// kwarg is listed, then `config` is rejected.
pub fn check_options(args: &AgentArgs, name: &str, fields: &[&str]) -> Result<()> {
    let unknown: Vec<String> = args
        .kwargs
        .keys()
        .filter(|k| !["version", "prompt_template_path", "config"].contains(&k.as_str()) && !fields.contains(&k.as_str()))
        .map(|k| format!("  Unknown option '{k}'."))
        .collect();
    if !unknown.is_empty() {
        bail!("Invalid kwargs for agent '{name}':\n{}\nSee `hbench agents schema {name}` for available options.", unknown.join("\n"));
    }
    if args.kwargs.get("config").is_some_and(|v| !v.is_null()) {
        bail!("Agent '{name}' does not support config. You can implement it.");
    }
    Ok(())
}

/// Optional int kwarg (pydantic lax int: ints or numeric strings, not bools or floats).
pub fn kw_int(args: &AgentArgs, k: &str) -> Result<Option<i64>> {
    match args.kwargs.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) if n.is_i64() => Ok(n.as_i64()),
        Some(Value::String(s)) if s.trim().parse::<i64>().is_ok() => Ok(s.trim().parse().ok()),
        Some(_) => bail!("Invalid value for '{k}': expected int"),
    }
}

/// Python `str(float)`.
pub fn py_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 { format!("{f:.1}") } else { f.to_string() }
}
