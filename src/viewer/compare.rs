//! `/api/compare`: the task x agent/model grid across jobs.

use super::*;
use serde_json::{Map, Value, json};

pub(super) fn compare(app: &App, jobs: &[String]) -> Res {
    let existing = app.list_jobs();
    if let Some(j) = jobs.iter().find(|j| !existing.contains(j)) {
        return Err(err(404, format!("Job '{j}' not found")));
    }
    let mut tasks: Vec<(Option<String>, String)> = vec![];
    let mut ams: Vec<(String, Option<String>, Option<String>, Option<String>)> = vec![];
    let mut cells: Map<String, Value> = Map::new();
    let opt = |v: &Value| v.as_str().map(String::from);
    for job in jobs {
        for sm in app.task_summaries(job) {
            let (source, task) = (opt(&sm["source"]), sm["task_name"].as_str().unwrap_or_default().to_string());
            let (agent, provider, model) = (opt(&sm["agent_name"]), opt(&sm["model_provider"]), opt(&sm["model_name"]));
            let task_key = format!("{}::{task}", source.clone().unwrap_or_default());
            let am_key = format!(
                "{job}::{}::{}::{}",
                agent.clone().unwrap_or_default(),
                provider.clone().unwrap_or_default(),
                model.clone().unwrap_or_default()
            );
            if !tasks.contains(&(source.clone(), task.clone())) {
                tasks.push((source, task));
            }
            let am = (job.clone(), agent, provider, model);
            if !ams.contains(&am) {
                ams.push(am);
            }
            let cell = json!({
                "job_name": job, "avg_reward": sm["avg_reward"], "avg_duration_ms": sm["avg_duration_ms"], "avg_cost_usd": sm["avg_cost_usd"],
                "avg_tokens": sm["avg_tokens"], "n_trials": sm["n_trials"], "n_completed": sm["n_completed"],
            });
            cells.entry(task_key).or_insert_with(|| json!({})).as_object_mut().unwrap().insert(am_key, cell);
        }
    }
    let reward_of = |c: &Value| c["avg_reward"].as_f64().unwrap_or(0.0);
    let task_avg = |key: &str| -> f64 {
        let cs: Vec<f64> = cells.get(key).and_then(Value::as_object).map(|m| m.values().map(reward_of).collect()).unwrap_or_default();
        if cs.is_empty() { 0.0 } else { cs.iter().sum::<f64>() / cs.len() as f64 }
    };
    let mut task_list: Vec<(f64, Value)> = tasks
        .into_iter()
        .map(|(src, name)| {
            let key = format!("{}::{name}", src.clone().unwrap_or_default());
            (task_avg(&key), json!({"source": src, "task_name": name, "key": key}))
        })
        .collect();
    task_list.sort_by(|a, b| {
        (-a.0)
            .partial_cmp(&-b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1["source"].as_str().unwrap_or("").cmp(b.1["source"].as_str().unwrap_or("")))
            .then_with(|| a.1["task_name"].as_str().cmp(&b.1["task_name"].as_str()))
    });
    let mut am_list: Vec<(f64, Value)> = ams
        .into_iter()
        .map(|(job, agent, provider, model)| {
            let key = format!(
                "{job}::{}::{}::{}",
                agent.clone().unwrap_or_default(),
                provider.clone().unwrap_or_default(),
                model.clone().unwrap_or_default()
            );
            let rs: Vec<f64> = cells.values().filter_map(|tc| tc.get(&key)).map(reward_of).collect();
            let avg = if rs.is_empty() { 0.0 } else { rs.iter().sum::<f64>() / rs.len() as f64 };
            (avg, json!({"job_name": job, "agent_name": agent, "model_provider": provider, "model_name": model, "key": key}))
        })
        .collect();
    let k = |v: &Value, f: &str| v[f].as_str().unwrap_or("").to_string();
    am_list.sort_by(|a, b| {
        (-a.0).partial_cmp(&-b.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| {
            [k(&a.1, "job_name"), k(&a.1, "agent_name"), k(&a.1, "model_provider"), k(&a.1, "model_name")].cmp(&[
                k(&b.1, "job_name"),
                k(&b.1, "agent_name"),
                k(&b.1, "model_provider"),
                k(&b.1, "model_name"),
            ])
        })
    });
    ok_json(
        json!({"tasks": task_list.into_iter().map(|x| x.1).collect::<Vec<_>>(), "agent_models": am_list.into_iter().map(|x| x.1).collect::<Vec<_>>(), "cells": cells}),
    )
}
