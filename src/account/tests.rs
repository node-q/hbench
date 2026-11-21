//! The browser sign-in against a fake authorization server: PKCE, the loopback listener's refusals (wrong
//! `state`, wrong `iss`, a second request, the timeout), refresh that is serialised and never resent, and
//! sign-out. Every token and revocation request must carry a DPoP proof that the fake checks (signature against
//! its JWK, `htm`, `htu`, a fresh `jti`), and a refresh must prove the key its token was bound to.

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
    /// The key thumbprint of each accepted request, with its grant (or `revoke`).
    proofs: Vec<(String, String)>,
    /// Refresh tokens and the key thumbprint each is bound to.
    bound: HashMap<String, String>,
    jtis: Vec<String>,
    /// Proofs that carried the fake's nonce.
    nonced: usize,
}

/// Check a DPoP proof as Keycloak does: `typ`, `alg`, an EC P-256 `jwk`, the ES256 signature against that JWK,
/// `htm`, `htu`, `iat` within a minute, and a `jti`. Its claims and the JWK thumbprint (RFC 7638).
fn check_proof(proof: &str, htm: &str, htu: &str) -> std::result::Result<(Value, String), String> {
    use p256::ecdsa::signature::Verifier;
    let parts: Vec<&str> = proof.split('.').collect();
    let [h, c, sig] = parts[..] else { return Err("not a JWS".into()) };
    let decode = |x: &str| -> std::result::Result<Value, String> {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(x).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    };
    let (header, claims) = (decode(h)?, decode(c)?);
    if header["typ"] != "dpop+jwt" || header["alg"] != "ES256" {
        return Err(format!("header {header}"));
    }
    let jwk = &header["jwk"];
    if jwk["kty"] != "EC" || jwk["crv"] != "P-256" || jwk.get("d").is_some() {
        return Err(format!("jwk {jwk}"));
    }
    let coord = |n: &str| URL_SAFE_NO_PAD.decode(jwk[n].as_str().unwrap_or_default()).unwrap_or_default();
    let point = [vec![4u8], coord("x"), coord("y")].concat();
    let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&point).map_err(|_| "a bad public key")?;
    let sig =
        p256::ecdsa::Signature::from_slice(&URL_SAFE_NO_PAD.decode(sig).map_err(|e| e.to_string())?).map_err(|_| "a bad signature")?;
    vk.verify(format!("{h}.{c}").as_bytes(), &sig).map_err(|_| "the signature does not verify")?;
    if claims["htm"] != htm || claims["htu"] != htu {
        return Err(format!("htm/htu {} {}, wanted {htm} {htu}", claims["htm"], claims["htu"]));
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    if claims["iat"].as_u64().is_none_or(|iat| iat.abs_diff(now) > 60) || claims["jti"].as_str().is_none_or(|j| j.len() < 16) {
        return Err(format!("claims {claims}"));
    }
    let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#, jwk["x"].as_str().unwrap(), jwk["y"].as_str().unwrap());
    Ok((claims, URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))))
}

struct FakeIdp {
    issuer: String,
    seen: Arc<Mutex<Seen>>,
    /// Refresh requests answered by dropping the connection (the answer is lost).
    drop_refreshes: Arc<AtomicUsize>,
    /// Refresh requests answered with a server error (the token may have been used).
    fail_refreshes: Arc<AtomicUsize>,
    /// Token requests refused with `invalid_dpop_proof`, before the grant is looked at.
    refuse_proofs: Arc<AtomicUsize>,
    /// Token requests without the fake's nonce answered `use_dpop_nonce`.
    want_nonce: Arc<AtomicUsize>,
}

const NONCE: &str = "fake-dpop-nonce";

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
        let (refuse_proofs, want_nonce) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (iss, s, drops, fails) = (issuer.clone(), seen.clone(), drop_refreshes.clone(), fail_refreshes.clone());
        let (refuses, nonces) = (refuse_proofs.clone(), want_nonce.clone());
        std::thread::spawn(move || {
            let mut serial = 0;
            for mut rq in server.incoming_requests() {
                let url = rq.url().to_string();
                let mut body = String::new();
                let _ = rq.as_reader().read_to_string(&mut body);
                let form: HashMap<String, String> = form_urlencoded::parse(body.as_bytes()).into_owned().collect();
                let reply = |status: u16, v: Value| tiny_http::Response::from_string(v.to_string()).with_status_code(status);
                let path = url.split('?').next().unwrap().to_string();
                // The DPoP proof of a token or revocation request: checked, and its key's thumbprint kept.
                let mut jkt = String::new();
                if path.ends_with("/token") || path.ends_with("/revoke") {
                    let dpop: Vec<String> =
                        rq.headers().iter().filter(|h| h.field.equiv("DPoP")).map(|h| h.value.as_str().to_string()).collect();
                    let checked = match &dpop[..] {
                        [one] => check_proof(one, "POST", &format!("http://127.0.0.1:{port}{path}")),
                        _ => Err(format!("{} DPoP headers", dpop.len())),
                    };
                    let refused = |why: String| {
                        tiny_http::Response::from_string(json!({"error": "invalid_dpop_proof", "error_description": why}).to_string())
                            .with_status_code(400)
                    };
                    let (claims, thumbprint) = match checked {
                        Ok(x) => x,
                        Err(why) => {
                            let _ = rq.respond(refused(why));
                            continue;
                        }
                    };
                    let jti = claims["jti"].as_str().unwrap().to_string();
                    let mut st = s.lock().unwrap();
                    if st.jtis.contains(&jti) {
                        drop(st);
                        let _ = rq.respond(refused("a jti used before".into()));
                        continue;
                    }
                    st.jtis.push(jti);
                    let nonced = claims["nonce"] == NONCE;
                    st.nonced += usize::from(nonced);
                    drop(st);
                    if !nonced && nonces.load(Ordering::SeqCst) > 0 {
                        nonces.fetch_sub(1, Ordering::SeqCst);
                        let resp = tiny_http::Response::from_string(json!({"error": "use_dpop_nonce"}).to_string())
                            .with_status_code(400)
                            .with_header(tiny_http::Header::from_bytes(&b"DPoP-Nonce"[..], NONCE.as_bytes()).unwrap());
                        let _ = rq.respond(resp);
                        continue;
                    }
                    if refuses.load(Ordering::SeqCst) > 0 {
                        refuses.fetch_sub(1, Ordering::SeqCst);
                        let _ = rq.respond(refused("refused by the test".into()));
                        continue;
                    }
                    jkt = thumbprint;
                }
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
                            let mut s = s.lock().unwrap();
                            s.proofs.push(("authorization_code".into(), jkt.clone()));
                            s.bound.insert(format!("refresh-{serial}"), jkt.clone());
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
                            st.proofs.push(("refresh_token".into(), jkt.clone()));
                            // A refresh token bound to a key is refused with any other key's proof.
                            let other_key = st.bound.get(&rt).is_some_and(|b| *b != jkt);
                            st.refreshes.push(rt);
                            if !reused && !other_key {
                                st.bound.insert(format!("refresh-{serial}"), jkt.clone());
                            }
                            drop(st);
                            if other_key {
                                let _ = rq.respond(reply(400, json!({"error": "invalid_dpop_proof"})));
                                continue;
                            }
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
                    let mut st = s.lock().unwrap();
                    st.revoked.push(form["token"].clone());
                    st.proofs.push(("revoke".into(), jkt.clone()));
                    drop(st);
                    let _ = rq.respond(tiny_http::Response::empty(200));
                } else {
                    let _ = rq.respond(tiny_http::Response::empty(404));
                }
            }
        });
        FakeIdp { issuer, seen, drop_refreshes, fail_refreshes, refuse_proofs, want_nonce }
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
    let stored: Vec<String> = store.0.lock().unwrap().keys().cloned().collect();
    assert_eq!(stored.len(), 2, "only the refresh token and its DPoP key are stored: {stored:?}");
    let pem = store.get(&dpop::store_key(&ep.store_key())).unwrap().unwrap();
    assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"), "PKCS#8");
    // The code was redeemed with a proof of that key.
    let key = dpop::Key::load(&store, &ep.store_key()).unwrap().unwrap();
    assert_eq!(idp.seen.lock().unwrap().proofs, [("authorization_code".to_string(), key.thumbprint())]);
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

/// A keychain whose writes fail (locked, full, or denied), and whose deletes may fail too.
struct BrokenWrites {
    inner: Memory,
    deletes_fail: bool,
}

impl Store for BrokenWrites {
    fn get(&self, key: &str) -> Result<Option<String>> {
        self.inner.get(key)
    }
    fn set(&self, _: &str, _: &str) -> Result<()> {
        bail!("the keychain refused the write")
    }
    fn delete(&self, key: &str) -> Result<()> {
        if self.deletes_fail { bail!("the keychain refused the delete") } else { self.inner.delete(key) }
    }
}

/// The rotation consumed the old token: when the new one cannot be saved, the old one is not kept to be replayed.
#[test]
fn a_rotated_token_that_cannot_be_saved_is_not_replayed() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, memory) = (idp.endpoints(), Memory::default());
    let (open, _seen) = browser(|_| {});
    let rt = rt();
    rt.block_on(login_browser_with(&ep, &memory, open, Duration::from_secs(20))).unwrap();
    forget(&ep);
    let store = BrokenWrites { inner: memory, deletes_fail: false };
    let err = rt.block_on(access_token(&ep, &store)).unwrap_err();
    assert!(format!("{err:#}").contains("signed out"), "{err:#}");
    assert!(store.get(&ep.store_key()).unwrap().is_none());
    assert!(rt.block_on(access_token(&ep, &store)).unwrap_err().is::<NotSignedIn>());
    assert_eq!(idp.seen.lock().unwrap().refreshes, ["refresh-1"]);
}

/// When neither the new token can be saved nor the old one deleted, the in-flight marker stays, and the next
/// refresh signs out instead of sending the old token again.
#[test]
fn a_token_left_in_the_keychain_after_use_is_never_resent() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, memory) = (idp.endpoints(), Memory::default());
    let (open, _seen) = browser(|_| {});
    let rt = rt();
    rt.block_on(login_browser_with(&ep, &memory, open, Duration::from_secs(20))).unwrap();
    forget(&ep);
    let store = BrokenWrites { inner: memory, deletes_fail: true };
    assert!(rt.block_on(access_token(&ep, &store)).is_err());
    assert_eq!(store.get(&ep.store_key()).unwrap().as_deref(), Some("refresh-1"), "the keychain kept it");
    // The keychain works again: the marked token is deleted, not sent.
    let store = store.inner;
    let err = rt.block_on(access_token(&ep, &store)).unwrap_err();
    assert!(format!("{err:#}").contains("interrupted"), "{err:#}");
    assert!(store.get(&ep.store_key()).unwrap().is_none());
    assert_eq!(idp.seen.lock().unwrap().refreshes, ["refresh-1"]);
}

/// A process killed between sending a refresh and keeping its answer leaves the marker: the next process signs
/// out instead of resending. A new sign-in clears it.
#[test]
fn a_refresh_killed_in_flight_is_never_resent() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let (open, _seen) = browser(|_| {});
    let rt = rt();
    rt.block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
    forget(&ep);
    // What a process killed after the request was sent leaves behind.
    refresh_lock(&ep).unwrap().mark("refresh-1").unwrap();
    let err = rt.block_on(access_token(&ep, &store)).unwrap_err();
    assert!(format!("{err:#}").contains("interrupted"), "{err:#}");
    assert!(store.get(&ep.store_key()).unwrap().is_none());
    assert!(idp.seen.lock().unwrap().refreshes.is_empty(), "nothing was sent");
    // A marker for another token (a stale one) does not stop a refresh.
    let (open, _seen) = browser(|_| {});
    rt.block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
    refresh_lock(&ep).unwrap().mark("some-older-token").unwrap();
    forget(&ep);
    rt.block_on(access_token(&ep, &store)).unwrap();
    assert_eq!(idp.seen.lock().unwrap().refreshes.len(), 1);
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
    let jkt = dpop::Key::load(&store, &ep.store_key()).unwrap().unwrap().thumbprint();
    assert!(rt.block_on(logout_with(&ep, &store)).unwrap());
    assert_eq!(idp.seen.lock().unwrap().revoked, ["refresh-1"]);
    // The revocation carried a proof of the sign-in's key, and the key is gone with the token.
    assert_eq!(idp.seen.lock().unwrap().proofs.last().unwrap(), &("revoke".to_string(), jkt));
    assert!(store.0.lock().unwrap().is_empty(), "the refresh token and the DPoP key are deleted");
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

/// The refresh proves the key the code was redeemed with, and the rotated token stays bound to it.
#[test]
fn refreshes_prove_the_sign_in_key() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let (open, _seen) = browser(|_| {});
    let rt = rt();
    rt.block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
    let jkt = dpop::Key::load(&store, &ep.store_key()).unwrap().unwrap().thumbprint();
    for want in ["access-2", "access-3"] {
        forget(&ep);
        assert_eq!(rt.block_on(access_token(&ep, &store)).unwrap(), want);
    }
    let seen = idp.seen.lock().unwrap();
    assert_eq!(seen.proofs.len(), 3);
    assert!(seen.proofs.iter().all(|(_, k)| *k == jkt), "{:?}", seen.proofs);
    assert_eq!(seen.bound["refresh-3"], jkt);
}

/// A second sign-in makes a new key; a refresh token proved with another key is refused by the server.
#[test]
fn each_sign_in_has_its_own_key() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let rt = rt();
    let mut keys = vec![];
    for _ in 0..2 {
        let (open, _seen) = browser(|_| {});
        rt.block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
        keys.push(dpop::Key::load(&store, &ep.store_key()).unwrap().unwrap().thumbprint());
    }
    assert_ne!(keys[0], keys[1]);
    // The first sign-in's token with the second sign-in's key: refused as a bad proof, and kept.
    store.set(&ep.store_key(), "refresh-1").unwrap();
    forget(&ep);
    let err = rt.block_on(access_token(&ep, &store)).unwrap_err();
    assert!(format!("{err:#}").contains("invalid_dpop_proof"), "{err:#}");
}

/// Without its key the stored refresh token can't be used: hbench signs out cleanly and sends nothing.
#[test]
fn a_missing_key_signs_out_without_sending_the_token() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let (open, _seen) = browser(|_| {});
    let rt = rt();
    rt.block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
    for damage in [None, Some("not a key")] {
        match damage {
            None => store.delete(&dpop::store_key(&ep.store_key())).unwrap(),
            Some(junk) => {
                store.set(&ep.store_key(), "refresh-1").unwrap();
                store.set(&dpop::store_key(&ep.store_key()), junk).unwrap();
            }
        }
        forget(&ep);
        let err = rt.block_on(access_token(&ep, &store)).unwrap_err();
        assert!(format!("{err:#}").contains("signed out: the sign-in's key is missing"), "{err:#}");
        assert!(store.get(&ep.store_key()).unwrap().is_none());
        assert!(rt.block_on(access_token(&ep, &store)).unwrap_err().is::<NotSignedIn>());
    }
    assert!(idp.seen.lock().unwrap().refreshes.is_empty(), "nothing was sent");
    // Signing out still works, and takes the damaged key away.
    assert!(!rt.block_on(logout_with(&ep, &store)).unwrap());
    assert!(store.0.lock().unwrap().is_empty());
}

/// A refused proof (`invalid_dpop_proof`) is checked before the grant: the token was not used, so it is kept,
/// and the error says so. The next refresh works.
#[test]
fn a_refused_proof_keeps_the_sign_in() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let (open, _seen) = browser(|_| {});
    let rt = rt();
    rt.block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
    forget(&ep);
    idp.refuse_proofs.store(1, Ordering::SeqCst);
    let err = rt.block_on(access_token(&ep, &store)).unwrap_err();
    assert!(format!("{err:#}").contains("invalid_dpop_proof") && format!("{err:#}").contains("kept"), "{err:#}");
    assert_eq!(store.get(&ep.store_key()).unwrap().as_deref(), Some("refresh-1"));
    assert!(dpop::Key::load(&store, &ep.store_key()).unwrap().is_some());
    assert_eq!(rt.block_on(access_token(&ep, &store)).unwrap(), "access-2");
}

/// A `use_dpop_nonce` answer is retried once with the server's `DPoP-Nonce`, for the code and for a refresh.
#[test]
fn the_servers_nonce_is_honoured() {
    let idp = FakeIdp::start(Redirect::Good);
    let (ep, store) = (idp.endpoints(), Memory::default());
    let (open, _seen) = browser(|_| {});
    let rt = rt();
    idp.want_nonce.store(1, Ordering::SeqCst);
    rt.block_on(login_browser_with(&ep, &store, open, Duration::from_secs(20))).unwrap();
    forget(&ep);
    idp.want_nonce.store(1, Ordering::SeqCst);
    assert_eq!(rt.block_on(access_token(&ep, &store)).unwrap(), "access-2");
    assert_eq!(idp.seen.lock().unwrap().nonced, 2);
    // The refused first attempt used nothing: one refresh token presented, once.
    assert_eq!(idp.seen.lock().unwrap().refreshes, ["refresh-1"]);
}

#[test]
fn a_proof_is_an_es256_jwt_for_one_request() {
    let key = dpop::Key::generate();
    let p = key.proof("POST", "https://auth.example/realms/market/protocol/openid-connect/token?x=1#f", Some("n"));
    let (claims, jkt) = check_proof(&p, "POST", "https://auth.example/realms/market/protocol/openid-connect/token").unwrap();
    assert_eq!((claims["nonce"].as_str(), jkt), (Some("n"), key.thumbprint()));
    assert!(claims.get("ath").is_none());
    assert_ne!(key.proof("POST", "https://a.example/t", None), key.proof("POST", "https://a.example/t", None), "a fresh jti");
    // The private key shows in neither the proof's header nor `Debug`.
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(p.split('.').next().unwrap()).unwrap()).unwrap();
    assert!(header["jwk"].get("d").is_none());
    assert_eq!(format!("{key:?}"), format!("dpop::Key({})", key.thumbprint()));
}
