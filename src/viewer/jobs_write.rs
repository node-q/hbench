//! Jobs mode: state-changing routes (delete, analyze, launch).

use super::*;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

impl App {
    pub(super) fn jobs_write(&self, method: &str, segs: &[&str], req: &Req) -> Res {
        let body = || -> std::result::Result<Value, Err> {
            if req.body.is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_slice(req.body).map_err(|e| err(422, format!("request body is not valid JSON: {e}")))
        };
        match (method, segs) {
            ("DELETE", ["jobs", job]) => {
                let dir = self.existing_job(job)?;
                std::fs::remove_dir_all(&dir).map_err(|e| err(500, format!("Failed to delete job: {e}")))?;
                ok_json(json!({"status": "ok", "message": format!("Job '{job}' deleted")}))
            }
            ("POST", ["jobs", job, "summarize"]) => {
                let dir = self.existing_job(job)?;
                let b = body()?;
                let mut args = analyze_args(&b);
                args.extend(["-n".into(), b.get("n_concurrent").and_then(Value::as_i64).unwrap_or(32).to_string()]);
                if b.get("only_failed").and_then(Value::as_bool).unwrap_or(false) {
                    args.push("--failing".into());
                }
                let report = self.run_analyze(&dir, args)?;
                let Some(report) = report else { return ok_json(json!({"n_trials_analyzed": 0})) };
                std::fs::write(dir.join("analysis.json"), serde_json::to_string_pretty(&report).unwrap())
                    .map_err(|e| err(500, e.to_string()))?;
                let n = report["results"].as_array().into_iter().flatten().filter(|r| !truthy(&r["error"])).count();
                ok_json(json!({"n_trials_analyzed": n}))
            }
            ("POST", ["jobs", job, "trials", trial, "summarize"]) => {
                let dir = self.existing_trial(job, trial)?;
                let report = self.run_analyze(&dir, analyze_args(&body()?))?.ok_or_else(|| err(500, "analysis produced no report"))?;
                let r = &report["results"][0];
                if let Some(e) = r["error"].as_str().filter(|e| !e.is_empty()) {
                    return Err(err(500, e));
                }
                ok_json(json!({"summary": r["summary"]}))
            }
            ("POST", ["run"]) => self.launch(body()?),
            ("POST", ["run", "config.yaml"]) => {
                let b = body()?;
                if !b.is_object() {
                    return Err(err(422, "Body must be a JSON object."));
                }
                let text = serde_yaml_ng::to_string(&normalize_local_paths(b)).map_err(|e| err(500, e.to_string()))?;
                Ok(Resp { status: 200, ctype: "application/x-yaml".into(), body: text.into_bytes(), headers: vec![] })
            }
            ("POST", ["run", "pick-directory"]) => match native_pick_directory() {
                Ok(p) => ok_json(json!({"path": p})),
                Err(e) => Err(err(501, e.to_string())),
            },
            ("DELETE", ["run", job]) => {
                let mut runs = self.runs.lock().unwrap();
                let run = runs.get_mut(*job).ok_or_else(|| err(404, "No running launch for this job."))?;
                if run.child.try_wait().ok().flatten().is_some() {
                    return Err(err(404, "No running launch for this job."));
                }
                // SIGINT: hbench's graceful cancel (tears down containers).
                let _ = std::process::Command::new("kill").args(["-INT", &run.child.id().to_string()]).status();
                ok_json(json!({"stopped": true}))
            }
            _ => Err(err(405, "Method Not Allowed")),
        }
    }

    /// `hbench analyze <path>`; returns its `analysis.json` report.
    fn run_analyze(&self, path: &Path, args: Vec<String>) -> std::result::Result<Option<Value>, Err> {
        let exe = std::env::current_exe().map_err(|e| err(500, e.to_string()))?;
        let out = std::process::Command::new(exe)
            .arg("analyze")
            .arg(path)
            .args(args)
            .arg("-o")
            .arg(&self.folder)
            .output()
            .map_err(|e| err(500, e.to_string()))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains("No trial directories found")
            || stdout.contains("trial directories found") && !out.status.success() && !stdout.contains("Report: ")
        {
            return Ok(None);
        }
        let report = stdout.lines().find_map(|l| l.strip_prefix("Report: ")).map(PathBuf::from).and_then(|p| read_json(&p));
        match report {
            Some(r) => Ok(Some(r)),
            None => Err(err(500, stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("analysis failed").trim().to_string())),
        }
    }

    fn launch(&self, data: Value) -> Res {
        if !data.is_object() {
            return Err(err(422, "Body must be a JSON object."));
        }
        let mut data = normalize_local_paths(data);
        data["jobs_dir"] = json!(self.folder.to_string_lossy());
        let mut raw = data.clone();
        crate::config::migrate_legacy(&mut raw);
        let cfg: crate::config::JobConfig =
            crate::config::strict_value(raw).map_err(|e| err(422, format!("Invalid run configuration: {e:#}")))?;
        if cfg.datasets.is_empty() && cfg.tasks.is_empty() {
            return Err(err(422, "Specify a dataset, a task, or a local path to run."));
        }
        let job_name = cfg.job_name.clone();
        data["job_name"] = json!(job_name);
        let mut runs = self.runs.lock().unwrap();
        if self.folder.join(&job_name).exists() || runs.contains_key(&job_name) {
            return Err(err(409, format!("A job named '{job_name}' already exists.")));
        }
        let work = std::env::temp_dir().join(format!("hbench-launch-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&work).map_err(|e| err(500, e.to_string()))?;
        let config_path = work.join("config.json");
        std::fs::write(&config_path, data.to_string()).map_err(|e| err(500, e.to_string()))?;
        let log_path = work.join("launch.log");
        let log = std::fs::File::create(&log_path).map_err(|e| err(500, e.to_string()))?;
        let exe = std::env::current_exe().map_err(|e| err(500, e.to_string()))?;
        let child = std::process::Command::new(exe)
            .args(["run", "-c"])
            .arg(&config_path)
            .args(["-y", "--quiet"])
            .stdout(log.try_clone().map_err(|e| err(500, e.to_string()))?)
            .stderr(log)
            .stdin(std::process::Stdio::null())
            .spawn()
            .map_err(|e| err(500, e.to_string()))?;
        runs.insert(job_name.clone(), LaunchedRun { child, log_path });
        ok_json(json!({"job_name": job_name}))
    }
}

fn analyze_args(b: &Value) -> Vec<String> {
    let g = |k: &str, d: &str| b.get(k).and_then(s).unwrap_or(d).to_string();
    vec!["-a".into(), g("agent", "codex"), "-m".into(), g("model", "openai/gpt-6-luna"), "-e".into(), g("environment", "docker")]
}

/// Local-path normalization: a bare dataset path that is a valid task dir becomes a `tasks` entry.
fn normalize_local_paths(mut data: Value) -> Value {
    let Some(datasets) = data.get("datasets").and_then(Value::as_array).cloned() else { return data };
    let mut remaining = vec![];
    let mut tasks: Vec<Value> = data.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default();
    for ds in datasets {
        let path = ds.get("path").and_then(s).filter(|p| !p.is_empty());
        let bare = path.is_some() && !ds.get("name").is_some_and(truthy) && !ds.get("repo").is_some_and(truthy);
        match path {
            Some(p) if bare && crate::analyze::is_valid_task_dir(Path::new(p)) => tasks.push(json!({"path": p})),
            _ => remaining.push(ds),
        }
    }
    data["datasets"] = Value::Array(remaining);
    if !tasks.is_empty() {
        data["tasks"] = Value::Array(tasks);
    }
    data
}

fn native_pick_directory() -> Result<Option<String>> {
    let run = |cmd: &str, args: &[&str]| -> Result<Option<String>> {
        let out = std::process::Command::new(cmd).args(args).output()?;
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((out.status.success() && !p.is_empty()).then_some(p))
    };
    if cfg!(target_os = "macos") {
        return run("osascript", &["-e", "POSIX path of (choose folder with prompt \"Select a dataset folder\")"]);
    }
    if cfg!(target_os = "linux") {
        let which = |c: &str| std::process::Command::new("which").arg(c).output().is_ok_and(|o| o.status.success());
        if which("zenity") {
            return run("zenity", &["--file-selection", "--directory"]);
        }
        if which("kdialog") {
            return run("kdialog", &["--getexistingdirectory"]);
        }
        bail!("Install zenity or kdialog to use the folder picker.");
    }
    bail!("No native folder picker for platform {:?}.", std::env::consts::OS)
}
