//! Registry credentials from the docker config file, which `docker login` and `oras login` write:
//! `$DOCKER_CONFIG/config.json`, else `~/.docker/config.json`. A registry's credential helper (`credHelpers`, else
//! `credsStore`) is asked first, then its `auths` entry (`auth`, `username` and `password`, or `registrytoken`).
//! A registry with no credentials is used anonymously.

use anyhow::{Context, Result, bail};
use base64::Engine;
use oci_client::secrets::RegistryAuth;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DOCKER_HUB_KEY: &str = "https://index.docker.io/v1/";
const DOCKER_HUB_HOSTS: &[&str] = &["docker.io", "index.docker.io", "registry-1.docker.io"];
const IDENTITY_TOKEN_USERNAME: &str = "<token>";

#[derive(Debug, Default, Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: BTreeMap<String, AuthEntry>,
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
    #[serde(default, rename = "credHelpers")]
    cred_helpers: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct AuthEntry {
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    identitytoken: Option<String>,
    #[serde(default)]
    registrytoken: Option<String>,
}

#[derive(Deserialize)]
struct HelperCredentials {
    #[serde(rename = "Username")]
    username: String,
    #[serde(rename = "Secret")]
    secret: String,
}

/// `$DOCKER_CONFIG/config.json`, else `~/.docker/config.json`.
pub fn docker_config_path() -> Option<PathBuf> {
    match std::env::var_os("DOCKER_CONFIG") {
        Some(dir) => Some(PathBuf::from(dir).join("config.json")),
        None => std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".docker/config.json")),
    }
}

/// The registry host a config key names: `https://ghcr.io/v1/` and `ghcr.io` are both `ghcr.io`, and Docker
/// Hub's names are all `index.docker.io`.
fn host(key: &str) -> &str {
    let host = key.strip_prefix("https://").or_else(|| key.strip_prefix("http://")).unwrap_or(key);
    let host = host.split('/').next().unwrap_or_default();
    if DOCKER_HUB_HOSTS.contains(&host) { "index.docker.io" } else { host }
}

/// How to authenticate to `registry`, from the docker config at `config` (anonymous when there is none).
pub fn registry_auth(config: Option<&Path>, registry: &str) -> Result<RegistryAuth> {
    let Some(path) = config else { return Ok(RegistryAuth::Anonymous) };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RegistryAuth::Anonymous),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let config: DockerConfig = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    auth_from(&config, registry, &run_credential_helper).with_context(|| format!("credentials for {registry} in {}", path.display()))
}

type HelperRunner = dyn Fn(&str, &str) -> Result<Option<HelperCredentials>>;

fn auth_from(config: &DockerConfig, registry: &str, run_helper: &HelperRunner) -> Result<RegistryAuth> {
    let wanted = host(registry);
    let helper = config.cred_helpers.iter().find(|(k, _)| host(k) == wanted).map(|(_, h)| h).or(config.creds_store.as_ref());
    if let Some(helper) = helper {
        let server = if wanted == "index.docker.io" { DOCKER_HUB_KEY } else { wanted };
        if let Some(c) = run_helper(helper, server)? {
            if c.username == IDENTITY_TOKEN_USERNAME {
                bail!(
                    "docker-credential-{helper} holds an identity token, which hbench can't use; log in with a username and a password or access token"
                );
            }
            return Ok(RegistryAuth::Basic(c.username, c.secret));
        }
    }
    let Some(entry) = config.auths.iter().find(|(k, _)| host(k) == wanted).map(|(_, e)| e) else {
        return Ok(RegistryAuth::Anonymous);
    };
    if let Some(encoded) = entry.auth.as_deref().filter(|a| !a.is_empty()) {
        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded).context("auths: auth is not base64")?;
        let decoded = String::from_utf8(decoded).context("auths: auth is not UTF-8")?;
        let (user, password) = decoded.split_once(':').context("auths: auth is not <username>:<password>")?;
        return Ok(RegistryAuth::Basic(user.to_string(), password.to_string()));
    }
    if let (Some(user), Some(password)) = (&entry.username, &entry.password) {
        return Ok(RegistryAuth::Basic(user.clone(), password.clone()));
    }
    if let Some(token) = &entry.registrytoken {
        return Ok(RegistryAuth::Bearer(token.clone()));
    }
    if entry.identitytoken.is_some() {
        bail!("auths holds an identity token, which hbench can't use; log in with a username and a password or access token");
    }
    Ok(RegistryAuth::Anonymous)
}

fn run_credential_helper(helper: &str, server: &str) -> Result<Option<HelperCredentials>> {
    credential_helper_get(Path::new(&format!("docker-credential-{helper}")), server)
}

/// `<program> get` for `server`: its credentials, or None when it has none for that server.
fn credential_helper_get(program: &Path, server: &str) -> Result<Option<HelperCredentials>> {
    let mut child = Command::new(program)
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {}", program.display()))?;
    child.stdin.take().context("credential helper stdin")?.write_all(server.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let message = String::from_utf8_lossy(if out.stdout.is_empty() { &out.stderr } else { &out.stdout }).trim().to_string();
        if message.to_lowercase().contains("credentials not found") {
            return Ok(None);
        }
        bail!("{} get failed: {message}", program.display());
    }
    Ok(Some(serde_json::from_slice(&out.stdout).with_context(|| format!("{} get printed no credentials", program.display()))?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use RegistryAuth::{Anonymous, Basic, Bearer};

    fn config(json: serde_json::Value) -> DockerConfig {
        serde_json::from_value(json).unwrap()
    }

    fn no_helper(_: &str, _: &str) -> Result<Option<HelperCredentials>> {
        panic!("no credential helper is configured")
    }

    fn basic(user: &str, password: &str) -> RegistryAuth {
        Basic(user.into(), password.into())
    }

    #[test]
    fn auths_entries_match_the_registry_host() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("publisher:s3cret:with-colon");
        let c = config(serde_json::json!({"auths": {
            "https://ghcr.io/v1/": {"auth": encoded},
            "localhost:5000": {"username": "local", "password": "pw"},
            "https://index.docker.io/v1/": {"registrytoken": "tok"},
            "identity.example": {"identitytoken": "refresh"},
            "empty.example": {},
        }}));
        let auth = |registry: &str| auth_from(&c, registry, &no_helper).unwrap();
        assert_eq!(auth("ghcr.io"), basic("publisher", "s3cret:with-colon"));
        assert_eq!(auth("localhost:5000"), basic("local", "pw"));
        assert_eq!(auth("registry-1.docker.io"), Bearer("tok".into()));
        assert_eq!(auth("localhost:5001"), Anonymous);
        assert_eq!(auth("empty.example"), Anonymous);
        let err = auth_from(&c, "identity.example", &no_helper).unwrap_err().to_string();
        assert!(err.contains("identity token"), "{err}");
        let bad = config(serde_json::json!({"auths": {"ghcr.io": {"auth": "%%%"}}}));
        assert!(auth_from(&bad, "ghcr.io", &no_helper).unwrap_err().to_string().contains("not base64"));
    }

    #[test]
    fn credential_helpers_come_before_auths() {
        let c = config(serde_json::json!({
            "auths": {"ghcr.io": {"username": "stale", "password": "stale"}, "quay.io": {"username": "q", "password": "q"}},
            "credsStore": "desktop",
            "credHelpers": {"ghcr.io": "gh"},
        }));
        let helper = |helper: &str, server: &str| -> Result<Option<HelperCredentials>> {
            Ok(match (helper, server) {
                ("gh", "ghcr.io") => Some(HelperCredentials { username: "from-gh".into(), secret: "gh-secret".into() }),
                ("desktop", DOCKER_HUB_KEY) => {
                    Some(HelperCredentials { username: IDENTITY_TOKEN_USERNAME.into(), secret: "refresh".into() })
                }
                ("desktop", _) => None,
                other => panic!("unexpected helper call {other:?}"),
            })
        };
        assert_eq!(auth_from(&c, "ghcr.io", &helper).unwrap(), basic("from-gh", "gh-secret"));
        assert_eq!(auth_from(&c, "quay.io", &helper).unwrap(), basic("q", "q"), "a store without the registry falls back to auths");
        assert!(auth_from(&c, "docker.io", &helper).unwrap_err().to_string().contains("identity token"));
    }

    #[test]
    fn a_credential_helper_is_asked_on_stdin() {
        let dir = std::env::temp_dir().join(format!("hbench-credentials-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let helper = dir.join("docker-credential-fake");
        let script = r#"#!/bin/sh
read -r server
case "$1:$server" in
  get:ghcr.io) echo '{"ServerURL":"ghcr.io","Username":"u","Secret":"s"}' ;;
  *) echo 'credentials not found in native keychain'; exit 1 ;;
esac
"#;
        std::fs::write(&helper, script).unwrap();
        std::fs::set_permissions(&helper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let still_being_written = |e: &anyhow::Error| {
            e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::ExecutableFileBusy))
        };
        let ask = |server: &str| loop {
            match credential_helper_get(&helper, server) {
                Err(e) if still_being_written(&e) => {}
                answer => break answer.unwrap(),
            }
        };
        let got = ask("ghcr.io").unwrap();
        assert_eq!((got.username.as_str(), got.secret.as_str()), ("u", "s"));
        assert!(ask("quay.io").is_none());
        assert!(credential_helper_get(&dir.join("missing"), "ghcr.io").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_config_is_anonymous() {
        let missing = std::env::temp_dir().join(format!("hbench-no-docker-config-{}.json", uuid::Uuid::new_v4().simple()));
        assert_eq!(registry_auth(Some(&missing), "ghcr.io").unwrap(), Anonymous);
        assert_eq!(registry_auth(None, "ghcr.io").unwrap(), Anonymous);
    }
}
