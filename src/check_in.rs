//! Checking a job in with a control plane as it starts (`run --check-in <url>`): before its first trial,
//! hbench posts the job's id, its build and what it runs, and writes the nonce it gets back into the job's
//! config.json, so the site can tell the run was registered before it started. The API token is read from
//! `HPG_API_TOKEN`. Nothing private is sent: no keys, agent kwargs or env, and tasks by name, not path.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::JobConfig;

pub const TOKEN_ENV: &str = "HPG_API_TOKEN";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CheckIn {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct Answer {
    nonce: String,
    registered_at: DateTime<Utc>,
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
        "agents": cfg.agents.iter().map(|a| json!({"name": a.name(), "model_name": a.model_name})).collect::<Vec<_>>(),
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

/// Check the job in, unless it already has (a resumed job keeps its nonce), and record the answer in
/// `cfg.check_in`. Fails rather than run unregistered when asked to register.
pub async fn register(cfg: &mut JobConfig, job_id: uuid::Uuid) -> Result<()> {
    let Some(check_in) = cfg.check_in.clone() else { return Ok(()) };
    if check_in.nonce.is_some() {
        return Ok(());
    }
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|t| !t.is_empty())
        .with_context(|| format!("--check-in needs an API token in {TOKEN_ENV} (the site's Your bench page makes one)"))?;
    let req = crate::agents::llm::Request {
        headers: vec![("authorization".into(), format!("Bearer {token}"))],
        body: json!({"job_id": job_id, "job_name": cfg.job_name, "hbench": crate::build_info::json(), "run": summary(cfg)}),
        timeout: Duration::from_secs(60),
    };
    let (status, body, _) =
        crate::agents::llm::send_raw(&check_in.url, &req).await.map_err(|e| anyhow::anyhow!("checking in at {}: {e}", check_in.url))?;
    if !(200..300).contains(&status) {
        bail!("checking in at {} answered {status}: {}", check_in.url, crate::env::tail(&body, 300));
    }
    let answer: Answer = serde_json::from_str(&body).with_context(|| format!("checking in at {}: unexpected answer", check_in.url))?;
    cfg.check_in = Some(CheckIn { url: check_in.url, nonce: Some(answer.nonce), registered_at: Some(answer.registered_at) });
    Ok(())
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
