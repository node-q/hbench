//! Checking a job in with a control plane as it starts (`run --check-in <url>`): before its first trial,
//! hbench posts the job's id, its build and what it runs, and writes the nonce it gets back into the job's
//! config.json, so the site can tell the run was registered before it started. The API token is read from
//! `HPG_API_TOKEN`. No keys or env are sent, agent kwargs only scrubbed of secrets and without local paths,
//! and tasks by name, not path.
//!
//! A quick run (`--quick`) names its dataset's anchor tasks; the control plane answers with a draw, the
//! anchors and a rotation, and hbench runs the drawn tasks instead. Where the control plane can't be
//! reached, or makes no draw, the anchors run alone.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::JobConfig;
use crate::trial::sha256_prefixed;

pub const TOKEN_ENV: &str = "HPG_API_TOKEN";
pub const QUICK: &str = "quick";
const LOCAL_PATH_KWARGS: &[&str] = &["prompt_template_path"];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CheckIn {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_set: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick_draw: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

impl CheckIn {
    pub fn quick(&self) -> bool {
        self.task_set.as_deref() == Some(QUICK)
    }
}

#[derive(Deserialize)]
struct Answer {
    nonce: String,
    registered_at: DateTime<Utc>,
    #[serde(default)]
    quick: Option<Draw>,
}

/// A quick run's tasks, as the control plane drew them.
#[derive(Deserialize)]
struct Draw {
    draw: Value,
    dataset: String,
    anchors: Vec<String>,
    rotation: Vec<String>,
    task_names: Vec<String>,
    digest: String,
}

/// What the job runs, as the control plane needs it to place the run: datasets, tasks, agents and
/// models, attempts, timeouts and resources.
pub fn summary(cfg: &JobConfig) -> Value {
    json!({
        "datasets": cfg.datasets.iter().map(|d| json!({
            "name": d.name,
            "version": d.version,
            "task_names": d.task_names,
            "exclude_task_names": d.exclude_task_names,
            "n_tasks": d.n_tasks,
        })).collect::<Vec<_>>(),
        "tasks": cfg.tasks.iter().map(|t| t.display_name()).collect::<Vec<_>>(),
        "agents": cfg.agents.iter().map(|a| json!({
            "name": a.name(),
            "model_name": a.model_name,
            "kwargs": sent_kwargs(&a.kwargs),
            "prompt_template_digest": a.kwargs.get("prompt_template_path").and_then(Value::as_str).map(|p| file_digest(Path::new(p))),
        })).collect::<Vec<_>>(),
        "extra_instruction_digests": cfg.extra_instruction_paths.iter().map(|p| file_digest(p))
            .chain(cfg.extra_instructions.iter().map(|t| sha256_prefixed(t.as_bytes())))
            .collect::<Vec<_>>(),
        "task_set": cfg.check_in.as_ref().and_then(|c| c.task_set.clone()),
        "n_attempts": cfg.n_attempts,
        "timeout_multiplier": cfg.timeout_multiplier,
        "agent_timeout_multiplier": cfg.agent_timeout_multiplier,
        "verifier_timeout_multiplier": cfg.verifier_timeout_multiplier,
        "agent_setup_timeout_multiplier": cfg.agent_setup_timeout_multiplier,
        "override_cpus": cfg.environment.override_cpus,
        "override_memory_mb": cfg.environment.override_memory_mb,
        "max_retries": cfg.retry.max_retries,
    })
}

/// An agent's kwargs as a check-in sends them: scrubbed of secrets, without paths on this machine.
fn sent_kwargs(kwargs: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    crate::redact::register_json(&json!(kwargs));
    crate::redact::value(
        kwargs.iter().filter(|(k, _)| !LOCAL_PATH_KWARGS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect(),
    )
}

/// Check the job in, unless it already has (a resumed job keeps its nonce), and record the answer in
/// `cfg.check_in`. Fails rather than run unregistered when asked to register, except that a quick run
/// the control plane can't take runs its anchors alone; the warnings returned say so.
pub async fn register(cfg: &mut JobConfig, job_id: uuid::Uuid) -> Result<Vec<String>> {
    let Some(check_in) = cfg.check_in.clone() else { return Ok(vec![]) };
    if check_in.nonce.is_some() {
        return Ok(vec![]);
    }
    if check_in.quick() {
        check_quick(cfg)?;
    }
    match ask(cfg, &check_in, job_id).await {
        Ok(answer) => {
            let mut warnings = vec![];
            let (mut quick_draw, mut digest) = (None, None);
            match answer.quick {
                Some(draw) if check_in.quick() => {
                    take_draw(cfg, &draw)?;
                    (quick_draw, digest) = (Some(draw.draw), Some(draw.digest));
                }
                None if check_in.quick() => {
                    warnings.push(format!("{} made no quick draw for this run; running the anchors alone", check_in.url))
                }
                _ => {}
            }
            cfg.check_in =
                Some(CheckIn { nonce: Some(answer.nonce), registered_at: Some(answer.registered_at), quick_draw, digest, ..check_in });
            Ok(warnings)
        }
        Err(Failure::Unavailable(e)) if check_in.quick() => {
            cfg.check_in = None;
            Ok(vec![format!("{e:#}; running the anchors alone, unregistered")])
        }
        Err(Failure::Unavailable(e) | Failure::Refused(e)) => Err(e),
    }
}

enum Failure {
    /// Not reached, or a server error: worth running the anchors alone.
    Unavailable(anyhow::Error),
    /// Answered, and said no.
    Refused(anyhow::Error),
}

/// A quick run names its one dataset's anchors, and takes nothing else from it.
fn check_quick(cfg: &JobConfig) -> Result<()> {
    match cfg.datasets.as_slice() {
        [d] if d.task_names.as_ref().is_some_and(|t| !t.is_empty()) && d.n_tasks.is_none() && d.exclude_task_names.is_none() => Ok(()),
        _ => bail!("--quick needs exactly one dataset, with its anchor tasks as task_names and no n_tasks or exclusions"),
    }
}

/// Runs the drawn tasks in place of the anchors, once the draw is for this dataset and holds all its anchors.
fn take_draw(cfg: &mut JobConfig, draw: &Draw) -> Result<()> {
    let dataset = &mut cfg.datasets[0];
    if dataset.name.as_deref() != Some(draw.dataset.as_str()) {
        bail!("the quick draw is for {}, and this run is on {}", draw.dataset, dataset.name.as_deref().unwrap_or("a local dataset"));
    }
    let mut drawn: Vec<String> = draw.anchors.iter().chain(&draw.rotation).cloned().collect();
    drawn.sort();
    drawn.dedup();
    if drawn != draw.task_names {
        bail!("the quick draw's task_names are not its anchors and rotation, sorted");
    }
    dataset.task_names = Some(draw.task_names.clone());
    Ok(())
}

async fn ask(cfg: &JobConfig, check_in: &CheckIn, job_id: uuid::Uuid) -> std::result::Result<Answer, Failure> {
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|t| !t.is_empty())
        .with_context(|| format!("--check-in needs an API token in {TOKEN_ENV} (the site's Your bench page makes one)"))
        .map_err(Failure::Refused)?;
    let req = crate::agents::llm::Request {
        headers: vec![("authorization".into(), format!("Bearer {token}"))],
        body: json!({"job_id": job_id, "job_name": cfg.job_name, "hbench": crate::build_info::json(), "run": summary(cfg)}),
        timeout: Duration::from_secs(60),
    };
    let (status, body, _) = crate::agents::llm::send_raw(&check_in.url, &req)
        .await
        .map_err(|e| Failure::Unavailable(anyhow::anyhow!("checking in at {}: {e}", check_in.url)))?;
    let answered = anyhow::anyhow!("checking in at {} answered {status}: {}", check_in.url, crate::env::tail(&body, 300));
    match status {
        200..300 => {}
        500.. => return Err(Failure::Unavailable(answered)),
        _ => return Err(Failure::Refused(answered)),
    }
    serde_json::from_str(&body).with_context(|| format!("checking in at {}: unexpected answer", check_in.url)).map_err(Failure::Refused)
}

fn file_digest(p: &Path) -> String {
    std::fs::read(p).map(|b| sha256_prefixed(&b)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    fn cfg(url: &str) -> JobConfig {
        let mut cfg = JobConfig { job_name: "checked".into(), ..Default::default() };
        cfg.check_in = Some(CheckIn { url: url.into(), ..Default::default() });
        cfg
    }

    /// One HTTP exchange on a local port: answers `status` and `body`, and hands back the request it got.
    fn serve_once(status: &str, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/check-ins", listener.local_addr().unwrap());
        let status = status.to_string();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
            let mut request = vec![];
            let mut buf = [0u8; 4096];
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length: ").map(|v| v.trim().parse::<usize>().unwrap()));
                    if len.is_some_and(|len| rest.len() >= len) {
                        break;
                    }
                }
            }
            let reply = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn a_check_in_records_the_nonce_and_sends_nothing_private() {
        let (url, server) = serve_once("200 OK", r#"{"nonce": "n-123", "registered_at": "2026-10-21T00:00:00Z"}"#);
        let mut c = cfg(&url);
        c.agents[0].model_name = Some("deepseek/deepseek-flash".into());
        c.agents[0].env.insert("DEEPSEEK_API_KEY".into(), "sk-should-not-leave".into());
        unsafe { std::env::set_var(TOKEN_ENV, "hpg_test_token_value") };
        let id = uuid::Uuid::new_v4();
        register(&mut c, id).await.unwrap();
        let request = server.join().unwrap();
        assert!(request.to_ascii_lowercase().contains("authorization: bearer hpg_test_token_value"), "{request}");
        assert!(request.contains(&id.to_string()) && request.contains("deepseek/deepseek-flash"), "{request}");
        assert!(!request.contains("sk-should-not-leave"), "{request}");
        let ci = c.check_in.unwrap();
        assert_eq!(ci.nonce.as_deref(), Some("n-123"));
        assert_eq!(ci.registered_at.unwrap().to_rfc3339(), "2026-10-21T00:00:00+00:00");
    }

    fn quick(url: &str) -> JobConfig {
        let mut c = cfg(url);
        c.check_in.as_mut().unwrap().task_set = Some(QUICK.into());
        c.datasets = vec![crate::config::DatasetConfig {
            name: Some("terminal-bench".into()),
            version: Some("2.0".into()),
            task_names: Some(vec!["a-anchor".into(), "c-anchor".into()]),
            ..Default::default()
        }];
        c
    }

    const DRAWN: &str = r#"{"nonce": "n-q", "registered_at": "2026-10-21T00:00:00Z", "cell": {"slug": "tb-flash", "exists": true},
        "quick": {"draw": 7, "dataset": "terminal-bench", "results_epoch": "0.2", "anchors": ["a-anchor", "c-anchor"],
        "rotation": ["b-drawn"], "task_names": ["a-anchor", "b-drawn", "c-anchor"], "digest": "sha256:d1"}}"#;

    #[tokio::test]
    async fn a_quick_run_runs_the_drawn_tasks_and_records_the_draw() {
        let (url, server) = serve_once("200 OK", DRAWN);
        let mut c = quick(&url);
        c.agents[0].kwargs.insert("prompt_template_path".into(), json!("/home/someone/prompt.j2"));
        c.agents[0].kwargs.insert("temperature".into(), json!(0.2));
        unsafe { std::env::set_var(TOKEN_ENV, "hpg_test_token_value") };
        assert!(register(&mut c, uuid::Uuid::new_v4()).await.unwrap().is_empty());
        let request = server.join().unwrap();
        assert!(request.contains(r#""task_set":"quick""#) && request.contains(r#""temperature":0.2"#), "{request}");
        assert!(!request.contains("/home/someone"), "{request}");
        assert_eq!(c.datasets[0].task_names.as_deref(), Some(&["a-anchor".to_string(), "b-drawn".into(), "c-anchor".into()][..]));
        let ci = c.check_in.unwrap();
        assert_eq!((ci.nonce.as_deref(), ci.quick_draw, ci.digest.as_deref()), (Some("n-q"), Some(json!(7)), Some("sha256:d1")));
    }

    #[test]
    fn kwargs_are_sent_without_secrets_or_local_paths() {
        let _g = crate::redact::test_lock();
        let kwargs = BTreeMap::from([
            ("api_key".to_string(), json!("sk-kwarg-should-not-leave")),
            ("prompt_template_path".to_string(), json!("/home/someone/prompt.j2")),
            ("temperature".to_string(), json!(0.2)),
        ]);
        let sent = sent_kwargs(&kwargs);
        assert_eq!(sent, BTreeMap::from([("api_key".to_string(), json!(crate::redact::MASK)), ("temperature".to_string(), json!(0.2))]));
    }

    #[tokio::test]
    async fn a_quick_run_with_no_draw_runs_its_anchors_registered() {
        let (url, server) = serve_once("200 OK", r#"{"nonce": "n-q", "registered_at": "2026-10-21T00:00:00Z", "quick": null}"#);
        let mut c = quick(&url);
        unsafe { std::env::set_var(TOKEN_ENV, "hpg_test_token_value") };
        let warnings = register(&mut c, uuid::Uuid::new_v4()).await.unwrap();
        server.join().unwrap();
        assert!(warnings[0].contains("no quick draw"), "{warnings:?}");
        assert_eq!(c.datasets[0].task_names.as_ref().unwrap().len(), 2);
        assert_eq!(c.check_in.unwrap().nonce.as_deref(), Some("n-q"));
    }

    #[tokio::test]
    async fn a_quick_run_the_site_cannot_take_runs_its_anchors_unregistered() {
        unsafe { std::env::set_var(TOKEN_ENV, "hpg_test_token_value") };
        let (url, server) = serve_once("503 Service Unavailable", r#"{"detail": "down"}"#);
        let mut c = quick(&url);
        assert!(register(&mut c, uuid::Uuid::new_v4()).await.unwrap()[0].contains("anchors alone, unregistered"));
        server.join().unwrap();
        assert!(c.check_in.is_none() && c.datasets[0].task_names.as_ref().unwrap().len() == 2);
        let mut unreachable = quick("http://127.0.0.1:9/api/check-ins");
        assert!(!register(&mut unreachable, uuid::Uuid::new_v4()).await.unwrap().is_empty());
        assert!(unreachable.check_in.is_none());
        let (url, server) = serve_once("409 Conflict", r#"{"detail": "check-in-taken"}"#);
        assert!(register(&mut quick(&url), uuid::Uuid::new_v4()).await.is_err());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn a_draw_for_another_dataset_or_not_its_anchors_and_rotation_is_refused() {
        unsafe { std::env::set_var(TOKEN_ENV, "hpg_test_token_value") };
        let other = DRAWN.replace(r#""dataset": "terminal-bench""#, r#""dataset": "swe-bench""#);
        let (url, server) = serve_once("200 OK", Box::leak(other.into_boxed_str()));
        let e = register(&mut quick(&url), uuid::Uuid::new_v4()).await.unwrap_err();
        server.join().unwrap();
        assert!(format!("{e:#}").contains("for swe-bench"), "{e:#}");
        let unsorted = DRAWN.replace(r#""task_names": ["a-anchor", "b-drawn", "c-anchor"]"#, r#""task_names": ["b-drawn"]"#);
        let (url, server) = serve_once("200 OK", Box::leak(unsorted.into_boxed_str()));
        assert!(register(&mut quick(&url), uuid::Uuid::new_v4()).await.is_err());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn a_quick_run_names_one_datasets_anchors() {
        let mut c = quick("http://127.0.0.1:9/unreachable");
        c.datasets[0].task_names = None;
        assert!(format!("{:#}", register(&mut c, uuid::Uuid::new_v4()).await.unwrap_err()).contains("anchor tasks"));
        let mut c = quick("http://127.0.0.1:9/unreachable");
        c.datasets[0].n_tasks = Some(5);
        assert!(register(&mut c, uuid::Uuid::new_v4()).await.is_err());
    }

    #[tokio::test]
    async fn a_refused_check_in_stops_the_job() {
        let (url, server) = serve_once("409 Conflict", r#"{"detail": "another participant checked this job in (check-in-taken)"}"#);
        unsafe { std::env::set_var(TOKEN_ENV, "hpg_test_token_value") };
        let err = register(&mut cfg(&url), uuid::Uuid::new_v4()).await.unwrap_err();
        server.join().unwrap();
        assert!(format!("{err:#}").contains("409") && format!("{err:#}").contains("check-in-taken"), "{err:#}");
    }

    #[tokio::test]
    async fn a_job_already_checked_in_is_not_checked_in_again() {
        let mut c = cfg("http://127.0.0.1:9/unreachable");
        c.check_in.as_mut().unwrap().nonce = Some("kept".into());
        register(&mut c, uuid::Uuid::new_v4()).await.unwrap();
        assert_eq!(c.check_in.unwrap().nonce.as_deref(), Some("kept"));
    }
}
