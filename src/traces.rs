//! `hbench traces export`: upstream's HF trace export (`hbench traces export`, utils/traces_utils.py),
//! written as JSONL rows (one row per episode) instead of a `datasets.Dataset`.

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

pub struct ExportOpts {
    pub recursive: bool,
    /// "all" | "last"
    pub episodes: String,
    /// None | "success" | "failure"
    pub success_filter: Option<String>,
    pub sharegpt: bool,
    pub include_instruction: bool,
    pub include_verifier_output: bool,
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// Python truthiness `a or b or ...` over JSON values.
fn first_truthy<'a>(vals: impl IntoIterator<Item = &'a Value>) -> Option<&'a Value> {
    vals.into_iter().find(|v| !matches!(v, Value::Null | Value::Bool(false)) && v.as_str() != Some("") && v.as_f64() != Some(0.0))
}

/// Python `str(float(x))`.
fn py_float_str(v: &Value) -> Option<String> {
    let f = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse().ok()?,
        Value::Bool(b) => *b as i64 as f64,
        _ => return None,
    };
    Some(if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 { format!("{f:.1}") } else { format!("{f:?}") })
}

fn is_trial_dir(p: &Path) -> bool {
    p.join("agent").exists()
}

pub fn iter_trial_dirs(root: &Path, recursive: bool) -> Vec<PathBuf> {
    if is_trial_dir(root) {
        return vec![root.to_path_buf()];
    }
    if !recursive {
        return vec![];
    }
    crate::atif::walk(root).into_iter().filter(|p| p.is_dir() && is_trial_dir(p)).collect()
}

/// result.json in the trial dir or up to 3 parents (multi-step `steps/<name>` dirs resolve to their trial).
fn load_result(trial_dir: &Path) -> Option<Value> {
    trial_dir.ancestors().take(4).map(|d| d.join("result.json")).find(|p| p.exists()).and_then(|p| read_json(&p)).filter(Value::is_object)
}

fn load_job_result(trial_dir: &Path) -> Option<Value> {
    trial_dir.ancestors().skip(1).take(6).filter_map(|d| read_json(&d.join("result.json"))).find(|v| v.get("stats").is_some())
}

struct RunMeta {
    agent_name: String,
    model_name: String,
    model_provider: String,
    start_time: Value,
    run_id: Value,
    task_name: Value,
    trial_name: Value,
}

fn run_metadata(raw: &Value) -> Option<RunMeta> {
    let config = &raw["config"];
    let agent_cfg = if config["agent"].is_object() { &config["agent"] } else { &config["agents"][0] };
    let info = &raw["agent_info"];
    let model = &info["model_info"];
    let s =
        |v: Option<&Value>, d: &str| v.map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).unwrap_or_else(|| d.into());
    let agent_name = s(first_truthy([&agent_cfg["name"], &info["name"], &config["agent_name"], &raw["agent_name"]]), "unknown-agent");
    if agent_name == "unknown-agent" {
        return None;
    }
    let unknown = |d: &str| Value::String(d.into());
    Some(RunMeta {
        agent_name,
        model_name: s(first_truthy([&model["name"], &agent_cfg["model_name"], &info["model_name"], &config["engine"]]), "unknown-model"),
        model_provider: s(
            first_truthy([&model["provider"], &agent_cfg["provider"], &info["provider"], &config["engine"]]),
            "unknown-provider",
        ),
        run_id: first_truthy([&config["job_id"], &config["job_name"], &raw["job_id"], &raw["run_id"], &raw["trial_name"]])
            .cloned()
            .unwrap_or(unknown("unknown-run")),
        start_time: first_truthy([&raw["started_at"], &raw["startedAt"]]).cloned().unwrap_or(unknown("")),
        task_name: first_truthy([&raw["task_name"], &config["task_name"]]).cloned().unwrap_or(unknown("unknown-task")),
        trial_name: first_truthy([&raw["trial_name"], &config["trial_name"]]).cloned().unwrap_or(unknown("unknown-trial")),
    })
}

/// Exception type or reward bucket ("1.0") for a trial: job-level stats first, then the trial's result.json.
fn trial_result_value(trial_dir: &Path, trial_name: &str) -> Option<String> {
    let has = |v: &Value| v.as_array().is_some_and(|a| a.iter().any(|x| x == trial_name));
    if let Some(job) = load_job_result(trial_dir)
        && let Some(evals) = job["stats"]["evals"].as_object()
    {
        for e in evals.values() {
            if let Some((name, _)) = e["exception_stats"].as_object().and_then(|m| m.iter().find(|(_, v)| has(v))) {
                return Some(name.clone());
            }
            if let Some((k, _)) = e["reward_stats"]["reward"].as_object().and_then(|m| m.iter().find(|(_, v)| has(v))) {
                return Some(py_float_str(&Value::String(k.clone())).unwrap_or_else(|| k.clone()));
            }
        }
    }
    let r = load_result(trial_dir)?;
    if let Some(t) = first_truthy([&r["exception_info"]["exception_type"]]) {
        return Some(t.as_str().map(str::to_string).unwrap_or_else(|| t.to_string()));
    }
    let reward = &r["verifier_result"]["rewards"]["reward"];
    (!reward.is_null()).then(|| py_float_str(reward).unwrap_or_else(|| reward.to_string()))
}

fn trial_is_success(trial_dir: &Path) -> Option<bool> {
    let name = trial_dir.file_name()?.to_string_lossy().into_owned();
    if let Some(v) = trial_result_value(trial_dir, &name) {
        return Some(v.parse::<f64>().is_ok_and(|f| f > 0.0));
    }
    let r = load_result(trial_dir)?;
    // upstream reads `verifier_result.reward` here (not `.rewards`), so this is effectively always false.
    Some(r["verifier_result"]["reward"].as_f64().is_some_and(|f| f > 0.0))
}

fn normalize_content(c: &Value) -> String {
    match c {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(a) if a.first().is_some_and(|f| f.get("text").is_some()) => {
            let t = &a[0]["text"];
            t.as_str().map(str::to_string).unwrap_or_else(|| crate::atif::py_dumps(t))
        }
        other => crate::atif::py_dumps(other),
    }
}

fn has_media(c: &Value) -> bool {
    c.as_array().is_some_and(|a| a.iter().any(|p| matches!(p["type"].as_str(), Some("image" | "audio"))))
}

/// One conversation per non-copied agent step (episode), each ending with that assistant turn.
fn episodes_from_trajectory(traj: &Value, meta: &RunMeta) -> Vec<Map<String, Value>> {
    let steps = traj["steps"].as_array().cloned().unwrap_or_default();
    let agent_name = first_truthy([&traj["agent"]["name"]]).cloned().unwrap_or(meta.agent_name.clone().into());
    let model_name = first_truthy([&traj["agent"]["model_name"]]).cloned().unwrap_or(meta.model_name.clone().into());
    let tools = first_truthy([&traj["agent"]["tool_definitions"]]).cloned();
    let ends: Vec<usize> =
        steps.iter().enumerate().filter(|(_, s)| s["source"] == "agent" && s["is_copied_context"] != true).map(|(i, _)| i).collect();
    let mut out = vec![];
    for (ep, &end) in ends.iter().enumerate() {
        let window = &steps[..=end];
        let last_agent = window.iter().rposition(|s| s["source"] == "agent");
        let mut conv = vec![];
        let mut tools_added = false;
        for (i, s) in window.iter().enumerate() {
            let message = if s["message"].is_null() { Value::String(String::new()) } else { s["message"].clone() };
            match s["source"].as_str() {
                Some("system") => {
                    let content = match (&tools, tools_added) {
                        (Some(t), false) => {
                            tools_added = true;
                            Value::String(format!(
                                "<tools>\n{}\n</tools>\n\n{}",
                                serde_json::to_string_pretty(t).unwrap(),
                                normalize_content(&message)
                            ))
                        }
                        _ => message,
                    };
                    conv.push(json!({"role": "user", "content": content}));
                }
                Some("user") => conv.push(json!({"role": "user", "content": message})),
                Some("agent") => {
                    let mut parts = vec![];
                    if let Some(r) = s["reasoning_content"].as_str().filter(|r| !r.is_empty()) {
                        parts.push(format!("<think>{r}</think>"));
                    }
                    let mut m = normalize_content(&message);
                    if !m.is_empty() {
                        if m.contains("</think>") && !m.contains("<think>") {
                            m = format!("<think>{m}");
                        }
                        parts.push(m);
                    }
                    for tc in s["tool_calls"].as_array().into_iter().flatten() {
                        let args = tc.get("arguments").cloned().unwrap_or(json!({}));
                        // name before arguments, as Python's insertion-ordered dict dumps it
                        let call = format!(
                            "{{\"name\": {}, \"arguments\": {}}}",
                            crate::atif::py_dumps(&tc["function_name"]),
                            crate::atif::py_dumps(&args)
                        );
                        parts.push(format!("<tool_call>\n{call}\n</tool_call>"));
                    }
                    conv.push(json!({"role": "assistant", "content": parts.join("\n")}));
                    if Some(i) != last_agent {
                        let obs: Vec<String> = s["observation"]["results"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|r| r.get("content").is_some())
                            .map(|r| normalize_content(&r["content"]))
                            .collect();
                        if !obs.is_empty() {
                            conv.push(json!({"role": "user", "content": obs.join("\n")}));
                        }
                    }
                }
                _ => {}
            }
        }
        if conv.is_empty() {
            continue;
        }
        let mut row = Map::new();
        row.insert("conversations".into(), Value::Array(conv));
        row.insert("agent".into(), agent_name.clone());
        row.insert("model".into(), model_name.clone());
        row.insert("model_provider".into(), meta.model_provider.clone().into());
        row.insert("date".into(), meta.start_time.clone());
        row.insert("task".into(), Value::Null);
        row.insert("episode".into(), format!("episode-{ep}").into());
        row.insert("run_id".into(), meta.run_id.clone());
        row.insert("trial_name".into(), Value::Null);
        if let Some(t) = &tools {
            row.insert("tool_definitions".into(), t.clone());
        }
        out.push(row);
    }
    out
}

fn sharegpt(conv: &Value) -> Value {
    Value::Array(
        conv.as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let from = match m["role"].as_str()? {
                    "user" => "human",
                    "assistant" => "gpt",
                    "system" => "system",
                    _ => return None,
                };
                Some(json!({"from": from, "value": normalize_content(&m["content"])}))
            })
            .collect(),
    )
}

fn trial_rows(trial_dir: &Path, meta: &RunMeta, o: &ExportOpts) -> Result<Vec<Map<String, Value>>> {
    let agent_dir = trial_dir.join("agent");
    let mut path = agent_dir.join("trajectory.json");
    if !path.exists() {
        return Ok(vec![]);
    }
    let trial_name = meta.trial_name.as_str().unwrap_or("").to_string();
    let result = trial_result_value(trial_dir, &trial_name);
    let mut rows: Vec<Map<String, Value>> = vec![];
    // Follow the continuation chain (`continued_trajectory_ref`), numbering episodes globally.
    for _ in 0..1000 {
        let Some(traj) = read_json(&path) else { break };
        if traj["steps"].as_array().into_iter().flatten().any(|s| {
            has_media(&s["message"]) || s["observation"]["results"].as_array().into_iter().flatten().any(|r| has_media(&r["content"]))
        }) {
            bail!("trajectory {} contains multimodal content; text-only export is not supported", path.display());
        }
        let offset = rows.len();
        for mut r in episodes_from_trajectory(&traj, meta) {
            let n: usize = r["episode"].as_str().unwrap()[8..].parse().unwrap();
            r.insert("episode".into(), format!("episode-{}", offset + n).into());
            rows.push(r);
        }
        match traj["continued_trajectory_ref"].as_str().filter(|s| !s.is_empty()) {
            Some(next) if agent_dir.join(next).exists() => path = agent_dir.join(next),
            _ => break,
        }
    }
    if o.episodes == "last" && !rows.is_empty() {
        rows = vec![rows.pop().unwrap()];
    }
    let instruction = o.include_instruction.then(|| instruction(&agent_dir)).flatten();
    let verifier = o.include_verifier_output.then(|| verifier_output(trial_dir)).flatten();
    for r in rows.iter_mut() {
        r.insert("task".into(), meta.task_name.clone());
        r.insert("trial_name".into(), meta.trial_name.clone());
        r.insert("result".into(), result.clone().into());
        if o.include_instruction {
            r.insert("instruction".into(), instruction.clone().into());
        }
        if o.include_verifier_output {
            r.insert("verifier_output".into(), verifier.clone().into());
        }
        if o.sharegpt {
            let s = sharegpt(&r["conversations"]);
            r.insert("conversations_sharegpt".into(), s);
        }
    }
    Ok(rows)
}

/// First non-blank string message of the earliest `trajectory*.json` (excluding summarization subagents).
fn instruction(agent_dir: &Path) -> Option<String> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(agent_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("trajectory") && n.ends_with(".json") && !n.contains(".summarization"))
        })
        .collect();
    paths.sort();
    let t = read_json(paths.first()?)?;
    t["steps"].as_array()?.iter().find_map(|s| s["message"].as_str().map(str::trim).filter(|m| !m.is_empty()).map(str::to_string))
}

fn verifier_output(trial_dir: &Path) -> Option<String> {
    let parts: Vec<String> = ["test-stdout.txt", "test-stderr.txt"]
        .iter()
        .filter_map(|f| std::fs::read(trial_dir.join("verifier").join(f)).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .collect();
    (!parts.is_empty()).then(|| parts.concat())
}

/// Export rows for every trial under `root`.
pub fn export(root: &Path, o: &ExportOpts) -> Result<Vec<Map<String, Value>>> {
    let trials = iter_trial_dirs(root, o.recursive);
    eprintln!("[traces] Found {} trial directories under {}", trials.len(), root.display());
    let mut rows = vec![];
    for t in trials {
        let Some(meta) = load_result(&t).as_ref().and_then(run_metadata) else { continue };
        if let Some(f) = o.success_filter.as_deref() {
            match trial_is_success(&t) {
                None => continue,
                Some(ok) if ok != (f == "success") => continue,
                _ => {}
            }
        }
        rows.extend(trial_rows(&t, &meta, o)?);
    }
    Ok(rows)
}

pub fn run(root: &Path, output: Option<&Path>, o: &ExportOpts) -> Result<()> {
    if !matches!(o.episodes.as_str(), "all" | "last") {
        bail!("--episodes must be one of: all, last");
    }
    let rows = export(root, o)?;
    let body: String = rows.iter().map(|r| serde_json::to_string(r).unwrap() + "\n").collect();
    match output {
        Some(p) => std::fs::write(p, body)?,
        None => print!("{body}"),
    }
    eprintln!("Exported {} rows from {}", rows.len(), root.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ExportOpts {
        ExportOpts {
            recursive: true,
            episodes: "all".into(),
            success_filter: None,
            sharegpt: false,
            include_instruction: false,
            include_verifier_output: false,
        }
    }

    fn fixture() -> PathBuf {
        let job = std::env::temp_dir().join(format!("hbench-traces-{}", uuid::Uuid::new_v4())).join("job");
        let trial = job.join("hello__abc1234");
        std::fs::create_dir_all(trial.join("agent")).unwrap();
        std::fs::create_dir_all(trial.join("verifier")).unwrap();
        std::fs::create_dir_all(job.join("other__def/agent")).unwrap(); // no result.json / trajectory: skipped
        std::fs::write(
            job.join("result.json"),
            json!({"id": uuid::Uuid::nil(), "started_at": "2026-01-01T00:00:00Z", "n_total_trials": 1,
                   "stats": {"evals": {"claude-code__m__adhoc": {"reward_stats": {"reward": {"1": ["hello__abc1234"]}}, "exception_stats": {}}}}})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            trial.join("result.json"),
            json!({"task_name": "hello", "trial_name": "hello__abc1234", "started_at": "2026-01-01T00:00:01Z",
                   "config": {"agent": {"name": "claude-code", "model_name": "anthropic/m"}, "job_id": "jid"},
                   "agent_info": {"name": "claude-code", "version": "2", "model_info": {"name": "m", "provider": "anthropic"}},
                   "verifier_result": {"rewards": {"reward": 1}}})
            .to_string(),
        )
        .unwrap();
        std::fs::write(trial.join("verifier/test-stdout.txt"), "ok\n").unwrap();
        std::fs::write(
            trial.join("agent/trajectory.json"),
            json!({"schema_version": "ATIF-v1.7", "agent": {"name": "claude-code", "version": "2", "model_name": "claude-m"},
            "steps": [
                {"step_id": 1, "source": "user", "message": "Make hello.txt"},
                {"step_id": 2, "source": "agent", "message": "Looking", "reasoning_content": "think",
                 "tool_calls": [{"tool_call_id": "t1", "function_name": "Bash", "arguments": {"command": "ls"}}],
                 "observation": {"results": [{"source_call_id": "t1", "content": "a.txt"}]}},
                {"step_id": 3, "source": "agent", "message": "Done."}
            ]})
            .to_string(),
        )
        .unwrap();
        job
    }

    #[test]
    fn export_format_matches_hbench() {
        let job = fixture();
        let rows = export(&job, &opts()).unwrap();
        assert_eq!(rows.len(), 2);
        let r0 = &rows[0];
        assert_eq!(
            Value::Object(r0.clone()),
            json!({
                "conversations": [
                    {"role": "user", "content": "Make hello.txt"},
                    {"role": "assistant", "content": "<think>think</think>\nLooking\n<tool_call>\n{\"name\": \"Bash\", \"arguments\": {\"command\": \"ls\"}}\n</tool_call>"}
                ],
                "agent": "claude-code", "model": "claude-m", "model_provider": "anthropic", "date": "2026-01-01T00:00:01Z",
                "task": "hello", "episode": "episode-0", "run_id": "jid", "trial_name": "hello__abc1234", "result": "1.0"
            })
        );
        // episode 1 replays history, adds step 2's observation, ends with the final assistant turn
        let c1 = rows[1]["conversations"].as_array().unwrap();
        assert_eq!(rows[1]["episode"], "episode-1");
        assert_eq!(c1.len(), 4);
        assert_eq!(c1[2], json!({"role": "user", "content": "a.txt"}));
        assert_eq!(c1[3], json!({"role": "assistant", "content": "Done."}));

        let o = ExportOpts {
            episodes: "last".into(),
            sharegpt: true,
            include_instruction: true,
            include_verifier_output: true,
            success_filter: Some("success".into()),
            ..opts()
        };
        let rows = export(&job.join("hello__abc1234"), &o).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["episode"], "episode-1");
        assert_eq!((rows[0]["instruction"].as_str(), rows[0]["verifier_output"].as_str()), (Some("Make hello.txt"), Some("ok\n")));
        assert_eq!(rows[0]["conversations_sharegpt"][3], json!({"from": "gpt", "value": "Done."}));
        assert!(export(&job, &ExportOpts { success_filter: Some("failure".into()), ..opts() }).unwrap().is_empty());

        let out = job.join("out.jsonl");
        run(&job, Some(&out), &opts()).unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap().lines().count(), 2);
        assert_eq!(py_float_str(&json!("1")).as_deref(), Some("1.0"));
        assert_eq!(py_float_str(&json!(0.25)).as_deref(), Some("0.25"));
    }
}
