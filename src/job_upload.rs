//! `hbench jobs upload`: send a finished job to a server that takes one, as a gzipped tar of its folder.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

const UPLOAD_TIMEOUT: Duration = Duration::from_secs(900);

pub struct Upload {
    pub job: PathBuf,
    pub url: String,
    pub token_env: Option<String>,
    pub headers: Vec<String>,
}

#[derive(Debug)]
pub struct Uploaded {
    pub bytes: u64,
    pub response: String,
}

/// The job folder as a gzipped tar whose one top-level entry is the folder itself, as `tar czf` writes it.
pub fn pack(job: &Path, out: &Path) -> Result<u64> {
    let name = job.file_name().and_then(|n| n.to_str()).context("the job dir has no name")?;
    let file = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(file, flate2::Compression::default()));
    tar.follow_symlinks(false);
    tar.append_dir_all(name, job).with_context(|| format!("packing {}", job.display()))?;
    tar.into_inner()?.finish()?;
    Ok(std::fs::metadata(out)?.len())
}

fn header(line: &str) -> Result<(http::HeaderName, http::HeaderValue)> {
    let (name, value) = line.split_once(':').with_context(|| format!("--header {line:?}: expected `Name: value`"))?;
    let name = http::HeaderName::from_bytes(name.trim().as_bytes()).with_context(|| format!("--header {line:?}"))?;
    let value = http::HeaderValue::from_str(value.trim()).with_context(|| format!("--header {line:?}"))?;
    Ok((name, value))
}

/// What a refusal says: a JSON body's `detail`, else the body itself, shortened.
fn detail(body: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    match parsed.as_ref().and_then(|v| v.get("detail")) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => body.chars().take(2000).collect(),
    }
}

pub async fn run(u: &Upload) -> Result<Uploaded> {
    if !u.job.join("result.json").is_file() {
        bail!("{} is not a finished job: it has no result.json", u.job.display());
    }
    if u.job.join("manifest.json").is_file() {
        let problems = crate::manifest::verify(&u.job)?;
        if !problems.is_empty() {
            bail!("{}'s files differ from its manifest.json, so it is not uploaded:\n{}", u.job.display(), problems.join("\n"));
        }
    } else {
        eprintln!("warning: {} has no manifest.json; a server that checks one will refuse it", u.job.display());
    }
    let token = match &u.token_env {
        Some(name) => Some(std::env::var(name).with_context(|| format!("{name} is not set"))?),
        None => None,
    };
    let headers = u.headers.iter().map(|h| header(h)).collect::<Result<Vec<_>>>()?;

    let archive = std::env::temp_dir().join(format!("hbench-upload-{}.tar.gz", uuid::Uuid::new_v4().simple()));
    let packed = pack(&u.job, &archive);
    let body = packed.and_then(|bytes| Ok((bytes, std::fs::read(&archive)?)));
    let _ = std::fs::remove_file(&archive);
    let (bytes, body) = body?;

    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder().user_agent(format!("hbench/{}", crate::build_info::VERSION)).timeout(UPLOAD_TIMEOUT).build()?;
    let mut request = client.post(&u.url).header(http::header::CONTENT_TYPE, "application/gzip").body(body);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request.send().await.with_context(|| format!("POST {}", u.url))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{} refused the upload ({status}): {}", u.url, detail(&text));
    }
    Ok(Uploaded { bytes, response: text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn job(root: &Path) -> PathBuf {
        let job = root.join("2026-10-23__10-00-00");
        std::fs::create_dir_all(job.join("task__abc1234/agent")).unwrap();
        std::fs::write(job.join("result.json"), "{}").unwrap();
        std::fs::write(job.join("config.json"), "{}").unwrap();
        std::fs::write(job.join("task__abc1234/result.json"), "{}").unwrap();
        std::fs::write(job.join("task__abc1234/agent/log.txt"), "hello").unwrap();
        job
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hbench-upload-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entries(archive: &Path) -> Vec<String> {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(archive).unwrap()));
        let mut names: Vec<String> = tar.entries().unwrap().map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn the_archive_holds_the_job_folder_under_its_own_name() {
        let root = scratch();
        let job = job(&root);
        let out = root.join("job.tar.gz");
        assert!(pack(&job, &out).unwrap() > 0);
        let names = entries(&out);
        assert!(names.iter().all(|n| n.starts_with("2026-10-23__10-00-00")), "{names:?}");
        assert!(names.contains(&"2026-10-23__10-00-00/task__abc1234/agent/log.txt".to_string()), "{names:?}");
    }

    #[test]
    fn headers_take_the_curl_form() {
        let (name, value) = header("hpg-run-token: hpr_abc").unwrap();
        assert_eq!((name.as_str(), value.to_str().unwrap()), ("hpg-run-token", "hpr_abc"));
        assert!(header("no colon").is_err());
    }

    #[test]
    fn a_refusal_shows_the_servers_detail() {
        assert_eq!(detail(r#"{"detail": "too big (bench-quota)"}"#), "too big (bench-quota)");
        assert_eq!(detail("Bad Gateway"), "Bad Gateway");
    }

    type Seen = (Vec<(String, String)>, Vec<u8>);

    fn serve_once(status: u16, reply: &'static str) -> (String, std::thread::JoinHandle<Seen>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/bench/uploads", server.server_addr().to_ip().unwrap());
        let handle = std::thread::spawn(move || {
            let mut request = server.recv().unwrap();
            let headers = request.headers().iter().map(|h| (h.field.as_str().as_str().to_lowercase(), h.value.to_string())).collect();
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body).unwrap();
            request.respond(tiny_http::Response::from_string(reply).with_status_code(status)).unwrap();
            (headers, body)
        });
        (url, handle)
    }

    #[tokio::test]
    async fn a_job_is_posted_as_gzip_with_the_token_and_headers() {
        let root = scratch();
        let job = job(&root);
        crate::manifest::write(&job, uuid::Uuid::new_v4()).unwrap();
        let (url, server) = serve_once(201, r#"{"id": 7}"#);
        // SAFETY: unique var name.
        unsafe { std::env::set_var("HBENCH_UPLOAD_TEST_TOKEN", "secret-token") };
        let upload = Upload {
            job: job.clone(),
            url,
            token_env: Some("HBENCH_UPLOAD_TEST_TOKEN".into()),
            headers: vec!["hpg-run-token: hpr_abc".into()],
        };
        let done = run(&upload).await.unwrap();
        let (headers, body) = server.join().unwrap();
        let has = |name: &str, value: &str| headers.iter().any(|(n, v)| n == name && v == value);
        assert!(has("content-type", "application/gzip") && has("authorization", "Bearer secret-token"), "{headers:?}");
        assert!(has("hpg-run-token", "hpr_abc"), "{headers:?}");
        assert_eq!((done.bytes as usize, done.response.as_str()), (body.len(), r#"{"id": 7}"#));
        let mut gz = flate2::read::GzDecoder::new(&body[..]);
        assert!(gz.read_to_end(&mut Vec::new()).is_ok());
    }

    #[tokio::test]
    async fn a_refusal_fails_with_the_servers_reason() {
        let root = scratch();
        let job = job(&root);
        let (url, server) = serve_once(422, r#"{"detail": "the job ran hbench 0.2.6 (local-hbench-version)"}"#);
        let upload = Upload { job, url, token_env: None, headers: vec![] };
        let err = run(&upload).await.unwrap_err().to_string();
        server.join().unwrap();
        assert!(err.contains("422") && err.contains("(local-hbench-version)"), "{err}");
    }

    #[tokio::test]
    async fn a_job_edited_after_it_was_sealed_is_not_sent() {
        let root = scratch();
        let job = job(&root);
        crate::manifest::write(&job, uuid::Uuid::new_v4()).unwrap();
        std::fs::write(job.join("result.json"), r#"{"edited": true}"#).unwrap();
        let upload = Upload { job, url: "http://127.0.0.1:9/never".into(), token_env: None, headers: vec![] };
        let err = run(&upload).await.unwrap_err().to_string();
        assert!(err.contains("differ from its manifest.json"), "{err}");
    }
}
