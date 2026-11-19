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

/// (status, `detail` of the JSON error body).
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
    /// `X-Viewer-Key`: the secret the viewer made at launch, which only its own page holds.
    pub key: Option<&'a str>,
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
    let headers = if path == "/index.html" { vec![("Content-Security-Policy", page_csp().to_string())] } else { vec![] };
    Some(Resp { status: 200, ctype: ctype.into(), body: body.to_vec(), headers })
}

/// The page's Content-Security-Policy. Script runs only from the viewer's own bundle and the index's inline
/// theme script (by hash), so markup that slips through from dataset or agent content cannot run script in the
/// viewer's origin. The desktop app's injected scripts are the webview's, outside the policy.
fn page_csp() -> &'static str {
    static CSP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CSP.get_or_init(|| {
        let index = embedded::VIEWER_FILES.iter().find(|(p, _)| *p == "/index.html").map(|(_, b)| String::from_utf8_lossy(b).into_owned());
        let hashes: Vec<String> = index.as_deref().map(inline_script_hashes).unwrap_or_default();
        format!(
            "default-src 'self'; script-src 'self'{}; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob: https:; \
             media-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; \
             form-action 'none'; frame-src 'none'; frame-ancestors 'none'",
            hashes.iter().map(|h| format!(" 'sha256-{h}'")).collect::<String>()
        )
    })
}

/// The sha256 (base64) of each inline `<script>` in an HTML document, for `script-src`.
pub(super) fn inline_script_hashes(html: &str) -> Vec<String> {
    use base64::Engine;
    use sha2::Digest;
    let mut out = vec![];
    let mut rest = html;
    while let Some(i) = rest.find("<script") {
        let tag_end = match rest[i..].find('>') {
            Some(e) => i + e,
            None => break,
        };
        let tag = &rest[i..tag_end];
        let body_end = match rest[tag_end..].find("</script>") {
            Some(e) => tag_end + e,
            None => break,
        };
        if !tag.contains(" src=") {
            let body = &rest[tag_end + 1..body_end];
            out.push(base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(body.as_bytes())));
        }
        rest = &rest[body_end..];
    }
    out
}

/// API answers are data, never documents: opened in a tab of their own (an SVG, a text file from a dataset),
/// they get no script and no plugins.
const API_CSP: &str =
    "default-src 'none'; img-src 'self' data:; media-src 'self'; style-src 'unsafe-inline'; sandbox; frame-ancestors 'none'";

const READ_ONLY: &str =
    "The viewer is read-only: it is not bound to a loopback address. Restart it with --host 127.0.0.1 to start runs or delete jobs.";

/// 127.0.0.1, ::1 (bracketed or not), any other loopback IP, or `localhost`.
pub(super) fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

const NEEDS_KEY: &str = "This request needs the viewer's key: open the address `hbench view` printed (it ends in #k=…), or reload the app.";

/// The names a loopback-bound viewer answers to: its loopback addresses on its own port. Anything else in
/// `Host` is a page that reached the port through a name of its own (DNS rebinding).
fn host_ok(port: u16, host: Option<&str>) -> bool {
    host.is_some_and(|h| {
        [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")].iter().any(|ok| h.eq_ignore_ascii_case(ok))
    })
}

/// I-20 (sign-in-products.md 7.2): any page in the person's browser can send requests to a loopback port, so
/// the viewer checks `Host` on every request, refuses an `Origin` other than its own on the API, and asks
/// for the launch key on every route that changes something or uses the market sign-in. A viewer bound to a
/// non-loopback address answers neither kind (it is read-only).
pub(super) fn guard(app: &App, req: &Req, path: &str) -> std::result::Result<(), Err> {
    if app.bound_to_loopback && !host_ok(app.port, req.host) {
        return Err(err(403, "Unknown host."));
    }
    let api = path.starts_with("/api/");
    if api && let Some(origin) = req.origin {
        let own = req.host.is_some_and(|h| origin.eq_ignore_ascii_case(&format!("http://{h}")));
        if !own {
            return Err(err(403, "Cross-origin request rejected."));
        }
    }
    let sensitive = !matches!(req.method, "GET" | "HEAD" | "OPTIONS") || path == "/api/account";
    if sensitive {
        if !app.bound_to_loopback {
            return Err(err(403, READ_ONLY));
        }
        let key_ok = req.key.is_some_and(|k| crate::account::loopback::constant_time_eq(k.as_bytes(), app.key.as_bytes()));
        if !key_ok {
            return Err(err(403, NEEDS_KEY));
        }
    }
    Ok(())
}

pub fn handle(app: &App, req: &Req) -> Resp {
    let api = req.url.starts_with("/api/");
    let mut r = route(app, req).unwrap_or_else(|(status, detail)| Resp {
        status,
        ctype: "application/json".into(),
        body: serde_json::to_vec(&json!({"detail": detail})).unwrap(),
        headers: vec![],
    });
    r.headers.push(("X-Content-Type-Options", "nosniff".into()));
    r.headers.push(("Referrer-Policy", "no-referrer".into()));
    if api || r.ctype.starts_with("application/json") {
        r.headers.push(("Content-Security-Policy", API_CSP.into()));
        r.headers.push(("Cache-Control", "no-store".into()));
    }
    r
}

fn route(app: &App, req: &Req) -> Res {
    let (path, query) = req.url.split_once('?').unwrap_or((req.url, ""));
    let q = Query::parse(query);
    guard(app, req, path)?;
    let Some(api) = path.strip_prefix("/api/") else {
        if req.method != "GET" {
            return Err(err(405, "Method Not Allowed"));
        }
        if path.starts_with("/assets/") || path.starts_with("/fonts/") {
            return static_file(path).ok_or_else(|| err(404, "Not Found"));
        }
        return static_file(if path == "/favicon.ico" || path == "/favicon.svg" { path } else { "/index.html" })
            .ok_or_else(|| err(404, "Not Found"));
    };
    let decoded: Vec<String> = api.split('/').map(|x| pct_decode(x, false)).collect();
    let segs: Vec<&str> = decoded.iter().map(String::as_str).collect();
    match (req.method, segs.as_slice()) {
        ("GET", ["health"]) => ok_json(json!({"status": "ok"})),
        // The page asks the viewer, and the viewer asks the market with hbench's sign-in: no token reaches it.
        ("GET", ["account"]) => {
            // Workers are plain threads, outside any runtime.
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| err(500, e.to_string()))?;
            ok_json(runtime.block_on(crate::account::account_view()))
        }
        ("GET", ["config"]) => {
            ok_json(json!({"folder": app.folder.to_string_lossy(), "mode": app.mode.as_str(), "environments": ENVIRONMENTS}))
        }
        ("GET", ["pricing"]) => pricing(&q.required("model")?),
        ("GET", s) if app.mode == Mode::Tasks => app.tasks_get(s, &q),
        ("GET", s) => app.jobs_get(s, &q),
        (m, s) if app.mode == Mode::Jobs => app.jobs_write(m, s, req),
        _ => Err(err(405, "Method Not Allowed")),
    }
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

/// The desktop app makes the key and passes it here, so it can hand it to its window.
pub(super) const KEY_ENV: &str = "HBENCH_VIEW_KEY";

/// The launch key: the desktop app's (`HBENCH_VIEW_KEY`, at least 32 URL-safe characters), else a new random one.
fn launch_key() -> (String, bool) {
    if let Ok(k) = std::env::var(KEY_ENV)
        && k.len() >= 32
        && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return (k, true);
    }
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("the OS random source failed");
    use base64::Engine;
    (base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b), false)
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
    let bound_to_loopback = is_loopback_host(host);
    if !bound_to_loopback {
        println!("  Read-only: {host} is not a loopback address, so starting runs and deleting jobs is turned off");
    }
    let (key, from_env) = launch_key();
    if bound_to_loopback {
        if from_env {
            println!("  Key: from {KEY_ENV}");
        } else {
            println!("  Open: http://{host}:{port}/#k={key}");
        }
    }
    let app = Arc::new(App::new(folder, mode, bound_to_loopback, port, key));
    // Fixed worker pool; the SPA fires a handful of parallel requests per page.
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
                    let (origin, host, key) = (header("origin"), header("host"), header("x-viewer-key"));
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
                            key: key.as_deref(),
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
