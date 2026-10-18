//! TaskConfig (task.toml).

use super::*;
use anyhow::Result;
use serde_json::{Map, Value, json};

/// TOML -> JSON in document order (Python `tomllib` dicts keep insertion order; hbench's `toml`
/// build sorts keys, so reorder by each key's first span).
pub(super) fn parse_toml(text: &str) -> Result<Map<String, Value>> {
    let mut v: Map<String, Value> = toml::from_str(text)?;
    let de = toml::de::DeTable::parse(text)?;
    reorder_table(&mut v, de.get_ref());
    Ok(v)
}

fn reorder_table(m: &mut Map<String, Value>, de: &toml::de::DeTable<'_>) {
    let mut keys: Vec<_> = de.iter().collect();
    keys.sort_by_key(|(k, _)| k.span().start);
    let mut out = Map::new();
    for (k, dv) in keys {
        if let Some(mut x) = m.shift_remove(k.get_ref().as_ref()) {
            reorder_value(&mut x, dv.get_ref());
            out.insert(k.get_ref().to_string(), x);
        }
    }
    out.append(m);
    *m = out;
}

fn reorder_value(v: &mut Value, de: &toml::de::DeValue<'_>) {
    match (v, de) {
        (Value::Object(m), toml::de::DeValue::Table(t)) => reorder_table(m, t),
        (Value::Array(a), toml::de::DeValue::Array(da)) => a.iter_mut().zip(da.iter()).for_each(|(x, d)| reorder_value(x, d.get_ref())),
        _ => {}
    }
}

/// Builds one pydantic model's `model_dump(mode="json")` minus None, in field order.
struct Model<'a> {
    inp: &'a Map<String, Value>,
    out: Map<String, Value>,
}

impl<'a> Model<'a> {
    fn new(inp: &'a Map<String, Value>) -> Self {
        Model { inp, out: Map::new() }
    }
    fn get(&self, k: &str) -> Option<&'a Value> {
        self.inp.get(k).filter(|v| !v.is_null())
    }
    fn opt(mut self, k: &str) -> Self {
        if let Some(v) = self.get(k) {
            self.out.insert(k.into(), v.clone());
        }
        self
    }
    fn val(mut self, k: &str, d: Value) -> Self {
        self.out.insert(k.into(), self.get(k).cloned().unwrap_or(d));
        self
    }
    fn float(mut self, k: &str, d: Option<f64>) -> Self {
        if let Some(v) = self.get(k).map(as_float).or(d.map(f64_value)) {
            self.out.insert(k.into(), v);
        }
        self
    }
    fn sub(mut self, k: &str, f: fn(&Map<String, Value>) -> Map<String, Value>, always: bool) -> Self {
        match self.get(k).and_then(Value::as_object) {
            Some(m) => {
                self.out.insert(k.into(), Value::Object(f(m)));
            }
            None if always => {
                self.out.insert(k.into(), Value::Object(f(&Map::new())));
            }
            None => {}
        }
        self
    }
    fn list(mut self, k: &str, f: fn(&Value) -> Value, always: bool) -> Self {
        match self.get(k).and_then(Value::as_array) {
            Some(a) => {
                self.out.insert(k.into(), a.iter().map(f).collect());
            }
            None if always => {
                self.out.insert(k.into(), json!([]));
            }
            None => {}
        }
        self
    }
    fn done(self) -> Map<String, Value> {
        self.out
    }
}

fn f64_value(f: f64) -> Value {
    serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
}

fn as_float(v: &Value) -> Value {
    match v.as_f64() {
        Some(f) if v.is_number() => f64_value(f),
        _ => v.clone(),
    }
}

fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// `TaskConfig.model_validate(toml).model_dump(mode="json")` minus None values.
pub(super) fn normalized_task_config(raw: &Map<String, Value>) -> Map<String, Value> {
    let mut raw = raw.clone();
    if let Some(v) = raw.shift_remove("version") {
        raw.entry("schema_version").or_insert(v);
    }
    Model::new(&raw)
        .val("schema_version", "1.4".into())
        .sub("task", package_cfg, false)
        .val("metadata", json!({}))
        .sub("verifier", verifier_cfg, true)
        .sub("agent", agent_cfg, true)
        .sub("environment", env_cfg, true)
        .sub("solution", |m| Model::new(m).val("env", json!({})).done(), true)
        .opt("source")
        .opt("multi_step_reward_strategy")
        .list("steps", |v| Value::Object(step_cfg(&obj(v))), false)
        .list("artifacts", artifact_value, true)
        .done()
}

fn package_cfg(m: &Map<String, Value>) -> Map<String, Value> {
    Model::new(m)
        .opt("name")
        .opt("version")
        .val("description", "".into())
        .list("authors", |v| Value::Object(Model::new(&obj(v)).opt("name").opt("email").done()), true)
        .val("keywords", json!([]))
        .done()
}

fn agent_cfg(m: &Map<String, Value>) -> Map<String, Value> {
    Model::new(m).opt("network_mode").opt("allowed_hosts").float("timeout_sec", None).opt("user").done()
}

fn verifier_cfg(m: &Map<String, Value>) -> Map<String, Value> {
    Model::new(m)
        .opt("network_mode")
        .opt("allowed_hosts")
        .float("timeout_sec", Some(600.0))
        .val("env", json!({}))
        .opt("user")
        .opt("environment_mode")
        .sub("environment", env_cfg, false)
        .list(
            "collect",
            |v| {
                Value::Object(
                    Model::new(&obj(v)).opt("command").val("service", "main".into()).float("timeout_sec", Some(60.0)).opt("user").done(),
                )
            },
            true,
        )
        .done()
}

fn size_to_mb(s: &str) -> Option<i64> {
    let s = s.trim().to_uppercase();
    let num = |t: &str| t.parse::<f64>().ok();
    if let Some(n) = s.strip_suffix('G') {
        num(n).map(|f| (f * 1024.0) as i64)
    } else if let Some(n) = s.strip_suffix('M') {
        num(n).map(|f| f as i64)
    } else if let Some(n) = s.strip_suffix('K') {
        num(n).map(|f| (f / 1024.0) as i64)
    } else {
        None
    }
}

pub(super) fn env_cfg(m: &Map<String, Value>) -> Map<String, Value> {
    let mut m = m.clone();
    for (legacy, field) in [("memory", "memory_mb"), ("storage", "storage_mb")] {
        if let Some(v) = m.shift_remove(legacy)
            && let Some(mb) = v.as_str().and_then(size_to_mb)
        {
            m.entry(field).or_insert(mb.into());
        }
    }
    // Deprecated allow_internet maps onto network_mode unless a policy was given.
    if let Some(allow) = m.get("allow_internet").and_then(Value::as_bool)
        && !m.contains_key("network_mode")
        && m.get("allowed_hosts").is_none_or(Value::is_null)
    {
        m.insert("network_mode".into(), (if allow { "public" } else { "no-network" }).into());
    }
    if let Some(Value::String(os)) = m.get_mut("os") {
        *os = os.to_lowercase();
    }
    Model::new(&m)
        .val("network_mode", "public".into())
        .opt("allowed_hosts")
        .float("build_timeout_sec", Some(600.0))
        .opt("docker_image")
        .val("os", "linux".into())
        .opt("cpus")
        .opt("memory_mb")
        .opt("storage_mb")
        .opt("gpus")
        .opt("gpu_types")
        .sub("tpu", |t| Model::new(t).opt("type").opt("topology").done(), false)
        .list(
            "mcp_servers",
            |v| {
                let mut s = obj(v);
                if s.get("transport").and_then(Value::as_str) == Some("http") {
                    s.insert("transport".into(), "streamable-http".into());
                }
                Value::Object(
                    Model::new(&s).opt("name").val("transport", "sse".into()).opt("url").opt("command").val("args", json!([])).done(),
                )
            },
            true,
        )
        .val("env", json!({}))
        .opt("skills_dir")
        .sub("healthcheck", healthcheck_cfg, false)
        .opt("workdir")
        .done()
}

fn healthcheck_cfg(m: &Map<String, Value>) -> Map<String, Value> {
    Model::new(m)
        .opt("command")
        .float("interval_sec", Some(5.0))
        .float("timeout_sec", Some(30.0))
        .float("start_period_sec", Some(0.0))
        .float("start_interval_sec", Some(5.0))
        .val("retries", 3.into())
        .done()
}

fn step_cfg(m: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Model::new(m).opt("name").sub("agent", agent_cfg, true).sub("verifier", verifier_cfg, true).done();
    if let Some(r) = m.get("min_reward").filter(|v| !v.is_null()) {
        let r = match r {
            Value::Object(d) => Value::Object(d.iter().map(|(k, v)| (k.clone(), as_float(v))).collect()),
            v => as_float(v),
        };
        out.insert("min_reward".into(), r);
    }
    let rest = Model::new(m).sub("healthcheck", healthcheck_cfg, false).list("artifacts", artifact_value, true).done();
    out.extend(rest);
    out
}

/// `str | ArtifactConfig` dumped in json mode (None dropped).
pub(super) fn artifact_value(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(Model::new(m).opt("source").opt("destination").val("exclude", json!([])).opt("service").done()),
        v => v.clone(),
    }
}

/// `TaskConfig.model_dump_toml()`.
pub(super) fn task_config_toml(data: &Map<String, Value>) -> Result<String> {
    const LEADING: [&str; 3] = ["schema_version", "source", "multi_step_reward_strategy"];
    const SECTIONS: [&str; 7] = ["task", "steps", "metadata", "verifier", "agent", "environment", "solution"];
    let table_like = |v: &Value| v.is_object() || v.as_array().is_some_and(|a| a.iter().any(Value::is_object));
    let mut root = Map::new();
    for f in LEADING {
        if let Some(v) = data.get(f).filter(|v| !v.is_object()) {
            root.insert(f.into(), v.clone());
        }
    }
    for (k, v) in data {
        if !LEADING.contains(&k.as_str()) && k != "artifacts" && !SECTIONS.contains(&k.as_str()) && !table_like(v) {
            root.insert(k.clone(), v.clone());
        }
    }
    if let Some(v) = data.get("artifacts").filter(|v| !v.is_object()) {
        root.insert("artifacts".into(), v.clone());
    }
    let mut parts = vec![];
    let mut emitted: Vec<String> = vec![];
    if !root.is_empty() {
        parts.push(toml_dumps(&root)?);
        emitted.extend(root.keys().cloned());
    }
    for s in ["task", "steps", "metadata", "verifier", "agent", "environment", "solution"] {
        if let Some(v) = data.get(s) {
            let mut one = Map::new();
            one.insert(s.into(), v.clone());
            parts.push(toml_dumps(&one)?);
            emitted.push(s.into());
        }
    }
    for (k, v) in data {
        if !emitted.contains(k) {
            let mut one = Map::new();
            one.insert(k.clone(), v.clone());
            parts.push(toml_dumps(&one)?);
            emitted.push(k.clone());
        }
    }
    let parts: Vec<&str> = parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
    Ok(parts.join("\n\n") + "\n")
}
