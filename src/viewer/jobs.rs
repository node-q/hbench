//! Jobs mode: job/trial lookup and the read-only API.

use super::*;
use chrono::{DateTime, NaiveDateTime};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const RESOURCE_MODES: &[&str] = &["auto", "limit", "request", "guarantee", "ignore"];

impl App {
    /// `JobScanner.list_jobs()`: every subdirectory, newest (reverse-sorted) first.
    pub(super) fn list_jobs(&self) -> Vec<String> {
        let mut v: Vec<String> = sorted_entries(&self.folder).into_iter().filter(|p| p.is_dir()).map(|p| name_of(&p)).collect();
        v.reverse();
        v
    }

    pub(super) fn list_trials(&self, job: &str) -> Vec<String> {
        let dir = self.folder.join(job);
        if job.is_empty() || job.contains('/') || !dir.is_dir() {
            return vec![];
        }
        sorted_entries(&dir)
            .into_iter()
            .filter(|d| d.is_dir() && (d.join("config.json").exists() || d.join("result.json").exists()))
            .map(|p| name_of(&p))
            .collect()
    }

    fn validate_job(&self, job: &str) -> std::result::Result<PathBuf, Err> {
        let d = resolve(&self.folder, job);
        if job.contains('/') || !strictly_under(&d, &self.folder) {
            return Err(err(400, "Invalid job name"));
        }
        Ok(d)
    }

    fn validate_trial(&self, job: &str, trial: &str) -> std::result::Result<PathBuf, Err> {
        let jd = self.validate_job(job)?;
        let d = resolve(&jd, trial);
        if trial.contains('/') || !strictly_under(&d, &jd) {
            return Err(err(400, "Invalid trial name"));
        }
        Ok(d)
    }

    pub(super) fn existing_trial(&self, job: &str, trial: &str) -> std::result::Result<PathBuf, Err> {
        let d = self.validate_trial(job, trial)?;
        if !d.exists() {
            return Err(err(404, format!("Trial '{trial}' not found in job '{job}'")));
        }
        Ok(d)
    }

    pub(super) fn existing_job(&self, job: &str) -> std::result::Result<PathBuf, Err> {
        let d = self.validate_job(job)?;
        if !d.exists() {
            return Err(err(404, format!("Job '{job}' not found")));
        }
        Ok(d)
    }

    pub(super) fn require_listed_job(&self, job: &str) -> std::result::Result<(), Err> {
        if self.list_jobs().iter().any(|j| j == job) { Ok(()) } else { Err(err(404, format!("Job '{job}' not found"))) }
    }

    fn step_root(&self, trial_dir: &Path, step: Option<&str>) -> std::result::Result<PathBuf, Err> {
        let Some(step) = step else { return Ok(trial_dir.to_path_buf()) };
        let d = resolve(&trial_dir.join("steps"), step);
        if step.contains('/') || !strictly_under(&d, trial_dir) {
            return Err(err(400, "Invalid step name"));
        }
        if !d.exists() {
            return Err(err(404, format!("Step '{step}' not found")));
        }
        Ok(d)
    }

    pub(super) fn jobs_get(&self, segs: &[&str], q: &Query, req: &Req) -> Res {
        let step = q.get("step");
        match segs {
            ["jobs"] => {
                let page = q.int("page", 1, Some(1), None)?;
                let page_size = q.int("page_size", 100, Some(1), Some(100))?;
                let mut items = self.job_summaries();
                let lower = |v: &Value| v.as_str().unwrap_or_default().to_lowercase();
                let any_in = |v: &Value, needle: &str| v.as_array().into_iter().flatten().any(|x| lower(x).contains(needle));
                if let Some(qs) = q.get("q").filter(|x| !x.is_empty()) {
                    let n = qs.to_lowercase();
                    items.retain(|s| {
                        lower(&s["name"]).contains(&n) || ["datasets", "agents", "providers", "models"].iter().any(|k| any_in(&s[*k], &n))
                    });
                }
                for (param, field) in [("agent", "agents"), ("provider", "providers"), ("model", "models")] {
                    let want = q.all(param);
                    if !want.is_empty() {
                        items.retain(|s| s[field].as_array().into_iter().flatten().any(|x| want.iter().any(|w| x.as_str() == Some(w))));
                    }
                }
                let dates = q.all("date");
                if !dates.is_empty() {
                    let now = chrono::Local::now().naive_local();
                    let cutoffs: Vec<NaiveDateTime> = dates
                        .iter()
                        .filter_map(|d| match d.as_str() {
                            "today" => Some(now - chrono::Duration::days(1)),
                            "week" => Some(now - chrono::Duration::weeks(1)),
                            "month" => Some(now - chrono::Duration::days(30)),
                            _ => None,
                        })
                        .collect();
                    if let Some(cutoff) = cutoffs.into_iter().min() {
                        // `started_at.replace(tzinfo=None) >= cutoff`: the timestamp's own wall clock vs local now.
                        let wall = |t: &str| {
                            DateTime::parse_from_rfc3339(t)
                                .map(|d| d.naive_local())
                                .ok()
                                .or_else(|| NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f").ok())
                        };
                        items.retain(|s| s["started_at"].as_str().and_then(wall).is_some_and(|d| d >= cutoff));
                    }
                }
                ok_json(paginate(items, page, page_size))
            }
            ["jobs", "filters"] => {
                let (mut a, mut p, mut m) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
                for sm in self.job_summaries() {
                    for (field, counts) in [("agents", &mut a), ("providers", &mut p), ("models", &mut m)] {
                        for v in sm[field].as_array().into_iter().flatten().filter_map(Value::as_str) {
                            *counts.entry(v.to_string()).or_insert(0) += 1;
                        }
                    }
                }
                ok_json(json!({"agents": filter_options(a), "providers": filter_options(p), "models": filter_options(m)}))
            }
            ["jobs", job] => {
                let dir = self.existing_job(job)?;
                match job_result(&dir) {
                    Some(mut r) => {
                        r["job_uri"] = json!(as_uri(&dir));
                        ok_json(r)
                    }
                    None => {
                        let n = sorted_entries(&dir).iter().filter(|d| d.is_dir()).count();
                        ok_json(json!({
                            "id": job, "started_at": null, "updated_at": null, "finished_at": null, "n_total_trials": n,
                            "stats": job_stats(&json!({"n_pending_trials": n})), "job_uri": as_uri(&dir),
                        }))
                    }
                }
            }
            ["jobs", job, "analysis"] => {
                let dir = self.existing_job(job)?;
                let p = dir.join("analysis.json");
                if !p.exists() {
                    return ok_json(json!({}));
                }
                ok_json(read_json(&p).ok_or_else(|| err(500, "Error reading analysis.json"))?)
            }
            ["jobs", job, "upload"] => {
                let dir = self.existing_job(job)?;
                if !dir.join("result.json").exists() {
                    return ok_json(json!({"status": "in_progress", "job_id": null, "view_url": null}));
                }
                // No hosted hub is reachable: report the signed-out status.
                ok_json(match job_result(&dir) {
                    Some(r) => json!({"status": "unauthenticated", "job_id": r["id"], "view_url": null}),
                    None => json!({"status": "unknown", "job_id": null, "view_url": null}),
                })
            }
            ["jobs", job, "config"] => {
                let cfg = (!job.contains('/')).then(|| job_config(&self.folder.join(job))).flatten();
                ok_json(cfg.ok_or_else(|| err(404, format!("Config for job '{job}' not found")))?)
            }
            ["jobs", job, "tasks", "filters"] => {
                self.require_listed_job(job)?;
                let mut counts: [BTreeMap<String, i64>; 5] = Default::default();
                for sm in self.task_summaries(job) {
                    for (i, k) in ["agent_name", "model_provider", "model_name"].iter().enumerate() {
                        *counts[i].entry(metadata_filter_value(sm[*k].as_str())).or_insert(0) += 1;
                    }
                    if let Some(t) = sm["task_name"].as_str().filter(|t| !t.is_empty()) {
                        *counts[3].entry(t.into()).or_insert(0) += 1;
                    }
                    *counts[4].entry(sm["source"].as_str().unwrap_or("adhoc").into()).or_insert(0) += 1;
                }
                let mut efforts = BTreeMap::new();
                for name in self.list_trials(job) {
                    let dir = self.folder.join(job).join(&name);
                    let cfg = trial_result(&dir)
                        .map(|r| r["config"].clone())
                        .or_else(|| trial_config(&dir).and_then(|c| serde_json::to_value(c).ok()));
                    if let Some(c) = cfg {
                        *efforts.entry(metadata_filter_value(reasoning_effort(&c).as_deref())).or_insert(0) += 1;
                    }
                }
                let [a, p, m, t, src] = counts;
                ok_json(json!({
                    "agents": filter_options(a), "providers": filter_options(p), "models": filter_options(m), "tasks": filter_options(t),
                    "sources": filter_options(src), "reasoning_efforts": filter_options(efforts),
                }))
            }
            ["jobs", job, "tasks"] => {
                let page = q.int("page", 1, Some(1), None)?;
                let page_size = q.int("page_size", 100, Some(1), Some(100))?;
                self.require_listed_job(job)?;
                let mut items = self.task_summaries(job);
                if let Some(qs) = q.get("q").filter(|x| !x.is_empty()) {
                    let n = qs.to_lowercase();
                    items.retain(|s| {
                        ["task_name", "agent_name", "model_provider", "model_name", "source"]
                            .iter()
                            .any(|k| s[*k].as_str().is_some_and(|v| v.to_lowercase().contains(&n)))
                    });
                }
                for (param, field) in [("agent", "agent_name"), ("provider", "model_provider"), ("model", "model_name")] {
                    let want = q.all(param);
                    if !want.is_empty() {
                        items.retain(|s| want.contains(&metadata_filter_value(s[field].as_str())));
                    }
                }
                let tasks = q.all("task");
                if !tasks.is_empty() {
                    items.retain(|s| s["task_name"].as_str().is_some_and(|t| tasks.iter().any(|x| x == t)));
                }
                if let Some(by) = q.get("sort_by").filter(|x| !x.is_empty()) {
                    sort_task_summaries(&mut items, by, q.get("sort_order") == Some("desc"));
                }
                ok_json(paginate(items, page, page_size))
            }
            ["jobs", job, "chart-trials"] => ok_json(Value::Array(self.trial_summaries(job)?)),
            ["jobs", job, "trials"] => {
                let page = q.int("page", 1, Some(1), None)?;
                let page_size = q.int("page_size", 100, Some(1), Some(100))?;
                let mut items = self.trial_summaries(job)?;
                if let Some(qs) = q.get("q").filter(|x| !x.is_empty()) {
                    let n = qs.to_lowercase();
                    items.retain(|s| {
                        ["name", "task_name", "agent_name", "model_provider", "model_name", "source"]
                            .iter()
                            .any(|k| s[*k].as_str().is_some_and(|v| v.to_lowercase().contains(&n)))
                    });
                }
                for (param, field) in [
                    ("reasoning_effort", "reasoning_effort"),
                    ("agent", "agent_name"),
                    ("provider", "model_provider"),
                    ("model", "model_name"),
                ] {
                    let want = q.all(param);
                    if !want.is_empty() {
                        items.retain(|s| want.contains(&metadata_filter_value(s[field].as_str())));
                    }
                }
                let tasks = q.all("task");
                if !tasks.is_empty() {
                    items.retain(|s| s["task_name"].as_str().is_some_and(|t| tasks.iter().any(|x| x == t)));
                }
                let sources = q.all("source");
                if !sources.is_empty() {
                    items.retain(|s| sources.iter().any(|x| x == s["source"].as_str().unwrap_or("adhoc")));
                }
                if let Some(by) = q.get("sort_by").filter(|x| !x.is_empty()) {
                    sort_trial_summaries(&mut items, by, q.get("sort_order") == Some("desc"));
                }
                ok_json(paginate(items, page, page_size))
            }
            ["jobs", job, "trials", trial] => {
                if let Some(r) =
                    (!job.contains('/') && !trial.contains('/')).then(|| trial_result(&self.folder.join(job).join(trial))).flatten()
                {
                    return ok_json(r);
                }
                let not_found = || err(404, format!("Trial '{trial}' not found in job '{job}'"));
                let raw_dir = self.folder.join(job).join(trial);
                let cfg = (!job.contains('/') && !trial.contains('/')).then(|| trial_config(&raw_dir)).flatten().ok_or_else(not_found)?;
                let dir = self.validate_trial(job, trial)?;
                ok_json(partial_trial_result(trial, &dir, &cfg))
            }
            ["jobs", job, "trials", trial, "navigation"] => {
                let names = self.list_trials(job);
                let i = names.iter().position(|n| n == trial).ok_or_else(|| err(404, "Trial not found"))?;
                let finished = job_result(&self.folder.join(job)).is_some_and(|r| truthy(&r["finished_at"]));
                ok_json(json!({
                    "previous": if i > 0 { json!(names[i - 1]) } else { Value::Null },
                    "next": names.get(i + 1).map_or(Value::Null, |n| json!(n)),
                    "index": i, "total": names.len(), "finished": finished,
                }))
            }
            ["jobs", job, "trials", trial, "trajectory"] => {
                let dir = self.existing_trial(job, trial)?;
                let agent = self.step_root(&dir, step)?.join("agent");
                let p = agent.join("trajectory.json");
                if !p.exists() {
                    // hbench extra: older claude-code runs only kept the native session; convert it like `hbench traces`.
                    return ok_json(crate::atif::claude_code_trajectory(&agent).unwrap_or(Value::Null));
                }
                ok_json(read_json(&p).ok_or_else(|| err(500, "Failed to parse trajectory.json"))?)
            }
            ["jobs", job, "trials", trial, "interaction"] => {
                let dir = self.existing_trial(job, trial)?;
                ok_json(interaction(&self.step_root(&dir, step)?)?)
            }
            ["jobs", job, "trials", trial, "verifier-output"] => {
                let dir = self.existing_trial(job, trial)?;
                let v = self.step_root(&dir, step)?.join("verifier");
                let text = |n: &str| match read_text(&v.join(n)) {
                    Ok(t) => str_or_null(t.as_deref()),
                    Err(_) => json!("[Error reading file]"),
                };
                ok_json(json!({
                    "stdout": text("test-stdout.txt"), "stderr": text("test-stderr.txt"), "ctrf": text("ctrf.json"),
                    "reward": read_json_object(&v.join("reward.json")), "reward_details": read_json_object(&v.join("reward-details.json")),
                }))
            }
            ["jobs", job, "trials", trial, "files"] => {
                let dir = self.existing_trial(job, trial)?;
                let mut out = vec![];
                scan_dir(&self.step_root(&dir, step)?, "", true, false, &mut out);
                ok_json(Value::Array(out))
            }
            ["jobs", job, "trials", trial, "recording"] => {
                let dir = self.existing_trial(job, trial)?;
                let p = dir.join("agent/recording.mp4");
                ok_json(if p.is_file() {
                    json!({"available": true, "file_path": "agent/recording.mp4", "media_type": "video/mp4", "size": p.metadata().map(|m| m.len()).unwrap_or(0)})
                } else {
                    json!({"available": false, "file_path": null, "media_type": null, "size": null})
                })
            }
            ["jobs", job, "trials", trial, "recording", "file"] => {
                let dir = self.existing_trial(job, trial)?;
                let p = dir.join("agent/recording.mp4");
                if !p.is_file() {
                    return Err(err(404, "Recording not found"));
                }
                let body = std::fs::read(&p).map_err(|e| err(500, e.to_string()))?;
                Ok(Resp {
                    status: 200,
                    ctype: "video/mp4".into(),
                    body,
                    headers: vec![("Content-Disposition", "inline; filename=\"recording.mp4\"".into())],
                })
            }
            ["jobs", job, "trials", trial, "files", rest @ ..] => {
                let dir = self.existing_trial(job, trial)?;
                let root = self.step_root(&dir, step)?;
                let full = resolve(&root, &rest.join("/"));
                if !strictly_under(&full, &dir) {
                    return Err(err(403, "Access denied"));
                }
                serve_file(&full)
            }
            ["jobs", job, "trials", trial, "artifacts"] => {
                let dir = self.existing_trial(job, trial)?;
                let a = self.step_root(&dir, step)?.join("artifacts");
                if !a.exists() {
                    return ok_json(json!({"files": [], "manifest": null}));
                }
                let mut files = vec![];
                scan_dir(&a, "", false, true, &mut files);
                ok_json(json!({"files": files, "manifest": read_json(&a.join("manifest.json"))}))
            }
            ["jobs", job, "trials", trial, "agent-logs"] => {
                let dir = self.existing_trial(job, trial)?;
                let agent = self.step_root(&dir, step)?.join("agent");
                let text = |p: &Path| match read_text(p) {
                    Ok(t) => str_or_null(t.as_deref()),
                    Err(_) => json!("[Error reading file]"),
                };
                let mut commands = vec![];
                let mut i = 0;
                while agent.join(format!("command-{i}")).exists() {
                    let p = agent.join(format!("command-{i}/stdout.txt"));
                    if p.exists() {
                        commands.push(json!({"index": i, "content": text(&p)}));
                    }
                    i += 1;
                }
                ok_json(json!({
                    "oracle": text(&agent.join("oracle.txt")), "setup": text(&agent.join("setup/stdout.txt")), "commands": commands,
                    "summary": text(&dir.join("analysis.md")), "analysis": read_json(&dir.join("analysis.json")),
                }))
            }
            ["jobs", job, "trials", trial, "stream"] => {
                let dir = self.stream_dir(job, trial, req)?;
                ok_json(json!({"state": "disconnected", "available": dir.join(STREAM_HANDLE_NAME).exists()}))
            }
            ["compare"] => {
                let jobs = q.all("job");
                if jobs.is_empty() {
                    q.required("job")?;
                }
                compare(self, &jobs)
            }
            ["run", "options"] => {
                let agents: Vec<&str> = {
                    let mut a = crate::agents::agent_names();
                    a.sort();
                    a
                };
                let kwargs: Map<String, Value> = agents.iter().map(|a| (a.to_string(), Value::Array(agent_kwarg_specs(a)))).collect();
                ok_json(json!({
                    "agents": agents, "agent_kwargs": kwargs, "environments": ENVIRONMENTS, "resource_modes": RESOURCE_MODES,
                    "defaults": serde_json::to_value(crate::config::JobConfig::default()).unwrap_or_default(), "jobs_dir": self.folder.to_string_lossy(),
                }))
            }
            ["run", "history"] => {
                let limit = q.int("limit", 50, None, None)?;
                let mut items: Vec<(std::time::SystemTime, String, Value)> = vec![];
                for entry in std::fs::read_dir(&self.folder).into_iter().flatten().flatten() {
                    let p = entry.path();
                    let cp = p.join("config.json");
                    if !p.is_dir() || !cp.exists() {
                        continue;
                    }
                    let Some(mtime) = cp.metadata().and_then(|m| m.modified()).ok() else { continue };
                    let Some(mut raw) = read_json_object(&cp) else { continue };
                    raw.as_object_mut().unwrap().entry("job_name").or_insert(json!(name_of(&p)));
                    let Some(cfg) = job_config_value(raw) else { continue };
                    items.push((mtime, name_of(&p), cfg));
                }
                items.sort_by_key(|item| std::cmp::Reverse(item.0));
                let n = items.len() as i64;
                let end = if limit >= 0 { limit.min(n) } else { (n + limit).max(0) };
                ok_json(Value::Array(
                    items.into_iter().take(end as usize).map(|(_, name, config)| json!({"job_name": name, "config": config})).collect(),
                ))
            }
            ["run", "models"] => ok_json(
                json!({"models": serde_json::from_str::<Value>(include_str!("../../assets/litellm/models.json")).unwrap_or_default()}),
            ),
            ["run", job, "status"] => {
                let job_ready = self.folder.join(job).join("result.json").exists();
                let mut runs = self.runs.lock().unwrap();
                let Some(run) = runs.get_mut(*job) else {
                    return ok_json(json!({"running": false, "returncode": null, "job_ready": job_ready, "log_tail": ""}));
                };
                let rc = run.child.try_wait().ok().flatten().map(|s| s.code().unwrap_or(-1));
                let lines: Vec<String> = std::fs::read_to_string(&run.log_path).unwrap_or_default().lines().map(String::from).collect();
                let tail = lines[lines.len().saturating_sub(40)..].join("\n");
                ok_json(json!({"running": rc.is_none(), "returncode": rc, "job_ready": job_ready, "log_tail": tail}))
            }
            _ => Err(err(404, "Not Found")),
        }
    }
}

/// `partial_trial_result_from_config`: an in-progress trial (config.json, no result.json).
fn partial_trial_result(trial_name: &str, dir: &Path, cfg: &crate::config::TrialConfig) -> Value {
    let c = serde_json::to_value(cfg).unwrap_or_default();
    let mi = split_model(cfg.agent.model_name.as_deref());
    let task = &c["task"];
    let task_id = match (task.get("name").and_then(s), task.get("git_url").and_then(s)) {
        (Some(n), _) if !n.contains('/') => json!({"path": n}),
        (Some(n), _) => {
            let (org, name) = n.split_once('/').unwrap();
            json!({"org": org, "name": name, "ref": task["ref"]})
        }
        (None, Some(g)) => json!({"git_url": g, "git_commit_id": task["git_commit_id"], "path": task["path"]}),
        _ => json!({"path": task["path"]}),
    };
    json!({
        "id": cfg.job_id.map_or_else(|| json!(uuid::Uuid::new_v4()), |u| json!(u)),
        "task_name": task_name_from_config(task),
        "trial_name": if cfg.trial_name.is_empty() { trial_name.to_string() } else { cfg.trial_name.clone() },
        "trial_uri": as_uri(dir), "task_id": task_id, "source": cfg.task.source, "task_checksum": "", "config": c,
        "agent_info": {"name": agent_name_from_config(&c["agent"]), "version": "", "model_info": mi.map(|(p, n)| json!({"name": n, "provider": p}))},
        "agent_result": null, "verifier_result": null, "verifier_environment_mode": null, "exception_info": null,
        "started_at": mtime_dt(&dir.join("config.json")).map(py_dt), "finished_at": null, "environment_setup": null, "agent_setup": null,
        "agent_execution": null, "verifier": null, "step_results": null,
    })
}

fn interaction(root: &Path) -> std::result::Result<Value, Err> {
    let user_traj = root.join("user-agent/trajectory.json");
    let bridge = root.join("agent/bridge-trajectory.json");
    let user_runtime = sorted_entries(&root.join("user-agent"))
        .into_iter()
        .find(|p| p.is_file() && *p != user_traj && p.extension().is_some_and(|e| e == "jsonl" || e == "txt"));
    let obj = |p: &Path, label: &str| -> std::result::Result<Value, Err> {
        if !p.exists() {
            return Ok(Value::Null);
        }
        let t = read_text(p).map_err(|e| err(500, format!("Failed to parse {label}: {e}")))?.unwrap_or_default();
        let v: Value = serde_json::from_str(&t).map_err(|e| err(500, format!("Failed to parse {label}: {e}")))?;
        if !v.is_object() {
            return Err(err(500, format!("Failed to parse {label}: expected a JSON object")));
        }
        Ok(v)
    };
    let user_trajectory = obj(&user_traj, "user-agent trajectory")?;
    let bridge_trajectory = obj(&bridge, "bridge trajectory export")?;
    let session = bridge_trajectory["session"]["state"]["acp_session_id"].as_str().filter(|x| !x.is_empty());
    let target_log = session.and_then(|id| {
        let mut found: Vec<PathBuf> =
            walk_files(&root.join("agent/sessions/projects")).into_iter().filter(|p| name_of(p) == format!("{id}.jsonl")).collect();
        found.sort();
        found.into_iter().next()
    });
    let lines = |p: &Option<PathBuf>, label: &str| -> std::result::Result<(Vec<Value>, Vec<Value>), Err> {
        let (mut events, mut errors) = (vec![], vec![]);
        let Some(p) = p else { return Ok((events, errors)) };
        let t = read_text(p).map_err(|e| err(500, format!("Failed to read {label}: {e}")))?.unwrap_or_default();
        for (i, line) in t.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(v) => events.push(v),
                Err(e) => errors.push(json!({"line_number": i + 1, "error": e.to_string(), "raw": line})),
            }
        }
        Ok((events, errors))
    };
    let (user_events, user_errors) = lines(&user_runtime, "user-agent runtime log")?;
    let (target_events, target_errors) = lines(&target_log, "target runtime log")?;
    let rel =
        |p: Option<&Path>| p.filter(|p| p.exists()).and_then(|p| p.strip_prefix(root).ok()).map(|r| r.to_string_lossy().replace('\\', "/"));
    Ok(json!({
        "available": !user_trajectory.is_null() || !bridge_trajectory.is_null(),
        "sources": {
            "user_trajectory": rel(Some(&user_traj)), "user_runtime": rel(user_runtime.as_deref()),
            "bridge_trajectory": rel(Some(&bridge)), "target_runtime": rel(target_log.as_deref()),
        },
        "user_trajectory": user_trajectory, "user_events": user_events, "user_parse_errors": user_errors,
        "bridge_trajectory": bridge_trajectory, "target_events": target_events, "target_parse_errors": target_errors,
    }))
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for p in sorted_entries(dir) {
        if p.is_dir() {
            out.extend(walk_files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// `_agent_kwarg_specs_for`: the launcher's form fields from the agent's options schema.
fn agent_kwarg_specs(name: &str) -> Vec<Value> {
    let Some(schema) = crate::schemas::agent(name).and_then(|s| serde_json::from_str::<Value>(s).ok()) else { return vec![] };
    let required: Vec<&str> = schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let mut specs = vec![];
    for (key, prop) in schema["properties"].as_object().into_iter().flatten() {
        let variants: Vec<&Value> = match prop.get("anyOf").and_then(Value::as_array) {
            Some(a) if !a.is_empty() => a.iter().collect(),
            _ => vec![prop],
        };
        let vs = variants.iter().copied().find(|v| v.get("type").and_then(s) != Some("null")).unwrap_or(prop);
        let choices = vs.get("enum").filter(|c| truthy(c));
        let kind = if choices.is_some() {
            "enum"
        } else if vs.get("$ref").is_some() || matches!(vs.get("type").and_then(s), Some("array" | "object")) {
            "json"
        } else {
            match vs.get("type").and_then(s) {
                Some("boolean") => "bool",
                Some("integer") => "int",
                Some("number") => "float",
                _ => "string",
            }
        };
        let mut spec = json!({"key": key, "kind": kind});
        for f in ["default", "description"] {
            if let Some(v) = prop.get(f) {
                spec[f] = v.clone();
            }
        }
        if let Some(c) = choices {
            spec["choices"] = c.clone();
        }
        if required.contains(&key.as_str()) {
            spec["required"] = json!(true);
        }
        specs.push(spec);
    }
    specs
}

/// LiteLLM pricing lookup (`/api/pricing`), from hbench's pinned LiteLLM price table.
pub(super) fn pricing(model: &str) -> Res {
    let entry = [model, model.split_once('/').map_or(model, |x| x.1)].into_iter().find_map(crate::agents::pricing::model_cost);
    let Some(e) = entry else { return Err(err(404, format!("No pricing entry for model '{model}'"))) };
    let input = e.get("input_cost_per_token").cloned().unwrap_or(Value::Null);
    let cache = e.get("cache_read_input_token_cost").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| input.clone());
    ok_json(
        json!({"model_name": model, "input_cost_per_token": input, "cache_read_input_token_cost": cache, "output_cost_per_token": e.get("output_cost_per_token").cloned().unwrap_or(Value::Null)}),
    )
}
