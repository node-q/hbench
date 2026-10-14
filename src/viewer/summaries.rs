//! Job, task and trial summaries and their sort orders.

use super::*;
use chrono::Utc;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::path::Path;

impl App {
    pub(super) fn job_summaries(&self) -> Vec<Value> {
        let mut out: Vec<(Option<f64>, Value)> = vec![];
        for name in self.list_jobs() {
            let dir = self.folder.join(&name);
            let result = job_result(&dir);
            let config = job_config(&dir);
            let (mut agents, mut providers, mut models, mut sources): (Vec<String>, Vec<String>, Vec<String>, Vec<String>) =
                Default::default();
            let mut environment_type = Value::Null;
            if let Some(c) = &config {
                let mut src: HashSet<String> = HashSet::new();
                for d in c["datasets"].as_array().into_iter().flatten() {
                    let local = d.get("path").is_some_and(|p| !p.is_null()) && d.get("repo").is_none_or(Value::is_null);
                    if local {
                        let p = d["path"].as_str().unwrap_or_default();
                        src.insert(path_label(p));
                    } else if let Some(n) = d.get("name").and_then(s) {
                        src.insert(n.into());
                    }
                }
                for t in c["tasks"].as_array().into_iter().flatten() {
                    if let Some(n) = t.get("name").and_then(s) {
                        src.insert(n.into());
                    } else if let Some(p) = t.get("path").and_then(s) {
                        src.insert(path_label(p));
                    }
                }
                sources = src.into_iter().collect();
                sources.sort();
                let mut a: Vec<String> =
                    c["agents"].as_array().into_iter().flatten().filter_map(|a| a.get("name").and_then(s).map(String::from)).collect();
                a.sort();
                a.dedup();
                agents = a;
                for ag in c["agents"].as_array().into_iter().flatten() {
                    if let Some(m) = ag.get("model_name").and_then(s).filter(|m| !m.is_empty()) {
                        match m.split_once('/') {
                            Some((p, n)) => {
                                providers.push(p.into());
                                models.push(n.into());
                            }
                            None => models.push(m.into()),
                        }
                    }
                }
                providers.sort();
                providers.dedup();
                models.sort();
                models.dedup();
                environment_type = c["environment"]["type"].clone();
            }
            let summary = match &result {
                Some(r) => {
                    let st = &r["stats"];
                    let evals: Map<String, Value> = st["evals"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .filter(|(_, e)| truthy(&e["metrics"]))
                        .map(|(k, e)| (k.clone(), json!({"metrics": e["metrics"]})))
                        .collect();
                    json!({
                        "name": name, "id": r["id"], "started_at": r["started_at"], "updated_at": r["updated_at"], "finished_at": r["finished_at"],
                        "n_total_trials": r["n_total_trials"], "n_completed_trials": st["n_completed_trials"], "n_errored_trials": st["n_errored_trials"],
                        "datasets": sources, "agents": agents, "providers": providers, "models": models, "environment_type": environment_type, "evals": evals,
                        "total_input_tokens": num_or_null(uncached(st["n_input_tokens"].as_i64(), st["n_cache_tokens"].as_i64())),
                        "total_cached_input_tokens": st["n_cache_tokens"], "total_output_tokens": st["n_output_tokens"], "total_cost_usd": st["cost_usd"],
                    })
                }
                None => json!({
                    "name": name, "id": null, "started_at": null, "updated_at": null, "finished_at": null, "n_total_trials": 0, "n_completed_trials": 0,
                    "n_errored_trials": 0, "datasets": sources, "agents": agents, "providers": providers, "models": models, "environment_type": environment_type,
                    "evals": {}, "total_input_tokens": null, "total_cached_input_tokens": null, "total_output_tokens": null, "total_cost_usd": null,
                }),
            };
            out.push((parse_dt(&summary["started_at"]).map(|d| ts(&d)), summary));
        }
        // Most recent first; jobs without started_at last (stable, like Python's reverse sort).
        out.sort_by(|a, b| {
            (b.0.is_some(), b.0.unwrap_or(0.0)).partial_cmp(&(a.0.is_some(), a.0.unwrap_or(0.0))).unwrap_or(std::cmp::Ordering::Equal)
        });
        out.into_iter().map(|x| x.1).collect()
    }

    pub(super) fn task_summaries(&self, job: &str) -> Vec<Value> {
        #[derive(Default)]
        struct G {
            n_trials: i64,
            n_completed: i64,
            n_errors: i64,
            exception_types: Vec<String>,
            total_reward: f64,
            reward_count: i64,
            reward_totals: Vec<(String, f64, i64)>,
            total_duration_ms: f64,
            duration_count: i64,
            total_tokens: i64,
            tokens_count: i64,
            totals: [(i64, i64); 3],
            total_cost: f64,
            cost_count: i64,
        }
        type Key = (Option<String>, Option<String>, Option<String>, Option<String>, String);
        let mut groups: Vec<(Key, G)> = vec![];
        let mut slot = |k: Key| -> usize {
            match groups.iter().position(|(x, _)| *x == k) {
                Some(i) => i,
                None => {
                    groups.push((k, G::default()));
                    groups.len() - 1
                }
            }
        };
        let mut entries: Vec<(usize, Option<Value>)> = vec![];
        for name in self.list_trials(job) {
            let dir = self.folder.join(job).join(&name);
            match trial_result(&dir) {
                Some(r) => {
                    let mi = &r["agent_info"]["model_info"];
                    let key = (
                        Some(agent_name_from_result(&r)),
                        mi.get("provider").and_then(s).map(String::from),
                        mi.get("name").and_then(s).map(String::from),
                        r.get("source").and_then(s).map(String::from),
                        r["task_name"].as_str().unwrap_or_default().to_string(),
                    );
                    entries.push((slot(key), Some(r)));
                }
                None => {
                    let Some(cfg) = trial_config(&dir) else { continue };
                    let c = serde_json::to_value(&cfg).unwrap_or_default();
                    let mi = split_model(cfg.agent.model_name.as_deref());
                    let key = (
                        Some(agent_name_from_config(&c["agent"])),
                        mi.as_ref().and_then(|m| m.0.clone()),
                        mi.map(|m| m.1),
                        cfg.task.source.clone(),
                        task_name_from_config(&c["task"]),
                    );
                    entries.push((slot(key), None));
                }
            }
        }
        for (i, r) in entries {
            let g = &mut groups[i].1;
            g.n_trials += 1;
            let Some(r) = r else { continue };
            let finished = parse_dt(&r["finished_at"]);
            if let Some(f) = finished {
                g.n_completed += 1;
                if let Some(st) = parse_dt(&r["started_at"]) {
                    g.total_duration_ms += (f - st).num_microseconds().unwrap_or(0) as f64 / 1e6 * 1000.0;
                    g.duration_count += 1;
                }
            }
            if let Some(t) = r["exception_info"].get("exception_type").and_then(s) {
                g.n_errors += 1;
                if !g.exception_types.iter().any(|x| x == t) {
                    g.exception_types.push(t.into());
                }
            }
            if finished.is_some() {
                let rewards = r["verifier_result"].get("rewards").filter(|v| truthy(v)).cloned().unwrap_or(json!({}));
                g.total_reward += rewards.get("reward").and_then(Value::as_f64).unwrap_or(0.0);
                g.reward_count += 1;
                for (k, v) in numeric_rewards(&rewards) {
                    match g.reward_totals.iter_mut().find(|x| x.0 == k) {
                        Some(x) => {
                            x.1 += v;
                            x.2 += 1;
                        }
                        None => g.reward_totals.push((k, v, 1)),
                    }
                }
            }
            let (ni, nc, no, cost) = token_totals(&r);
            let unc = uncached(ni, nc);
            if let (true, Some(u), Some(c), Some(o)) = (finished.is_some(), unc, nc, no) {
                g.total_tokens += u + c + o;
                g.tokens_count += 1;
            }
            for (slot, v) in g.totals.iter_mut().zip([unc, nc, no]) {
                if let Some(v) = v {
                    slot.0 += v;
                    slot.1 += 1;
                }
            }
            if let Some(c) = cost {
                g.total_cost += c;
                g.cost_count += 1;
            }
        }
        let avg = |t: f64, n: i64| if n > 0 { json!(t / n as f64) } else { Value::Null };
        groups
            .into_iter()
            .map(|((agent, provider, model, source, task), mut g)| {
                let avg_reward = if g.n_completed < g.n_trials {
                    Value::Null
                } else if g.reward_count > 0 {
                    json!(g.total_reward / g.reward_count as f64)
                } else {
                    json!(0.0)
                };
                g.exception_types.sort();
                let evals = evals_from_rewards(g.reward_totals.iter().map(|(k, t, n)| (k.clone(), t / *n as f64)).collect());
                json!({
                    "task_name": task, "source": source, "agent_name": agent, "model_provider": provider, "model_name": model,
                    "n_trials": g.n_trials, "n_completed": g.n_completed, "n_errors": g.n_errors, "exception_types": g.exception_types,
                    "avg_reward": avg_reward, "evals": evals, "avg_duration_ms": avg(g.total_duration_ms, g.duration_count),
                    "avg_tokens": avg(g.total_tokens as f64, g.tokens_count),
                    "avg_input_tokens": avg(g.totals[0].0 as f64, g.totals[0].1), "avg_cached_input_tokens": avg(g.totals[1].0 as f64, g.totals[1].1),
                    "avg_output_tokens": avg(g.totals[2].0 as f64, g.totals[2].1), "avg_cost_usd": avg(g.total_cost, g.cost_count),
                })
            })
            .collect()
    }

    pub(super) fn trial_summaries(&self, job: &str) -> std::result::Result<Vec<Value>, Err> {
        self.require_listed_job(job)?;
        let mut out = vec![];
        for name in self.list_trials(job) {
            let dir = self.folder.join(job).join(&name);
            let Some(r) = trial_result(&dir) else {
                let Some(cfg) = trial_config(&dir) else { continue };
                let c = serde_json::to_value(&cfg).unwrap_or_default();
                let mi = split_model(cfg.agent.model_name.as_deref());
                out.push(json!({
                    "name": name, "task_name": task_name_from_config(&c["task"]), "id": c["job_id"], "source": cfg.task.source,
                    "agent_name": agent_name_from_config(&c["agent"]), "reported_agent_name": null, "reasoning_effort": reasoning_effort(&c),
                    "model_provider": mi.as_ref().and_then(|m| m.0.clone()), "model_name": mi.map(|m| m.1), "reward": null, "evals": {},
                    "error_type": null, "started_at": null, "finished_at": null, "input_tokens": null, "cached_input_tokens": null,
                    "output_tokens": null, "cost_usd": null,
                }));
                continue;
            };
            let mi = &r["agent_info"]["model_info"];
            let evals = evals_from_rewards(numeric_rewards(&r["verifier_result"]["rewards"]));
            let reward = evals
                .as_object()
                .and_then(|m| m.values().next())
                .map(|e| e["metrics"][0].as_object().and_then(|m| m.values().next().cloned()).unwrap_or(Value::Null))
                .unwrap_or(Value::Null);
            let (ni, nc, no, cost) = token_totals(&r);
            out.push(json!({
                "name": name, "task_name": r["task_name"], "id": r["id"], "source": r.get("source").cloned().unwrap_or(Value::Null),
                "agent_name": agent_name_from_result(&r), "reported_agent_name": r["agent_info"]["name"], "reasoning_effort": reasoning_effort(&r["config"]),
                "model_provider": mi.get("provider").cloned().unwrap_or(Value::Null), "model_name": mi.get("name").cloned().unwrap_or(Value::Null),
                "reward": reward, "evals": evals, "error_type": r["exception_info"].get("exception_type").cloned().unwrap_or(Value::Null),
                "started_at": r.get("started_at").cloned().unwrap_or(Value::Null), "finished_at": r.get("finished_at").cloned().unwrap_or(Value::Null),
                "input_tokens": num_or_null(uncached(ni, nc)), "cached_input_tokens": num_or_null(nc), "output_tokens": num_or_null(no), "cost_usd": num_or_null(cost),
            }));
        }
        Ok(out)
    }
}

fn path_label(p: &str) -> String {
    let n = name_of(Path::new(p));
    if n.is_empty() { p.into() } else { n }
}

/// Python `sort(key=..., reverse=...)` (stable; reverse keeps equal items in order).
fn py_sort<K: PartialOrd>(items: &mut [Value], reverse: bool, key: impl Fn(&Value) -> K) {
    items.sort_by(|a, b| {
        let (x, y) = (key(a), key(b));
        let o = x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal);
        if reverse { o.reverse() } else { o }
    });
}

pub(super) fn sort_task_summaries(items: &mut [Value], by: &str, reverse: bool) {
    let st = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    let opt = |v: &Value, k: &str| (v[k].is_null(), v[k].as_f64().unwrap_or(0.0));
    match by {
        "task_name" | "agent_name" | "model_provider" | "model_name" | "source" => py_sort(items, reverse, |v| st(v, by)),
        "n_trials" | "n_errors" => py_sort(items, reverse, |v| v[by].as_f64().unwrap_or(0.0)),
        "avg_duration_ms" | "avg_reward" | "avg_input_tokens" | "avg_cached_input_tokens" | "avg_output_tokens" | "avg_cost_usd" => {
            py_sort(items, reverse, |v| opt(v, by))
        }
        "exception_types" => py_sort(items, reverse, |v| v["exception_types"][0].as_str().unwrap_or("").to_string()),
        _ => {}
    }
}

const TRIAL_SUMMARY_FIELDS: &[&str] = &[
    "name",
    "task_name",
    "id",
    "source",
    "agent_name",
    "reported_agent_name",
    "reasoning_effort",
    "model_provider",
    "model_name",
    "reward",
    "error_type",
    "started_at",
    "finished_at",
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "cost_usd",
];

/// A sortable form of a TrialSummary field (datetimes compare as instants, numbers numerically).
#[derive(PartialEq, PartialOrd)]
enum SortKey {
    Num(f64),
    Str(String),
}

pub(super) fn sort_trial_summaries(items: &mut Vec<Value>, by: &str, reverse: bool) {
    let (mut present, missing): (Vec<Value>, Vec<Value>) = if by == "duration" {
        items.drain(..).partition(|s| !s["started_at"].is_null())
    } else if TRIAL_SUMMARY_FIELDS.contains(&by) {
        items.drain(..).partition(|s| !s[by].is_null())
    } else {
        return;
    };
    if by == "duration" {
        let now = ts(&Utc::now());
        py_sort(&mut present, reverse, |s| {
            let end = parse_dt(&s["finished_at"]).map_or(now, |d| ts(&d));
            end - parse_dt(&s["started_at"]).map_or(0.0, |d| ts(&d))
        });
    } else {
        py_sort(&mut present, reverse, |s| match (&s[by], by) {
            (v, "started_at" | "finished_at") => SortKey::Num(parse_dt(v).map_or(0.0, |d| ts(&d))),
            (Value::Number(n), _) => SortKey::Num(n.as_f64().unwrap_or(0.0)),
            (v, _) => SortKey::Str(v.as_str().unwrap_or_default().to_string()),
        });
    }
    *items = if reverse { missing.into_iter().chain(present).collect() } else { present.into_iter().chain(missing).collect() };
}
