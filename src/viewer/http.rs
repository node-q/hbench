//! HTTP layer: request/response types, routing, static files and the server loop.

use super::*;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/viewer_files.rs"));
}

pub struct Resp {
    pub status: u16,
    pub ctype: String,
    pub body: Vec<u8>,
    pub headers: Vec<(&'static str, String)>,
}

/// (status, FastAPI `detail`).
pub(super) type Err = (u16, Value);
pub(super) type Res = std::result::Result<Resp, Err>;

pub(super) fn err(status: u16, detail: impl Into<String>) -> Err {
    (status, Value::String(detail.into()))
}

pub(super) fn ok_json(v: Value) -> Res {
    Ok(Resp { status: 200, ctype: "application/json".into(), body: serde_json::to_vec(&v).unwrap(), headers: vec![] })
}

pub(super) fn ok_text(s: String) -> Res {
    Ok(Resp { status: 200, ctype: "text/plain; charset=utf-8".into(), body: s.into_bytes(), headers: vec![] })
}

pub struct Req<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub origin: Option<&'a str>,
    pub host: Option<&'a str>,
    pub client_loopback: bool,
    pub body: &'a [u8],
}

fn static_file(path: &str) -> Option<Resp> {
    let (_, body) = embedded::VIEWER_FILES.iter().find(|(p, _)| *p == path)?;
    let ctype = match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "ico" => "image/vnd.microsoft.icon",
        "ttf" => "font/ttf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    };
    Some(Resp { status: 200, ctype: ctype.into(), body: body.to_vec(), headers: vec![] })
}

/// Origins allowed to make state-changing requests: the viewer's own origin, when its Host is loopback.
fn write_allowed(req: &Req) -> bool {
    let Some(origin) = req.origin else { return true };
    let Some(host) = req.host else { return false };
    let hostname = if host.starts_with('[') { host.split(']').next().map(|h| h.trim_start_matches('[')) } else { host.split(':').next() };
    matches!(hostname, Some("127.0.0.1" | "localhost" | "::1")) && origin == format!("http://{host}")
}

pub fn handle(app: &App, req: &Req) -> Resp {
    route(app, req).unwrap_or_else(|(status, detail)| Resp {
        status,
        ctype: "application/json".into(),
        body: serde_json::to_vec(&json!({"detail": detail})).unwrap(),
        headers: vec![],
    })
}

fn route(app: &App, req: &Req) -> Res {
    let (path, query) = req.url.split_once('?').unwrap_or((req.url, ""));
    let q = Query::parse(query);
    if !matches!(req.method, "GET" | "HEAD" | "OPTIONS") && !write_allowed(req) {
        return Err(err(403, "Cross-origin request rejected."));
    }
    let Some(api) = path.strip_prefix("/api/") else {
        if req.method != "GET" {
            return Err(err(405, "Method Not Allowed"));
        }
        if path.starts_with("/assets/") || path.starts_with("/fonts/") {
            return static_file(path).ok_or_else(|| err(404, "Not Found"));
        }
        if path == "/auth/callback" {
            let msg = q.get("error").unwrap_or("Hub sign-in is not supported by hbench");
            return Ok(Resp {
                status: 400,
                ctype: "text/html; charset=utf-8".into(),
                body: format!("<h1>Sign-in failed</h1><p>{}</p>", html_escape(msg)).into_bytes(),
                headers: vec![],
            });
        }
        return static_file(if path == "/favicon.ico" || path == "/favicon.svg" { path } else { "/index.html" })
            .ok_or_else(|| err(404, "Not Found"));
    };
    let decoded: Vec<String> = api.split('/').map(|x| pct_decode(x, false)).collect();
    let segs: Vec<&str> = decoded.iter().map(String::as_str).collect();
    match (req.method, segs.as_slice()) {
        ("GET", ["health"]) => ok_json(json!({"status": "ok"})),
        ("GET", ["config"]) => {
            ok_json(json!({"folder": app.folder.to_string_lossy(), "mode": app.mode.as_str(), "environments": ENVIRONMENTS}))
        }
        ("GET", ["pricing"]) => pricing(&q.required("model")?),
        ("GET", ["auth", "status"]) => ok_json(json!({"authenticated": false, "username": null})),
        ("GET", ["auth", "organizations"]) => Err(err(401, NOT_AUTHENTICATED)),
        ("GET", ["auth", "login-url"]) => Err(err(501, "Hub sign-in is not supported by hbench.")),
        ("POST", ["auth", "logout"]) => ok_json(json!({"status": "ok"})),
        ("GET", s) if app.mode == Mode::Tasks => app.tasks_get(s, &q),
        ("GET", s) => app.jobs_get(s, &q, req),
        (m, s) if app.mode == Mode::Jobs => app.jobs_write(m, s, req),
        _ => Err(err(405, "Method Not Allowed")),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Folder-type auto-detection (jobs or tasks).
pub fn detect_mode(folder: &Path) -> Result<Mode> {
    let subdirs: Vec<PathBuf> = sorted_entries(folder).into_iter().filter(|d| d.is_dir() && !name_of(d).starts_with('.')).collect();
    if subdirs.is_empty() {
        bail!("Folder has no subdirectories. Use --tasks or --jobs to specify the folder type.");
    }
    for d in subdirs {
        if d.join("config.json").exists() {
            return Ok(Mode::Jobs);
        }
        if d.join("task.toml").exists() {
            return Ok(Mode::Tasks);
        }
    }
    bail!("Could not auto-detect folder type. Use --tasks or --jobs to specify.")
}

/// `8080` or `8080-8089`.
pub fn parse_port_range(s: &str) -> Result<(u16, u16)> {
    let (a, b) = s.split_once('-').unwrap_or((s, s));
    Ok((a.trim().parse()?, b.trim().parse()?))
}

pub fn serve(folder: &Path, mode: Option<Mode>, host: &str, ports: (u16, u16)) -> Result<()> {
    let folder = std::path::absolute(folder)?;
    if !folder.exists() {
        bail!("Folder '{}' does not exist", folder.display());
    }
    let folder = folder.canonicalize()?;
    let mode = match mode {
        Some(m) => m,
        None => detect_mode(&folder)?,
    };
    let server = (ports.0..=ports.1)
        .find_map(|p| tiny_http::Server::http((host, p)).ok().map(|s| (s, p)))
        .ok_or_else(|| anyhow!("No available port found in range {}-{}", ports.0, ports.1))?;
    let (server, port) = (Arc::new(server.0), server.1);
    println!("Starting hbench viewer");
    println!("  {}: {}", if mode == Mode::Tasks { "Tasks folder" } else { "Jobs folder" }, folder.display());
    println!("  Mode: {}", mode.as_str());
    println!("  Server: http://{host}:{port}");
    let app = Arc::new(App::new(folder, mode));
    // ponytail: fixed worker pool; the SPA fires a handful of parallel requests per page.
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let (server, app) = (server.clone(), app.clone());
            std::thread::spawn(move || {
                for mut rq in server.incoming_requests() {
                    let header = |name: &str| {
                        rq.headers()
                            .iter()
                            .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
                            .map(|h| h.value.as_str().to_string())
                    };
                    let (origin, host) = (header("origin"), header("host"));
                    let loopback = rq.remote_addr().is_some_and(|a| a.ip().is_loopback());
                    let method = rq.method().as_str().to_uppercase();
                    let url = rq.url().to_string();
                    let mut body = vec![];
                    let _ = rq.as_reader().read_to_end(&mut body);
                    let r = handle(
                        &app,
                        &Req {
                            method: &method,
                            url: &url,
                            origin: origin.as_deref(),
                            host: host.as_deref(),
                            client_loopback: loopback,
                            body: &body,
                        },
                    );
                    let mut resp = tiny_http::Response::from_data(r.body).with_status_code(r.status);
                    resp.add_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], r.ctype.as_bytes()).unwrap());
                    for (k, v) in &r.headers {
                        if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                            resp.add_header(h);
                        }
                    }
                    // Without `Connection: close`, a reused keep-alive connection stalls ~4s in tiny_http.
                    resp.add_header(tiny_http::Header::from_bytes(&b"Connection"[..], &b"close"[..]).unwrap());
                    let _ = rq.respond(resp);
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}
