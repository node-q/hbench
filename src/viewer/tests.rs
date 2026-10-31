use super::tasks::task_config_dump;
use super::*;
use serde_json::{Value, json};
use std::path::Path;

fn get(app: &App, url: &str) -> (u16, Value) {
    let r = handle(app, &Req { method: "GET", url, origin: None, host: Some("127.0.0.1:8080"), body: b"" });
    (r.status, serde_json::from_slice(&r.body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&r.body).into_owned())))
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn trial(dir: &Path, task: &str, reward: Option<f64>, err: Option<&str>, started: &str, finished: Option<&str>, cost: f64) {
    let r = json!({
        "id": uuid::Uuid::new_v4(), "task_name": task, "trial_name": name_of(dir), "trial_uri": "", "task_id": {"path": task}, "source": "ds", "task_checksum": "",
        "config": {"task": {"path": task}, "agent": {"name": "claude-code", "kwargs": {"reasoning_effort": "high"}}},
        "agent_info": {"name": "claude-code", "version": "2", "model_info": {"name": "m", "provider": "anthropic"}},
        "agent_result": {"n_input_tokens": 100, "n_cache_tokens": 30, "n_output_tokens": 5, "cost_usd": cost},
        "verifier_result": reward.map(|r| json!({"rewards": {"reward": r, "aux": 2}})),
        "exception_info": err.map(|e| json!({"exception_type": e})), "started_at": started, "finished_at": finished,
    });
    write(&dir.join("result.json"), &r.to_string());
}

/// jobs/{old,new}: new has 3 trials over 2 tasks (one errored), plus one in-progress trial (config only).
fn fixture() -> App {
    let root = std::env::temp_dir().join(format!("hbench-view-{}", uuid::Uuid::new_v4()));
    let new = root.join("2026-01-02__00-00-00");
    write(
        &new.join("config.json"),
        r#"{"job_name":"n","agents":[{"name":"claude-code","model_name":"anthropic/m"}],"datasets":[{"name":"ds","version":2}]}"#,
    );
    write(
        &new.join("result.json"),
        r#"{"id":"00000000-0000-0000-0000-000000000001","started_at":"2026-01-02T00:00:00Z","finished_at":null,"n_total_trials":4,"stats":{"n_trials":3,"n_errors":1,"evals":{"k":{"metrics":[{"mean":0.5}]}},"n_input_tokens":300,"n_cache_tokens":90,"cost_usd":0.6}}"#,
    );
    trial(&new.join("a__1"), "a", Some(1.0), None, "2026-01-02T00:00:00Z", Some("2026-01-02T00:00:10Z"), 0.1);
    trial(&new.join("a__2"), "a", Some(0.0), Some("AgentTimeoutError"), "2026-01-02T00:00:00Z", Some("2026-01-02T00:00:30Z"), 0.2);
    trial(&new.join("b__1"), "b", Some(0.75), None, "2026-01-02T00:00:00Z", Some("2026-01-02T00:00:20Z"), 0.3);
    write(&new.join("c__1/config.json"), r#"{"task":{"path":"/t/c"},"trial_name":"c__1","agent":{"name":"oracle"}}"#);
    write(&new.join("a__1/agent/x.txt"), "a\r\nb");
    write(&root.join("2025-01-01__00-00-00/config.json"), "{}");
    write(&root.join("secret.txt"), "top secret");
    App::new(root.canonicalize().unwrap(), Mode::Jobs, true)
}

#[test]
fn jobs_list_sort_filter_paginate() {
    let app = fixture();
    let (_, j) = get(&app, "/api/jobs");
    assert_eq!((j["total"].as_i64(), j["total_pages"].as_i64()), (Some(2), Some(1)));
    let first = &j["items"][0];
    assert_eq!(first["name"], "2026-01-02__00-00-00");
    assert_eq!(
        (first["n_completed_trials"].as_i64(), first["n_errored_trials"].as_i64(), first["total_input_tokens"].as_i64()),
        (Some(3), Some(1), Some(210))
    );
    assert_eq!(
        (first["datasets"].clone(), first["providers"].clone(), first["models"].clone()),
        (json!(["ds"]), json!(["anthropic"]), json!(["m"]))
    );
    assert_eq!(j["items"][1]["agents"], json!(["oracle"])); // default agent, no result -> sorted last
    assert_eq!(get(&app, "/api/jobs?q=ANTHROPIC").1["total"], 1);
    assert_eq!(get(&app, "/api/jobs?agent=oracle&agent=nope").1["items"][0]["name"], "2025-01-01__00-00-00");
    let p = get(&app, "/api/jobs?page=2&page_size=1").1;
    assert_eq!((p["items"][0]["name"].as_str(), p["total_pages"].as_i64()), (Some("2025-01-01__00-00-00"), Some(2)));
    assert_eq!(get(&app, "/api/jobs?page_size=101").0, 422);
    assert_eq!(get(&app, "/api/jobs/filters").1["agents"], json!([{"value": "claude-code", "count": 1}, {"value": "oracle", "count": 1}]));
}

#[test]
fn task_and_trial_summaries() {
    let app = fixture();
    let job = "2026-01-02__00-00-00";
    let (_, t) = get(&app, &format!("/api/jobs/{job}/tasks?sort_by=avg_reward&sort_order=desc"));
    let names: Vec<&str> = t["items"].as_array().unwrap().iter().map(|x| x["task_name"].as_str().unwrap()).collect();
    assert_eq!(names, ["c", "b", "a"]); // in-progress group (avg None) first when descending
    let a = &t["items"][2];
    assert_eq!(
        (a["n_trials"].as_i64(), a["n_errors"].as_i64(), a["avg_reward"].as_f64(), a["avg_duration_ms"].as_f64()),
        (Some(2), Some(1), Some(0.5), Some(20000.0))
    );
    assert_eq!(
        (a["avg_input_tokens"].as_f64(), a["avg_tokens"].as_f64(), a["evals"]["aux"]["metrics"][0]["aux"].as_f64()),
        (Some(70.0), Some(105.0), Some(2.0))
    );
    assert_eq!(a["exception_types"], json!(["AgentTimeoutError"]));
    let (_, tr) = get(&app, &format!("/api/jobs/{job}/trials?sort_by=cost_usd&sort_order=desc"));
    let names: Vec<&str> = tr["items"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["c__1", "b__1", "a__2", "a__1"]); // None first when descending
    assert_eq!((tr["items"][1]["reward"].as_f64(), tr["items"][1]["reasoning_effort"].as_str()), (Some(0.75), Some("high")));
    assert_eq!(get(&app, &format!("/api/jobs/{job}/trials?reasoning_effort=__missing__")).1["total"], 1);
    assert_eq!(
        get(&app, &format!("/api/jobs/{job}/tasks/filters")).1["reasoning_efforts"],
        json!([{"value": "__missing__", "count": 1}, {"value": "high", "count": 3}])
    );
    let nav = get(&app, &format!("/api/jobs/{job}/trials/a__2/navigation")).1;
    assert_eq!((nav["previous"].as_str(), nav["next"].as_str(), nav["total"].as_i64()), (Some("a__1"), Some("b__1"), Some(4)));
    let partial = get(&app, &format!("/api/jobs/{job}/trials/c__1")).1;
    assert_eq!(
        (partial["task_name"].as_str(), partial["agent_info"]["name"].as_str(), partial["finished_at"].is_null()),
        (Some("c"), Some("oracle"), true)
    );
    assert_eq!(get(&app, &format!("/api/jobs/{job}/trials/a__1/files/agent/x.txt")).1, "a\nb");
    let cmp = get(&app, &format!("/api/compare?job={job}")).1;
    assert_eq!(cmp["tasks"][0]["task_name"], "b"); // highest avg reward first
}

#[test]
fn path_guards_and_spa() {
    let app = fixture();
    let job = "2026-01-02__00-00-00";
    for url in [
        format!("/api/jobs/{job}/trials/a__1/files/../../../secret.txt"),
        format!("/api/jobs/{job}/trials/a__1/files/%2E%2E/%2E%2E/%2E%2E/secret.txt"),
    ] {
        let (s, body) = get(&app, &url);
        assert_eq!(s, 403, "{url} -> {body}");
    }
    assert_eq!(get(&app, "/api/jobs/..").0, 400);
    assert_eq!(get(&app, "/api/jobs/nope").0, 404);
    assert_eq!(get(&app, &format!("/api/jobs/{job}/trials/a__1/verifier-output?step=../../..")).0, 400);
    let r = handle(
        &app,
        &Req {
            method: "DELETE",
            url: &format!("/api/jobs/{job}"),
            origin: Some("http://evil.example"),
            host: Some("127.0.0.1:8080"),
            body: b"",
        },
    );
    assert_eq!(r.status, 403);
    assert!(app.folder.join(job).exists());
    let page = handle(&app, &Req { method: "GET", url: "/jobs/x/trials/y", origin: None, host: None, body: b"" });
    assert!(String::from_utf8(page.body).unwrap().contains(r#"<div id="root">"#));
}

#[test]
fn write_routes_need_a_loopback_bind() {
    let local = fixture();
    let exposed = App::new(local.folder.clone(), Mode::Jobs, false);
    let job = "2026-01-02__00-00-00";
    let delete = |app: &App, origin: Option<&str>, host: &str| {
        let url = format!("/api/jobs/{job}");
        handle(app, &Req { method: "DELETE", url: &url, origin, host: Some(host), body: b"" })
    };
    for (origin, host) in [(None, "10.0.0.5:8080"), (Some("http://10.0.0.5:8080"), "10.0.0.5:8080"), (None, "127.0.0.1:8080")] {
        let r = delete(&exposed, origin, host);
        assert_eq!(r.status, 403, "{origin:?} {host}");
        assert!(String::from_utf8_lossy(&r.body).contains("read-only"));
    }
    let r = handle(&exposed, &Req { method: "POST", url: "/api/run", origin: None, host: Some("10.0.0.5:8080"), body: b"{}" });
    assert_eq!(r.status, 403);
    assert!(exposed.folder.join(job).exists());
    assert_eq!(get(&exposed, &format!("/api/jobs/{job}")).0, 200);
    assert_eq!(delete(&local, None, "127.0.0.1:8080").status, 200);
    assert!(!local.folder.join(job).exists());
}

#[test]
fn loopback_hosts() {
    for h in ["127.0.0.1", "127.0.0.2", "::1", "[::1]", "localhost", "LOCALHOST"] {
        assert!(is_loopback_host(h), "{h}");
    }
    for h in ["0.0.0.0", "::", "10.0.0.5", "example.com", ""] {
        assert!(!is_loopback_host(h), "{h}");
    }
}

#[test]
fn task_config_dump_fills_defaults_and_migrates() {
    let c = task_config_dump(
        "version = \"1.0\"\n[environment]\nmemory = \"2G\"\nos = \"LINUX\"\nallow_internet = false\n[agent]\ntimeout_sec = 600\n",
    )
    .unwrap();
    assert_eq!(
        (c["schema_version"].as_str(), c["environment"]["memory_mb"].as_i64(), c["environment"]["os"].as_str()),
        (Some("1.0"), Some(2048), Some("linux"))
    );
    assert_eq!(c["environment"]["network_mode"], "no-network");
    assert!(c["environment"].get("allow_internet").is_none() && c["agent"]["timeout_sec"].is_f64());
    assert_eq!(c["artifacts"], json!([]));
}

#[test]
fn jobs_list_newest_config_first_whatever_the_name() {
    let root = std::env::temp_dir().join(format!("hbench-view-{}", uuid::Uuid::new_v4()));
    let now = std::time::SystemTime::now();
    for (job, age_secs) in [("zebra-oldest", 300), ("apple-newest", 0), ("middle", 100)] {
        let config = root.join(job).join("config.json");
        write(&config, "{}");
        let written = now - std::time::Duration::from_secs(age_secs);
        std::fs::File::options().write(true).open(&config).unwrap().set_modified(written).unwrap();
    }
    let app = App::new(root.canonicalize().unwrap(), Mode::Jobs, true);
    assert_eq!(app.list_jobs(), ["apple-newest", "middle", "zebra-oldest"]);
    std::fs::remove_dir_all(&root).unwrap();
}
