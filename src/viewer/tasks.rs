//! Tasks mode.

use super::*;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `TaskConfig.model_validate_toml(...).model_dump(mode="json")`, driven by the exported TaskConfig schema:
/// properties in field order, defaults (incl. default factories) filled, legacy fields migrated.
pub(super) fn task_config_dump(toml_text: &str) -> Option<Value> {
    let mut data: Value = serde_json::to_value(toml::from_str::<toml::Table>(toml_text).ok()?).ok()?;
    let o = data.as_object_mut()?;
    if let Some(v) = o.remove("version") {
        o.entry("schema_version").or_insert(v);
    }
    let schema: Value = serde_json::from_str(crate::schemas::TASK).ok()?;
    let mut out = dump_model(&data, &schema, &schema)?;
    // allow_internet (deprecated) -> network_mode, unless set explicitly.
    let legacy = |env_in: Option<&Value>, env_out: &mut Value| {
        let Some(allow) = env_in.and_then(|e| e.get("allow_internet")).and_then(Value::as_bool) else { return };
        if env_in.is_some_and(|e| e.get("network_mode").is_some()) || !env_out["allowed_hosts"].is_null() {
            return;
        }
        env_out["network_mode"] = json!(if allow { "public" } else { "no-network" });
    };
    legacy(data.get("environment"), &mut out["environment"]);
    if !out["verifier"]["environment"].is_null() {
        legacy(data.get("verifier").and_then(|v| v.get("environment")), &mut out["verifier"]["environment"]);
    }
    if let Some(steps) = out["steps"].as_array_mut() {
        for (i, st) in steps.iter_mut().enumerate() {
            if !st["verifier"]["environment"].is_null() {
                legacy(data["steps"][i].get("verifier").and_then(|v| v.get("environment")), &mut st["verifier"]["environment"]);
            }
        }
    }
    Some(out)
}

fn deref<'a>(schema: &'a Value, root: &'a Value) -> &'a Value {
    match schema.get("$ref").and_then(s).and_then(|r| r.strip_prefix("#/$defs/")) {
        Some(name) => &root["$defs"][name],
        None => schema,
    }
}

fn dump_model(data: &Value, model: &Value, root: &Value) -> Option<Value> {
    let mut data = data.clone();
    if let Some(o) = data.as_object_mut() {
        // EnvironmentConfig._migrate_legacy_resource_fields
        for (old, new) in [("memory", "memory_mb"), ("storage", "storage_mb")] {
            if model["properties"].get(new).is_some()
                && let Some(Value::String(size)) = o.remove(old)
            {
                o.entry(new).or_insert(json!(parse_size_mb(&size)?));
            }
        }
        if let Some(Value::String(os)) = o.get_mut("os") {
            *os = os.to_lowercase();
        }
    }
    let mut out = Map::new();
    for (k, prop) in model["properties"].as_object()? {
        if k == "allow_internet" {
            continue; // Field(exclude=True)
        }
        let v = match data.get(k) {
            Some(v) => dump_value(v, prop, root)?,
            None => prop.get("default").cloned().unwrap_or_else(|| default_for(prop, root)),
        };
        out.insert(k.clone(), v);
    }
    Some(Value::Object(out))
}

/// Default-factory value for a schema property without an explicit `default`.
fn default_for(prop: &Value, root: &Value) -> Value {
    let p = deref(prop, root);
    if p.get("properties").is_some() {
        return dump_model(&json!({}), p, root).unwrap_or(Value::Null);
    }
    match p.get("type").and_then(s) {
        Some("array") => json!([]),
        Some("object") => json!({}),
        _ => Value::Null,
    }
}

fn dump_value(v: &Value, prop: &Value, root: &Value) -> Option<Value> {
    if v.is_null() {
        return Some(Value::Null);
    }
    if let Some(variants) = prop.get("anyOf").and_then(Value::as_array) {
        let fits = |var: &Value| {
            let d = deref(var, root);
            match (v, d.get("type").and_then(s)) {
                (Value::Object(_), _) if d.get("properties").is_some() => true,
                (Value::Object(_), Some("object"))
                | (Value::Array(_), Some("array"))
                | (Value::String(_), Some("string"))
                | (Value::Bool(_), Some("boolean"))
                | (Value::Number(_), Some("number")) => true,
                (Value::Number(n), Some("integer")) => n.is_i64() || n.is_u64(),
                (Value::String(_), None) => d.get("enum").is_some(),
                _ => false,
            }
        };
        return match variants.iter().find(|var| fits(var)) {
            Some(var) => dump_value(v, var, root),
            None => Some(v.clone()),
        };
    }
    let p = deref(prop, root);
    if p.get("properties").is_some() {
        return dump_model(v, p, root);
    }
    Some(match (v, p.get("type").and_then(s)) {
        (Value::Array(items), Some("array")) => match p.get("items") {
            Some(it) => Value::Array(items.iter().map(|x| dump_value(x, it, root)).collect::<Option<_>>()?),
            None => v.clone(),
        },
        (Value::Object(m), Some("object")) => match p.get("additionalProperties").filter(|a| a.is_object()) {
            Some(ap) => Value::Object(m.iter().map(|(k, x)| Some((k.clone(), dump_value(x, ap, root)?))).collect::<Option<_>>()?),
            None => v.clone(),
        },
        (Value::Number(n), Some("number")) if n.is_i64() || n.is_u64() => json!(n.as_f64()?),
        _ => v.clone(),
    })
}

/// `EnvironmentConfig._parse_size_to_mb`.
fn parse_size_mb(size: &str) -> Option<i64> {
    let s = size.trim().to_uppercase();
    let (n, unit) = s.split_at(s.len().checked_sub(1)?);
    let f: f64 = n.parse().ok()?;
    Some(match unit {
        "G" => (f * 1024.0) as i64,
        "M" => f as i64,
        "K" => (f / 1024.0) as i64,
        _ => return None,
    })
}

struct TaskInfo {
    config: Option<Value>,
    has_instruction: bool,
    has_environment: bool,
    has_tests: bool,
    has_solution: bool,
    has_docker_compose: bool,
}

fn task_info(dir: &Path) -> TaskInfo {
    let toml_text = std::fs::read_to_string(dir.join("task.toml")).ok();
    let has_steps =
        toml_text.as_deref().and_then(|t| toml::from_str::<toml::Table>(t).ok()).and_then(|t| t.get("steps").cloned()).is_some_and(|s| {
            match s {
                toml::Value::Array(a) => !a.is_empty(),
                toml::Value::Table(t) => !t.is_empty(),
                toml::Value::String(x) => !x.is_empty(),
                _ => true,
            }
        });
    TaskInfo {
        config: toml_text.as_deref().and_then(task_config_dump),
        has_instruction: dir.join("instruction.md").exists() || has_steps,
        has_environment: dir.join("environment").exists(),
        has_tests: dir.join("tests").exists(),
        has_solution: dir.join("solution").exists(),
        has_docker_compose: dir.join("environment/docker-compose.yaml").exists(),
    }
}

/// `TaskDefinitionScanner.get_instruction`.
fn task_instruction(dir: &Path, config: Option<&Value>) -> Value {
    let p = dir.join("instruction.md");
    if p.exists() {
        return read_text(&p).ok().flatten().map_or(Value::Null, Value::String);
    }
    let steps = config.and_then(|c| c["steps"].as_array()).filter(|s| !s.is_empty());
    let Some(steps) = steps else { return Value::Null };
    let sections: Vec<String> = steps
        .iter()
        .enumerate()
        .filter_map(|(i, st)| {
            let name = st["name"].as_str()?;
            let text = read_text(&dir.join("steps").join(name).join("instruction.md")).ok()??;
            Some(format!("## Step {}: {name}\n\n{}", i + 1, text.trim_end()))
        })
        .collect();
    if sections.is_empty() { Value::Null } else { json!(sections.join("\n\n---\n\n")) }
}

impl App {
    fn list_tasks(&self) -> Vec<String> {
        sorted_entries(&self.folder).into_iter().filter(|d| d.is_dir() && d.join("task.toml").exists()).map(|p| name_of(&p)).collect()
    }

    fn task_definition_summaries(&self) -> Vec<Value> {
        self.list_tasks()
            .into_iter()
            .map(|name| {
                let i = task_info(&self.folder.join(&name));
                let flags = |mut v: Value| {
                    for (k, b) in [
                        ("has_instruction", i.has_instruction),
                        ("has_environment", i.has_environment),
                        ("has_tests", i.has_tests),
                        ("has_solution", i.has_solution),
                        ("has_docker_compose", i.has_docker_compose),
                    ] {
                        v[k] = json!(b);
                    }
                    v
                };
                match &i.config {
                    Some(c) => {
                        let env = &c["environment"];
                        let mut v = json!({"name": name, "version": c["schema_version"], "source": c["source"], "metadata": c["metadata"]});
                        v = flags(v);
                        for (k, val) in [
                            ("agent_timeout_sec", &c["agent"]["timeout_sec"]),
                            ("verifier_timeout_sec", &c["verifier"]["timeout_sec"]),
                            ("os", &env["os"]),
                            ("cpus", &env["cpus"]),
                            ("memory_mb", &env["memory_mb"]),
                            ("storage_mb", &env["storage_mb"]),
                            ("gpus", &env["gpus"]),
                        ] {
                            v[k] = val.clone();
                        }
                        v
                    }
                    None => {
                        let mut v = json!({"name": name, "version": "1.0", "source": null, "metadata": {}});
                        v = flags(v);
                        for k in ["agent_timeout_sec", "verifier_timeout_sec", "os", "cpus", "memory_mb", "storage_mb", "gpus"] {
                            v[k] = Value::Null;
                        }
                        v
                    }
                }
            })
            .collect()
    }

    fn validate_task(&self, name: &str) -> std::result::Result<PathBuf, Err> {
        let d = resolve(&self.folder, name);
        if name.contains('/') || !strictly_under(&d, &self.folder) {
            return Err(err(400, "Invalid task name"));
        }
        Ok(d)
    }

    pub(super) fn tasks_get(&self, segs: &[&str], q: &Query) -> Res {
        match segs {
            ["task-definitions", "filters"] => {
                let (mut diff, mut cat, mut tags) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
                for sm in self.task_definition_summaries() {
                    let m = &sm["metadata"];
                    if let Some(d) = m.get("difficulty").and_then(s) {
                        *diff.entry(d.to_string()).or_insert(0) += 1;
                    }
                    if let Some(c) = m.get("category").and_then(s) {
                        *cat.entry(c.to_string()).or_insert(0) += 1;
                    }
                    for t in m.get("tags").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                        *tags.entry(t.to_string()).or_insert(0) += 1;
                    }
                }
                ok_json(json!({"difficulties": filter_options(diff), "categories": filter_options(cat), "tags": filter_options(tags)}))
            }
            ["task-definitions"] => {
                let page = q.int("page", 1, Some(1), None)?;
                let page_size = q.int("page_size", 100, Some(1), Some(100))?;
                let mut items = self.task_definition_summaries();
                if let Some(qs) = q.get("q").filter(|x| !x.is_empty()) {
                    let n = qs.to_lowercase();
                    items.retain(|s| {
                        s["name"].as_str().is_some_and(|x| x.to_lowercase().contains(&n))
                            || s["source"].as_str().is_some_and(|x| x.to_lowercase().contains(&n))
                            || s["metadata"].as_object().into_iter().flatten().any(|(_, v)| py_str(v).to_lowercase().contains(&n))
                    });
                }
                for (param, field) in [("difficulty", "difficulty"), ("category", "category")] {
                    let want = q.all(param);
                    if !want.is_empty() {
                        items.retain(|s| s["metadata"].get(field).is_some_and(|v| want.iter().any(|w| v.as_str() == Some(w))));
                    }
                }
                let tags = q.all("tag");
                if !tags.is_empty() {
                    items.retain(|s| {
                        tags.iter().any(|t| {
                            s["metadata"].get("tags").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x.as_str() == Some(t)))
                        })
                    });
                }
                ok_json(paginate(items, page, page_size))
            }
            ["task-definitions", name] => {
                let dir = self.validate_task(name)?;
                if !dir.exists() || !dir.join("task.toml").exists() {
                    return Err(err(404, format!("Task '{name}' not found")));
                }
                let i = task_info(&dir);
                ok_json(json!({
                    "name": name, "task_dir": dir.to_string_lossy(), "config": i.config.clone().unwrap_or(json!({})),
                    "instruction": task_instruction(&dir, i.config.as_ref()), "has_instruction": i.has_instruction, "has_environment": i.has_environment,
                    "has_tests": i.has_tests, "has_solution": i.has_solution, "has_docker_compose": i.has_docker_compose,
                }))
            }
            ["task-definitions", name, "files"] => {
                let dir = self.validate_task(name)?;
                if !dir.exists() {
                    return Err(err(404, format!("Task '{name}' not found")));
                }
                let mut out = vec![];
                scan_dir(&dir, "", true, false, &mut out);
                ok_json(Value::Array(out))
            }
            ["task-definitions", name, "files", rest @ ..] => {
                let dir = self.validate_task(name)?;
                if !dir.exists() {
                    return Err(err(404, format!("Task '{name}' not found")));
                }
                let full = resolve(&dir, &rest.join("/"));
                if !full.starts_with(&dir) {
                    return Err(err(403, "Access denied"));
                }
                serve_file(&full)
            }
            _ => Err(err(404, "Not Found")),
        }
    }
}

/// Python `str()` of a JSON value (metadata search).
fn py_str(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Null => "None".into(),
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        x => x.to_string(),
    }
}
