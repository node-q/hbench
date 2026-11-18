//! The Actuaria market's registry (hbench-studio `docs/design/market-platform.md` §6–§7, D115, D116).
//!
//! - A version is named by its **canonical name**, `<publisher>/<listing>:<tag>`, the name its own `listing.json`
//!   states. That name, never a repository path, is what the lock file records.
//! - Each buyer organisation has its own repositories, `<org>/<publisher>/<listing>`. `-d <listing>@<tag>` and
//!   `-d <publisher>/<listing>@<tag>` resolve to one of them, for the organisation [`resolve_org`] finds.
//! - The registry's token endpoint is pinned ([`MARKET_TOKEN_URL`]). A challenge that names any other realm is
//!   refused before a credential is sent anywhere (registry threat model, R-6).
//! - A pulled version whose `listing.json` names another listing or version than the one asked for is refused
//!   ([`check_listing`]; D115, released-versions.md R4).

use anyhow::{Context, Result, bail};
use oci_client::secrets::RegistryAuth;
use serde::Deserialize;
use std::fmt;
use std::time::Duration;

/// The market's registry host.
pub const MARKET_REGISTRY: &str = "registry.actuaria.io";
/// The market's token endpoint: the only realm a credential for [`MARKET_REGISTRY`] is ever sent to. It must equal
/// the `realm` the registry's challenge names (zot's `http.auth.bearer.realm`).
pub const MARKET_TOKEN_URL: &str = "https://registry.actuaria.io/token";
/// The publisher a listing has when the request names none. Actuaria is the only publisher until third-party
/// publishers arrive (D121); then the catalogue says (see [`publisher_of`]).
pub const DEFAULT_PUBLISHER: &str = "actuaria";
/// The environment variable that names the organisation, when `--org` does not.
pub const ORG_ENV: &str = "HBENCH_ORG";
/// Organisation slugs the market keeps for itself (D115): no buyer's repositories live under them.
const RESERVED_ORGS: &[&str] = &["unlisted"];
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;

/// A registry whose token endpoint is pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The registry host, as `oci://<registry>` names it.
    pub registry: String,
    /// The pinned token endpoint: the exact realm the registry's challenge must name.
    pub token_url: String,
}

#[cfg(test)]
thread_local! {
    /// A market stand-in for tests on this thread: its endpoint, and the credentials the docker config would hold.
    pub(crate) static TEST_MARKET: std::cell::RefCell<Option<(Endpoint, RegistryAuth)>> = const { std::cell::RefCell::new(None) };
}

/// The pinned endpoint of `registry`, when it is the market's registry.
pub fn endpoint_for(registry: &str) -> Option<Endpoint> {
    #[cfg(test)]
    if let Some((e, _)) = TEST_MARKET.with(|m| m.borrow().clone()).filter(|(e, _)| e.registry == registry) {
        return Some(e);
    }
    (registry == MARKET_REGISTRY).then(|| Endpoint { registry: MARKET_REGISTRY.into(), token_url: MARKET_TOKEN_URL.into() })
}

pub fn is_market(registry: &str) -> bool {
    endpoint_for(registry).is_some()
}

/// The credentials the docker config holds for the market registry `registry`.
pub fn credentials(registry: &str) -> Result<RegistryAuth> {
    #[cfg(test)]
    if let Some((_, auth)) = TEST_MARKET.with(|m| m.borrow().clone()).filter(|(e, _)| e.registry == registry) {
        return Ok(auth);
    }
    super::credentials::registry_auth(super::credentials::docker_config_path().as_deref(), registry)
}

/// A market version's canonical name, `<publisher>/<listing>:<tag>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalName {
    pub publisher: String,
    pub listing: String,
    pub tag: String,
}

impl fmt::Display for CanonicalName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}:{}", self.publisher, self.listing, self.tag)
    }
}

impl CanonicalName {
    /// The version `-d <listing>@<tag>` or `-d <publisher>/<listing>@<tag>` asks for; `name` is the part before `@`.
    pub fn requested(name: &str, tag: &str) -> Result<CanonicalName> {
        let (publisher, listing) = match name.split_once('/') {
            Some((publisher, listing)) => (publisher.to_string(), listing.to_string()),
            None => (publisher_of(name), name.to_string()),
        };
        for (what, part) in [("publisher", &publisher), ("listing", &listing)] {
            if !super::is_repository_component(part) {
                bail!(
                    "{name}@{tag}: {part:?} is not a {what} name (lowercase letters and digits, separated by . _ or -); name a market version <listing>@<tag> or <publisher>/<listing>@<tag>"
                );
            }
        }
        if !super::is_tag(tag) {
            bail!("{name}@{tag}: the version is not an OCI tag");
        }
        Ok(CanonicalName { publisher, listing, tag: tag.to_string() })
    }

    /// The repository that holds this version for organisation `org`: `<org>/<publisher>/<listing>`.
    pub fn repository(&self, org: &str) -> String {
        format!("{org}/{}/{}", self.publisher, self.listing)
    }
}

/// The publisher of `listing` when the request names none.
///
/// Seam: the catalogue (the market API) will answer this, and a listing two publishers share will need
/// `<publisher>/<listing>`. Until third-party publishers exist (D121) every listing is Actuaria's.
fn publisher_of(_listing: &str) -> String {
    DEFAULT_PUBLISHER.to_string()
}

/// The organisation whose repositories a market pull reads: `--org`, else `$HBENCH_ORG`, else the signed-in
/// organisation.
pub fn resolve_org(explicit: Option<&str>) -> Result<String> {
    let env = std::env::var(ORG_ENV).ok();
    org_from(explicit, env.as_deref(), signed_in_org)
}

/// Seam: the organisation the signed-in person or installation belongs to, from the market API's `whoami`
/// (market-platform.md §7). A person in several picks one with `--org`; a machine credential belongs to one.
/// Sign-in does not exist yet, so there is never one.
fn signed_in_org() -> Result<Option<String>> {
    Ok(None)
}

fn org_from(explicit: Option<&str>, env: Option<&str>, whoami: impl FnOnce() -> Result<Option<String>>) -> Result<String> {
    let given = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let (org, from) = match (given(explicit), given(env)) {
        (Some(org), _) => (org, "--org"),
        (None, Some(org)) => (org, ORG_ENV),
        (None, None) => match whoami()? {
            Some(org) => (org, "the signed-in account"),
            None => bail!(
                "a market version is pulled from your organisation's repositories: name the organisation with --org <org> or {ORG_ENV}"
            ),
        },
    };
    if !super::is_repository_component(&org) {
        bail!("{from}: {org:?} is not an organisation name (lowercase letters and digits, separated by . _ or -)");
    }
    if RESERVED_ORGS.contains(&org.as_str()) {
        bail!("{from}: {org:?} is reserved by the market, not an organisation");
    }
    Ok(org)
}

/// The fields of a version's `listing.json` that name it. The rest describes the listing and is not read here.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Listing {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub publisher: Option<String>,
}

pub fn parse_listing(bytes: &[u8]) -> Result<Listing> {
    serde_json::from_slice(bytes).context("listing.json is not a listing (it needs a name and a version)")
}

/// Refuse a pulled version, `reference`, whose own `listing.json` names another listing or version than the
/// `name@version` asked for. `publisher` is the publisher asked for, on the market, where the listing must name
/// one; elsewhere it is None and only the name and version are compared.
pub fn check_listing(listing: &Listing, name: &str, version: &str, publisher: Option<&str>, reference: &str) -> Result<()> {
    let stated = match &listing.publisher {
        Some(p) => format!("{p}/{}:{}", listing.name, listing.version),
        None => format!("{}:{}", listing.name, listing.version),
    };
    let asked = match publisher {
        Some(p) => format!("{p}/{name}:{version}"),
        None => format!("{name}:{version}"),
    };
    let refuse = |why: &str| {
        anyhow::anyhow!(
            "refused {reference}: {why}. Its own listing.json names {stated}, but {asked} was asked for, so the registry served another version under this name. Nothing was unpacked."
        )
    };
    if listing.name != name {
        return Err(refuse("it is another listing"));
    }
    if listing.version != version {
        return Err(refuse("it is another version"));
    }
    match (publisher, listing.publisher.as_deref()) {
        (Some(_), None) => Err(refuse("its listing names no publisher")),
        (Some(want), Some(got)) if want != got => Err(refuse("it is another publisher's listing")),
        _ => Ok(()),
    }
}

/// The registry token for pulling `repository` from the market registry `endpoint`, using `credentials` (the
/// docker config's for that registry).
///
/// The registry's challenge must name exactly the pinned token endpoint; any other realm, or no bearer challenge
/// at all, is refused before any credential is sent. The credentials then go to the pinned endpoint, never to a
/// URL the registry supplied, and redirects are not followed.
pub async fn registry_auth(endpoint: &Endpoint, repository: &str, credentials: RegistryAuth) -> Result<RegistryAuth> {
    let registry = &endpoint.registry;
    let (user, password) = match credentials {
        RegistryAuth::Basic(user, password) => (user, password),
        // A registry token goes to the registry itself, never to a realm.
        RegistryAuth::Bearer(token) => return Ok(RegistryAuth::Bearer(token)),
        RegistryAuth::Anonymous => {
            bail!("{registry}: the docker config has no credentials for the market's registry, and it serves nothing anonymously")
        }
    };
    let client = http_client()?;
    let scheme = if super::store::is_loopback(registry) { "http" } else { "https" };
    let probe =
        client.get(format!("{scheme}://{registry}/v2/")).send().await.with_context(|| format!("asking {registry} how to authenticate"))?;
    let service = pinned_challenge(endpoint, &probe)?;
    let mut url = reqwest::Url::parse(&endpoint.token_url).with_context(|| format!("the pinned token endpoint {}", endpoint.token_url))?;
    url.query_pairs_mut().append_pair("scope", &format!("repository:{repository}:pull"));
    if let Some(service) = &service {
        url.query_pairs_mut().append_pair("service", service);
    }
    let response = client
        .get(url)
        .basic_auth(user, Some(password))
        .send()
        .await
        .with_context(|| format!("asking {} for a token", endpoint.token_url))?;
    let status = response.status();
    if !status.is_success() {
        bail!("{}: the token service refused a pull token for {registry}/{repository} ({status})", endpoint.token_url);
    }
    let body = response.bytes().await.with_context(|| format!("reading the token from {}", endpoint.token_url))?;
    if body.len() > MAX_TOKEN_RESPONSE_BYTES {
        bail!("{}: the token response is over {MAX_TOKEN_RESPONSE_BYTES} bytes", endpoint.token_url);
    }
    #[derive(Deserialize)]
    struct TokenResponse {
        token: Option<String>,
        access_token: Option<String>,
    }
    let t: TokenResponse = serde_json::from_slice(&body).with_context(|| format!("{}: the response is not a token", endpoint.token_url))?;
    let token = t
        .token
        .or(t.access_token)
        .filter(|t| !t.is_empty())
        .with_context(|| format!("{}: the response holds no token", endpoint.token_url))?;
    Ok(RegistryAuth::Bearer(token))
}

fn http_client() -> Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(TOKEN_TIMEOUT)
        .timeout(TOKEN_TIMEOUT)
        .user_agent(super::store::USER_AGENT)
        .build()
        .context("creating the HTTP client")
}

/// The `service` of the registry's bearer challenge in `probe`, once every challenge it sends names the pinned
/// realm.
fn pinned_challenge(endpoint: &Endpoint, probe: &reqwest::Response) -> Result<Option<String>> {
    let registry = &endpoint.registry;
    let headers: Vec<&str> = probe.headers().get_all(reqwest::header::WWW_AUTHENTICATE).iter().filter_map(|v| v.to_str().ok()).collect();
    challenge_service(endpoint, &headers).with_context(|| {
        format!(
            "refused {registry}: its token endpoint is pinned to {}, and no credentials were sent (a changed registry config or someone in between can cause this; report it to Actuaria)",
            endpoint.token_url
        )
    })
}

/// [`pinned_challenge`] on the `WWW-Authenticate` header values.
fn challenge_service(endpoint: &Endpoint, headers: &[&str]) -> Result<Option<String>> {
    let mut service = None;
    let mut bearer = false;
    for header in headers {
        let challenges = http_auth::parse_challenges(header).map_err(|e| anyhow::anyhow!("an unreadable challenge, {header:?}: {e}"))?;
        for c in challenges {
            if !c.scheme.eq_ignore_ascii_case("bearer") {
                bail!("the registry asked for {} authentication, not a bearer token", c.scheme);
            }
            let param = |key: &str| c.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.to_unescaped());
            match param("realm") {
                Some(realm) if realm == endpoint.token_url => {}
                Some(realm) => bail!("the registry's challenge names another token endpoint, {realm}"),
                None => bail!("the registry's challenge names no token endpoint"),
            }
            service = service.or(param("service"));
            bearer = true;
        }
    }
    if !bearer {
        bail!("the registry did not ask for a token");
    }
    Ok(service)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market() -> Endpoint {
        Endpoint { registry: MARKET_REGISTRY.into(), token_url: MARKET_TOKEN_URL.into() }
    }

    #[test]
    fn the_market_registry_is_known_by_its_host() {
        assert_eq!(endpoint_for("registry.actuaria.io"), Some(market()));
        assert!(!is_market("ghcr.io"));
        assert!(!is_market("registry.actuaria.io.example"));
    }

    #[test]
    fn a_requested_version_resolves_to_its_canonical_name_and_its_organisations_repository() {
        let c = CanonicalName::requested("underwriting-starter", "0.2").unwrap();
        assert_eq!(c.to_string(), "actuaria/underwriting-starter:0.2");
        assert_eq!(c.repository("acme"), "acme/actuaria/underwriting-starter");
        let c = CanonicalName::requested("other-pub/underwriting-starter", "0.2").unwrap();
        assert_eq!(c.to_string(), "other-pub/underwriting-starter:0.2");
        assert_eq!(c.repository("acme"), "acme/other-pub/underwriting-starter");
        for bad in ["a/b/c", "Upper", "/listing", "pub/", ""] {
            assert!(CanonicalName::requested(bad, "0.2").is_err(), "{bad}");
        }
        assert!(CanonicalName::requested("listing", "not a tag").is_err());
    }

    #[test]
    fn the_organisation_comes_from_the_flag_then_the_environment_then_whoami() {
        let never = || -> Result<Option<String>> { panic!("whoami is asked only when nothing names the organisation") };
        assert_eq!(org_from(Some("acme"), Some("other"), never).unwrap(), "acme");
        assert_eq!(org_from(None, Some("other"), never).unwrap(), "other");
        assert_eq!(org_from(Some(" "), Some("other"), never).unwrap(), "other");
        assert_eq!(org_from(None, None, || Ok(Some("signed-in".into()))).unwrap(), "signed-in");
        let err = org_from(None, None, signed_in_org).unwrap_err().to_string();
        assert!(err.contains("--org") && err.contains(ORG_ENV), "{err}");
        assert!(org_from(Some("unlisted"), None, never).unwrap_err().to_string().contains("reserved"));
        assert!(org_from(Some("Acme/x"), None, never).unwrap_err().to_string().contains("not an organisation name"));
    }

    #[test]
    fn a_listing_must_name_what_was_asked_for() {
        let listing = |name: &str, version: &str, publisher: Option<&str>| Listing {
            name: name.into(),
            version: version.into(),
            publisher: publisher.map(String::from),
        };
        let r = "registry.actuaria.io/acme/actuaria/starter@sha256:00";
        let ok = listing("starter", "0.2", Some("actuaria"));
        check_listing(&ok, "starter", "0.2", Some("actuaria"), r).unwrap();
        check_listing(&listing("starter", "0.2", None), "starter", "0.2", None, r).unwrap();

        let err = |l: &Listing, publisher: Option<&str>| check_listing(l, "starter", "0.2", publisher, r).unwrap_err().to_string();
        let e = err(&listing("other", "0.2", Some("actuaria")), Some("actuaria"));
        assert!(
            e.contains("another listing") && e.contains("names actuaria/other:0.2") && e.contains("actuaria/starter:0.2 was asked for"),
            "{e}"
        );
        assert!(e.contains("Nothing was unpacked"), "{e}");
        let e = err(&listing("starter", "0.1", Some("actuaria")), Some("actuaria"));
        assert!(e.contains("another version") && e.contains("actuaria/starter:0.1"), "{e}");
        assert!(err(&listing("starter", "0.2", Some("mallory")), Some("actuaria")).contains("another publisher's listing"));
        assert!(err(&listing("starter", "0.2", None), Some("actuaria")).contains("names no publisher"));
        assert!(err(&listing("starter", "0.1", None), None).contains("another version"));

        assert_eq!(
            parse_listing(br#"{"schema_version": 1, "name": "starter", "version": "0.2", "title": "t"}"#).unwrap(),
            listing("starter", "0.2", None)
        );
        assert!(parse_listing(br#"{"name": "starter"}"#).is_err());
    }

    #[test]
    fn a_challenge_must_name_the_pinned_realm() {
        let m = market();
        let realm = |r: &str| format!(r#"Bearer realm="{r}",service="registry.actuaria.io",scope="repository:a/b/c:pull""#);
        assert_eq!(challenge_service(&m, &[&realm(MARKET_TOKEN_URL)]).unwrap().as_deref(), Some("registry.actuaria.io"));
        assert_eq!(challenge_service(&m, &[&format!(r#"bearer realm="{MARKET_TOKEN_URL}""#)]).unwrap(), None);
        let err = |headers: &[&str]| challenge_service(&m, headers).unwrap_err().to_string();
        assert!(err(&[&realm("https://evil.example/token")]).contains("another token endpoint, https://evil.example/token"));
        assert!(err(&[&realm("https://registry.actuaria.io/token/../evil")]).contains("another token endpoint"));
        assert!(err(&[&realm("http://registry.actuaria.io/token")]).contains("another token endpoint"), "the scheme is pinned too");
        assert!(err(&[&realm(MARKET_TOKEN_URL), &realm("https://evil.example/token")]).contains("another token endpoint"));
        assert!(err(&[r#"Basic realm="registry""#]).contains("Basic authentication"));
        assert!(err(&[r#"Bearer service="registry.actuaria.io""#]).contains("names no token endpoint"));
        assert!(err(&[]).contains("did not ask for a token"));
    }

    #[tokio::test]
    async fn a_registry_token_from_the_docker_config_is_used_as_it_is_and_no_credentials_are_refused() {
        // Neither case asks the registry or the token endpoint anything, so these hosts are never contacted.
        let m = Endpoint { registry: "registry.invalid".into(), token_url: "https://registry.invalid/token".into() };
        assert_eq!(registry_auth(&m, "a/b/c", RegistryAuth::Bearer("t".into())).await.unwrap(), RegistryAuth::Bearer("t".into()));
        let err = registry_auth(&m, "a/b/c", RegistryAuth::Anonymous).await.unwrap_err().to_string();
        assert!(err.contains("no credentials"), "{err}");
    }
}
