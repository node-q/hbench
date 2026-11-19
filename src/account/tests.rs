//! The browser sign-in against a fake authorization server: PKCE, the loopback listener's refusals (wrong
//! `state`, wrong `iss`, a second request, the timeout), refresh that is serialised and never resent, and
//! sign-out.

use super::loopback::{Verdict, judge};
use super::store::{Memory, Store};
use super::*;
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How the fake server answers the authorization request.
#[derive(Clone, Copy, PartialEq)]
enum Redirect {
    /// The right code, state and issuer.
    Good,
    /// Another issuer in `iss` (a mix-up).
    WrongIss,
    /// No `iss` at all.
    NoIss,
    /// `error=access_denied` (the person declined).
    Denied,
}

#[derive(Default)]
struct Seen {
    challenge: String,
    redirect_uri: String,
    nonce: String,
    code: String,
    /// Refresh tokens in the order they were presented.
    refreshes: Vec<String>,
    revoked: Vec<String>,
}

struct FakeIdp {
    issuer: String,
    seen: Arc<Mutex<Seen>>,
    /// Refresh requests answered by dropping the connection (the answer is lost).
    drop_refreshes: Arc<AtomicUsize>,
    /// Refresh requests answered with a server error (the token may have been used).
    fail_refreshes: Arc<AtomicUsize>,
}

fn jwt(claims: Value) -> String {
    format!("{}.{}.sig", URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#), URL_SAFE_NO_PAD.encode(claims.to_string()))
}

fn query(url: &str) -> HashMap<String, String> {
    form_urlencoded::parse(url.split_once('?').map(|x| x.1).unwrap_or("").as_bytes()).into_owned().collect()
}

impl FakeIdp {
    fn start(redirect: Redirect) -> FakeIdp {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let issuer = format!("http://127.0.0.1:{port}/realms/market");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (drop_refreshes, fail_refreshes) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (iss, s, drops, fails) = (issuer.clone(), seen.clone(), drop_refreshes.clone(), fail_refreshes.clone());
        std::thread::spawn(move || {
            let mut serial = 0;
            for mut rq in server.incoming_requests() {
                let url = rq.url().to_string();
                let mut body = String::new();
                let _ = rq.as_reader().read_to_string(&mut body);
                let form: HashMap<String, String> = form_urlencoded::parse(body.as_bytes()).into_owned().collect();
                let reply = |status: u16, v: Value| tiny_http::Response::from_string(v.to_string()).with_status_code(status);
                let path = url.split('?').next().unwrap().to_string();
                if path.ends_with("/protocol/openid-connect/auth") {
                    let q = query(&url);
                    assert_eq!(q["client_id"], BROWSER_CLIENT);
                    assert_eq!(q["code_challenge_method"], "S256");
                    assert_eq!(q["response_type"], "code");
                    assert!(q["scope"].split(' ').any(|x| x == "offline_access"));
                    let code = format!("code-{}", uuid::Uuid::new_v4());
                    {
                        let mut s = s.lock().unwrap();
                        s.challenge = q["code_challenge"].clone();
                        s.redirect_uri = q["redirect_uri"].clone();
                        s.nonce = q["nonce"].clone();
                        s.code = code.clone();
                    }
                    let mut to = reqwest::Url::parse(&q["redirect_uri"]).unwrap();
                    {
                        let mut qp = to.query_pairs_mut();
                        match redirect {
                            Redirect::Denied => qp.append_pair("error", "access_denied"),
                            _ => qp.append_pair("code", &code),
                        };
                        qp.append_pair("state", &q["state"]);
                        match redirect {
                            Redirect::WrongIss => drop(qp.append_pair("iss", "https://evil.example/realms/market")),
                            Redirect::NoIss => {}
                            _ => drop(qp.append_pair("iss", &iss)),
                        }
                    }
                    let resp = tiny_http::Response::empty(302)
                        .with_header(tiny_http::Header::from_bytes(&b"Location"[..], to.as_str().as_bytes()).unwrap());
                    let _ = rq.respond(resp);
                } else if path.ends_with("/protocol/openid-connect/token") {
                    serial += 1;
                    let tokens = |n: u32, nonce: &str| {
                        json!({"access_token": format!("access-{n}"), "token_type": "Bearer", "expires_in": 300, "refresh_token": format!("refresh-{n}"),
                            "id_token": jwt(json!({"iss": iss, "aud": BROWSER_CLIENT, "azp": BROWSER_CLIENT, "nonce": nonce}))})
                    };
                    match form.get("grant_type").map(String::as_str) {
                        Some("authorization_code") => {
                            let s = s.lock().unwrap();
                            let pkce = URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes()));
                            let ok = form["code"] == s.code
                                && pkce == s.challenge
                                && form["redirect_uri"] == s.redirect_uri
                                && form["client_id"] == BROWSER_CLIENT;
                            let _ = rq.respond(if ok {
                                reply(200, tokens(serial, &s.nonce))
                            } else {
                                reply(400, json!({"error": "invalid_grant"}))
                            });
                        }
                        Some("refresh_token") => {
                            let rt = form["refresh_token"].clone();
                            let mut st = s.lock().unwrap();
                            let reused = st.refreshes.contains(&rt) || st.revoked.contains(&rt);
                            st.refreshes.push(rt);
                            drop(st);
                            if drops.load(Ordering::SeqCst) > 0 {
                                drops.fetch_sub(1, Ordering::SeqCst);
                                drop(rq.into_writer()); // read, then close without an answer
                                continue;
                            }
                            if fails.load(Ordering::SeqCst) > 0 {
                                fails.fetch_sub(1, Ordering::SeqCst);
                                let _ = rq.respond(reply(502, json!({})));
                                continue;
                            }
                            let _ = rq.respond(if reused {
                                reply(400, json!({"error": "invalid_grant"}))
                            } else {
                                reply(200, tokens(serial, ""))
                            });
                        }
                        _ => drop(rq.respond(reply(400, json!({"error": "unsupported_grant_type"})))),
                    }
                } else if path.ends_with("/protocol/openid-connect/revoke") {
                    s.lock().unwrap().revoked.push(form["token"].clone());
                    let _ = rq.respond(tiny_http::Response::empty(200));
                } else {
                    let _ = rq.respond(tiny_http::Response::empty(404));
                }
            }
        });
        FakeIdp { issuer, seen, drop_refreshes, fail_refreshes }
    }

    fn endpoints(&self) -> Endpoints {
        Endpoints { issuer: self.issuer.clone(), whoami: format!("{}/nope", self.issuer), client_id: BROWSER_CLIENT.into() }
    }
}

/// A raw HTTP/1.1 GET (no redirects followed): status, Location, body.
fn get(url: &str, host_header: Option<&str>) -> std::io::Result<(u16, Option<String>, String)> {
    let u = reqwest::Url::parse(url).unwrap();
    let addr = format!("{}:{}", u.host_str().unwrap(), u.port().unwrap());
    let mut s = TcpStream::connect(&addr)?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let target = match u.query() {
        Some(q) => format!("{}?{q}", u.path()),
        None => u.path().to_string(),
    };
    write!(s, "GET {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", host_header.unwrap_or(&addr))?;
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    let status = out.split(' ').nth(1).and_then(|x| x.parse().ok()).unwrap_or(0);
    let location = out.lines().find_map(|l| l.strip_prefix("Location: ").or_else(|| l.strip_prefix("location: "))).map(String::from);
    let body = out.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default();
    Ok((status, location, body))
}

/// What the browser saw at the loopback listener: status, page, the address it was sent to.
type Seen3 = std::sync::mpsc::Receiver<(u16, String, String)>;

/// A browser that follows the authorization redirect to the loopback listener; what it saw there is sent back.
fn browser(before: impl FnOnce(&str) + Send + 'static) -> (impl FnOnce(&str) -> Result<()>, Seen3) {
    let (tx, rx) = std::sync::mpsc::channel();
    let open = move |url: &str| {
        let url = url.to_string();
        std::thread::spawn(move || {
            let (_, loc, _) = get(&url, None).unwrap();
            let loc = loc.expect("the authorization server redirects");
            before(&loc);
            let (status, _, body) = get(&loc, None).unwrap();
            let _ = tx.send((status, body, loc));
        });
        Ok(())
    };
    (open, rx)
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap()
}

#[test]
fn signs_in_with_pkce_and_keeps_only_the_refresh_token() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let (open, seen) = browser(|_| {});
    rt().block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
    let (status, page, callback) = seen.recv().unwrap();
    assert_eq!(status, 200);
    // The redirect went to 127.0.0.1 on /callback, and the page shows neither the code nor a token.
    assert!(callback.starts_with("http://127.0.0.1:") && callback.contains("/callback?"), "{callback}");
    let code = query(&callback)["code"].clone();
    assert!(!page.contains(&code) && !page.contains("access-") && !page.contains("refresh-"), "{page}");
    assert!(page.contains("signed in"));
    assert_eq!(store.get(&ep.store_key()).unwrap().as_deref(), Some("refresh-1"));
    assert_eq!(store.0.lock().unwrap().len(), 1, "only the refresh token is stored");
    // The verifier matched the S256 challenge (the fake refuses the code otherwise), and the redirect is exact.
    assert!(idp.seen.lock().unwrap().redirect_uri.starts_with("http://127.0.0.1:"));
}

#[test]
fn a_second_request_finds_the_listener_closed() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let (open, seen) = browser(|_| {});
    rt().block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
    let (_, _, callback) = seen.recv().unwrap();
    // The same response again (a replay, or a second tab): nothing listens any more.
    std::thread::sleep(Duration::from_millis(200));
    assert!(get(&callback, None).is_err(), "the listener still answers after the sign-in");
}

#[test]
fn a_wrong_state_neither_completes_nor_cancels() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let (tx, rx) = std::sync::mpsc::channel();
    let open = move |url: &str| {
        let url = url.to_string();
        std::thread::spawn(move || {
            let (_, loc, _) = get(&url, None).unwrap();
            let loc = loc.unwrap();
            // Forged: the right code, another state; then a request with no state; then another path.
            let q = query(&loc);
            let base = loc.split('?').next().unwrap().to_string();
            let forged = format!(
                "{base}?code={}&state=forged&iss={}",
                q["code"],
                form_urlencoded::byte_serialize(q["iss"].as_bytes()).collect::<String>()
            );
            let a = get(&forged, None).unwrap();
            let b = get(&format!("{base}?code={}", q["code"]), None).unwrap();
            let c = get(&loc.replace("/callback", "/other"), None).unwrap();
            // Repeated parameters are refused too.
            let d = get(&format!("{loc}&state=x"), None).unwrap();
            let _ = tx.send((a, b, c, d));
        });
        Ok(())
    };
    let err = rt().block_on(login_browser_with(&ep, &store, open, Duration::from_secs(3))).unwrap_err();
    let (a, b, c, d) = rx.recv().unwrap();
    assert_eq!((a.0, b.0, c.0, d.0), (400, 400, 404, 400));
    assert!(!a.2.contains("code-"));
    assert!(format!("{err:#}").contains("not completed"), "{err:#}");
    assert!(store.get(&ep.store_key()).unwrap().is_none());
}

#[test]
fn a_wrong_or_missing_issuer_stops_the_sign_in() {
    for redirect in [Redirect::WrongIss, Redirect::NoIss] {
        let idp = FakeIdp::start(redirect);
        let (ep, store) = (idp.endpoints(), Memory::default());
        let (open, seen) = browser(|_| {});
        let err = rt().block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap_err();
        assert!(format!("{err:#}").contains("issuer"), "{err:#}");
        let (status, page, callback) = seen.recv().unwrap();
        assert_eq!(status, 400);
        assert!(!page.contains(&query(&callback)["code"]));
        assert!(store.get(&ep.store_key()).unwrap().is_none());
        // The code was never redeemed.
        assert!(idp.seen.lock().unwrap().refreshes.is_empty());
    }
}

#[test]
fn a_declined_sign_in_stops() {
    let idp = FakeIdp::start(Redirect::Denied);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let (open, _seen) = browser(|_| {});
    let err = rt().block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap_err();
    assert!(format!("{err:#}").contains("access_denied"), "{err:#}");
}

#[test]
fn the_wait_ends_after_the_timeout() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let started = Instant::now();
    let err = rt().block_on(login_browser_with(&ep, &store, |_| Ok(()), Duration::from_secs(1))).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(format!("{err:#}").contains("sign-in was not completed"), "{err:#}");
}

#[test]
fn no_browser_still_works_from_the_printed_address() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    // The opener fails; someone opens the address by hand.
    let (manual, seen) = browser(|_| {});
    let open = move |url: &str| {
        manual(url).unwrap();
        bail!("no browser")
    };
    rt().block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
    assert_eq!(seen.recv().unwrap().0, 200);
}

#[test]
fn judge_refuses_other_hosts_and_methods() {
    let (state, iss) = ("s", "https://auth.actuaria.io/realms/market");
    let ok = "/callback?code=c&state=s&iss=https%3A%2F%2Fauth.actuaria.io%2Frealms%2Fmarket";
    assert_eq!(judge("GET", ok, Some("127.0.0.1:5000"), 5000, state, iss), Verdict::Code("c".into()));
    for host in [None, Some("localhost:5000"), Some("evil.example:5000"), Some("127.0.0.1:5001")] {
        assert!(matches!(judge("GET", ok, host, 5000, state, iss), Verdict::Ignore(400, _)), "{host:?}");
    }
    assert!(matches!(judge("POST", ok, Some("127.0.0.1:5000"), 5000, state, iss), Verdict::Ignore(405, _)));
    let two_codes = format!("{ok}&code=d");
    assert!(matches!(judge("GET", &two_codes, Some("127.0.0.1:5000"), 5000, state, iss), Verdict::Fail(_)));
}

#[test]
fn pkce_verifier_and_challenge() {
    let (v, c) = pkce_pair();
    assert_eq!(v.len(), 43);
    assert_eq!(c, URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes())));
    assert_ne!(pkce_pair().0, v);
}

/// Sign in, then refresh: the token rotates, a stale one is never sent, and `invalid_grant` signs out.
#[test]
fn refresh_rotates_and_invalid_grant_signs_out() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let (open, _seen) = browser(|_| {});
    let rt = rt();
    rt.block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
    // A cached access token is used without a refresh.
    assert_eq!(rt.block_on(access_token(&ep, &store)).unwrap(), "access-1");
    assert!(idp.seen.lock().unwrap().refreshes.is_empty());
    // Expired in this process: a refresh, which rotates the stored token.
    forget(&ep);
    let a = rt.block_on(access_token(&ep, &store)).unwrap();
    assert_eq!(a, "access-2");
    assert_eq!(store.get(&ep.store_key()).unwrap().as_deref(), Some("refresh-2"));
    // Another process signed out meanwhile: this one notices instead of using its cache.
    store.delete(&ep.store_key()).unwrap();
    assert!(rt.block_on(access_token(&ep, &store)).unwrap_err().is::<NotSignedIn>());
    // A replayed (stale) token is refused: the store is cleared and the person told.
    store.set(&ep.store_key(), "refresh-1").unwrap();
    forget(&ep);
    let err = rt.block_on(access_token(&ep, &store)).unwrap_err();
    assert!(format!("{err:#}").contains("signed out"), "{err:#}");
    assert!(store.get(&ep.store_key()).unwrap().is_none());
}

#[test]
fn a_refresh_whose_answer_is_lost_is_never_resent() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let (open, _seen) = browser(|_| {});
    let rt = rt();
    rt.block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
    forget(&ep);
    idp.drop_refreshes.store(1, Ordering::SeqCst);
    let err = rt.block_on(access_token(&ep, &store)).unwrap_err();
    assert!(format!("{err:#}").contains("never sends a refresh token twice"), "{err:#}");
    assert!(store.get(&ep.store_key()).unwrap().is_none());
    // Asking again sends nothing: there is no token left to send.
    assert!(rt.block_on(access_token(&ep, &store)).unwrap_err().is::<NotSignedIn>());
    assert_eq!(idp.seen.lock().unwrap().refreshes, ["refresh-1"]);
}

#[test]
fn a_refresh_that_fails_at_the_server_is_never_resent() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let (open, _seen) = browser(|_| {});
    let rt = rt();
    rt.block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
    forget(&ep);
    idp.fail_refreshes.store(1, Ordering::SeqCst);
    let err = rt.block_on(access_token(&ep, &store)).unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 502"), "{err:#}");
    assert!(store.get(&ep.store_key()).unwrap().is_none());
    assert!(rt.block_on(access_token(&ep, &store)).unwrap_err().is::<NotSignedIn>());
    assert_eq!(idp.seen.lock().unwrap().refreshes, ["refresh-1"]);
}

#[test]
fn refresh_is_serialised_across_callers() {
    let idp = FakeIdp::start(Redirect::Good);
    let ep = idp.endpoints();
    let store = Arc::new(Memory::default());
    let (open, _seen) = browser(|_| {});
    rt().block_on(login_browser_with(&ep, store.as_ref(), open, Duration::from_secs(20))).unwrap();
    // Eight callers with no cached access token at once. Each holds the lock while it refreshes, so none
    // presents a token another already rotated (the fake answers a reused one with invalid_grant).
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (ep, store) = (ep.clone(), store.clone());
            std::thread::spawn(move || {
                forget(&ep);
                rt().block_on(access_token(&ep, store.as_ref()))
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap().unwrap();
    }
    let seen = idp.seen.lock().unwrap();
    let mut unique = seen.refreshes.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), seen.refreshes.len(), "a refresh token was sent twice: {:?}", seen.refreshes);
}

#[test]
fn logout_revokes_and_clears() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let (open, _seen) = browser(|_| {});
    let rt = rt();
    rt.block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
    assert!(rt.block_on(logout_with(&ep, &store)).unwrap());
    assert_eq!(idp.seen.lock().unwrap().revoked, ["refresh-1"]);
    assert!(store.get(&ep.store_key()).unwrap().is_none());
    assert!(rt.block_on(access_token(&ep, &store)).unwrap_err().is::<NotSignedIn>());
    // Signing out twice is fine.
    assert!(!rt.block_on(logout_with(&ep, &store)).unwrap());
}

#[test]
fn release_builds_pin_the_issuer() {
    // In this (debug) test build the override is honoured; the constants are what a release build uses.
    assert_eq!(ISSUER, "https://auth.actuaria.io/realms/market");
    assert_eq!(BROWSER_CLIENT, "essentials-desktop");
    let e = Endpoints { issuer: ISSUER.into(), whoami: WHOAMI_URL.into(), client_id: BROWSER_CLIENT.into() };
    assert_eq!(e.endpoint("token"), "https://auth.actuaria.io/realms/market/protocol/openid-connect/token");
}
