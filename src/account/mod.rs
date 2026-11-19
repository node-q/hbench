//! Signing in to the Actuaria market: `hbench login --browser`, `hbench logout`, `hbench whoami`.
//!
//! The browser flow is the authorization code grant with PKCE (S256) and a loopback redirect, as RFC 8252
//! says, for the public client `essentials-desktop` (hbench-studio docs/design/sign-in-products.md section 7,
//! D133). The refresh token lives in the OS keychain and nowhere else (D120); refresh is serialised across
//! processes and a refresh token is never sent twice (market-idp.md I-5).
//!
//! The issuer, the client id and the redirect URI are pinned in released builds. A debug build (a developer's
//! or a test's) reads `HBENCH_DEV_MARKET_ISSUER` and `HBENCH_DEV_MARKET_API`; a release build has no way to
//! change them at run time.

pub mod loopback;
pub mod store;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use store::Store;

/// The market's issuer (market-idp.md section 3). Hard to reverse: released clients pin it.
pub const ISSUER: &str = "https://auth.actuaria.io/realms/market";
/// The public client of the browser flow (sign-in-products.md 7.1). The device flow's `hbench-cli` is not this.
pub const BROWSER_CLIENT: &str = "essentials-desktop";
/// The market API's `whoami`. The market API is not built yet (node-q/actuaria-registry); this address is a
/// placeholder until its contract names one.
pub const WHOAMI_URL: &str = "https://api.actuaria.io/v1/whoami";
/// Scopes of the browser flow. `organization:<alias>` is added when a company is chosen (5.5), not built yet.
const SCOPES: &str = "openid offline_access";
/// How long the loopback listener waits for the browser.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const HTTP_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 3 } else { 30 });
/// An access token this close to its expiry is refreshed instead of used.
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);

/// Where sign-in talks to: the pinned issuer and market API, or a debug build's override.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub issuer: String,
    pub whoami: String,
    pub client_id: String,
}

impl Endpoints {
    /// The pinned endpoints. Only a debug build (`cfg(debug_assertions)`, which is what `cargo build`, `cargo
    /// test` and `npm run dev` make) reads the two development variables; a release build ignores them.
    pub fn pinned() -> Endpoints {
        let mut e = Endpoints { issuer: ISSUER.into(), whoami: WHOAMI_URL.into(), client_id: BROWSER_CLIENT.into() };
        if cfg!(debug_assertions) {
            if let Some(i) = std::env::var("HBENCH_DEV_MARKET_ISSUER").ok().filter(|v| !v.is_empty()) {
                eprintln!("(debug build) sign-in issuer: {i}");
                e.issuer = i.trim_end_matches('/').to_string();
            }
            if let Some(w) = std::env::var("HBENCH_DEV_MARKET_API").ok().filter(|v| !v.is_empty()) {
                e.whoami = format!("{}/v1/whoami", w.trim_end_matches('/'));
            }
        }
        e
    }

    /// Keycloak's endpoints sit under the realm's issuer; they are derived, never discovered, so nothing fetched
    /// at run time can point hbench elsewhere.
    fn endpoint(&self, name: &str) -> String {
        format!("{}/protocol/openid-connect/{name}", self.issuer)
    }

    /// The keychain entry's account name: one entry per issuer and client.
    fn store_key(&self) -> String {
        format!("{}@{}", self.client_id, self.issuer)
    }
}

/// `n` random bytes, base64url without padding.
fn random_b64(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("the OS random source failed");
    URL_SAFE_NO_PAD.encode(b)
}

/// PKCE (RFC 7636): a 43-character verifier from 32 random bytes and its S256 challenge.
pub fn pkce_pair() -> (String, String) {
    let verifier = random_b64(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// What one sign-in attempt carries between the browser and the token endpoint.
pub struct Attempt {
    pub verifier: String,
    pub state: String,
    pub nonce: String,
    pub redirect_uri: String,
    pub authorize_url: String,
}

impl Attempt {
    pub fn new(ep: &Endpoints, redirect_uri: &str) -> Attempt {
        let (verifier, challenge) = pkce_pair();
        let (state, nonce) = (random_b64(24), random_b64(24));
        let mut u = reqwest::Url::parse(&ep.endpoint("auth")).expect("the issuer is a URL");
        u.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &ep.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Attempt { verifier, state, nonce, redirect_uri: redirect_uri.into(), authorize_url: u.into() }
    }
}

fn http() -> Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::Client::builder()
        .user_agent(format!("hbench/{}", crate::build_info::VERSION))
        .timeout(HTTP_TIMEOUT)
        // The token endpoint never redirects; following one could send a code or a refresh token elsewhere.
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

fn form(pairs: &[(&str, &str)]) -> String {
    form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()
}

/// What happened to a request to the identity provider.
enum Sent {
    /// An answer, with its status and JSON body (`Null` when it was not JSON).
    Answered(u16, Value),
    /// The connection was never made, so nothing was sent: the token is still unused.
    Unsent(String),
    /// The request may have reached the server and its answer was lost.
    Lost(String),
}

async fn post_form(url: &str, body: String) -> Result<Sent> {
    let r = http()?.post(url).header("Content-Type", "application/x-www-form-urlencoded").body(body).send().await;
    Ok(match r {
        Ok(resp) => {
            let status = resp.status().as_u16();
            match resp.bytes().await {
                Ok(b) => Sent::Answered(status, serde_json::from_slice(&b).unwrap_or(Value::Null)),
                Err(e) => Sent::Lost(e.to_string()),
            }
        }
        Err(e) if e.is_connect() => Sent::Unsent(e.to_string()),
        Err(e) => Sent::Lost(e.to_string()),
    })
}

/// The `error` of an OAuth error body, when it is a plain code.
fn oauth_error(v: &Value) -> Option<&str> {
    v.get("error").and_then(Value::as_str).filter(|e| e.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
}

/// A JWT's claims, without checking its signature: only for an ID token received directly from the token
/// endpoint over TLS (OpenID Connect Core 3.1.3.7, item 6).
fn jwt_claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?).ok()
}

struct Tokens {
    access: String,
    refresh: Option<String>,
    expires_in: u64,
}

fn tokens_of(v: &Value) -> Result<Tokens> {
    let access = v.get("access_token").and_then(Value::as_str).context("the token endpoint returned no access token")?;
    if !v.get("token_type").and_then(Value::as_str).is_some_and(|t| t.eq_ignore_ascii_case("bearer")) {
        bail!("the token endpoint returned a token that is not a bearer token");
    }
    Ok(Tokens {
        access: access.into(),
        refresh: v.get("refresh_token").and_then(Value::as_str).map(String::from),
        expires_in: v.get("expires_in").and_then(Value::as_u64).unwrap_or(60),
    })
}

/// The ID token must be ours: our issuer, our client, our nonce.
fn check_id_token(v: &Value, ep: &Endpoints, nonce: &str) -> Result<()> {
    let claims = v.get("id_token").and_then(Value::as_str).and_then(jwt_claims).context("the token endpoint returned no ID token")?;
    if claims.get("iss").and_then(Value::as_str) != Some(ep.issuer.as_str()) {
        bail!("the ID token names another issuer");
    }
    let aud_ok = match claims.get("aud") {
        Some(Value::String(a)) => a == &ep.client_id,
        Some(Value::Array(a)) => a.iter().any(|x| x.as_str() == Some(&ep.client_id)),
        _ => false,
    };
    if !aud_ok || claims.get("azp").and_then(Value::as_str).is_some_and(|a| a != ep.client_id) {
        bail!("the ID token was issued to another client");
    }
    if claims.get("nonce").and_then(Value::as_str) != Some(nonce) {
        bail!("the ID token does not carry this sign-in's nonce");
    }
    Ok(())
}

/// The cross-process refresh lock (one per user): two processes never hold, and so never send, the same
/// refresh token (market-idp.md section 3, I-5).
///
/// The lock file also holds the in-flight marker: the fingerprint of the refresh token being sent, written
/// before the request and cleared once the store holds the outcome. A process killed in between leaves it,
/// and the next one, finding the stored token marked, signs out instead of sending it again.
struct RefreshLock(std::fs::File);

impl RefreshLock {
    fn marker(token: &str) -> String {
        URL_SAFE_NO_PAD.encode(fingerprint(token))
    }

    /// Whether `token` was sent by a refresh that never recorded its outcome.
    fn in_flight(&mut self, token: &str) -> Result<bool> {
        use std::io::{Read, Seek};
        let mut s = String::new();
        self.0.rewind()?;
        self.0.read_to_string(&mut s)?;
        Ok(s.trim() == Self::marker(token))
    }

    fn mark(&mut self, token: &str) -> Result<()> {
        self.write(&Self::marker(token)).context("recording the refresh in flight")
    }

    fn clear(&mut self) -> Result<()> {
        self.write("")
    }

    fn write(&mut self, s: &str) -> Result<()> {
        use std::io::{Seek, Write};
        self.0.set_len(0)?;
        self.0.rewind()?;
        self.0.write_all(s.as_bytes())?;
        self.0.sync_all()?;
        Ok(())
    }
}

fn refresh_lock(ep: &Endpoints) -> Result<RefreshLock> {
    // Tests lock in a scratch directory, not the person's cache.
    let dir = if cfg!(test) { std::env::temp_dir().join("hbench-test-signin-locks") } else { crate::env::cache_root() };
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = format!("signin-{}.lock", &URL_SAFE_NO_PAD.encode(Sha256::digest(ep.store_key().as_bytes()))[..16]);
    let f = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join(name))?;
    f.lock().context("waiting for the sign-in lock")?;
    Ok(RefreshLock(f))
}

/// The access token this process last got, with the refresh token it came with (so a sign-out or a new
/// sign-in by another process is noticed).
struct Cached {
    refresh_fp: Vec<u8>,
    access: String,
    expires: Instant,
}

static CACHE: Mutex<Option<HashMap<String, Cached>>> = Mutex::new(None);

fn fingerprint(s: &str) -> Vec<u8> {
    Sha256::digest(s.as_bytes()).to_vec()
}

fn remember(ep: &Endpoints, refresh: &str, t: &Tokens) {
    let expires = Instant::now() + Duration::from_secs(t.expires_in);
    let c = Cached { refresh_fp: fingerprint(refresh), access: t.access.clone(), expires };
    CACHE.lock().unwrap().get_or_insert_default().insert(ep.store_key(), c);
}

fn forget(ep: &Endpoints) {
    if let Some(m) = CACHE.lock().unwrap().as_mut() {
        m.remove(&ep.store_key());
    }
}

/// Raised when there is no sign-in to use.
#[derive(Debug)]
pub struct NotSignedIn;

impl std::fmt::Display for NotSignedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not signed in to Actuaria: run `hbench login --browser`")
    }
}

impl std::error::Error for NotSignedIn {}

/// Whether a refresh token is stored (no network).
pub fn signed_in(ep: &Endpoints, store: &dyn Store) -> Result<bool> {
    Ok(store.get(&ep.store_key())?.is_some())
}

/// A valid access token: this process's cached one, else a refresh under the lock. A refresh token whose
/// request was sent and not answered is never sent again: hbench forgets it and asks for a new sign-in.
pub async fn access_token(ep: &Endpoints, store: &dyn Store) -> Result<String> {
    let key = ep.store_key();
    let Some(current) = store.get(&key)? else {
        forget(ep);
        return Err(NotSignedIn.into());
    };
    if let Some(c) = CACHE.lock().unwrap().as_ref().and_then(|m| m.get(&key))
        && c.refresh_fp == fingerprint(&current)
        && c.expires > Instant::now() + EXPIRY_MARGIN
    {
        return Ok(c.access.clone());
    }
    let mut lock = refresh_lock(ep)?;
    // Read again under the lock: another process may have refreshed (and rotated) it meanwhile.
    let Some(refresh) = store.get(&key)? else {
        forget(ep);
        return Err(NotSignedIn.into());
    };
    if lock.in_flight(&refresh)? {
        // A process sent it and stopped before it kept the answer: the server may have rotated it already.
        store.delete(&key)?;
        lock.clear()?;
        forget(ep);
        bail!(
            "signed out: an earlier sign-in refresh was interrupted, and hbench never sends a refresh token twice. Run `hbench login --browser` to sign in again"
        );
    }
    lock.mark(&refresh)?;
    let r = refresh_with(ep, store, &key, &refresh).await;
    // Left in place only when the store could not be brought up to date (see `refresh_with`).
    if !matches!(&r, Err(e) if e.is::<StoreStale>()) {
        lock.clear()?;
    }
    r
}

/// The keychain still holds a refresh token that was sent: the in-flight marker stays so it is never sent again.
#[derive(Debug)]
struct StoreStale;

impl std::fmt::Display for StoreStale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the used sign-in could not be removed from the OS keychain")
    }
}

impl std::error::Error for StoreStale {}

/// Delete a sent refresh token from the store; on failure the error says so, which keeps the in-flight marker.
fn drop_sent(store: &dyn Store, key: &str) -> Result<()> {
    store.delete(key).map_err(|e| anyhow::Error::new(StoreStale).context(format!("{e:#}")))
}

/// One refresh request under the lock, with the in-flight marker written. Every path that may have used the
/// token leaves the store without it.
async fn refresh_with(ep: &Endpoints, store: &dyn Store, key: &str, refresh: &str) -> Result<String> {
    let key = key.to_string();
    let body = form(&[("grant_type", "refresh_token"), ("refresh_token", refresh), ("client_id", &ep.client_id)]);
    match post_form(&ep.endpoint("token"), body).await? {
        Sent::Answered(200, v) => {
            // Keycloak rotates the refresh token on use: an answer we can't read may still have consumed it.
            let t = match tokens_of(&v) {
                Ok(t) => t,
                Err(e) => {
                    forget(ep);
                    drop_sent(store, &key)?;
                    return Err(e.context("signed out: the refresh answer could not be used, and hbench never sends a refresh token twice"));
                }
            };
            let next = t.refresh.clone().unwrap_or_else(|| refresh.to_string());
            if next != refresh
                && let Err(e) = store.set(&key, &next)
            {
                // The old token was consumed by the rotation: keeping it would replay it on the next refresh.
                forget(ep);
                drop_sent(store, &key)?;
                return Err(
                    e.context("signed out: the renewed sign-in could not be saved to the OS keychain; run `hbench login --browser`")
                );
            }
            remember(ep, &next, &t);
            Ok(t.access)
        }
        Sent::Answered(400 | 401, v) if oauth_error(&v) == Some("invalid_grant") => {
            forget(ep);
            drop_sent(store, &key)?;
            bail!(
                "signed out: Actuaria refused the stored sign-in (it ended, or was used elsewhere). Run `hbench login --browser` to sign in again"
            )
        }
        Sent::Answered(status @ 500.., _) => {
            // The server may have used the token before it failed: never send it again.
            forget(ep);
            drop_sent(store, &key)?;
            bail!(
                "signed out: Actuaria's sign-in service failed while refreshing (HTTP {status}); run `hbench login --browser` to sign in again"
            )
        }
        Sent::Answered(status, v) => {
            bail!("refreshing the sign-in failed: HTTP {status}{}", oauth_error(&v).map(|e| format!(" ({e})")).unwrap_or_default())
        }
        Sent::Unsent(e) => bail!("couldn't reach Actuaria's sign-in service: {e}"),
        Sent::Lost(e) => {
            // It may have been used: sending it again would look like a stolen token being replayed.
            forget(ep);
            drop_sent(store, &key)?;
            bail!(
                "the sign-in refresh was interrupted ({e}); hbench never sends a refresh token twice, so sign in again with `hbench login --browser`"
            )
        }
    }
}

/// Redeem an authorization code with the verifier, and keep the refresh token.
pub async fn redeem(ep: &Endpoints, store: &dyn Store, attempt: &Attempt, code: &str) -> Result<()> {
    let body = form(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", &attempt.redirect_uri),
        ("client_id", &ep.client_id),
        ("code_verifier", &attempt.verifier),
    ]);
    let v = match post_form(&ep.endpoint("token"), body).await? {
        Sent::Answered(200, v) => v,
        Sent::Answered(status, v) => {
            bail!("Actuaria refused the sign-in code: HTTP {status}{}", oauth_error(&v).map(|e| format!(" ({e})")).unwrap_or_default())
        }
        Sent::Unsent(e) | Sent::Lost(e) => bail!("couldn't reach Actuaria's sign-in service: {e}"),
    };
    let t = tokens_of(&v)?;
    check_id_token(&v, ep, &attempt.nonce)?;
    let refresh = t.refresh.as_deref().context("Actuaria returned no refresh token (the offline_access scope was not granted)")?;
    let mut lock = refresh_lock(ep)?;
    store.set(&ep.store_key(), refresh)?;
    lock.clear()?;
    remember(ep, refresh, &t);
    Ok(())
}

/// The market API's `whoami` for the signed-in person. It creates the personal organisation on first use.
pub async fn whoami(ep: &Endpoints, store: &dyn Store) -> Result<Value> {
    let token = access_token(ep, store).await?;
    let resp = http()?.get(&ep.whoami).bearer_auth(token).send().await.with_context(|| format!("reaching {}", ep.whoami))?;
    let status = resp.status();
    let body = resp.bytes().await?;
    if !status.is_success() {
        bail!("{} answered HTTP {}", ep.whoami, status.as_u16());
    }
    serde_json::from_slice(&body).with_context(|| format!("{} answered something that is not JSON", ep.whoami))
}

/// Open `url` in the system browser. Errors when no opener could be started.
pub fn open_browser(url: &str) -> Result<()> {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(windows) {
        // Not `cmd /c start`: cmd would read the `&` in the query.
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    let status = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| anyhow!("{e}"))?;
    if !status.success() {
        bail!("the opener exited with {status}");
    }
    Ok(())
}

/// The line that carries the sign-in address. The desktop app reads it, after [`NO_BROWSER_LINE`], to show the
/// address when no browser opened (desktop/src-tauri/src/signin.rs).
pub const URL_LINE_PREFIX: &str = "Sign in at: ";
pub const NO_BROWSER_LINE: &str = "Couldn't open a browser. Open this address to sign in:";

/// `hbench login --browser`, with the browser opener and the store given (tests pass fakes).
pub async fn login_browser_with(ep: &Endpoints, store: &dyn Store, open: impl FnOnce(&str) -> Result<()>, timeout: Duration) -> Result<()> {
    let listener = loopback::Listener::bind()?;
    let attempt = Attempt::new(ep, &listener.redirect_uri());
    match open(&attempt.authorize_url) {
        Ok(()) => println!("Opened your browser to sign in. If it didn't open, go to the address below."),
        Err(_) => println!("{NO_BROWSER_LINE}"),
    }
    println!("{URL_LINE_PREFIX}{}", attempt.authorize_url);
    println!("Waiting for you to finish in the browser (up to {} minutes)…", timeout.as_secs() / 60);
    let (state, issuer) = (attempt.state.clone(), ep.issuer.clone());
    let deadline = Instant::now() + timeout;
    let pending = tokio::task::spawn_blocking(move || listener.wait(&state, &issuer, deadline)).await??;
    let code = pending.code.clone();
    let done = redeem(ep, store, &attempt, &code).await;
    // The browser waits for this answer, so its page says how the sign-in really ended.
    let ok = done.is_ok();
    let _ = tokio::task::spawn_blocking(move || pending.finish(if ok { Ok(()) } else { Err("Actuaria refused the sign-in code") })).await;
    done
}

/// `hbench login --browser`.
pub async fn login_browser() -> Result<()> {
    let ep = Endpoints::pinned();
    let store = store::Keychain;
    login_browser_with(&ep, &store, open_browser, LOGIN_TIMEOUT).await?;
    println!("Signed in to Actuaria.");
    match whoami(&ep, &store).await {
        Ok(v) => print_whoami(&v),
        Err(e) => eprintln!("Signed in, but `whoami` failed: {e:#}"),
    }
    Ok(())
}

/// Revoke the refresh token at the issuer and forget it. The browser's own session stays signed in.
pub async fn logout_with(ep: &Endpoints, store: &dyn Store) -> Result<bool> {
    let key = ep.store_key();
    let _lock = refresh_lock(ep)?;
    let Some(refresh) = store.get(&key)? else {
        forget(ep);
        return Ok(false);
    };
    let body = form(&[("token", &refresh), ("token_type_hint", "refresh_token"), ("client_id", &ep.client_id)]);
    let revoked = post_form(&ep.endpoint("revoke"), body).await;
    // Forget it whatever the answer: a sign-out must not leave a usable token behind on this machine.
    store.delete(&key)?;
    forget(ep);
    match revoked? {
        Sent::Answered(200, _) => Ok(true),
        Sent::Answered(status, _) => bail!("signed out on this machine, but Actuaria did not confirm revoking the sign-in (HTTP {status})"),
        Sent::Unsent(e) | Sent::Lost(e) => bail!("signed out on this machine, but couldn't reach Actuaria to revoke the sign-in: {e}"),
    }
}

/// `hbench logout`.
pub async fn logout() -> Result<()> {
    if logout_with(&Endpoints::pinned(), &store::Keychain).await? {
        println!("Signed out. Your browser may still be signed in to Actuaria; sign out there too if this is a shared computer.");
    } else {
        println!("Not signed in.");
    }
    Ok(())
}

fn print_whoami(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

/// `hbench whoami`.
pub async fn whoami_cmd() -> Result<()> {
    print_whoami(&whoami(&Endpoints::pinned(), &store::Keychain).await?);
    Ok(())
}

/// For the local viewer: the account as the page may see it. Never a token.
pub async fn account_view() -> Value {
    let (ep, store) = (Endpoints::pinned(), store::Keychain);
    match signed_in(&ep, &store) {
        Ok(false) => serde_json::json!({"signed_in": false}),
        Err(e) => serde_json::json!({"signed_in": false, "error": format!("{e:#}")}),
        Ok(true) => match whoami(&ep, &store).await {
            Ok(v) => serde_json::json!({"signed_in": true, "account": v}),
            Err(e) if e.is::<NotSignedIn>() => serde_json::json!({"signed_in": false}),
            Err(e) => {
                let still = signed_in(&ep, &store).unwrap_or(false);
                serde_json::json!({"signed_in": still, "account": null, "error": format!("{e:#}")})
            }
        },
    }
}
