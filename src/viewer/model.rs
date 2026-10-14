//! Model loading (pydantic validate + dump).

use super::*;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// `JobConfig.model_validate(...)` dumped in full (`model_dump(mode="json")`).
pub(super) fn job_config_value(raw: Value) -> Option<Value> {
    let mut raw = raw;
    // scanner._normalize_viewer_job_config: numeric dataset refs -> strings.
    if let Some(ds) = raw.get_mut("datasets").and_then(Value::as_array_mut) {
        for d in ds.iter_mut().filter_map(Value::as_object_mut) {
            for k in ["version", "ref"] {
                if let Some(Value::Number(n)) = d.get(k) {
                    let s = n.to_string();
                    d.insert(k.into(), Value::String(s));
                }
            }
        }
    }
    crate::config::migrate_legacy(&mut raw);
    let cfg: crate::config::JobConfig = serde_json::from_value(raw).ok()?;
    serde_json::to_value(cfg).ok()
}

pub(super) fn job_config(job_dir: &Path) -> Option<Value> {
    job_config_value(read_json(&job_dir.join("config.json"))?)
}

/// `JobResult` as pydantic loads it: required fields present, `updated_at` derived, `trial_results` defaulted.
pub(super) fn job_result(job_dir: &Path) -> Option<Value> {
    let mut r = read_json_object(&job_dir.join("result.json"))?;
    let o = r.as_object_mut()?;
    let status = o.remove("status");
    if o.get("updated_at").is_none_or(Value::is_null) {
        let from_status = status.as_ref().and_then(|s| s.get("updated_at")).filter(|v| truthy(v)).cloned();
        let v = from_status
            .or_else(|| o.get("finished_at").filter(|v| truthy(v)).cloned())
            .or_else(|| o.get("started_at").cloned())
            .unwrap_or(Value::Null);
        o.insert("updated_at".into(), v);
    }
    if !(o.contains_key("id") && o.contains_key("started_at") && o.contains_key("n_total_trials")) {
        return None;
    }
    let stats = o.get("stats").cloned().unwrap_or(json!({}));
    o.insert("stats".into(), job_stats(&stats));
    let mut out = Map::new();
    for k in ["id", "started_at", "updated_at", "finished_at", "n_total_trials", "stats"] {
        out.insert(k.into(), o.get(k).cloned().unwrap_or(Value::Null));
    }
    out.insert("trial_results".into(), o.get("trial_results").cloned().unwrap_or(json!([])));
    let mut out = Value::Object(out);
    norm_dts(&mut out);
    Some(out)
}

/// `JobStats` with defaults filled and legacy `n_trials`/`n_errors` migrated.
pub(super) fn job_stats(s: &Value) -> Value {
    let g = |k: &str| s.get(k).cloned();
    let int = |k: &str, legacy: Option<&str>| g(k).or_else(|| legacy.and_then(&g)).unwrap_or(json!(0));
    let evals: Map<String, Value> = s
        .get("evals")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, e)| {
                    let d = |f: &str, def: Value| e.get(f).cloned().unwrap_or(def);
                    (
                        k.clone(),
                        json!({"n_trials": d("n_trials", json!(0)), "n_errors": d("n_errors", json!(0)), "metrics": d("metrics", json!([])), "pass_at_k": d("pass_at_k", json!({})),
                               "reward_stats": d("reward_stats", json!({})), "exception_stats": d("exception_stats", json!({}))}),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "n_completed_trials": int("n_completed_trials", Some("n_trials")),
        "n_errored_trials": int("n_errored_trials", Some("n_errors")),
        "n_running_trials": int("n_running_trials", None),
        "n_pending_trials": int("n_pending_trials", None),
        "n_cancelled_trials": int("n_cancelled_trials", None),
        "n_retries": int("n_retries", None),
        "evals": evals,
        "n_input_tokens": g("n_input_tokens").unwrap_or(Value::Null),
        "n_cache_tokens": g("n_cache_tokens").unwrap_or(Value::Null),
        "n_output_tokens": g("n_output_tokens").unwrap_or(Value::Null),
        "cost_usd": g("cost_usd").unwrap_or(Value::Null),
    })
}

/// A trial result that validates as upstream's `TrialResult` (required fields present); the file is its dump.
pub(super) fn trial_result(trial_dir: &Path) -> Option<Value> {
    let mut r = read_json_object(&trial_dir.join("result.json"))?;
    norm_dts(&mut r);
    ["id", "task_name", "trial_name", "trial_uri", "task_id", "task_checksum", "config", "agent_info"]
        .iter()
        .all(|k| r.get(k).is_some())
        .then_some(r)
}

/// `TrialConfig.model_validate_json(config.json)`: pruned defaults restored.
pub(super) fn trial_config(trial_dir: &Path) -> Option<crate::config::TrialConfig> {
    let mut v = read_json_object(&trial_dir.join("config.json"))?;
    let o = v.as_object_mut()?;
    o.entry("trial_name").or_insert(json!(""));
    o.entry("trials_dir").or_insert(json!("trials"));
    serde_json::from_value(v).ok()
}

pub(super) fn agent_name_from_config(agent: &Value) -> String {
    match (agent.get("name").and_then(s), agent.get("import_path").and_then(s)) {
        (Some(n), _) if !n.is_empty() => n.into(),
        (_, Some(ip)) if !ip.is_empty() => ip.into(),
        (None, None) => "oracle".into(),
        _ => "unknown".into(),
    }
}

pub(super) fn agent_name_from_result(r: &Value) -> String {
    let agent = &r["config"]["agent"];
    if agent.get("import_path").is_some_and(truthy) {
        agent_name_from_config(agent)
    } else {
        r["agent_info"]["name"].as_str().unwrap_or_default().into()
    }
}

pub(super) fn reasoning_effort(config: &Value) -> Option<String> {
    config["agent"]["kwargs"].get("reasoning_effort").and_then(s).filter(|x| !x.is_empty()).map(String::from)
}

/// `model_info_from_model_name`: (provider, name).
pub(super) fn split_model(model: Option<&str>) -> Option<(Option<String>, String)> {
    let m = model.filter(|m| !m.is_empty())?;
    Some(match m.split_once('/') {
        Some((p, n)) => (Some(p.into()), n.into()),
        None => (None, m.into()),
    })
}

pub(super) fn task_name_from_config(task: &Value) -> String {
    if let Some(n) = task.get("name").and_then(s).filter(|n| !n.is_empty()) {
        return n.into();
    }
    // LocalTaskId/GitTaskId.get_name(): the path's (resolved, for local tasks) final component.
    let p = PathBuf::from(task.get("path").and_then(s).unwrap_or_default());
    let p = if task.get("git_url").is_some_and(truthy) {
        p
    } else {
        std::path::absolute(&p).map(|a| a.canonicalize().unwrap_or(a)).unwrap_or(p)
    };
    name_of(&p)
}

/// `TrialResult.compute_token_cost_totals()`.
pub(super) fn token_totals(r: &Value) -> (Option<i64>, Option<i64>, Option<i64>, Option<f64>) {
    let ctxs: Vec<&Value> = if r.get("agent_result").is_some_and(|a| !a.is_null()) {
        vec![&r["agent_result"]]
    } else {
        r.get("step_results")
            .and_then(Value::as_array)
            .map(|s| s.iter().map(|x| &x["agent_result"]).filter(|a| !a.is_null()).collect())
            .unwrap_or_default()
    };
    let sum =
        |k: &str| ctxs.iter().filter_map(|c| c.get(k).and_then(Value::as_i64)).fold(None, |acc: Option<i64>, x| Some(acc.unwrap_or(0) + x));
    let cost = ctxs
        .iter()
        .filter_map(|c| c.get("cost_usd").and_then(Value::as_f64))
        .fold(None, |acc: Option<f64>, x| Some(acc.unwrap_or(0.0) + x));
    (sum("n_input_tokens"), sum("n_cache_tokens"), sum("n_output_tokens"), cost)
}

/// Numeric (non-bool) rewards as floats.
pub(super) fn numeric_rewards(rewards: &Value) -> Vec<(String, f64)> {
    rewards
        .as_object()
        .map(|m| m.iter().filter_map(|(k, v)| if v.is_number() { v.as_f64().map(|f| (k.clone(), f)) } else { None }).collect())
        .unwrap_or_default()
}

/// `evals_from_rewards`: one `{key: {metrics: [{key: value}]}}` per numeric reward, `reward` first.
pub(super) fn evals_from_rewards(rewards: Vec<(String, f64)>) -> Value {
    let mut numeric = rewards;
    numeric.sort_by(|a, b| (a.0 != "reward", &a.0).cmp(&(b.0 != "reward", &b.0)));
    Value::Object(numeric.into_iter().map(|(k, v)| (k.clone(), json!({"metrics": [{k: v}]}))).collect())
}
