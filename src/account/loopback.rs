//! The loopback redirect of RFC 8252 (section 7.3): a listener on `127.0.0.1` on a port the system picks,
//! which accepts exactly one authorization response on `/callback`, with this attempt's `state` and the pinned
//! issuer in `iss` (RFC 9207), then stops listening. Its answer to the browser holds no code and no token.

use anyhow::{Result, anyhow, bail};
use std::time::Instant;

pub struct Listener {
    server: tiny_http::Server,
    port: u16,
}

/// What the listener does with one request.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// Not the authorization response (another path, a stale or forged `state`): answer and keep waiting.
    Ignore(u16, &'static str),
    /// The response to this attempt, with the code.
    Code(String),
    /// The response to this attempt, refused: stop with this reason.
    Fail(String),
}

/// One query parameter, refused when it is repeated.
fn single<'a>(pairs: &'a [(String, String)], name: &str) -> std::result::Result<Option<&'a str>, ()> {
    let mut it = pairs.iter().filter(|(k, _)| k == name);
    let first = it.next().map(|(_, v)| v.as_str());
    if it.next().is_some() { Err(()) } else { Ok(first) }
}

/// Judge a request to the listener on `port` for the attempt with `state`, from the issuer `issuer`.
pub fn judge(method: &str, url: &str, host: Option<&str>, port: u16, state: &str, issuer: &str) -> Verdict {
    // A page that reaches the port through another name (DNS rebinding) is not the browser's redirect.
    if host != Some(&format!("127.0.0.1:{port}")) {
        return Verdict::Ignore(400, "Wrong host.");
    }
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    if path != "/callback" {
        return Verdict::Ignore(404, "Not found.");
    }
    if method != "GET" {
        return Verdict::Ignore(405, "Method not allowed.");
    }
    let pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    // A request without this attempt's state may come from anything on the machine: it neither completes nor
    // cancels the sign-in.
    match single(&pairs, "state") {
        Ok(Some(s)) if constant_time_eq(s.as_bytes(), state.as_bytes()) => {}
        _ => return Verdict::Ignore(400, "This is not the sign-in hbench is waiting for."),
    }
    match single(&pairs, "iss") {
        Ok(Some(i)) if i == issuer => {}
        Ok(Some(_)) => return Verdict::Fail("the answer came from another issuer".into()),
        _ => return Verdict::Fail("the answer did not name its issuer".into()),
    }
    if let Ok(Some(e)) = single(&pairs, "error") {
        let code: String = e.chars().filter(|c| c.is_ascii_lowercase() || *c == '_').take(64).collect();
        return Verdict::Fail(format!("Actuaria answered `{code}`"));
    }
    match single(&pairs, "code") {
        Ok(Some(c)) if !c.is_empty() => Verdict::Code(c.into()),
        _ => Verdict::Fail("the answer carried no code".into()),
    }
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

const STYLE: &str =
    "body{font:16px/1.5 system-ui,sans-serif;max-width:36em;margin:15vh auto;padding:0 1.5em;color:#1c1424}h1{font-size:1.4em}";

/// The page shown in the browser. Fixed text only: no code, no token, nothing from the request.
fn page(ok: bool, reason: Option<&str>) -> String {
    let (title, body) = if ok {
        ("You're signed in", "You can close this tab and go back to Actuaria Studio Essentials or your terminal.".to_string())
    } else {
        let why = reason.map(|r| format!(" ({})", html_escape(r))).unwrap_or_default();
        (
            "Sign-in didn't finish",
            format!("The sign-in was not completed{why}. Go back to Actuaria Studio Essentials or your terminal and try again."),
        )
    };
    format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>{title}</title><style>{STYLE}</style><h1>{title}</h1><p>{body}</p></html>"
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn respond(rq: tiny_http::Request, status: u16, html: String) {
    let mut resp = tiny_http::Response::from_string(html).with_status_code(status);
    for (k, v) in [
        ("Content-Type", "text/html; charset=utf-8"),
        ("Cache-Control", "no-store"),
        ("Referrer-Policy", "no-referrer"),
        ("Content-Security-Policy", "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'"),
        ("X-Content-Type-Options", "nosniff"),
        ("Connection", "close"),
    ] {
        resp.add_header(tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap());
    }
    let _ = rq.respond(resp);
}

impl Listener {
    /// Listen on 127.0.0.1, never `localhost` or another address, on a port the system picks.
    pub fn bind() -> Result<Listener> {
        let server = tiny_http::Server::http("127.0.0.1:0").map_err(|e| anyhow!("listening on 127.0.0.1 for the sign-in: {e}"))?;
        let port = server.server_addr().to_ip().map(|a| a.port()).ok_or_else(|| anyhow!("the sign-in listener has no port"))?;
        Ok(Listener { server, port })
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.port)
    }

    /// Wait for this attempt's authorization response until `deadline`; return its code, with the browser's
    /// request still open so the page can tell how the sign-in ended. The listener is closed when this
    /// returns, whatever the outcome: nothing more is accepted.
    pub fn wait(self, state: &str, issuer: &str, deadline: Instant) -> Result<Pending> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("sign-in was not completed: nothing came back from the browser in time");
            }
            let Some(rq) = self.server.recv_timeout(left)? else { continue };
            let host = rq.headers().iter().find(|h| h.field.equiv("Host")).map(|h| h.value.as_str().to_string());
            match judge(rq.method().as_str(), rq.url(), host.as_deref(), self.port, state, issuer) {
                Verdict::Ignore(status, msg) => {
                    let mut resp = tiny_http::Response::from_string(msg).with_status_code(status);
                    resp.add_header(tiny_http::Header::from_bytes(&b"Connection"[..], &b"close"[..]).unwrap());
                    let _ = rq.respond(resp);
                }
                Verdict::Code(code) => return Ok(Pending { rq, code }),
                Verdict::Fail(why) => {
                    respond(rq, 400, page(false, Some(&why)));
                    bail!("sign-in was not completed: {why}");
                }
            }
        }
    }
}

/// The browser's authorization response, accepted and not yet answered.
pub struct Pending {
    rq: tiny_http::Request,
    pub code: String,
}

impl Pending {
    /// Answer the browser: signed in, or why not. The page holds no code and no token.
    pub fn finish(self, outcome: std::result::Result<(), &str>) {
        match outcome {
            Ok(()) => respond(self.rq, 200, page(true, None)),
            Err(why) => respond(self.rq, 400, page(false, Some(why))),
        }
    }
}
