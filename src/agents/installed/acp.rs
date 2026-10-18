//! Generic ACP agent.
//! Installs a registry (or git-source) ACP server plus the bundled `acp_runner.py`, which
//! drives one prompt over the Agent Client Protocol and writes acp-summary.json.

use super::common::{installed_dir, nvm_node_install_snippet};
use super::pins;
use super::{exec_agent, exec_root};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const RUNNER: &str = include_str!("../../../assets/agents/acp/acp_runner.py");
const OUTPUT_FILENAME: &str = "acp.txt";
const SUMMARY_FILENAME: &str = "acp-summary.json";
const EVENTS_FILENAME: &str = "acp-events.jsonl";
const SOURCE_PROVENANCE_FILENAME: &str = "acp-source-provenance.json";
const LAUNCHER_REMOTE_PATH: &str = "/installed-agent/acp-launch.sh";
const RUNNER_REMOTE_PATH: &str = "/installed-agent/acp_runner.py";
const RUNNER_VENV_PATH: &str = "/opt/hbench-acp-venv";
const RUNNER_PYTHON_VERSION: &str = "3.12";
const PYTHON_INSTALL_DIR: &str = "/opt/hbench-acp-python";
const UV_INSTALL_DIR: &str = "/opt/hbench-acp-tools";
const BINARY_INSTALL_DIR: &str = "/opt/hbench-acp-agent";
const SOURCE_TOKEN_ENV: &str = "HBENCH_ACP_SOURCE_GIT_TOKEN";
/// Older name, still honoured on the host.
const LEGACY_SOURCE_TOKEN_ENV: &str = "HBENCH_ACP_SOURCE_GIT_TOKEN";
const MAX_SOURCE_FILES: usize = 10_000;
const MAX_SOURCE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;

// `acp:<id>[@version]` agent names resolve to the generic ACP agent with `registry_spec=<id>[@version]`.

pub const ACP_SHORTHAND_PREFIX: &str = "acp:";
pub const DEFAULT_REGISTRY_REF: &str = "main";
pub const DEFAULT_REGISTRY_CACHE_DIR: &str = ".cache/acp-registry";

#[allow(dead_code)] // the factory (mod.rs) does not route `acp:<id>` names yet
pub fn is_acp_registry_shorthand(agent_name: &str) -> bool {
    agent_name.starts_with(ACP_SHORTHAND_PREFIX)
}

#[allow(dead_code)] // the factory (mod.rs) does not route `acp:<id>` names yet
pub fn registry_spec_from_agent_name(agent_name: &str) -> Result<String> {
    match agent_name.strip_prefix(ACP_SHORTHAND_PREFIX) {
        Some(s) => Ok(s.to_string()),
        None => bail!("ACP registry shorthand must start with {ACP_SHORTHAND_PREFIX}"),
    }
}

#[allow(dead_code)] // the factory (mod.rs) does not route `acp:<id>` names yet
/// Factory path for `acp:<spec>` names: the ACP agent with `registry_spec` set.
pub fn create_from_agent_name(agent_name: &str, mut args: AgentArgs) -> Result<AgentBox> {
    args.kwargs.insert("registry_spec".into(), Value::String(registry_spec_from_agent_name(agent_name)?));
    create(args)
}

/// `[acp:]<id>[@version]` -> (id, version).
pub fn parse_registry_spec(spec: &str) -> Result<(String, Option<String>)> {
    let s = spec.trim();
    let s = s.strip_prefix(ACP_SHORTHAND_PREFIX).unwrap_or(s);
    let (id, version) = match s.split_once('@') {
        Some((id, v)) => (id, Some(v)),
        None => (s, None),
    };
    if id.is_empty() {
        bail!("ACP registry spec must include an agent id");
    }
    if version == Some("") {
        bail!("ACP registry version cannot be empty");
    }
    Ok((id.to_string(), version.map(String::from)))
}

/// GET a JSON document with GitHub headers (30s timeout), via host curl.
pub fn fetch_json(url: &str) -> Result<Value> {
    let mut c = std::process::Command::new("curl");
    c.args(["-fsSL", "--max-time", "30", "-H", "Accept: application/vnd.github+json", "-H", "User-Agent: hbench-acp-registry"]);
    if let Some(t) = std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()) {
        c.args(["-H", &format!("Authorization: Bearer {t}")]);
    }
    let out = c.arg(url).output().context("running curl")?;
    if !out.status.success() {
        bail!("Failed to fetch {url}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

fn entry_payload(fetch: &dyn Fn(&str) -> Result<Value>, id: &str, revision: &str) -> Result<Value> {
    let p = fetch(&format!("https://raw.githubusercontent.com/agentclientprotocol/registry/{revision}/{id}/agent.json"))?;
    if !p.is_object() {
        bail!("Unexpected ACP registry entry payload for {id}");
    }
    Ok(p)
}

fn entry_version(p: &Value) -> Result<String> {
    match p["version"].as_str() {
        Some(v) if !v.trim().is_empty() => Ok(v.to_string()),
        _ => bail!("ACP registry entry is missing a valid version"),
    }
}

fn write_entry(p: &Value, path: &Path) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, serde_json::to_string_pretty(p)? + "\n")?;
    Ok(())
}

/// urllib's `quote(s, safe="")`.
fn url_quote(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Fetch the registry entry for `spec`; a pinned version is searched in the entry's git history.
pub fn resolve_registry_entry_payload(
    spec: &str,
    registry_ref: &str,
    cache_dir: &Path,
    fetch: &dyn Fn(&str) -> Result<Value>,
) -> Result<Value> {
    let (id, requested) = parse_registry_spec(spec)?;
    let cache_dir = std::path::absolute(cache_dir)?;
    let latest = entry_payload(fetch, &id, registry_ref)?;
    let latest_version = entry_version(&latest)?;
    let Some(requested) = requested.filter(|v| *v != latest_version) else {
        write_entry(&latest, &cache_dir.join(registry_ref).join(&id).join("agent.json"))?;
        return Ok(latest);
    };
    let path = url_quote(&format!("{id}/agent.json"));
    let mut shas = vec![];
    for page in 1.. {
        let p = fetch(&format!("https://api.github.com/repos/agentclientprotocol/registry/commits?path={path}&per_page=100&page={page}"))?;
        let Some(items) = p.as_array() else { bail!("Unexpected ACP registry commit payload for {id}") };
        if items.is_empty() {
            break;
        }
        shas.extend(items.iter().filter_map(|i| i["sha"].as_str()).filter(|s| !s.is_empty()).map(String::from));
    }
    for sha in shas {
        let p = entry_payload(fetch, &id, &sha)?;
        if entry_version(&p)? == requested {
            write_entry(&p, &cache_dir.join("versions").join(&id).join(&requested).join("agent.json"))?;
            return Ok(p);
        }
    }
    bail!("ACP registry agent version not found: {id}@{requested}")
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinaryTarget {
    pub archive: String,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PackageDistribution {
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LocalTarget {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Distribution {
    pub binary: Option<BTreeMap<String, BinaryTarget>>,
    pub npx: Option<PackageDistribution>,
    pub uvx: Option<PackageDistribution>,
    pub local: Option<LocalTarget>,
}

#[allow(dead_code)] // name/description are required by the schema only
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub distribution: Distribution,
}

fn validate_env(env: &BTreeMap<String, String>) -> Result<()> {
    let re = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap();
    if env.keys().any(|k| !re.is_match(k)) {
        bail!("ACP launcher env keys must be POSIX-compatible shell variable names");
    }
    Ok(())
}

fn normalize_checksum(c: &str) -> Result<String> {
    let n = c.trim();
    let n = n.strip_prefix("sha256:").unwrap_or(n);
    if n.len() != 64 || !n.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("Binary archive checksum must be a SHA-256 hex digest or 'sha256:<digest>'");
    }
    Ok(n.to_lowercase())
}

impl RegistryEntry {
    /// Parse + apply the pydantic validators (normalizes checksum and local cmd).
    pub fn from_value(v: Value) -> Result<RegistryEntry> {
        let mut e: RegistryEntry = serde_json::from_value(v).map_err(|e| anyhow!("Invalid ACP registry entry: {e}"))?;
        let d = &mut e.distribution;
        for t in d.binary.iter_mut().flat_map(|m| m.values_mut()) {
            if !t.archive.starts_with("https://") {
                bail!("Binary archive URL must use HTTPS");
            }
            validate_env(&t.env)?;
            t.checksum = t.checksum.as_deref().map(normalize_checksum).transpose()?;
        }
        for p in [&d.npx, &d.uvx].into_iter().flatten() {
            validate_env(&p.env)?;
        }
        if let Some(l) = &mut d.local {
            if l.cmd.trim().is_empty() {
                bail!("ACP local distribution cmd cannot be empty");
            }
            l.cmd = l.cmd.trim().to_string();
            validate_env(&l.env)?;
        }
        if d.binary.is_none() && d.npx.is_none() && d.uvx.is_none() && d.local.is_none() {
            bail!("ACP registry entry must define at least one distribution");
        }
        Ok(e)
    }
}

/// A selected distribution: kind plus the target it launches.
#[derive(Debug, Clone)]
enum Target {
    Binary(BinaryTarget),
    Npx(PackageDistribution),
    Uvx(PackageDistribution),
    Local(LocalTarget),
}

impl Target {
    fn kind(&self) -> &'static str {
        match self {
            Target::Binary(_) => "binary",
            Target::Npx(_) => "npx",
            Target::Uvx(_) => "uvx",
            Target::Local(_) => "local",
        }
    }
}

fn binary_command_name(cmd: &str) -> String {
    Path::new(cmd).file_name().map(|n| n.to_string_lossy().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "acp-agent".into())
}

fn default_source_dir() -> String {
    ".".into()
}
fn default_manifest_path() -> String {
    "hbench-agent.json".into()
}
fn default_lockfile() -> String {
    "uv.lock".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpSource {
    pub repo_url: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    #[serde(default = "default_source_dir")]
    pub source_dir: String,
    #[serde(default = "default_manifest_path")]
    pub manifest_path: String,
    #[serde(default)]
    pub manifest_sha256: Option<String>,
}

fn normalize_relative_path(v: &str, label: &str) -> Result<()> {
    if v.is_empty() || v.contains('\0') || v.contains('\\') {
        bail!("{label} must be a normalized POSIX relative path");
    }
    if v.starts_with('/') || v.split('/').any(|p| p == "..") {
        bail!("{label} must remain under the agent source root");
    }
    let parts: Vec<&str> = v.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    let normalized = if parts.is_empty() { ".".to_string() } else { parts.join("/") };
    if normalized != v || normalized.starts_with("./") {
        bail!("{label} must be normalized");
    }
    Ok(())
}

impl AcpSource {
    fn validate(&self) -> Result<()> {
        let u = &self.repo_url;
        if u.len() > 512 || u.contains('\0') || u != u.trim() {
            bail!("repo_url must be a trimmed URL of at most 512 chars");
        }
        if !(u.starts_with("https://") || u.starts_with("ssh://")) {
            bail!("repo_url must use https:// or ssh:// (file:// and other transports are not allowed)");
        }
        if netloc(u).contains('@') {
            bail!("repo_url must not contain embedded credentials");
        }
        let r = &self.git_ref;
        if r.is_empty() || r.len() > 256 || r.starts_with('-') || r.chars().any(char::is_whitespace) || r.contains('\0') {
            bail!("ref must be a non-empty git ref or commit that does not start with '-' and contains no whitespace");
        }
        normalize_relative_path(&self.source_dir, "source_dir")?;
        normalize_relative_path(&self.manifest_path, "manifest_path")?;
        if let Some(s) = &self.manifest_sha256
            && !Regex::new(r"^[0-9a-f]{64}$").unwrap().is_match(s)
        {
            bail!("manifest_sha256 must be 64 lowercase hex characters");
        }
        Ok(())
    }
    fn is_pinned_commit(&self) -> bool {
        Regex::new(r"^[0-9a-f]{7,40}$").unwrap().is_match(&self.git_ref)
    }
    fn repo_name(&self) -> String {
        let n = self.repo_url.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let n = n.strip_suffix(".git").unwrap_or(n);
        if n.is_empty() { "source".into() } else { n.to_string() }
    }
}

/// `scheme://netloc/...` -> netloc.
fn netloc(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split(['/', '?', '#']).next().unwrap_or("")
}

#[derive(Debug, Clone, Deserialize)]
pub struct UvRuntime {
    pub kind: String,
    pub python: String,
    #[serde(default = "default_source_dir")]
    pub project: String,
    #[serde(default = "default_lockfile")]
    pub lockfile: String,
    pub entrypoint: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceManifest {
    pub schema_version: i64,
    pub id: String,
    pub version: String,
    pub protocol: String,
    pub runtime: UvRuntime,
}

impl SourceManifest {
    fn parse(bytes: &[u8]) -> Result<SourceManifest> {
        let m: SourceManifest = serde_json::from_slice(bytes).map_err(|e| anyhow!("Invalid ACP source manifest: {e}"))?;
        let rt = &m.runtime;
        if m.schema_version != 1 || m.protocol != "acp" || rt.kind != "python-uv" || rt.python != "3.12" || rt.lockfile != "uv.lock" {
            bail!("Invalid ACP source manifest: unsupported schema_version/protocol/runtime");
        }
        if !Regex::new(r"^[A-Za-z0-9](?:[A-Za-z0-9._-]{0,98}[A-Za-z0-9])?$").unwrap().is_match(&m.id) {
            bail!("manifest id must be a 1-100 character alphanumeric slug using only '.', '_', or '-' separators");
        }
        if m.version.is_empty() || m.version.chars().count() > 100 {
            bail!("Invalid ACP source manifest: version must be 1-100 characters");
        }
        normalize_relative_path(&rt.project, "project")?;
        normalize_relative_path(&rt.lockfile, "lockfile")?;
        if rt.entrypoint.is_empty() || rt.entrypoint.len() > 32 {
            bail!("Invalid ACP source manifest: runtime.entrypoint must have 1-32 items");
        }
        if rt.entrypoint.iter().any(|a| a.is_empty() || a.chars().count() > 1024 || a.contains('\0')) {
            bail!("runtime.entrypoint arguments must be non-empty, NUL-free, and at most 1024 characters");
        }
        let cmd = &rt.entrypoint[0];
        if cmd.starts_with('/') || cmd.split('/').filter(|p| !p.is_empty() && *p != ".").count() != 1 {
            bail!("runtime.entrypoint[0] must be a command name resolved from the project virtual environment");
        }
        Ok(m)
    }
}

/// Reject symlinks/special files and oversize trees; return the verified manifest and its sha256.
fn load_verified_manifest(source: &AcpSource, checkout: &Path) -> Result<(SourceManifest, String)> {
    let (mut files, mut bytes) = (0usize, 0u64);
    let mut stack = vec![checkout.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)? {
            let p = e?.path();
            let md = std::fs::symlink_metadata(&p)?;
            if md.file_type().is_symlink() {
                bail!("ACP agent source must not contain symlinks");
            }
            if md.is_dir() {
                stack.push(p);
                continue;
            }
            if !md.is_file() {
                bail!("ACP agent source must contain only regular files");
            }
            files += 1;
            bytes += md.len();
            if files > MAX_SOURCE_FILES {
                bail!("ACP agent source exceeds the file-count limit");
            }
            if bytes > MAX_SOURCE_BYTES {
                bail!("ACP agent source exceeds the size limit");
            }
        }
    }
    let root = checkout.join(&source.source_dir).canonicalize().map_err(|_| anyhow!("ACP source_dir does not exist in the repository"))?;
    if !root.starts_with(checkout) || !root.is_dir() {
        bail!("ACP source_dir does not exist in the repository");
    }
    let manifest = root.join(&source.manifest_path);
    if !manifest.is_file() {
        bail!("ACP source manifest does not exist in the repository");
    }
    let raw = std::fs::read(&manifest)?;
    if raw.len() > MAX_MANIFEST_BYTES {
        bail!("ACP source manifest exceeds 64 KiB");
    }
    let digest = crate::task::hex(&Sha256::digest(&raw));
    if source.manifest_sha256.as_ref().is_some_and(|d| *d != digest) {
        bail!("ACP source manifest digest does not match manifest_sha256");
    }
    Ok((SourceManifest::parse(&raw)?, digest))
}

/// Validate project/lockfile layout; return the remote project dir.
fn source_remote_project(manifest: &SourceManifest, source_root: &Path) -> Result<String> {
    let rt = &manifest.runtime;
    let project = source_root.join(&rt.project).canonicalize().map_err(|_| anyhow!("ACP manifest project directory does not exist"))?;
    if !project.starts_with(source_root) {
        bail!("ACP project must remain under source_dir");
    }
    if !project.is_dir() {
        bail!("ACP manifest project directory does not exist");
    }
    if !project.join("pyproject.toml").is_file() {
        bail!("ACP source project must contain pyproject.toml");
    }
    if !project.join(&rt.lockfile).is_file() {
        bail!("ACP source project lockfile does not exist");
    }
    Ok(if rt.project == "." { "/installed-agent/source".into() } else { format!("/installed-agent/source/{}", rt.project) })
}

fn source_launcher_script(manifest: &SourceManifest, remote_project: &str) -> String {
    let entry = manifest.runtime.entrypoint.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    format!(
        "#!/usr/bin/env sh\nset -eu\ncd {}\nPATH={}:$PATH\nexport PATH\nexec {entry} \"$@\"\n",
        quote(remote_project),
        quote(&format!("{remote_project}/.venv/bin"))
    )
}

/// GIT_ASKPASS override that feeds HBENCH_ACP_SOURCE_GIT_TOKEN (github.com https only).
fn source_askpass_env(askpass_dir: &Path, repo_url: &str, token: Option<String>) -> Result<BTreeMap<String, String>> {
    let Some(token) = token.filter(|t| !t.is_empty()) else { return Ok(BTreeMap::new()) };
    let host = netloc(repo_url).rsplit('@').next().unwrap_or("").to_lowercase();
    let (hostname, port) = match host.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), Some(p.to_string())),
        None => (host.clone(), None),
    };
    if !repo_url.starts_with("https://") || hostname != "github.com" || !matches!(port.as_deref(), None | Some("443")) {
        bail!("{SOURCE_TOKEN_ENV} can authenticate only https://github.com repositories");
    }
    let script = askpass_dir.join("acp-source-askpass.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  *Username*|*username*) printf '%s\\n' 'x-access-token' ;;\n  *Password*|*password*) printf '%s\\n' \"${SOURCE_TOKEN_ENV}\" ;;\n  *) exit 1 ;;\nesac\n"
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(BTreeMap::from([("GIT_ASKPASS".into(), script.to_string_lossy().to_string()), (SOURCE_TOKEN_ENV.into(), token)]))
}

const SCOPED_GIT_ENV_VARS: &[&str] = &[
    "GIT_ASKPASS",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_SYSTEM",
    "GIT_PROXY_COMMAND",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_SSH_VARIANT",
    "SSH_ASKPASS",
    "SSH_ASKPASS_REQUIRE",
];

async fn run_git(args: &[&str], cwd: Option<&Path>, extra_env: &BTreeMap<String, String>) -> Result<String> {
    let mut c = tokio::process::Command::new("git");
    c.args(args)
        .env_remove(SOURCE_TOKEN_ENV)
        .env_remove(LEGACY_SOURCE_TOKEN_ENV)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1");
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    if !extra_env.is_empty() {
        for (k, _) in std::env::vars() {
            if SCOPED_GIT_ENV_VARS.contains(&k.as_str()) || k.starts_with("GIT_CONFIG_KEY_") || k.starts_with("GIT_CONFIG_VALUE_") {
                c.env_remove(&k);
            }
        }
        c.env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null").envs(extra_env);
    }
    let out = c.output().await.context("running git")?;
    if !out.status.success() {
        let raw = if out.stderr.is_empty() { &out.stdout } else { &out.stderr };
        let d = String::from_utf8_lossy(raw).trim().to_string();
        let tail: String = d.chars().rev().take(500).collect::<Vec<_>>().into_iter().rev().collect();
        bail!("git command failed (exit {}): {tail}", out.status.code().unwrap_or(-1));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Fetch `source.ref` into `checkout` (depth 1), strip .git, return the commit SHA.
async fn fetch_source(source: &AcpSource, checkout: &Path) -> Result<String> {
    let askpass = std::env::temp_dir().join(format!("hbench-acp-askpass-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&askpass)?;
    let r = async {
        let fetch_env = source_askpass_env(
            &askpass,
            &source.repo_url,
            std::env::var(SOURCE_TOKEN_ENV).or_else(|_| std::env::var(LEGACY_SOURCE_TOKEN_ENV)).ok(),
        )?;
        let none = BTreeMap::new();
        run_git(&["init", "--quiet", &checkout.to_string_lossy()], None, &none).await?;
        run_git(&["remote", "add", "origin", "--", &source.repo_url], Some(checkout), &none).await?;
        run_git(
            &["-c", "protocol.file.allow=never", "fetch", "--quiet", "--depth", "1", "--no-tags", "origin", &source.git_ref],
            Some(checkout),
            &fetch_env,
        )
        .await?;
        run_git(&["checkout", "--quiet", "--detach", "FETCH_HEAD"], Some(checkout), &none).await?;
        run_git(&["rev-parse", "HEAD"], Some(checkout), &none).await
    }
    .await;
    let _ = std::fs::remove_dir_all(&askpass);
    let sha = r?;
    if source.is_pinned_commit() && !sha.starts_with(&source.git_ref) {
        bail!("Fetched ACP source commit does not match the pinned ref");
    }
    std::fs::remove_dir_all(checkout.join(".git"))?;
    Ok(sha)
}

pub struct Acp {
    args: AgentArgs,
    registry_spec: Option<String>,
    registry_ref: String,
    registry_cache_dir: PathBuf,
    entry: Option<RegistryEntry>,
    source: Option<AcpSource>,
    manifest: Option<SourceManifest>,
    commit_sha: Option<String>,
    manifest_digest: Option<String>,
    distribution_preference: Vec<String>,
    permission_mode: String,
    authenticate_method_id: Option<String>,
    auth_policy: String,
    target_platform: Option<String>,
    selected_kind: Option<&'static str>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(Acp::new(args)?))
}

fn parse_distribution_preference(v: Option<&Value>) -> Result<Vec<String>> {
    let raw: Vec<String> = match v {
        None | Some(Value::Null) => return Ok(["binary", "npx", "uvx", "local"].map(String::from).to_vec()),
        Some(Value::String(s)) => s.split(',').map(String::from).collect(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().map(String::from).ok_or_else(|| anyhow!("distribution_preference: expected strings")))
            .collect::<Result<_>>()?,
        Some(_) => bail!("distribution_preference: expected a string or list of strings"),
    };
    let values: Vec<String> = raw.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let mut invalid: Vec<&str> = values.iter().map(String::as_str).filter(|v| !["binary", "npx", "uvx", "local"].contains(v)).collect();
    if !invalid.is_empty() {
        invalid.sort();
        bail!("Unsupported ACP distribution preference(s): {}", invalid.join(", "));
    }
    if values.is_empty() {
        bail!("ACP distribution preference cannot be empty");
    }
    Ok(values)
}

impl Acp {
    pub fn new(args: AgentArgs) -> Result<Acp> {
        let kw = |k: &str| args.kwargs.get(k).filter(|v| !v.is_null());
        let permission_mode = args.kw("permission_mode").unwrap_or_else(|| "allow".into());
        if !["allow", "deny"].contains(&permission_mode.as_str()) {
            bail!("Invalid kwargs for agent 'acp':\n  permission_mode: Input should be 'allow' or 'deny'");
        }
        let registry_inputs = ["registry_spec", "registry_entry", "registry_entry_path"].iter().filter(|k| kw(k).is_some()).count();
        if registry_inputs > 1 {
            bail!("Provide only one of registry_spec, registry_entry, or registry_entry_path for the ACP agent");
        }
        if kw("source").is_some() && registry_inputs > 0 {
            bail!("ACP source cannot be combined with a registry distribution");
        }
        let registry_spec = args.kw("registry_spec").map(|s| s.strip_prefix(ACP_SHORTHAND_PREFIX).map(String::from).unwrap_or(s));
        let entry = match (kw("registry_entry"), args.kw("registry_entry_path")) {
            (None, None) => None,
            (_, Some(p)) => {
                let raw = std::fs::read_to_string(&p).map_err(|e| anyhow!("registry_entry_path {p}: {e}"))?;
                Some(RegistryEntry::from_value(serde_json::from_str(&raw)?)?)
            }
            (Some(Value::String(s)), None) => {
                let v: Value =
                    serde_json::from_str(s).map_err(|_| anyhow!("registry_entry must be a JSON object when passed as a string"))?;
                Some(RegistryEntry::from_value(v)?)
            }
            (Some(v @ Value::Object(_)), None) => Some(RegistryEntry::from_value(v.clone())?),
            (Some(_), None) => bail!("ACP agent requires registry_entry or registry_entry_path with an ACP registry record"),
        };
        let source = match kw("source") {
            None => None,
            Some(v) => {
                let s: AcpSource = serde_json::from_value(v.clone()).map_err(|e| anyhow!("Invalid ACP source: {e}"))?;
                s.validate()?;
                Some(s)
            }
        };
        if entry.is_none() && registry_spec.is_none() && source.is_none() {
            bail!("ACP agent requires a registry distribution or a git source");
        }
        let distribution_preference = parse_distribution_preference(kw("distribution_preference"))?;
        let auth_raw = args.kw("auth_policy").unwrap_or_else(|| "auto".into());
        let auth_policy = auth_raw.trim().to_lowercase();
        if !["auto", "explicit", "disabled"].contains(&auth_policy.as_str()) {
            bail!("Unsupported ACP auth policy: {auth_raw}. Valid values: auto, disabled, explicit");
        }
        Ok(Acp {
            registry_ref: args.kw("registry_ref").unwrap_or_else(|| DEFAULT_REGISTRY_REF.into()),
            registry_cache_dir: PathBuf::from(args.kw("registry_cache_dir").unwrap_or_else(|| DEFAULT_REGISTRY_CACHE_DIR.into())),
            authenticate_method_id: args.kw("authenticate_method_id"),
            target_platform: args.kw("target_platform"),
            registry_spec,
            entry,
            source,
            manifest: None,
            commit_sha: None,
            manifest_digest: None,
            distribution_preference,
            permission_mode,
            auth_policy,
            selected_kind: None,
            args,
        })
    }

    fn identity(&self) -> (String, String) {
        if let Some(m) = &self.manifest {
            return (m.id.clone(), m.version.clone());
        }
        if let Some(s) = &self.source {
            return (format!("acp-source:{}", s.repo_name()), s.git_ref.clone());
        }
        if let Some(e) = &self.entry {
            return (e.id.clone(), e.version.clone());
        }
        (format!("acp:{}", self.registry_spec.as_deref().unwrap_or("")), "unknown".into())
    }

    fn source_provenance(&self) -> Value {
        let Some(s) = &self.source else { return Value::Null };
        json!({
            "repo_url": s.repo_url, "ref": s.git_ref, "commit_sha": self.commit_sha,
            "source_dir": s.source_dir, "manifest_path": s.manifest_path,
            "manifest_sha256": self.manifest_digest.clone().or(s.manifest_sha256.clone()),
        })
    }

    async fn ensure_registry_entry(&mut self) -> Result<()> {
        if self.entry.is_some() {
            return Ok(());
        }
        let Some(spec) = self.registry_spec.clone() else { bail!("ACP registry spec is not configured") };
        let (r, dir) = (self.registry_ref.clone(), self.registry_cache_dir.clone());
        let payload = tokio::task::spawn_blocking(move || resolve_registry_entry_payload(&spec, &r, &dir, &fetch_json)).await??;
        self.entry = Some(RegistryEntry::from_value(payload)?);
        Ok(())
    }

    fn normalize_platform(&self, system: &str, machine: &str) -> Result<String> {
        let s = system.trim().to_lowercase();
        let m = machine.trim().to_lowercase();
        let s = match s.as_str() {
            "windows_nt" | "mingw64_nt" | "msys_nt" | "cygwin_nt" => "windows",
            o => o,
        };
        let m = match m.as_str() {
            "amd64" => "x86_64",
            "arm64" => "aarch64",
            o => o,
        };
        let id = format!("{s}-{m}");
        if !["linux-x86_64", "linux-aarch64", "darwin-x86_64", "darwin-aarch64", "windows-x86_64", "windows-aarch64"].contains(&id.as_str())
        {
            bail!("Unsupported ACP platform '{id}' for registry entry {}", self.entry.as_ref().map(|e| e.id.as_str()).unwrap_or(""));
        }
        Ok(id)
    }

    fn select_distribution(&self, platform: &str) -> Result<Target> {
        let Some(e) = &self.entry else { bail!("ACP registry entry has not been resolved yet") };
        let d = &e.distribution;
        for kind in &self.distribution_preference {
            match kind.as_str() {
                "binary" => {
                    if let Some(t) = d.binary.as_ref().and_then(|b| b.get(platform)) {
                        return Ok(Target::Binary(t.clone()));
                    }
                }
                "npx" if d.npx.is_some() => return Ok(Target::Npx(d.npx.clone().unwrap())),
                "uvx" if d.uvx.is_some() => return Ok(Target::Uvx(d.uvx.clone().unwrap())),
                "local" if d.local.is_some() => return Ok(Target::Local(d.local.clone().unwrap())),
                _ => {}
            }
        }
        let mut avail = vec![];
        if let Some(b) = d.binary.as_ref().filter(|b| !b.is_empty()) {
            avail.push(format!("binary[{}]", b.keys().cloned().collect::<Vec<_>>().join(", ")));
        }
        for (n, present) in [("npx", d.npx.is_some()), ("uvx", d.uvx.is_some()), ("local", d.local.is_some())] {
            if present {
                avail.push(n.into());
            }
        }
        let avail = if avail.is_empty() { "none".into() } else { avail.join(", ") };
        bail!("No compatible ACP distribution found for {} on {platform}. Available: {avail}", e.id)
    }

    fn mcp_servers_payload(&self) -> Value {
        Value::Array(
            self.args
                .mcp_servers
                .iter()
                .filter_map(|s| match s.transport() {
                    "stdio" => Some(json!({"name": s.name, "command": s.command, "args": s.args, "env": []})),
                    "sse" => Some(json!({"type": "sse", "name": s.name, "url": s.url, "headers": []})),
                    "streamable-http" => Some(json!({"type": "http", "name": s.name, "url": s.url, "headers": []})),
                    _ => None,
                })
                .collect(),
        )
    }

    fn run_env(&self) -> BTreeMap<String, String> {
        let (id, version) = self.identity();
        let mut env = BTreeMap::from([
            ("HBENCH_ACP_MCP_SERVERS_JSON".to_string(), self.mcp_servers_payload().to_string()),
            ("HBENCH_ACP_PERMISSION_MODE".to_string(), self.permission_mode.clone()),
            ("HBENCH_ACP_AUTH_POLICY".to_string(), self.auth_policy.clone()),
            ("HBENCH_ACP_AGENT_ID".to_string(), id),
            ("HBENCH_ACP_AGENT_VERSION".to_string(), version),
        ]);
        if let Some(m) = self.authenticate_method_id.clone().filter(|m| !m.is_empty()) {
            env.insert("HBENCH_ACP_AUTHENTICATE_METHOD_ID".into(), m);
        }
        if let Some(m) = self.args.model_name.clone().filter(|m| !m.is_empty()) {
            env.insert("HBENCH_ACP_REQUESTED_MODEL".into(), m);
        }
        env
    }

    async fn upload_launcher_and_runner(&self, cx: &AgentCtx<'_>, launcher: &str) -> Result<()> {
        let local = cx.logs_dir.join("acp-launch.sh");
        std::fs::write(&local, launcher)?;
        cx.env.upload_file(&local, LAUNCHER_REMOTE_PATH).await?;
        let tmp = std::env::temp_dir().join(format!("hbench-acp-runner-{}.py", uuid::Uuid::new_v4().simple()));
        std::fs::write(&tmp, RUNNER)?;
        let r = cx.env.upload_file(&tmp, RUNNER_REMOTE_PATH).await;
        let _ = std::fs::remove_file(&tmp);
        r?;
        cx.env.exec_root(&format!("chmod +x {LAUNCHER_REMOTE_PATH} {RUNNER_REMOTE_PATH}"), None).await?;
        Ok(())
    }

    async fn install_source(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        let checkout = std::env::temp_dir().join(format!("hbench-acp-source-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&checkout)?;
        let checkout = checkout.canonicalize()?;
        let r = self.install_source_from_checkout(cx, &checkout).await;
        let _ = std::fs::remove_dir_all(&checkout);
        r
    }

    async fn install_source_from_checkout(&mut self, cx: &AgentCtx<'_>, checkout: &Path) -> Result<()> {
        let source = self.source.clone().unwrap();
        self.commit_sha = Some(fetch_source(&source, checkout).await?);
        let (manifest, digest) = load_verified_manifest(&source, checkout)?;
        self.manifest_digest = Some(digest);
        let prov = json!({"id": manifest.id, "version": manifest.version, "provenance": Value::Null});
        self.manifest = Some(manifest.clone());
        let mut prov = prov;
        prov["provenance"] = self.source_provenance();
        std::fs::write(cx.logs_dir.join(SOURCE_PROVENANCE_FILENAME), serde_json::to_string_pretty(&prov)?)?;

        let root = checkout.join(&source.source_dir).canonicalize()?;
        let remote_project = source_remote_project(&manifest, &root)?;
        let uid = exec_agent(cx, "id -u", &BTreeMap::new()).await?.stdout.trim().to_string();
        if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
            bail!("Could not determine the ACP execution user's numeric UID");
        }
        cx.env.upload_dir(&root, "/installed-agent/source").await?;
        exec_root(cx, &format!("chown -R {uid} /installed-agent/source && chmod -R u+rwX,go-w /installed-agent/source"), &BTreeMap::new())
            .await?;
        exec_root(cx, &dependencies_command("uvx"), &debian_env()).await?;
        let project = quote(&remote_project);
        let lockfile = quote(&format!("{remote_project}/{}", manifest.runtime.lockfile));
        exec_agent(
            cx,
            &format!(
                "test -f {project}/pyproject.toml && test -f {lockfile} && {RUNNER_VENV_PATH}/bin/uv sync --frozen \
                 --cache-dir /installed-agent/source/.uv-cache --project {project} --python {}",
                manifest.runtime.python
            ),
            &BTreeMap::new(),
        )
        .await?;
        self.upload_launcher_and_runner(cx, &source_launcher_script(&manifest, &remote_project)).await?;
        self.selected_kind = Some("source");
        Ok(())
    }

    async fn install(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        if self.source.is_some() {
            return self.install_source(cx).await;
        }
        self.ensure_registry_entry().await?;
        let platform = match &self.target_platform {
            Some(p) => p.clone(),
            None => {
                let r = cx.exec("uname -s && uname -m", &BTreeMap::new(), None).await?;
                if r.code != 0 || r.stdout.is_empty() {
                    bail!("Failed to detect ACP runtime platform inside environment");
                }
                let lines: Vec<&str> = r.stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
                if lines.len() < 2 {
                    bail!("Unexpected platform detection output: {:?}", r.stdout);
                }
                self.normalize_platform(lines[0], lines[1])?
            }
        };
        let target = self.select_distribution(&platform)?;
        self.selected_kind = Some(target.kind());
        exec_root(cx, &dependencies_command(target.kind()), &debian_env()).await?;
        match &target {
            Target::Npx(_) => {
                exec_agent(cx, &node_install_command(), &BTreeMap::new()).await?;
            }
            Target::Binary(b) => {
                exec_root(cx, &binary_install_command(b), &BTreeMap::new()).await?;
            }
            _ => {}
        }
        self.upload_launcher_and_runner(cx, &launcher_script(&target)).await
    }
}

fn debian_env() -> BTreeMap<String, String> {
    BTreeMap::from([("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())])
}

fn dependencies_command(kind: &str) -> String {
    let mut apt = vec!["tar"];
    let mut apk = vec!["tar"];
    let mut dnf = vec!["tar"];
    if kind == "binary" {
        apt.extend(["unzip", "bzip2", "xz-utils"]);
        apk.extend(["unzip", "bzip2", "xz"]);
        dnf.extend(["unzip", "bzip2", "xz"]);
    }
    if kind == "npx" {
        // glibc distros get Node via nvm (node_install_command); Alpine uses its packaged Node.
        apk.extend(["nodejs", "npm"]);
    }
    let (apt, apk, dnf) = (apt.join(" "), apk.join(" "), dnf.join(" "));
    let install_uv = if kind == "uvx" { "1" } else { "0" };
    let uv = format!("{UV_INSTALL_DIR}/uv");
    let py = format!("{RUNNER_VENV_PATH}/bin/python");
    let apt_fn = super::APT_INSTALL_FN;
    format!(
        "set -euo pipefail
{apt_fn}            export UV_PYTHON_INSTALL_DIR={PYTHON_INSTALL_DIR}
            export UV_CACHE_DIR=/opt/hbench-acp-bootstrap-cache
            if command -v apt-get >/dev/null 2>&1; then
              export DEBIAN_FRONTEND=noninteractive
              hb_apt_install python3 curl ca-certificates {apt}
            elif command -v apk >/dev/null 2>&1; then
              apk add --no-cache python3 curl ca-certificates {apk}
            elif command -v dnf >/dev/null 2>&1; then
              dnf install -y python3 curl ca-certificates {dnf}
            elif command -v yum >/dev/null 2>&1; then
              yum install -y python3 curl ca-certificates {dnf}
            else
              echo \"Unsupported package manager for ACP agent setup\" >&2
              exit 1
            fi

            # Task images may provide a Python too old for agent-client-protocol. Install uv
            # independently, then use it to provision the runner with a supported Python.
            if [ ! -x {uv} ]; then
              mkdir -p {UV_INSTALL_DIR}
              curl -LsSf {uv_installer} | env UV_INSTALL_DIR={UV_INSTALL_DIR} sh
            fi

            if [ ! -x {py} ] || ! {py} -c \"import acp, sys; raise SystemExit(sys.version_info < (3, 12))\" >/dev/null 2>&1; then
              rm -rf {RUNNER_VENV_PATH}
              mkdir -p {PYTHON_INSTALL_DIR}
              {uv} python install {RUNNER_PYTHON_VERSION}
              {uv} venv --python {RUNNER_PYTHON_VERSION} {RUNNER_VENV_PATH}
              {uv} pip install --python {py} {protocol}
            fi

            chmod -R a+rX {PYTHON_INSTALL_DIR} {RUNNER_VENV_PATH}

            if [ \"{install_uv}\" = \"1\" ] && [ ! -x {RUNNER_VENV_PATH}/bin/uvx ]; then
              {uv} pip install --python {py} {uv_pip}
            fi",
        uv_installer = pins::uv_installer(),
        protocol = pins::ACP_PROTOCOL.requirement(),
        uv_pip = pins::UV.requirement(),
    )
}

fn node_install_command() -> String {
    format!(
        "set -euo pipefail; if ldd --version 2>&1 | grep -qi musl || [ -f /etc/alpine-release ]; then node --version && npm --version; else {}; fi",
        nvm_node_install_snippet(22)
    )
}

fn binary_install_command(t: &BinaryTarget) -> String {
    let archive = quote(&t.archive);
    let checksum = quote(t.checksum.as_deref().unwrap_or(""));
    let name = quote(&binary_command_name(&t.cmd));
    let dir = quote(BINARY_INSTALL_DIR);
    format!(
        "set -euo pipefail
tmp_archive=\"$(mktemp)\"
expected_checksum={checksum}
rm -rf {dir}
mkdir -p {dir}/dist
curl -fsSL {archive} -o \"$tmp_archive\"
if [ -n \"$expected_checksum\" ]; then
  actual_checksum=\"$(python3 - \"$tmp_archive\" <<'PY'
import hashlib
import pathlib
import sys

print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)\"
  if [ \"$actual_checksum\" != \"$expected_checksum\" ]; then
    echo \"Checksum mismatch for ACP binary archive\" >&2
    echo \"expected: $expected_checksum\" >&2
    echo \"actual:   $actual_checksum\" >&2
    exit 1
  fi
fi
case {archive} in
  *.tar.gz|*.tgz)
    tar -xzf \"$tmp_archive\" -C {dir}/dist
    ;;
  *.tar.bz2|*.tbz2)
    tar -xjf \"$tmp_archive\" -C {dir}/dist
    ;;
  *.zip)
    unzip -q \"$tmp_archive\" -d {dir}/dist
    ;;
  *)
    cp \"$tmp_archive\" {dir}/dist/{name}
    chmod +x {dir}/dist/{name}
    ;;
esac
chmod -R a+rX {dir}
rm -f \"$tmp_archive\""
    )
}

fn launcher_script(target: &Target) -> String {
    let (env, parts): (&BTreeMap<String, String>, Vec<String>) = match target {
        Target::Binary(b) => {
            (&b.env, [vec![format!("{BINARY_INSTALL_DIR}/dist/{}", binary_command_name(&b.cmd))], b.args.clone()].concat())
        }
        Target::Npx(p) => (&p.env, [vec!["npx".into(), "-y".into(), p.package.clone()], p.args.clone()].concat()),
        Target::Uvx(p) => (&p.env, [vec![format!("{RUNNER_VENV_PATH}/bin/uvx"), p.package.clone()], p.args.clone()].concat()),
        Target::Local(l) => (&l.env, [vec![l.cmd.clone()], l.args.clone()].concat()),
    };
    let mut exports: String = env.iter().map(|(k, v)| format!("export {k}={}\n", quote(v))).collect();
    if matches!(target, Target::Npx(_)) {
        // Prefer the nvm-installed Node over any (older) system Node.
        exports = format!(
            "nvm_node_bin=\"$(ls -d \"${{NVM_DIR:-$HOME/.nvm}}\"/versions/node/*/bin 2>/dev/null | sort -V | tail -n 1)\"\n\
             if [ -n \"$nvm_node_bin\" ]; then PATH=\"$nvm_node_bin:$PATH\"; export PATH; fi\n{exports}"
        );
    }
    let cmd = parts.iter().map(|p| quote(p)).collect::<Vec<_>>().join(" ");
    format!("#!/usr/bin/env sh\nset -eu\n{exports}exec {cmd} \"$@\"\n")
}

/// Usage + metadata from the runner's acp-summary.json.
fn apply_summary(summary: &Value, ctx: &mut AgentContext, metadata_extra: Value) {
    let usage_update = summary.get("latest_usage_update").filter(|v| !v.is_null() && *v != &json!({})).cloned();
    let cost = usage_update.as_ref().map(|u| &u["cost"]).cloned().unwrap_or(Value::Null);
    if cost["currency"].as_str().is_some_and(|c| c.eq_ignore_ascii_case("USD"))
        && let Some(a) = cost["amount"].as_f64()
    {
        ctx.cost_usd = Some(a);
    }
    let usage = &summary["prompt_response"]["usage"];
    let n = |k: &str| usage[k].as_f64().map(|f| f as i64);
    if let Some(v) = n("inputTokens") {
        ctx.n_input_tokens = Some(v);
    }
    if let Some(v) = n("outputTokens") {
        ctx.n_output_tokens = Some(v);
    }
    if let Some(v) = n("cachedReadTokens") {
        ctx.n_cache_tokens = Some(v);
    }
    let g = |k: &str| summary.get(k).cloned().unwrap_or(Value::Null);
    let mut acp = json!({
        "auth_policy": g("auth_policy"),
        "selected_authenticate_method_id": g("selected_authenticate_method_id"),
        "authenticate_response": g("authenticate_response"),
        "requested_model": g("requested_model"),
        "resolved_session_model_id": g("resolved_session_model_id"),
        "set_model_response": g("set_model_response"),
        "set_model_error": g("set_model_error"),
        "set_model_attempts": g("set_model_attempts"),
        "initial_session_models": summary["session"].get("models").cloned().unwrap_or(Value::Null),
        "latest_session_info_update": g("latest_session_info_update"),
        "initialize": g("initialize"),
        "agent_info": g("agent_info"),
        "auth_methods": g("auth_methods"),
        "error": g("error"),
        "prompt_response": g("prompt_response"),
        "latest_usage_update": usage_update.unwrap_or(Value::Null),
        "events_file": EVENTS_FILENAME,
    });
    if let (Some(a), Value::Object(extra)) = (acp.as_object_mut(), metadata_extra) {
        a.extend(extra);
    }
    ctx.metadata = Some(json!({ "acp": acp }));
}

#[async_trait::async_trait]
impl Agent for Acp {
    fn name(&self) -> &str {
        "acp"
    }
    fn version(&self) -> Option<String> {
        self.manifest.as_ref().map(|m| m.version.clone()).or_else(|| self.entry.as_ref().map(|e| e.version.clone()))
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        if self.source.is_none() {
            self.ensure_registry_entry().await?;
        }
        installed_dir(cx).await?;
        // Non-RuntimeError install failures are wrapped; exec failures pass through classified.
        self.install(cx)
            .await
            .map_err(|e| if crate::env::error_kind(&e) == "RuntimeError" { anyhow!("Agent install failed: {e:#}") } else { e })
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let cmd = format!(
            "{RUNNER_VENV_PATH}/bin/python {RUNNER_REMOTE_PATH}     --instruction={}     --logs-dir=/logs/agent     \
             --launcher={LAUNCHER_REMOTE_PATH}     2>&1 | stdbuf -oL tee /logs/agent/{OUTPUT_FILENAME}",
            quote(&instruction)
        );
        exec_agent(cx, &cmd, &self.run_env()).await.map(|_| ())
    }

    fn populate_context_post_run(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        let Some(summary) = std::fs::read_to_string(logs_dir.join(SUMMARY_FILENAME))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .filter(Value::is_object)
        else {
            return;
        };
        let (id, version) = self.identity();
        let from_registry = self.source.is_none();
        let extra = json!({
            "registry_entry_id": if from_registry { Value::from(id) } else { Value::Null },
            "registry_entry_version": if from_registry { Value::from(version) } else { Value::Null },
            "source": self.source_provenance(),
            "selected_distribution": self.selected_kind,
        });
        apply_summary(&summary, ctx, extra);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;

    fn entry() -> Value {
        json!({
            "id": "codex-acp", "name": "Codex CLI", "version": "0.10.0", "description": "ACP adapter",
            "distribution": {
                "binary": {"linux-x86_64": {"archive": "https://example.com/codex-acp-linux-x86_64.tar.gz", "cmd": "./codex-acp"}},
                "npx": {"package": "@zed-industries/codex-acp@0.10.0"},
            },
        })
    }

    fn agent(kwargs: Value, model: Option<&str>) -> Result<Acp> {
        Acp::new(common::test_args(kwargs, &[], model))
    }

    fn err(kwargs: Value) -> String {
        format!("{:#}", agent(kwargs, None).err().unwrap())
    }

    #[test]
    fn options_validation() {
        let a = agent(json!({"registry_entry": entry()}), None).unwrap();
        assert_eq!(a.version().as_deref(), Some("0.10.0"));
        assert_eq!(a.identity(), ("codex-acp".into(), "0.10.0".into()));
        assert!(err(json!({})).contains("requires a registry distribution or a git source"));
        assert!(err(json!({"registry_entry": entry(), "registry_spec": "x"})).contains("Provide only one of"));
        assert!(err(json!({"registry_entry": entry(), "auth_policy": "sometimes"})).contains("Unsupported ACP auth policy"));
        assert!(err(json!({"registry_entry": entry(), "distribution_preference": "npx,bogus"})).contains("preference(s): bogus"));
        assert!(err(json!({"registry_entry": entry(), "permission_mode": "maybe"})).contains("permission_mode"));
        assert!(
            err(json!({"source": {"repo_url": "https://github.com/o/r", "ref": "main"}, "registry_spec": "x"}))
                .contains("cannot be combined")
        );
        let mut e = entry();
        e["distribution"]["binary"]["linux-x86_64"]["archive"] = json!("http://x");
        assert!(err(json!({"registry_entry": e})).contains("HTTPS"));
        let mut e = entry();
        e["distribution"]["binary"]["linux-x86_64"]["checksum"] = json!("nope");
        assert!(err(json!({"registry_entry": e})).contains("SHA-256"));
        let mut e = entry();
        e["distribution"]["npx"]["env"] = json!({"BAD-KEY": "x"});
        assert!(err(json!({"registry_entry": e})).contains("POSIX"));
        let mut e = entry();
        e["distribution"] = json!({"local": {"cmd": "  "}});
        assert!(err(json!({"registry_entry": e})).contains("cmd cannot be empty"));
        // string entry and shorthand prefix stripping
        assert!(agent(json!({"registry_entry": entry().to_string()}), None).is_ok());
        let s = agent(json!({"registry_spec": "acp:opencode@1.3.9"}), None).unwrap();
        assert_eq!(s.identity(), ("acp:opencode@1.3.9".into(), "unknown".into()));
        assert_eq!(s.version(), None);
    }

    #[test]
    fn distribution_selection_and_platform() {
        let a = agent(json!({"registry_entry": entry()}), None).unwrap();
        assert_eq!(a.select_distribution("linux-x86_64").unwrap().kind(), "binary");
        assert_eq!(a.select_distribution("linux-aarch64").unwrap().kind(), "npx");
        let b = agent(json!({"registry_entry": entry(), "distribution_preference": ["uvx"]}), None).unwrap();
        assert_eq!(
            format!("{}", b.select_distribution("linux-x86_64").unwrap_err()),
            "No compatible ACP distribution found for codex-acp on linux-x86_64. Available: binary[linux-x86_64], npx"
        );
        assert_eq!(a.normalize_platform("Linux", "arm64").unwrap(), "linux-aarch64");
        assert_eq!(a.normalize_platform("MINGW64_NT", "AMD64").unwrap(), "windows-x86_64");
        assert!(a.normalize_platform("Linux", "riscv64").is_err());
    }

    #[test]
    fn launchers_and_install_commands() {
        let bin = BinaryTarget {
            archive: "https://example.com/a.tar.gz".into(),
            cmd: "/usr/local/bin/codex-acp".into(),
            args: vec!["--stdio".into()],
            env: BTreeMap::from([("OPENAI_API_KEY".into(), "test key".into())]),
            checksum: None,
        };
        let l = launcher_script(&Target::Binary(bin.clone()));
        assert!(l.contains("export OPENAI_API_KEY='test key'") && l.contains("exec /opt/hbench-acp-agent/dist/codex-acp --stdio \"$@\""));
        assert!(!l.contains("/usr/local/bin"));
        let l = launcher_script(&Target::Uvx(PackageDistribution {
            package: "fast-agent-acp==0.6.10".into(),
            args: vec!["serve".into()],
            env: BTreeMap::new(),
        }));
        assert!(l.contains("/opt/hbench-acp-venv/bin/uvx fast-agent-acp==0.6.10 serve"));
        let l = launcher_script(&Target::Npx(PackageDistribution {
            package: "@zed-industries/codex-acp@0.10.0".into(),
            args: vec![],
            env: BTreeMap::new(),
        }));
        assert!(
            l.contains("\"${NVM_DIR:-$HOME/.nvm}\"/versions/node/*/bin")
                && l.contains("exec npx -y @zed-industries/codex-acp@0.10.0 \"$@\"")
        );
        let l = launcher_script(&Target::Local(LocalTarget {
            cmd: "node".into(),
            args: vec!["/opt/local acp/server.js".into()],
            env: BTreeMap::new(),
        }));
        assert_eq!(l, "#!/usr/bin/env sh\nset -eu\nexec node '/opt/local acp/server.js' \"$@\"\n");

        let d = dependencies_command("npx");
        let line = |k: &str| d.lines().find(|l| l.contains(k)).unwrap().to_string();
        assert!(!line("hb_apt_install python3").contains("nodejs") && line("apk add").contains("nodejs npm"));
        let d = dependencies_command("uvx");
        assert!(d.starts_with(&format!(
            "set -euo pipefail\n{}            export UV_PYTHON_INSTALL_DIR=/opt/hbench-acp-python\n",
            super::super::APT_INSTALL_FN
        )));
        assert!(d.contains("curl -LsSf https://astral.sh/uv/0.13.1/install.sh | env UV_INSTALL_DIR=/opt/hbench-acp-tools sh"));
        assert!(d.contains("/opt/hbench-acp-tools/uv venv --python 3.12 /opt/hbench-acp-venv") && d.contains("[ \"1\" = \"1\" ]"));
        assert!(dependencies_command("binary").contains("xz-utils"));
        let n = node_install_command();
        assert!(n.contains("nvm install 22") && n.contains("musl"));
        let c = binary_install_command(&BinaryTarget { checksum: Some("ab".repeat(32)), ..bin });
        assert!(
            c.contains(&format!("expected_checksum={}", "ab".repeat(32)))
                && c.contains("cp \"$tmp_archive\" /opt/hbench-acp-agent/dist/codex-acp")
        );
    }

    #[test]
    fn run_env_and_registry_spec() {
        let a = agent(
            json!({"registry_entry": entry(), "auth_policy": "Explicit", "authenticate_method_id": "openai-api-key"}),
            Some("openai/gpt-5.4"),
        )
        .unwrap();
        let e = a.run_env();
        assert_eq!(e["HBENCH_ACP_AUTH_POLICY"], "explicit");
        assert_eq!(e["HBENCH_ACP_AUTHENTICATE_METHOD_ID"], "openai-api-key");
        assert_eq!(e["HBENCH_ACP_REQUESTED_MODEL"], "openai/gpt-5.4");
        assert_eq!(e["HBENCH_ACP_PERMISSION_MODE"], "allow");
        assert_eq!(e["HBENCH_ACP_MCP_SERVERS_JSON"], "[]");
        assert_eq!(e["HBENCH_ACP_AGENT_ID"], "codex-acp");

        assert_eq!(parse_registry_spec("opencode").unwrap(), ("opencode".into(), None));
        assert_eq!(parse_registry_spec("acp:opencode@1.3.9").unwrap(), ("opencode".into(), Some("1.3.9".into())));
        assert!(parse_registry_spec("opencode@").is_err() && parse_registry_spec("@1").is_err());
        assert!(is_acp_registry_shorthand("acp:opencode") && !is_acp_registry_shorthand("acp"));
        assert_eq!(registry_spec_from_agent_name("acp:opencode@1.3.9").unwrap(), "opencode@1.3.9");
        assert_eq!(url_quote("opencode/agent.json"), "opencode%2Fagent.json");
    }

    #[test]
    fn registry_resolves_historical_version() {
        let fetch = |url: &str| -> Result<Value> {
            Ok(if url.ends_with("/main/opencode/agent.json") {
                json!({"id": "opencode", "version": "1.3.9"})
            } else if url.contains("commits?path=opencode%2Fagent.json") && url.ends_with("&per_page=100&page=1") {
                json!([{"sha": "sha-old"}, {"sha": "sha-new"}])
            } else if url.contains("commits?path=opencode%2Fagent.json") && url.ends_with("&per_page=100&page=2") {
                json!([])
            } else if url.ends_with("/sha-old/opencode/agent.json") {
                json!({"id": "opencode", "version": "1.2.0"})
            } else if url.ends_with("/sha-new/opencode/agent.json") {
                json!({"id": "opencode", "version": "1.3.8"})
            } else {
                bail!("Unexpected URL: {url}")
            })
        };
        let dir = std::env::temp_dir().join(format!("hbench-acp-reg-{}", uuid::Uuid::new_v4()));
        let p = resolve_registry_entry_payload("opencode@1.2.0", "main", &dir, &fetch).unwrap();
        assert_eq!(p["version"], "1.2.0");
        let cached: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("versions/opencode/1.2.0/agent.json")).unwrap()).unwrap();
        assert_eq!(cached["version"], "1.2.0");
        resolve_registry_entry_payload("opencode", "main", &dir, &fetch).unwrap();
        assert!(dir.join("main/opencode/agent.json").is_file());
        assert!(
            resolve_registry_entry_payload("opencode@9.9", "main", &dir, &fetch).unwrap_err().to_string().contains("version not found")
        );
    }

    #[test]
    fn summary_usage_and_metadata() {
        let a = agent(json!({"registry_entry": entry()}), None).unwrap();
        let dir = std::env::temp_dir().join(format!("hbench-acp-sum-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ctx = AgentContext::default();
        a.populate_context_post_run(&dir, &mut ctx);
        assert!(ctx.is_empty());
        let summary = json!({
            "prompt_response": {"stopReason": "end_turn", "usage": {"inputTokens": 101, "outputTokens": 5, "cachedReadTokens": 7, "totalTokens": 113}},
            "latest_usage_update": {"cost": {"amount": 0.12, "currency": "USD"}},
            "auth_policy": "explicit",
            "requested_model": "openai/gpt-5.4",
            "session": {"models": {"currentModelId": "gpt-5.3-codex/medium"}},
            "error": {"type": "RuntimeError", "message": "agent failed to start"},
        });
        std::fs::write(dir.join(SUMMARY_FILENAME), summary.to_string()).unwrap();
        a.populate_context_post_run(&dir, &mut ctx);
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens, ctx.n_cache_tokens, ctx.cost_usd), (Some(101), Some(5), Some(7), Some(0.12)));
        let m = &ctx.metadata.unwrap()["acp"];
        assert_eq!(m["registry_entry_id"], "codex-acp");
        assert_eq!(m["auth_policy"], "explicit");
        assert_eq!(m["initial_session_models"]["currentModelId"], "gpt-5.3-codex/medium");
        assert_eq!(m["error"]["message"], "agent failed to start");
        assert_eq!(m["events_file"], "acp-events.jsonl");
    }

    #[test]
    fn source_validation_and_manifest() {
        let s = agent(json!({"source": {"repo_url": "https://github.com/acme/my-agent.git", "ref": "main"}}), None).unwrap();
        assert_eq!(s.identity(), ("acp-source:my-agent".into(), "main".into()));
        assert!(err(json!({"source": {"repo_url": "file:///x", "ref": "main"}})).contains("https:// or ssh://"));
        assert!(err(json!({"source": {"repo_url": "https://u:p@github.com/x", "ref": "main"}})).contains("credentials"));
        assert!(err(json!({"source": {"repo_url": "https://github.com/x", "ref": "-x"}})).contains("ref must be"));
        assert!(err(json!({"source": {"repo_url": "https://github.com/x", "ref": "m", "source_dir": "./a"}})).contains("normalized"));
        assert!(err(json!({"source": {"repo_url": "https://github.com/x", "ref": "m", "source_dir": "../a"}})).contains("remain under"));

        let tmp = std::env::temp_dir().join(format!("hbench-acp-src-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(source_askpass_env(&tmp, "https://github.com/o/r", None).unwrap().is_empty());
        assert_eq!(source_askpass_env(&tmp, "https://github.com/o/r", Some("tok".into())).unwrap()[SOURCE_TOKEN_ENV], "tok");
        assert!(source_askpass_env(&tmp, "https://gitlab.com/o/r", Some("tok".into())).is_err());

        let co = tmp.join("co").canonicalize().unwrap_or_else(|_| {
            std::fs::create_dir_all(tmp.join("co")).unwrap();
            tmp.join("co").canonicalize().unwrap()
        });
        let manifest = json!({"schema_version": 1, "id": "my-agent", "version": "1.0", "protocol": "acp", "runtime": {"kind": "python-uv", "python": "3.12", "entrypoint": ["my-agent", "--acp"]}}).to_string();
        std::fs::write(co.join("hbench-agent.json"), &manifest).unwrap();
        std::fs::write(co.join("pyproject.toml"), "").unwrap();
        std::fs::write(co.join("uv.lock"), "").unwrap();
        let src = |sha: Option<&str>| AcpSource {
            repo_url: "https://github.com/o/r".into(),
            git_ref: "main".into(),
            source_dir: ".".into(),
            manifest_path: "hbench-agent.json".into(),
            manifest_sha256: sha.map(String::from),
        };
        let (m, digest) = load_verified_manifest(&src(None), &co).unwrap();
        assert_eq!(digest, crate::task::hex(&Sha256::digest(manifest.as_bytes())));
        assert_eq!(source_remote_project(&m, &co).unwrap(), "/installed-agent/source");
        assert_eq!(
            source_launcher_script(&m, "/installed-agent/source"),
            "#!/usr/bin/env sh\nset -eu\ncd /installed-agent/source\nPATH=/installed-agent/source/.venv/bin:$PATH\nexport PATH\nexec my-agent --acp \"$@\"\n"
        );
        assert!(load_verified_manifest(&src(Some(&"0".repeat(64))), &co).unwrap_err().to_string().contains("digest does not match"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", co.join("link")).unwrap();
            assert!(load_verified_manifest(&src(None), &co).unwrap_err().to_string().contains("symlinks"));
        }
    }
}
