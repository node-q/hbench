//! `--print-config` output.

use super::*;
use crate::config::{AgentConfig, EnvConfig, RetryConfig, VerifierRunConfig};
use serde::Serialize;
use serde_json::{Map, Value, json};

/// `ExecConfig.model_dump(mode="json", exclude_defaults=True)`.
pub(super) fn print_value(c: &ExecConfig) -> Value {
    let mut root = Map::new();
    if c.schema_version != "1.0" {
        root.insert("schema_version".into(), c.schema_version.clone().into());
    }
    let mut map = Map::new();
    map.insert("compile".into(), serde_json::to_value(&c.map.compile).unwrap_or_default());
    if let Some(j) = job_print(&c.map.job) {
        map.insert("job".into(), j);
    }
    root.insert("map".into(), Value::Object(map));
    if let Some(r) = &c.reduce {
        let mut red = Map::new();
        red.insert("task".into(), serde_json::to_value(&r.task).unwrap_or_default());
        if let Some(j) = job_print(&r.job) {
            red.insert("job".into(), j);
        }
        root.insert("reduce".into(), Value::Object(red));
    }
    Value::Object(root)
}

/// Drop keys equal to their default (or absent-from-default and empty).
fn prune(v: Value, default: &Value) -> Map<String, Value> {
    let Value::Object(m) = v else { return Map::new() };
    m.into_iter()
        .filter(|(k, val)| match default.get(k) {
            Some(d) => val != d,
            None => !(val.is_null() || val.as_array().is_some_and(Vec::is_empty) || val.as_object().is_some_and(Map::is_empty)),
        })
        .collect()
}

/// Round-trip through hbench's type (upstream field order, float coercion, env templating), then
/// restore the caller's key order for free-form maps.
fn canonical<T: serde::de::DeserializeOwned + Serialize + Default>(raw: &Map<String, Value>) -> Value {
    let mut v = serde_json::from_value::<T>(Value::Object(raw.clone())).ok().and_then(|t| serde_json::to_value(t).ok()).unwrap_or_default();
    for key in ["env", "kwargs"] {
        if let (Some(Value::Object(orig)), Some(Value::Object(out))) = (raw.get(key), v.get_mut(key)) {
            let mut ordered = Map::new();
            for k in orig.keys() {
                if let Some(x) = out.shift_remove(k) {
                    ordered.insert(k.clone(), x);
                }
            }
            ordered.append(out);
            *out = ordered;
        }
    }
    v
}

fn default_value<T: Serialize + Default>() -> Value {
    serde_json::to_value(T::default()).unwrap_or_default()
}

fn agent_print(raw: &Map<String, Value>) -> Value {
    let mut m = prune(canonical::<AgentConfig>(raw), &default_value::<AgentConfig>());
    // upstream's field default for `name` is None; the validator fills "oracle", so it always shows.
    let name = if raw.get("import_path").is_some_and(|v| !v.is_null()) && raw.get("name").is_none_or(Value::is_null) {
        None
    } else {
        Some(raw.get("name").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| "oracle".into()))
    };
    let mut out = Map::new();
    if let Some(n) = name {
        out.insert("name".into(), n);
    }
    m.shift_remove("name");
    out.append(&mut m);
    Value::Object(out)
}

/// upstream's field default for `type` is None; the validator fills "docker" unless an
/// import_path is set, so `type` shows whenever the environment differs from the default.
fn env_print(raw: &Map<String, Value>) -> Map<String, Value> {
    let mut m = prune(canonical::<EnvConfig>(raw), &default_value::<EnvConfig>());
    m.shift_remove("type");
    if m.is_empty() && raw.get("type").is_none_or(|t| t.is_null() || t == "docker") {
        return m;
    }
    let mut out = Map::new();
    match raw.get("type").filter(|t| !t.is_null()) {
        Some(t) => {
            out.insert("type".into(), t.clone());
        }
        None if raw.get("import_path").is_none_or(Value::is_null) => {
            out.insert("type".into(), "docker".into());
        }
        None => {}
    }
    out.append(&mut m);
    out
}

fn job_print(j: &ExecJobConfig) -> Option<Value> {
    let mut m = Map::new();
    if let Some(n) = &j.job_name {
        m.insert("job_name".into(), n.clone().into());
    }
    if let Some(d) = &j.jobs_dir {
        m.insert("jobs_dir".into(), d.0.clone().into());
    }
    if j.n_attempts != 1 {
        m.insert("n_attempts".into(), j.n_attempts.into());
    }
    if j.n_concurrent_trials != 4 {
        m.insert("n_concurrent_trials".into(), j.n_concurrent_trials.into());
    }
    if j.quiet {
        m.insert("quiet".into(), true.into());
    }
    let sections = [
        ("retry", prune(canonical::<RetryConfig>(&j.retry), &default_value::<RetryConfig>())),
        ("environment", env_print(&j.environment)),
        ("verifier", prune(canonical::<VerifierRunConfig>(&j.verifier), &default_value::<VerifierRunConfig>())),
    ];
    for (k, v) in sections {
        if !v.is_empty() {
            m.insert(k.into(), Value::Object(v));
        }
    }
    if !j.metrics.is_empty() {
        let d = json!({"type": "mean", "kwargs": {}});
        m.insert("metrics".into(), j.metrics.iter().map(|x| Value::Object(prune(x.clone(), &d))).collect());
    }
    let agents: Vec<Value> = j.agents.iter().map(agent_print).collect();
    if agents != [json!({"name": "oracle"})] {
        m.insert("agents".into(), agents.into());
    }
    (!m.is_empty()).then_some(Value::Object(m))
}
