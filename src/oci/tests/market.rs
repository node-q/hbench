//! The market's client-side checks, end to end against a stand-in market registry on localhost: repository
//! resolution, the lock file's canonical names, `--locked` by digest, the listing check, realm pinning and the
//! signature hook.

use super::super::market::{Endpoint, TEST_MARKET};
use super::super::verify::{Subject, Verifier};
use super::super::{download_with, lockfile};
use super::{Scratch, example, info, rewrite, scratch, write_layout};
use crate::config::{DatasetConfig, TaskRef};
use crate::oci::format::{LISTING_MEDIA_TYPE, MANIFEST_MEDIA_TYPE, REF_NAME, TITLE};
use crate::oci::lockfile::LockMode;
use crate::oci::{dataset_tasks, pin_datasets_at, sha256_digest};
use base64::Engine;
use oci_client::secrets::RegistryAuth;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const TOKEN: &str = "pull-token";
const USER: &str = "buyer";
const PASSWORD: &str = "s3cret";

/// One request the stand-in received: method, path with query, and its Authorization header.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    url: String,
    authorization: Option<String>,
}

/// A market registry on localhost serving the OCI layout `<layouts>/<listing>` as `<any org>/actuaria/<listing>`,
/// with its token endpoint at `/token`. Its challenge names `http://<host><realm_path>`; `/token` is the pinned one.
struct FakeMarket {
    host: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeMarket {
    fn start(layouts: &Path, realm_path: &str) -> FakeMarket {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let host = format!("127.0.0.1:{}", server.server_addr().to_ip().unwrap().port());
        let realm = format!("http://{host}{realm_path}");
        let (seen, stop) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicBool::new(false)));
        let (layouts, seen2, stop2) = (layouts.to_path_buf(), seen.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                let Ok(Some(req)) = server.recv_timeout(std::time::Duration::from_millis(50)) else { continue };
                let authorization = req.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.as_str().to_string());
                let s = Seen { method: req.method().to_string(), url: req.url().to_string(), authorization };
                seen2.lock().unwrap().push(s.clone());
                let (status, headers, body) = respond(&layouts, &realm, &s);
                let mut response = tiny_http::Response::from_data(body).with_status_code(status);
                for (k, v) in headers {
                    response.add_header(tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap());
                }
                let _ = req.respond(response);
            }
        });
        FakeMarket { host, seen, stop, thread: Some(thread) }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint { registry: self.host.clone(), token_url: format!("http://{}/token", self.host) }
    }

    /// Make this the market for the rest of the test's thread, with the docker config holding `credentials`.
    fn install(&self, credentials: RegistryAuth) {
        TEST_MARKET.with(|m| *m.borrow_mut() = Some((self.endpoint(), credentials)));
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }
}

impl Drop for FakeMarket {
    fn drop(&mut self) {
        TEST_MARKET.with(|m| *m.borrow_mut() = None);
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.thread.take().map(|t| t.join());
    }
}

type Reply = (u16, Vec<(String, String)>, Vec<u8>);

fn respond(layouts: &Path, realm: &str, s: &Seen) -> Reply {
    let challenge = || vec![("WWW-Authenticate".to_string(), format!(r#"Bearer realm="{realm}",service="fake-market""#))];
    let path = s.url.split('?').next().unwrap_or_default();
    if path == "/token" || path.starts_with("/evil") {
        let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{PASSWORD}")));
        return match s.authorization.as_deref() == Some(basic.as_str()) {
            true => (200, vec![], json!({ "token": TOKEN }).to_string().into_bytes()),
            false => (401, vec![], vec![]),
        };
    }
    if path == "/v2/" || s.authorization.as_deref() != Some(&format!("Bearer {TOKEN}")) {
        return (401, challenge(), vec![]);
    }
    let not_found = (404, vec![], br#"{"errors":[{"code":"NAME_UNKNOWN","message":"not found"}]}"#.to_vec());
    // /v2/<org>/actuaria/<listing>/(manifests|blobs)/<reference>
    let parts: Vec<&str> = path.trim_start_matches("/v2/").split('/').collect();
    let [_org, "actuaria", listing, kind, reference] = parts.as_slice() else { return not_found };
    let layout = layouts.join(listing);
    let blob = |digest: &str| std::fs::read(layout.join("blobs/sha256").join(digest.trim_start_matches("sha256:"))).ok();
    let digest = if reference.starts_with("sha256:") {
        reference.to_string()
    } else {
        let Ok(index) = std::fs::read(layout.join("index.json")) else { return not_found };
        let index: Value = serde_json::from_slice(&index).unwrap();
        let found = index["manifests"].as_array().unwrap().iter().find(|m| m["annotations"][REF_NAME] == *reference);
        let Some(d) = found.and_then(|m| m["digest"].as_str()) else { return not_found };
        d.to_string()
    };
    let Some(bytes) = blob(&digest) else { return not_found };
    let content_type = if *kind == "manifests" { MANIFEST_MEDIA_TYPE } else { "application/octet-stream" };
    let headers = vec![("Content-Type".into(), content_type.into()), ("Docker-Content-Digest".into(), digest)];
    (200, headers, bytes)
}

/// Add a `listing.json` layer holding `listing` to the manifest tagged `version` in the layout `layout`; the new
/// manifest digest.
fn add_listing(layout: &Path, version: &str, listing: Value) -> String {
    let bytes = serde_json::to_vec(&listing).unwrap();
    let digest = sha256_digest(&bytes);
    std::fs::write(layout.join("blobs/sha256").join(digest.trim_start_matches("sha256:")), &bytes).unwrap();
    rewrite(layout, version, |manifest, _| {
        manifest["layers"].as_array_mut().unwrap().push(json!({
            "mediaType": LISTING_MEDIA_TYPE, "digest": digest, "size": bytes.len(),
            "annotations": {TITLE: "listing.json"},
        }));
    })
}

/// `-d <name>@<version> --org <org>` from the market at `market`, cached under `cache`.
fn market_dataset(market: &FakeMarket, name: &str, version: &str, org: &str, cache: &Path) -> DatasetConfig {
    DatasetConfig {
        name: Some(name.into()),
        version: Some(version.into()),
        registry: Some(format!("oci://{}", market.host)),
        org: Some(org.into()),
        download_dir: Some(cache.to_path_buf()),
        ..Default::default()
    }
}

/// A market with listing `hello`, version 0.2, whose listing.json is `listing`; (layouts, market, digest).
fn market_with(listing: Value) -> (Scratch, FakeMarket, String) {
    let layouts = scratch("market");
    write_layout(&layouts, &info("hello", "0.2"), &[example("hello-world")]).unwrap();
    let digest = add_listing(&layouts.join("hello"), "0.2", listing);
    let market = FakeMarket::start(&layouts, "/token");
    market.install(RegistryAuth::Basic(USER.into(), PASSWORD.into()));
    (layouts, market, digest)
}

fn good_listing() -> Value {
    json!({"schema_version": 1, "publisher": "actuaria", "name": "hello", "version": "0.2", "title": "Hello"})
}

/// A signature check that records what it was asked, and whether the version's first task was already unpacked
/// at that moment; it refuses every version when `refuse` is set.
struct Recording {
    task_dir: PathBuf,
    refuse: bool,
    calls: Mutex<Vec<(Subject, bool)>>,
}

#[async_trait::async_trait]
impl Verifier for Recording {
    async fn verify(&self, _: &crate::oci::store::Store, subject: &Subject) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push((subject.clone(), self.task_dir.exists()));
        if self.refuse {
            anyhow::bail!("no valid signature");
        }
        Ok(())
    }
}

async fn pin(lock: &Path, d: &DatasetConfig, mode: LockMode) -> anyhow::Result<DatasetConfig> {
    let mut d = vec![d.clone()];
    pin_datasets_at(lock, &mut d, mode).await?;
    Ok(d.remove(0))
}

#[tokio::test]
async fn a_market_version_resolves_to_its_organisations_repository_and_is_locked_by_its_canonical_name() {
    let (_layouts, market, digest) = market_with(good_listing());
    let (cache, locks) = (scratch("cache"), scratch("lock"));
    let lock = locks.join(lockfile::FILE);

    let d = pin(&lock, &market_dataset(&market, "hello", "0.2", "acme", &cache), LockMode::Write).await.unwrap();
    assert_eq!(d.digest.as_deref(), Some(digest.as_str()));
    let text = std::fs::read_to_string(&lock).unwrap();
    assert!(text.contains(&format!("\"actuaria/hello:0.2\" = \"{digest}\"")), "{text}");
    assert!(!text.contains("acme"), "the lock file never records the organisation: {text}");

    let token_requests: Vec<Seen> = market.seen().into_iter().filter(|s| s.url.starts_with("/token")).collect();
    assert!(!token_requests.is_empty());
    for t in &token_requests {
        assert!(t.url.contains("scope=repository%3Aacme%2Factuaria%2Fhello%3Apull"), "{}", t.url);
        assert!(t.authorization.as_deref().is_some_and(|a| a.starts_with("Basic ")));
    }
    assert!(market.seen().iter().any(|s| s.url == "/v2/acme/actuaria/hello/manifests/0.2"), "{:?}", market.seen());

    // `<publisher>/<listing>` names the same version.
    let tasks = dataset_tasks(&market_dataset(&market, "actuaria/hello", "0.2", "acme", &cache)).await.unwrap();
    let oci = tasks[0].oci.as_ref().unwrap();
    assert_eq!((oci.repository.as_str(), oci.manifest_digest.as_str()), ("acme/actuaria/hello", digest.as_str()));
    assert_eq!(tasks[0].source.as_deref(), Some("hello"));

    // The signature check runs before anything is unpacked, on the canonical name and the digest.
    let task_dir = crate::registry::local_path(&tasks[0]);
    let recording = Recording { task_dir: task_dir.clone(), refuse: false, calls: Mutex::new(vec![]) };
    download_with(&tasks.iter().collect::<Vec<&TaskRef>>(), &recording).await.unwrap();
    let calls = recording.calls.into_inner().unwrap();
    assert_eq!(calls.len(), 1);
    let (subject, unpacked_before) = &calls[0];
    assert!(!unpacked_before, "the check ran after the task was unpacked");
    assert_eq!(subject.canonical_name, "actuaria/hello:0.2");
    assert_eq!((subject.repository.as_str(), subject.manifest_digest.as_str()), ("acme/actuaria/hello", digest.as_str()));
    assert!(task_dir.join("task.toml").is_file());

    // The same lock file serves another organisation, and --locked pulls by digest without resolving the tag.
    market.clear();
    let other = scratch("cache-globex");
    let d = pin(&lock, &market_dataset(&market, "hello", "0.2", "globex", &other), LockMode::Locked).await.unwrap();
    assert_eq!(d.digest.as_deref(), Some(digest.as_str()));
    assert!(market.seen().is_empty(), "--locked resolved a tag: {:?}", market.seen());
    let tasks = dataset_tasks(&d).await.unwrap();
    assert_eq!(tasks[0].oci.as_ref().unwrap().repository, "globex/actuaria/hello");
    assert!(market.seen().iter().all(|s| !s.url.ends_with("/manifests/0.2")), "pulled by tag: {:?}", market.seen());
    assert!(market.seen().iter().any(|s| s.url == format!("/v2/globex/actuaria/hello/manifests/{digest}")));

    let err = pin(&lock, &market_dataset(&market, "hello", "0.3", "acme", &cache), LockMode::Locked).await.unwrap_err();
    assert_eq!(err.to_string(), "--locked: actuaria/hello:0.3 is not in hbench.lock");
}

#[tokio::test]
async fn a_version_whose_listing_names_another_listing_or_version_is_refused_before_it_is_unpacked() {
    for (listing, why) in [
        (json!({"publisher": "actuaria", "name": "goodbye", "version": "0.2"}), "it is another listing"),
        (json!({"publisher": "actuaria", "name": "hello", "version": "0.1"}), "it is another version"),
        (json!({"publisher": "mallory", "name": "hello", "version": "0.2"}), "it is another publisher's listing"),
        (json!({"name": "hello", "version": "0.2"}), "its listing names no publisher"),
    ] {
        let (_layouts, market, digest) = market_with(listing.clone());
        let cache = scratch("cache");
        let err = dataset_tasks(&market_dataset(&market, "hello", "0.2", "acme", &cache)).await.unwrap_err().to_string();
        assert!(err.starts_with(&format!("refused {}/acme/actuaria/hello@{digest}: {why}", market.host)), "{listing}: {err}");
        assert!(err.contains("actuaria/hello:0.2 was asked for"), "{err}");
        assert!(!cache.join("tasks").exists(), "something was unpacked");
    }

    // A market version that carries no listing.json can't be checked, so it is refused too.
    let layouts = scratch("market");
    write_layout(&layouts, &info("hello", "0.2"), &[example("hello-world")]).unwrap();
    let market = FakeMarket::start(&layouts, "/token");
    market.install(RegistryAuth::Basic(USER.into(), PASSWORD.into()));
    let err = dataset_tasks(&market_dataset(&market, "hello", "0.2", "acme", &scratch("cache"))).await.unwrap_err().to_string();
    assert!(err.contains("carries no listing.json") && err.contains("actuaria/hello:0.2"), "{err}");
}

#[tokio::test]
async fn a_challenge_naming_another_realm_is_refused_and_no_credentials_are_sent() {
    let layouts = scratch("market");
    write_layout(&layouts, &info("hello", "0.2"), &[example("hello-world")]).unwrap();
    add_listing(&layouts.join("hello"), "0.2", good_listing());
    // The stand-in's challenge sends clients to /evil/token on its own host, which would mint a token.
    let market = FakeMarket::start(&layouts, "/evil/token");
    market.install(RegistryAuth::Basic(USER.into(), PASSWORD.into()));
    let evil = format!("http://{}/evil/token", market.host);
    let (cache, locks) = (scratch("cache"), scratch("lock"));
    let d = market_dataset(&market, "hello", "0.2", "acme", &cache);

    let err = format!("{:#}", pin(&locks.join(lockfile::FILE), &d, LockMode::Write).await.unwrap_err());
    assert!(err.contains("token endpoint is pinned") && err.contains("no credentials were sent") && err.contains(&evil), "{err}");
    let seen = market.seen();
    assert_eq!(seen.iter().map(|s| (s.method.as_str(), s.url.as_str())).collect::<Vec<_>>(), [("GET", "/v2/")], "{seen:?}");
    assert!(seen.iter().all(|s| s.authorization.is_none()), "a credential was sent: {seen:?}");
    assert!(!locks.join(lockfile::FILE).exists());
    assert!(dataset_tasks(&d).await.is_err());
    assert!(market.seen().iter().all(|s| s.authorization.is_none() && !s.url.contains("token")), "{:?}", market.seen());
}

#[tokio::test]
async fn a_refused_signature_check_unpacks_nothing() {
    let (_layouts, market, _) = market_with(good_listing());
    let cache = scratch("cache");
    let tasks = dataset_tasks(&market_dataset(&market, "hello", "0.2", "acme", &cache)).await.unwrap();
    let task_dir = crate::registry::local_path(&tasks[0]);
    let refusing = Recording { task_dir: task_dir.clone(), refuse: true, calls: Mutex::new(vec![]) };
    let err = format!("{:#}", download_with(&tasks.iter().collect::<Vec<&TaskRef>>(), &refusing).await.unwrap_err());
    assert!(err.contains("signatures do not check out") && err.contains("no valid signature"), "{err}");
    assert!(!task_dir.exists(), "a task was unpacked");
    assert!(!cache.join("tasks").exists());

    // The check that ships until the signing format is decided accepts the version and unpacks it.
    download_with(&tasks.iter().collect::<Vec<&TaskRef>>(), &crate::oci::verify::Pending).await.unwrap();
    assert!(task_dir.join("task.toml").is_file());
}

#[tokio::test]
async fn market_requests_are_checked_before_anything_is_asked() {
    let cache = scratch("cache");
    let locks = scratch("lock");
    let lock = locks.join(lockfile::FILE);
    let market = |registry: &str, org: Option<&str>| DatasetConfig {
        name: Some("hello".into()),
        version: Some("0.2".into()),
        registry: Some(registry.into()),
        org: org.map(String::from),
        download_dir: Some(cache.to_path_buf()),
        ..Default::default()
    };
    let err = |d: DatasetConfig| {
        let lock = lock.clone();
        async move { format!("{:#}", pin(&lock, &d, LockMode::Read).await.unwrap_err()) }
    };
    let e = err(market("oci://registry.actuaria.io/acme", Some("acme"))).await;
    assert!(e.contains("takes no namespace"), "{e}");
    let e = err(market("oci://registry.actuaria.io", Some("unlisted"))).await;
    assert!(e.contains("reserved"), "{e}");
    let e = err(market("oci://ghcr.io/node-q/hbench-datasets", Some("acme"))).await;
    assert!(e.contains("--org acme names an organisation in the market's registry"), "{e}");
    let e = err(DatasetConfig { name: Some("a/b/c".into()), ..market("oci://registry.actuaria.io", Some("acme")) }).await;
    assert!(e.contains("not a listing name") || e.contains("not a publisher name"), "{e}");
    // The market is never listed or opened without a repository.
    let e = format!("{:#}", crate::oci::list("oci://registry.actuaria.io").await.unwrap_err());
    assert!(e.contains("market's registry"), "{e}");
}

#[test]
fn a_listing_layer_is_not_a_task() {
    let layouts = scratch("layout");
    write_layout(&layouts, &info("hello", "0.2"), &[example("hello-world")]).unwrap();
    add_listing(&layouts.join("hello"), "0.2", good_listing());
    let layout = layouts.join("hello");
    let index: Value = serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    let digest = index["manifests"][0]["digest"].as_str().unwrap().to_string();
    let read = |d: &str| std::fs::read(layout.join("blobs/sha256").join(d.trim_start_matches("sha256:"))).unwrap();
    let manifest_bytes = read(&digest);
    let manifest = crate::oci::format::parse_manifest(&manifest_bytes).unwrap();
    let config = read(manifest.config().digest().as_ref());
    let dataset = crate::oci::format::Dataset::new(&manifest, &config).unwrap();
    assert_eq!(dataset.layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["hello-world"]);
    assert!(dataset.listing.is_some());

    let mut twice: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let listing = twice["layers"][1].clone();
    twice["layers"].as_array_mut().unwrap().push(listing);
    let err = crate::oci::format::parse_manifest(&serde_json::to_vec(&twice).unwrap()).unwrap_err().to_string();
    assert!(err.contains("at most one"), "{err}");
    let mut only: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    only["layers"].as_array_mut().unwrap().remove(0);
    let err = crate::oci::format::parse_manifest(&serde_json::to_vec(&only).unwrap()).unwrap_err().to_string();
    assert!(err.contains("at least one task layer"), "{err}");
}
