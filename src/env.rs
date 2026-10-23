//! Docker Compose environment.
//!
//! Files move with `docker compose cp` (never bind mounts for /logs), so the same
//! code works against remote daemons (ssh contexts). Network allowlists use
//! an egress-control sidecar (vendored under assets/egress-sidecar); see [`EgressMethod`].

use crate::task::{EnvironmentConfig, Healthcheck, MAIN, NetworkMode, NetworkPolicy};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

macro_rules! error_kinds {
    ($($variant:ident => $name:literal,)*) => {
        /// Exception type of a failed trial step, as written to result.json and matched by the retry
        /// include/exclude lists. Every agent reports its failures with these names; `Other` holds names
        /// read back from files (retry lists, older results) that hbench no longer raises.
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum ErrorKind {
            $($variant,)*
            Other(String),
        }

        impl ErrorKind {
            pub fn as_str(&self) -> &str {
                match self {
                    $(ErrorKind::$variant => $name,)*
                    ErrorKind::Other(s) => s,
                }
            }
        }

        impl From<&str> for ErrorKind {
            fn from(s: &str) -> Self {
                match s {
                    $($name => ErrorKind::$variant,)*
                    s => ErrorKind::Other(s.into()),
                }
            }
        }
    };
}

error_kinds! {
    Runtime => "RuntimeError",
    Timeout => "TimeoutError",
    Cancelled => "CancelledError",
    Environment => "EnvironmentError",
    EnvironmentStart => "EnvironmentStartError",
    EnvironmentStartTimeout => "EnvironmentStartTimeoutError",
    Healthcheck => "HealthcheckError",
    AgentSetup => "AgentSetupError",
    AgentSetupTimeout => "AgentSetupTimeoutError",
    AgentTimeout => "AgentTimeoutError",
    NonZeroAgentExitCode => "NonZeroAgentExitCodeError",
    AgentAuthentication => "AgentAuthenticationError",
    AgentSafetyRefusal => "AgentSafetyRefusalError",
    ModelNotFound => "ModelNotFoundError",
    UnsupportedModelParameters => "UnsupportedModelParametersError",
    NetworkConnection => "NetworkConnectionError",
    ContextWindowExceeded => "ContextWindowExceededError",
    OutputTokenExceeded => "OutputTokenExceededError",
    ApiBadRequest => "ApiBadRequestError",
    ApiRateLimit => "ApiRateLimitError",
    ApiUsageLimit => "ApiUsageLimitError",
    ApiOverloaded => "ApiOverloadedError",
    ApiInternalServer => "ApiInternalServerError",
    ApiConnection => "ApiConnectionError",
    ApiConnectionClosed => "ApiConnectionClosedError",
    ApiResponseStalled => "ApiResponseStalledError",
    ApiProviderResourceNotFound => "ApiProviderResourceNotFoundError",
    UnknownApi => "UnknownApiError",
    AddTestsDir => "AddTestsDirError",
    DownloadVerifierDir => "DownloadVerifierDirError",
    VerifierTimeout => "VerifierTimeoutError",
    VerifierOutputParse => "VerifierOutputParseError",
    RewardFileEmpty => "RewardFileEmptyError",
    RewardFileNotFound => "RewardFileNotFoundError",
    Regrade => "RegradeError",
}

impl ErrorKind {
    /// Agent errors that are recorded while the verifier still runs (the agent timeout, non-zero
    /// exit and classified agent/API failure family).
    pub fn is_agent_run_error(&self) -> bool {
        use ErrorKind::*;
        match self {
            AgentTimeout
            | NonZeroAgentExitCode
            | AgentAuthentication
            | AgentSafetyRefusal
            | ModelNotFound
            | UnsupportedModelParameters
            | NetworkConnection
            | ContextWindowExceeded
            | OutputTokenExceeded
            | ApiBadRequest
            | ApiRateLimit
            | ApiUsageLimit
            | ApiOverloaded
            | ApiInternalServer
            | ApiConnection
            | ApiConnectionClosed
            | ApiResponseStalled
            | ApiProviderResourceNotFound
            | UnknownApi => true,
            Runtime
            | Timeout
            | Cancelled
            | Environment
            | EnvironmentStart
            | EnvironmentStartTimeout
            | Healthcheck
            | AgentSetup
            | AgentSetupTimeout
            | AddTestsDir
            | DownloadVerifierDir
            | VerifierTimeout
            | VerifierOutputParse
            | RewardFileEmpty
            | RewardFileNotFound
            | Regrade
            | Other(_) => false,
        }
    }
}

impl From<String> for ErrorKind {
    fn from(s: String) -> Self {
        ErrorKind::from(s.as_str())
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<&str> for ErrorKind {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// Error carrying an exception type (used for result.json + retry policy).
#[derive(Debug)]
pub struct KindError {
    pub kind: ErrorKind,
    pub msg: String,
}
impl std::fmt::Display for KindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}
impl std::error::Error for KindError {}

pub fn kind_err(kind: ErrorKind, msg: impl Into<String>) -> anyhow::Error {
    KindError { kind, msg: msg.into() }.into()
}

/// The exception type of `e`: the first `KindError` in its chain, else `RuntimeError`.
pub fn error_kind(e: &anyhow::Error) -> ErrorKind {
    e.chain().find_map(|c| c.downcast_ref::<KindError>()).map_or(ErrorKind::Runtime, |k| k.kind.clone())
}

/// Reads `KEY=value` records and then the command, each NUL-terminated, from stdin; exports the
/// records, closes stdin and runs the command. Secrets and commands then never appear on a command
/// line, where any local user could read them with `ps` or /proc, on the host or in the container.
/// Works the same on rootless Docker and Docker Desktop, since only `docker compose exec`'s stdin is used.
const EXEC_FROM_STDIN: &str = r#"while IFS= read -r -d '' __hb && [ -n "$__hb" ]; do export -- "$__hb"; done; IFS= read -r -d '' __hb_cmd; unset __hb; exec </dev/null; eval "$__hb_cmd""#;

/// The stdin [`EXEC_FROM_STDIN`] reads: env records, an empty record, then the command.
fn exec_stdin(env: &BTreeMap<String, String>, command: &str) -> Vec<u8> {
    let mut b = Vec::new();
    for (k, v) in env {
        b.extend_from_slice(format!("{k}={v}\0").as_bytes());
    }
    b.push(0);
    b.extend_from_slice(command.as_bytes());
    b.push(0);
    b
}

/// Write `content` to a new file in the host's temp dir that only this user can read (mode 600).
pub fn private_temp_file(prefix: &str, content: &[u8]) -> Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(&path)?.write_all(content)?;
    Ok(path)
}

/// Append-only per-trial log (trial.log), and with `--observe` stderr, under `who`.
pub struct TrialLog {
    file: Mutex<std::fs::File>,
    who: String,
}
impl TrialLog {
    pub fn create(path: &Path, who: &str) -> Result<Arc<TrialLog>> {
        let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Arc::new(TrialLog { file: Mutex::new(f), who: who.to_string() }))
    }
    pub fn line(&self, s: &str) {
        self.record(s);
        if crate::observe::enabled() {
            crate::observe::print(&self.who, s);
        }
    }
    /// trial.log only: for what `--observe` has already printed as it came.
    pub fn record(&self, s: &str) {
        let ts = chrono::Utc::now().format("%H:%M:%S%.3f");
        let _ = writeln!(self.file.lock().unwrap(), "[{ts}] {}", crate::redact::text(s));
    }
    pub fn who(&self) -> &str {
        &self.who
    }
    pub fn observer(&self) -> Option<crate::observe::Lines> {
        crate::observe::enabled().then(|| crate::observe::Lines::new(&self.who))
    }
}

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Metadata-only commands (mkdir, test -d, rm -f, ps, inspect, reading logs): a healthy daemon answers in seconds.
const QUICK_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// `stop` / `down`: docker gives each container a 10s grace before it kills it.
const STOP_TIMEOUT: Duration = Duration::from_secs(120);
/// Tearing a trial's project down: `stop`, or `down` with or without its images and volumes.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(300);
/// Copying files in or out (`docker compose cp`, a tar of a directory): large trees over an ssh context take minutes.
const COPY_TIMEOUT: Duration = Duration::from_secs(600);
/// Emptying log dirs with `rm -rf`, which a chatty agent can fill with many files.
const CLEAR_DIRS_TIMEOUT: Duration = Duration::from_secs(120);
/// Applying the egress allowlist, or probing nftables in a throwaway sidecar container.
const EGRESS_SETUP_TIMEOUT: Duration = Duration::from_secs(120);
/// Building the egress sidecar image, which pulls its base image the first time.
const SIDECAR_BUILD_TIMEOUT: Duration = Duration::from_secs(900);
/// `docker version` before a run: an ssh context may take a few seconds to connect.
const DAEMON_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

const EGRESS_SERVICE: &str = "hbench-egress-sidecar";
const EGRESS_READY: &str = "/tmp/hbench-egress-sidecar.ready";
/// gost's HTTP proxy port (gost.yaml).
const PROXY_PORT: u16 = 3128;
/// The network the proxy reaches the outside through, in the `proxy` method.
const OUTSIDE_NETWORK: &str = "hb-egress";

/// How a trial's network policy is enforced. Both sidecar methods run the same gost
/// allowlist and set HTTP_PROXY/HTTPS_PROXY in the task container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EgressMethod {
    /// The task container shares the sidecar's network namespace and nftables sends all of its TCP
    /// through gost, and rejects everything else. Needs nftables with fib (CONFIG_NFT_FIB_IPV4/IPV6)
    /// in the daemon's kernel, which Linux and Docker Desktop both have.
    #[serde(rename = "nft+proxy")]
    NftProxy,
    /// The task container sits on an internal network with no route out, so the HTTP proxy is its
    /// only way out: raw TCP even to an allowed host fails. For a kernel without those nftables
    /// features. Public phases connect it to the outside network again.
    #[serde(rename = "proxy")]
    Proxy,
}

/// What result.json records about egress enforcement.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EgressInfo {
    pub enforced: bool,
    pub method: EgressMethod,
    /// Destinations (`host:port`) the allowlist refused, each once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied: Vec<String>,
}

/// Picks the method for a run that needs egress control (`restricted`). `forced` is
/// HBENCH_EGRESS_METHOD, which the e2e suite uses to test `proxy` on a Linux daemon.
fn choose_method(restricted: bool, nft: bool, forced: Option<&str>) -> Result<Option<EgressMethod>> {
    if !restricted {
        return Ok(None);
    }
    Ok(Some(match forced.filter(|f| !f.is_empty()) {
        Some("proxy") => EgressMethod::Proxy,
        Some("nft+proxy") if nft => EgressMethod::NftProxy,
        Some("nft+proxy") => bail!("HBENCH_EGRESS_METHOD=nft+proxy, but the docker host kernel does not take the egress nftables rules"),
        Some(f) => bail!("HBENCH_EGRESS_METHOD must be proxy or nft+proxy, not {f:?}"),
        None if nft => EgressMethod::NftProxy,
        None => EgressMethod::Proxy,
    }))
}

/// HTTP_PROXY and friends, both cases, for the task container. Compose services stay direct.
fn proxy_env(method: EgressMethod, services: &[String]) -> BTreeMap<String, String> {
    let host = if method == EgressMethod::NftProxy { "127.0.0.1" } else { EGRESS_SERVICE };
    let proxy = format!("http://{host}:{PROXY_PORT}");
    let mut no_proxy = vec!["localhost".to_string(), "127.0.0.1".into(), "::1".into()];
    no_proxy.extend(services.iter().filter(|s| *s != MAIN).cloned());
    let no_proxy = no_proxy.join(",");
    let mut env = BTreeMap::new();
    for (k, v) in [("http_proxy", &proxy), ("https_proxy", &proxy), ("no_proxy", &no_proxy)] {
        env.insert(k.to_string(), v.clone());
        env.insert(k.to_uppercase(), v.clone());
    }
    env
}

/// The compose override that puts the sidecar in front of `services`.
fn egress_compose(
    method: EgressMethod,
    image: &str,
    project: &str,
    baseline: &NetworkPolicy,
    services: &[String],
    own_networks: &[String],
) -> Result<Value> {
    let mut sidecar = json!({
        "image": image,
        // Built locally; a missing image must not turn into an anonymous Docker Hub pull.
        "pull_policy": "never",
        "entrypoint": ["/opt/egress-sidecar/entrypoint.sh"],
        "init": true,
        "environment": {
            "EGRESS_CONTROL_METHOD": serde_json::to_value(method)?,
            "EGRESS_CONTROL_INITIAL_NETWORK_MODE": serde_json::to_value(baseline.mode)?,
            "EGRESS_CONTROL_INITIAL_ALLOWED_HOSTS": baseline.allowed_hosts.join(" "),
        },
        // nftables, and SO_MARK on gost's own connections.
        "cap_add": ["NET_ADMIN", "NET_RAW"],
        "healthcheck": {"test": ["CMD", "test", "-f", EGRESS_READY], "interval": "1s", "timeout": "5s", "retries": 30},
    });
    let depends = json!({EGRESS_SERVICE: {"condition": "service_healthy"}});
    let mut doc = serde_json::Map::new();
    let mut svcs = serde_json::Map::new();
    match method {
        EgressMethod::NftProxy => {
            for s in services {
                svcs.insert(s.clone(), json!({"network_mode": format!("service:{EGRESS_SERVICE}"), "depends_on": depends}));
            }
        }
        EgressMethod::Proxy => {
            sidecar["networks"] = json!(["default", OUTSIDE_NETWORK]);
            let nets = if baseline.mode == NetworkMode::Public { json!(["default", OUTSIDE_NETWORK]) } else { json!(["default"]) };
            for s in services {
                svcs.insert(s.clone(), json!({"networks": nets, "depends_on": depends}));
            }
            for s in own_networks {
                svcs.insert(s.clone(), json!({"networks": [OUTSIDE_NETWORK]}));
            }
            doc.insert(
                "networks".into(),
                json!({"default": {"internal": true}, OUTSIDE_NETWORK: {"name": format!("{project}_{OUTSIDE_NETWORK}")}}),
            );
        }
    }
    svcs.insert(EGRESS_SERVICE.into(), sidecar);
    doc.insert("services".into(), Value::Object(svcs));
    Ok(Value::Object(doc))
}

/// The destinations gost's allowlist refused, from its JSON log.
fn parse_denials(logs: &str) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    for line in logs.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if v.get("kind").and_then(Value::as_str) != Some("bypass") {
            continue;
        }
        if let Some(rest) = v.get("msg").and_then(Value::as_str).and_then(|m| m.strip_prefix("bypass: ")) {
            out.insert(rest.split(',').next().unwrap_or(rest).trim().to_string());
        }
    }
    out.into_iter().collect()
}
pub const LOG_DIRS: [&str; 3] = ["/logs/agent", "/logs/verifier", "/logs/artifacts"];

/// Everything needed to start one compose project.
pub struct EnvSpec {
    pub project: String,
    pub env_dir: PathBuf,
    pub config: EnvironmentConfig,
    /// Where generated compose overrides are written.
    pub compose_dir: PathBuf,
    pub force_build: bool,
    pub build_timeout: Duration,
    /// Job-level `environment.env` (resolved): container startup env + every exec.
    pub persistent_env: BTreeMap<String, String>,
    pub override_cpus: Option<u32>,
    pub override_memory_mb: Option<u64>,
    pub override_gpus: Option<u32>,
    pub mounts: Vec<Value>,
    pub extra_compose: Vec<PathBuf>,
    pub baseline: NetworkPolicy,
    /// Other policies phases will switch to (decides whether egress control is needed).
    pub phase_policies: Vec<NetworkPolicy>,
    /// Upload environment/ into the workdir for prebuilt images without a build spec.
    pub upload_env_dir: bool,
    pub keep_containers: bool,
    pub delete: bool,
    /// Log dirs to create (agent env: all three; separate verifier: /logs/verifier).
    pub log_dirs: Vec<&'static str>,
}

pub struct DockerEnv {
    spec: EnvSpec,
    files: Vec<PathBuf>,
    compose_env: BTreeMap<String, String>,
    build_image: Option<String>,
    egress: Option<EgressMethod>,
    /// The services behind the sidecar.
    egress_services: Vec<String>,
    log: Arc<TrialLog>,
}

/// Docker image/project names: lowercase, alnum first, restricted charset.
pub fn sanitize(name: &str, allow_dot: bool) -> String {
    let mut s: String = name
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '_' | '-' => c,
            '.' if allow_dot => c,
            _ => '-',
        })
        .collect();
    if !s.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        s.insert(0, '0');
    }
    s
}

pub fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn to_owned_args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

pub fn tail(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut i = s.len() - n;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

const SSH_ATTEMPTS: u32 = 5;

/// The docker CLI's ssh transport failed before a session was open (sshd MaxStartups/MaxSessions under
/// load), so the command never reached the daemon and is safe to rerun. A session that drops once
/// open reads "Connection to HOST closed by remote host" instead and is not matched.
fn ssh_not_connected(r: &ExecResult) -> bool {
    let e = &r.stderr;
    let pre_session = ["kex_exchange_identification", "Session open refused", "Connection refused"].iter().any(|m| e.contains(m))
        || ["Connection closed by ", "Connection reset by "].iter().any(|m| e.contains(m) && e.contains(" port "));
    r.code != 0 && e.contains("dial-stdio") && pre_session
}

/// Pull/registry/builder/daemon failures worth another `up` or `build`, as opposed to errors in the
/// task's compose setup or Dockerfile.
fn transient_docker_error(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    if s.contains("no space left on device") {
        return false;
    }
    [
        "toomanyrequests",
        "too many requests",
        "tls handshake timeout",
        "connection reset",
        "i/o timeout",
        "unexpected eof",
        "context deadline exceeded",
        "net/http: request canceled",
        "temporary failure in name resolution",
        "503 service unavailable",
        "502 bad gateway",
        "504 gateway timeout",
        "500 internal server error",
        // BuildKit losing its builder or connection mid-build (e.g. another job switched or removed it).
        "graceful_stop",
        "code = unavailable",
    ]
    .iter()
    .any(|p| s.contains(p))
}

/// Bytes of a command's stdout/stderr kept from the start and from the end; the middle of longer
/// output is dropped so a chatty command can't exhaust memory.
const OUTPUT_HEAD: usize = 1 << 20;
const OUTPUT_TAIL: usize = 8 << 20;

async fn read_capped(
    mut r: impl tokio::io::AsyncRead + Unpin,
    head_cap: usize,
    tail_cap: usize,
    mut observer: Option<crate::observe::Lines>,
) -> std::io::Result<String> {
    use tokio::io::AsyncReadExt;
    let (mut head, mut rest, mut dropped) = (Vec::new(), Vec::new(), 0usize);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf).await?;
        if let Some(o) = observer.as_mut() {
            if n == 0 {
                o.finish();
            } else {
                o.feed(&buf[..n]);
            }
        }
        let k = (head_cap - head.len()).min(n);
        head.extend_from_slice(&buf[..k]);
        rest.extend_from_slice(&buf[k..n]);
        if rest.len() > 2 * tail_cap || (n == 0 && rest.len() > tail_cap) {
            let d = rest.len() - tail_cap;
            rest.drain(..d);
            dropped += d;
        }
        if n == 0 {
            break;
        }
    }
    if dropped > 0 {
        head.extend_from_slice(format!("\n[hbench: {dropped} bytes of output dropped]\n").as_bytes());
    }
    head.extend_from_slice(&rest);
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// Services in task compose files, each with whether it sets `network_mode` and whether it sets `networks`.
fn compose_services(files: &[PathBuf]) -> Result<BTreeMap<String, (bool, bool)>> {
    let mut out: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for f in files {
        let doc: Value = serde_yaml_ng::from_str(&std::fs::read_to_string(f)?).with_context(|| format!("parsing {}", f.display()))?;
        if let Some(services) = doc.get("services").and_then(Value::as_object) {
            for (name, cfg) in services {
                let e = out.entry(name.clone()).or_default();
                e.0 |= cfg.get("network_mode").is_some();
                e.1 |= cfg.get("networks").is_some();
            }
        }
    }
    out.entry(MAIN.into()).or_default();
    out.remove(EGRESS_SERVICE);
    Ok(out)
}

/// Services in task compose files that don't declare their own networking.
fn egress_eligible_services(files: &[PathBuf]) -> Result<Vec<String>> {
    Ok(compose_services(files)?.into_iter().filter(|(_, (m, n))| !m && !n).map(|(s, _)| s).collect())
}

/// Services that pick their own networks, which keeps them out of egress control. The `proxy`
/// method makes the default network internal, so these get the outside network as well.
fn own_network_services(files: &[PathBuf]) -> Result<Vec<String>> {
    Ok(compose_services(files)?.into_iter().filter(|(_, (m, n))| !m && *n).map(|(s, _)| s).collect())
}

/// Whether the daemon's kernel takes the sidecar's nftables ruleset, tried in a throwaway container.
/// Only an answer from the check itself is cached; a docker failure (exit 125+, timeout) is retried next time.
async fn nft_support(image: &str, log: &TrialLog) -> bool {
    static CACHE: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    CACHE
        .get_or_try_init(|| async {
            let out = tokio::time::timeout(
                EGRESS_SETUP_TIMEOUT,
                Command::new("docker")
                    .args(["container", "run", "--rm", "--cap-add", "NET_ADMIN", "--entrypoint", "network-policy", image, "check-nftables"])
                    .kill_on_drop(true)
                    .output(),
            )
            .await;
            match out {
                Ok(Ok(o)) if o.status.code().is_some_and(|c| c < 125) => Ok(o.status.success()),
                Ok(Ok(o)) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("timed out".into()),
            }
        })
        .await
        .copied()
        .unwrap_or_else(|e| {
            log.line(&format!("nftables check failed ({e}); using the proxy method for this trial"));
            false
        })
}

/// `docker args` with a timeout (killed when it runs out).
async fn docker_output(args: &[&str], timeout: Duration) -> Result<std::process::Output> {
    let out = Command::new("docker").args(args).kill_on_drop(true).output();
    tokio::time::timeout(timeout, out)
        .await
        .map_err(|_| anyhow!("`docker {}` timed out after {timeout:?}", args.join(" ")))?
        .map_err(Into::into)
}

/// The vendored egress sidecar image, content-addressed, built if it's missing. Checked on every call,
/// not memoized: an image pruned mid-run must be rebuilt, and compose never pulls it (`pull_policy: never`).
async fn egress_image(log: &TrialLog) -> Result<String> {
    static BUILD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = BUILD.lock().await;
    let files: [(&str, &str, u32); 5] = [
        ("Dockerfile", include_str!("../assets/egress-sidecar/Dockerfile"), 0o644),
        ("gost.yaml", include_str!("../assets/egress-sidecar/gost.yaml"), 0o644),
        ("allowlist.txt", "", 0o644),
        ("entrypoint.sh", include_str!("../assets/egress-sidecar/entrypoint.sh"), 0o755),
        ("bin/network-policy", include_str!("../assets/egress-sidecar/bin/network-policy"), 0o755),
    ];
    let mut h = sha2::Sha256::default();
    for (n, c, _) in files {
        sha2::Digest::update(&mut h, n.as_bytes());
        sha2::Digest::update(&mut h, c.as_bytes());
    }
    let hash = crate::task::hex(&sha2::Digest::finalize(h))[..12].to_string();
    let image = format!("hbench-egress-sidecar:{hash}");
    let exists = docker_output(&["image", "inspect", &image], QUICK_COMMAND_TIMEOUT).await?.status.success();
    if !exists {
        let dir = cache_root().join("egress-sidecar").join(&hash);
        std::fs::create_dir_all(dir.join("bin"))?;
        for (n, c, mode) in files {
            let p = dir.join(n);
            std::fs::write(&p, c)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode))?;
        }
        log.line(&format!("building egress sidecar image {image}"));
        let out = docker_output(&["build", "-t", &image, &dir.to_string_lossy()], SIDECAR_BUILD_TIMEOUT).await?;
        if !out.status.success() {
            bail!("building egress sidecar failed: {}", String::from_utf8_lossy(&out.stderr));
        }
    }
    Ok(image)
}

pub fn cache_root() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    home.join(".cache/hbench")
}

impl DockerEnv {
    pub async fn new(spec: EnvSpec, log: Arc<TrialLog>) -> Result<DockerEnv> {
        let ec = &spec.config;
        let env_dir = &spec.env_dir;
        let has_dockerfile = env_dir.join("Dockerfile").exists();
        let task_compose = env_dir.join("docker-compose.yaml");
        let prebuilt = ec.docker_image.clone().filter(|_| !(spec.force_build && has_dockerfile));

        let mut main = serde_json::Map::new();
        let build_image = match (&prebuilt, has_dockerfile) {
            (Some(img), _) => {
                main.insert("image".into(), json!(img));
                None
            }
            (None, true) => {
                let name = env_dir.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let dir = env_dir.clone();
                let hash = tokio::task::spawn_blocking(move || crate::task::hash_dir(&dir)).await??;
                let image = sanitize(&format!("hb__{name}-{}", &hash[..12]), true);
                main.insert("build".into(), json!({"context": env_dir}));
                main.insert("image".into(), json!(image));
                Some(image)
            }
            // Main is defined entirely by the task's docker-compose.yaml.
            (None, false) => None,
        };
        main.insert("command".into(), json!(["sh", "-c", "sleep infinity"]));
        // Like a Dockerfile WORKDIR, make sure the task workdir exists (prebuilt images may lack it).
        if let Some(w) = &ec.workdir {
            main.insert("working_dir".into(), json!(w));
        }
        if let Some(c) = spec.override_cpus.or(ec.cpus) {
            main.insert("cpus".into(), json!(c as f64));
        }
        let memory_mb = spec.override_memory_mb.or(ec.effective_memory_mb()?);
        if let Some(m) = memory_mb {
            main.insert("mem_limit".into(), json!(format!("{m}m")));
        }
        if let Some(g) = spec.override_gpus.or(ec.gpus).filter(|g| *g > 0) {
            if ec.gpu_types.as_ref().is_some_and(|t| !t.is_empty()) {
                log.line("warning: docker cannot select a GPU type; ignoring gpu_types");
            }
            main.insert(
                "deploy".into(),
                json!({"resources": {"reservations": {"devices": [{"driver": "nvidia", "count": g, "capabilities": ["gpu"]}]}}}),
            );
        }

        // Network: egress sidecar when any phase needs no-network/allowlist.
        let policies: Vec<&NetworkPolicy> = std::iter::once(&spec.baseline).chain(spec.phase_policies.iter()).collect();
        let restricted = policies.iter().any(|p| p.mode != NetworkMode::Public);
        let image = if restricted { Some(egress_image(&log).await?) } else { None };
        let nft = match &image {
            Some(i) => nft_support(i, &log).await,
            None => false,
        };
        let egress = choose_method(restricted, nft, std::env::var("HBENCH_EGRESS_METHOD").ok().as_deref())?;
        if let Some(m) = egress {
            log.line(&format!("egress control: {}", serde_json::to_value(m)?.as_str().unwrap_or_default()));
        }

        std::fs::create_dir_all(&spec.compose_dir)?;
        let write = |name: &str, v: Value| -> Result<PathBuf> {
            let p = spec.compose_dir.join(name);
            std::fs::write(&p, serde_json::to_string_pretty(&v)?)?;
            Ok(p)
        };
        let mut files = vec![write("docker-compose-base.json", json!({"services": {"main": main}}))?];
        let mut user_files = vec![];
        if task_compose.exists() {
            user_files.push(task_compose);
        }
        user_files.extend(spec.extra_compose.iter().cloned());
        files.extend(user_files.iter().cloned());
        let mut startup_env = crate::task::resolve_env_vars(&ec.env)?;
        startup_env.extend(spec.persistent_env.clone());
        let mut egress_services = vec![];
        if egress.is_some() {
            egress_services = egress_eligible_services(&user_files)?;
            // Fail closed: a main that sets its own networking would bypass the sidecar.
            if !egress_services.iter().any(|s| s == MAIN) {
                bail!("the task's main service sets its own network_mode or networks, so its network policy cannot be enforced");
            }
        }
        // Not in compose_env: the docker CLI itself would use these proxies.
        let mut main_env = startup_env.clone();
        if let Some(m) = egress {
            main_env.extend(proxy_env(m, &egress_services));
        }
        if !main_env.is_empty() {
            files.push(write("docker-compose-env.json", json!({"services": {"main": {"environment": main_env}}}))?);
        }
        // /logs dirs are named volumes, not host bind mounts: services that mount
        // ${HOST_*_PATH} share them with main, and it works on remote daemons too.
        let log_volume = |dir: &str| format!("hb-logs-{}", dir.trim_start_matches("/logs/"));
        let mut volumes: Vec<Value> =
            spec.log_dirs.iter().map(|d| json!({"type": "volume", "source": log_volume(d), "target": d})).collect();
        volumes.extend(spec.mounts.iter().cloned());
        if !volumes.is_empty() {
            let decl: serde_json::Map<String, Value> = spec.log_dirs.iter().map(|d| (log_volume(d), json!({}))).collect();
            files.push(write("docker-compose-mounts.json", json!({"services": {"main": {"volumes": volumes}}, "volumes": decl}))?);
        }
        if let (Some(m), Some(image)) = (egress, &image) {
            files.push(write(
                "docker-compose-egress.json",
                egress_compose(m, image, &spec.project, &spec.baseline, &egress_services, &own_network_services(&user_files)?)?,
            )?);
        }

        // Variables task-authored compose files may interpolate.
        let mut compose_env = startup_env.clone();
        let trial_dir = spec.compose_dir.parent().unwrap_or(&spec.compose_dir);
        compose_env.insert("CONTEXT_DIR".into(), env_dir.display().to_string());
        compose_env.insert("MAIN_IMAGE_NAME".into(), build_image.clone().unwrap_or_default());
        if let Some(img) = &prebuilt {
            compose_env.insert("PREBUILT_IMAGE_NAME".into(), img.clone());
        }
        if let Some(c) = spec.override_cpus.or(ec.cpus) {
            compose_env.insert("CPUS".into(), c.to_string());
        }
        if let Some(m) = memory_mb {
            compose_env.insert("MEMORY".into(), format!("{m}M"));
        }
        for (dir, name) in [("verifier", "VERIFIER_LOGS"), ("agent", "AGENT_LOGS"), ("artifacts", "ARTIFACTS")] {
            compose_env.insert(format!("ENV_{name}_PATH"), format!("/logs/{dir}"));
            let host = if spec.log_dirs.contains(&format!("/logs/{dir}").as_str()) {
                format!("hb-logs-{dir}")
            } else {
                trial_dir.join(dir).display().to_string()
            };
            compose_env.insert(format!("HOST_{name}_PATH"), host);
        }
        compose_env.insert(
            "DOCKER_CONFIG".into(),
            std::env::var("DOCKER_CONFIG").unwrap_or_else(|_| cache_home().join(".docker").display().to_string()),
        );
        crate::redact::register_env(&compose_env);
        Ok(DockerEnv { spec, files, compose_env, build_image, egress, egress_services, log })
    }

    pub fn workdir(&self) -> Option<&str> {
        self.spec.config.workdir.as_deref()
    }

    fn compose(&self, args: &[String]) -> Command {
        let mut c = Command::new("docker");
        c.arg("compose").arg("--project-name").arg(&self.spec.project);
        c.arg("--project-directory").arg(&self.spec.env_dir);
        for f in &self.files {
            c.arg("-f").arg(f);
        }
        // Own process group: terminal Ctrl-C reaches only hbench, which then tears down cleanly.
        c.args(args).envs(&self.compose_env).kill_on_drop(true).process_group(0);
        if self.spec.env_dir.is_dir() {
            c.current_dir(&self.spec.env_dir);
        }
        c
    }

    async fn run(&self, args: Vec<String>, timeout: Option<Duration>, label: &str, stdin: Option<Vec<u8>>) -> Result<ExecResult> {
        self.run_cmd(self.compose(&args), timeout, label, stdin).await
    }

    async fn run_cmd(&self, mut c: Command, timeout: Option<Duration>, label: &str, stdin: Option<Vec<u8>>) -> Result<ExecResult> {
        c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
        self.log.record(&format!("$ {label}"));
        if crate::observe::enabled() {
            crate::observe::command(self.log.who(), label);
        }
        for attempt in 1.. {
            let r = self.spawn_once(&mut c, timeout, label, stdin.clone()).await?;
            if attempt < SSH_ATTEMPTS && ssh_not_connected(&r) {
                self.log.line(&format!("ssh transport not connected; retrying (attempt {})", attempt + 1));
                tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
                continue;
            }
            return Ok(r);
        }
        unreachable!()
    }

    async fn spawn_once(&self, c: &mut Command, timeout: Option<Duration>, label: &str, stdin: Option<Vec<u8>>) -> Result<ExecResult> {
        let mut child = c.spawn().context("spawning `docker compose` (is docker installed?)")?;
        if let Some(data) = stdin {
            let mut si = child.stdin.take().unwrap();
            si.write_all(&data).await?;
            drop(si);
        }
        let (stdout, stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let out = async {
            tokio::try_join!(
                child.wait(),
                read_capped(stdout, OUTPUT_HEAD, OUTPUT_TAIL, self.log.observer()),
                read_capped(stderr, OUTPUT_HEAD, OUTPUT_TAIL, self.log.observer())
            )
        };
        let (status, stdout, stderr) = match timeout {
            Some(t) => match tokio::time::timeout(t, out).await {
                Ok(r) => r?,
                Err(_) => {
                    self.log.line(&format!("timed out after {t:?}"));
                    return Err(kind_err(ErrorKind::Timeout, format!("`{label}` timed out after {t:?}")));
                }
            },
            None => out.await?,
        };
        let r = ExecResult { code: status.code().unwrap_or(-1), stdout, stderr };
        self.log.record(&format!("exit={} stdout:\n{}stderr:\n{}", r.code, tail(&r.stdout, 20000), tail(&r.stderr, 20000)));
        if crate::observe::enabled() {
            crate::observe::exit(self.log.who(), r.code);
        }
        Ok(r)
    }

    async fn run_ok(&self, args: Vec<String>, timeout: Option<Duration>) -> Result<ExecResult> {
        let label = args.join(" ");
        let r = self.run(args, timeout, &label, None).await?;
        if r.code != 0 {
            bail!("docker compose {label} failed (exit {}): {}{}", r.code, tail(&r.stdout, 2000), tail(&r.stderr, 4000));
        }
        Ok(r)
    }

    pub async fn start(&self) -> Result<()> {
        // Serialize builds of one image; later trials in this process reuse it. Waiting here for
        // another trial's build doesn't count against this trial's timeout.
        static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<bool>>>>> = OnceLock::new();
        let lock =
            self.build_image.as_ref().map(|i| LOCKS.get_or_init(Default::default).lock().unwrap().entry(i.clone()).or_default().clone());
        let built = match &lock {
            Some(l) => Some(l.lock().await),
            None => None,
        };
        let started = async move {
            if let Some(mut built) = built {
                if !*built {
                    self.retrying(&["build"]).await?;
                    *built = true;
                }
            } else if self.spec.env_dir.join("docker-compose.yaml").exists() {
                self.retrying(&["build"]).await?;
            }
            let _ = self.run(to_owned_args(&["down", "--remove-orphans"]), Some(STOP_TIMEOUT), "down (stale)", None).await;
            if self.egress.is_some() {
                egress_image(&self.log).await?;
            }
            self.up().await?;
            if self.egress == Some(EgressMethod::Proxy) && self.spec.baseline.mode != NetworkMode::Public {
                self.check_isolated().await?;
            }
            anyhow::Ok(())
        };
        match tokio::time::timeout(self.spec.build_timeout, started).await {
            Ok(r) => r.map_err(|e| kind_err(ErrorKind::EnvironmentStart, format!("{e:#}")))?,
            Err(_) => {
                return Err(kind_err(
                    ErrorKind::EnvironmentStartTimeout,
                    format!("Environment start timed out after {} seconds", self.spec.build_timeout.as_secs_f64()),
                ));
            }
        }
        if !self.spec.log_dirs.is_empty() {
            let d = self.spec.log_dirs.join(" ");
            let r = self.exec_root(&format!("mkdir -p {d} && chmod 777 /logs {d}"), Some(QUICK_COMMAND_TIMEOUT)).await?;
            if r.code != 0 {
                bail!("Failed to create /logs dirs (is bash available in the image?): {}{}", r.stdout, r.stderr);
            }
        }
        if self.spec.upload_env_dir && self.spec.config.docker_image.is_some() {
            let d = &self.spec.env_dir;
            let has_spec = d.join("Dockerfile").exists() || d.join("docker-compose.yaml").exists();
            if !has_spec && d.is_dir() && std::fs::read_dir(d)?.next().is_some() {
                let wd = match self.workdir() {
                    Some(w) => w.to_string(),
                    None => self.exec("pwd", None, &BTreeMap::new(), None).await?.stdout.trim().to_string(),
                };
                self.upload_dir(d, if wd.is_empty() { "/" } else { &wd }).await?;
            }
        }
        Ok(())
    }

    async fn up(&self) -> Result<()> {
        self.retrying(&["up", "--detach", "--wait"]).await
    }

    /// `docker compose args`, retried when a pull, the builder or the daemon fails transiently.
    async fn retrying(&self, args: &[&str]) -> Result<()> {
        let label = args.join(" ");
        for attempt in 1.. {
            let r = self.run(to_owned_args(args), None, &label, None).await?;
            if r.code == 0 {
                break;
            }
            // Pruned between the check in start() and here: rebuild it and go again.
            let sidecar_gone = self.egress.is_some()
                && r.stderr.contains("hbench-egress-sidecar")
                && r.stderr.to_ascii_lowercase().contains("no such image");
            if attempt == 3 || !(sidecar_gone || transient_docker_error(&r.stderr)) {
                bail!("docker compose {label} failed (exit {}): {}{}", r.code, tail(&r.stdout, 2000), tail(&r.stderr, 4000));
            }
            if sidecar_gone {
                egress_image(&self.log).await?;
            }
            self.log.line(&format!("transient docker error; retrying `{label}` (attempt {})", attempt + 1));
            tokio::time::sleep(Duration::from_secs(10 * attempt)).await;
        }
        Ok(())
    }

    /// `bash -c command` in main, with persistent env < `env`. Non-zero exit is not an error.
    pub async fn exec(
        &self,
        command: &str,
        user: Option<&str>,
        env: &BTreeMap<String, String>,
        timeout: Option<Duration>,
    ) -> Result<ExecResult> {
        self.exec_in(command, user, env, timeout, self.workdir()).await
    }

    pub async fn exec_in(
        &self,
        command: &str,
        user: Option<&str>,
        env: &BTreeMap<String, String>,
        timeout: Option<Duration>,
        cwd: Option<&str>,
    ) -> Result<ExecResult> {
        let mut merged = self.spec.persistent_env.clone();
        merged.extend(env.clone());
        crate::redact::register_env(&merged);
        let mut args = to_owned_args(&["exec", "-T"]);
        if let Some(w) = cwd {
            args.extend(to_owned_args(&["-w", w]));
        }
        if let Some(u) = user {
            args.extend(to_owned_args(&["-u", u]));
        }
        args.extend(to_owned_args(&["main", "bash", "-c", EXEC_FROM_STDIN]));
        let keys: Vec<&str> = merged.keys().map(String::as_str).collect();
        // Log env keys only: values may be secrets.
        let label = format!("exec user={} env={keys:?}: {}", user.unwrap_or("-"), tail(command, 2000));
        self.run(args, timeout, &label, Some(exec_stdin(&merged, command))).await
    }

    pub async fn exec_root(&self, command: &str, timeout: Option<Duration>) -> Result<ExecResult> {
        self.exec(command, Some("root"), &BTreeMap::new(), timeout).await
    }

    /// Exec in any service: main uses bash + main defaults; sidecars use `sh -c`, no workdir/env.
    pub async fn service_exec(&self, service: &str, command: &str, user: Option<&str>, timeout: Option<Duration>) -> Result<ExecResult> {
        if service == MAIN {
            return self.exec(command, user, &BTreeMap::new(), timeout).await;
        }
        let mut args = to_owned_args(&["exec", "-T"]);
        if let Some(u) = user {
            args.extend(to_owned_args(&["-u", u]));
        }
        args.extend(to_owned_args(&[service, "sh", "-c", command]));
        let label = format!("exec [{service}] {}", tail(command, 2000));
        self.run(args, timeout, &label, None).await
    }

    pub async fn is_dir(&self, service: &str, path: &str) -> Result<bool> {
        Ok(self.service_exec(service, &format!("test -d {}", quote(path)), Some("root"), Some(QUICK_COMMAND_TIMEOUT)).await?.code == 0)
    }

    pub async fn upload_dir(&self, src: &Path, dst: &str) -> Result<()> {
        let r = self.exec_root(&format!("mkdir -p {}", quote(dst)), Some(QUICK_COMMAND_TIMEOUT)).await?;
        if r.code != 0 {
            bail!("mkdir {dst} failed: {}", r.stderr);
        }
        self.run_ok(vec!["cp".into(), format!("{}/.", src.display()), format!("main:{dst}")], Some(COPY_TIMEOUT)).await.map(|_| ())
    }

    pub async fn upload_file(&self, src: &Path, dst: &str) -> Result<()> {
        if let Some(parent) = Path::new(dst).parent().map(|p| p.display().to_string()).filter(|p| !p.is_empty() && p != "/") {
            self.exec_root(&format!("mkdir -p {}", quote(&parent)), Some(QUICK_COMMAND_TIMEOUT)).await?;
        }
        self.run_ok(vec!["cp".into(), src.display().to_string(), format!("main:{dst}")], Some(COPY_TIMEOUT)).await.map(|_| ())
    }

    pub async fn download_dir(&self, service: &str, src: &str, dst: &Path) -> Result<()> {
        std::fs::create_dir_all(dst)?;
        self.run_ok(vec!["cp".into(), format!("{service}:{}/.", src.trim_end_matches('/')), dst.display().to_string()], Some(COPY_TIMEOUT))
            .await?;
        self.strip_links(dst);
        Ok(())
    }

    /// `docker cp` keeps container symlinks (absolute targets intact) and special files; hbench's own
    /// host readers and writers would follow them out of the trial dir, so nothing but files and dirs stays.
    fn strip_links(&self, root: &Path) {
        let n = strip_links(root);
        if n > 0 {
            self.log.line(&format!("removed {n} symlink(s) or special file(s) from {}", root.display()));
        }
    }

    pub async fn download_file(&self, service: &str, src: &str, dst: &Path) -> Result<()> {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        self.run_ok(vec!["cp".into(), format!("{service}:{src}"), dst.display().to_string()], Some(COPY_TIMEOUT)).await?;
        self.strip_links(dst);
        Ok(())
    }

    /// Tar inside the service with `--exclude` patterns, copy the archive out, extract locally.
    pub async fn download_dir_excluding(&self, service: &str, src: &str, dst: &Path, exclude: &[String]) -> Result<()> {
        let tmp = format!("/tmp/hbench-transfer-{}.tar.gz", uuid::Uuid::new_v4().simple());
        let flags: Vec<String> = exclude.iter().map(|p| format!("--exclude={}", quote(p))).collect();
        let r = self
            .service_exec(service, &format!("tar czf {tmp} {} -C {} .", flags.join(" "), quote(src)), Some("root"), Some(COPY_TIMEOUT))
            .await?;
        if r.code != 0 {
            bail!("tar in {service} failed: {}", r.stderr);
        }
        std::fs::create_dir_all(dst)?;
        let local = std::env::temp_dir().join(format!("hbench-{}.tar.gz", uuid::Uuid::new_v4().simple()));
        let res = async {
            self.download_file(service, &tmp, &local).await?;
            let out = Command::new("tar").arg("xzf").arg(&local).arg("-C").arg(dst).output().await?;
            if !out.status.success() {
                bail!("extracting artifact archive failed: {}", String::from_utf8_lossy(&out.stderr));
            }
            self.strip_links(dst);
            Ok(())
        }
        .await;
        let _ = std::fs::remove_file(&local);
        let _ = self.service_exec(service, &format!("rm -f {tmp}"), Some("root"), Some(QUICK_COMMAND_TIMEOUT)).await;
        res
    }

    /// Interactive shell in main (inherits the terminal; no own process group so the TTY works).
    pub async fn attach(&self) -> Result<std::process::ExitStatus> {
        let mut c = Command::new("docker");
        c.arg("compose").arg("--project-name").arg(&self.spec.project).arg("--project-directory").arg(&self.spec.env_dir);
        for f in &self.files {
            c.arg("-f").arg(f);
        }
        c.args(["exec", "-it"]);
        if let Some(w) = self.workdir() {
            c.args(["-w", w]);
        }
        c.args(["main", "bash"]).envs(&self.compose_env);
        Ok(c.status().await?)
    }

    pub async fn stop_service(&self, service: &str) -> Result<()> {
        self.run_ok(to_owned_args(&["stop", service]), Some(STOP_TIMEOUT)).await.map(|_| ())
    }

    pub async fn set_network_policy(&self, p: &NetworkPolicy) -> Result<()> {
        if self.egress.is_none() {
            if p.mode == NetworkMode::Public && self.spec.baseline.mode == NetworkMode::Public {
                return Ok(());
            }
            bail!("Docker egress control was not enabled when the environment started, so it cannot enforce this network policy.");
        }
        let proxy = self.egress == Some(EgressMethod::Proxy);
        // Close the direct route before the proxy opens anything, and open it only after.
        if proxy && p.mode != NetworkMode::Public {
            self.connect_outside(false).await?;
            self.check_isolated().await?;
        }
        self.apply_proxy_policy(p).await?;
        if proxy && p.mode == NetworkMode::Public {
            self.connect_outside(true).await?;
        }
        Ok(())
    }

    async fn apply_proxy_policy(&self, p: &NetworkPolicy) -> Result<()> {
        let mut args = to_owned_args(&["exec", "--no-TTY", EGRESS_SERVICE, "network-policy"]);
        match p.mode {
            NetworkMode::Public => args.push("allow-all".into()),
            NetworkMode::NoNetwork => args.push("deny-all".into()),
            NetworkMode::Allowlist => {
                args.push("allow".into());
                args.extend(p.allowed_hosts.iter().cloned());
            }
        }
        self.run_ok(args, Some(EGRESS_SETUP_TIMEOUT)).await.map(|_| ())
    }

    async fn docker(&self, args: &[&str]) -> Result<String> {
        let out = docker_output(args, QUICK_COMMAND_TIMEOUT).await?;
        if !out.status.success() {
            bail!("docker {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    async fn container_id(&self, service: &str) -> Result<String> {
        let r = self.run_ok(vec!["ps".into(), "-q".into(), service.into()], Some(QUICK_COMMAND_TIMEOUT)).await?;
        r.stdout.split_whitespace().next().map(str::to_string).ok_or_else(|| anyhow!("no running container for {service}"))
    }

    async fn container_networks(&self, id: &str) -> Result<Vec<String>> {
        let out = self.docker(&["container", "inspect", "-f", "{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}", id]).await?;
        Ok(out.split_whitespace().map(str::to_string).collect())
    }

    /// `proxy` method: attach the services behind the sidecar to the outside network, or detach them.
    async fn connect_outside(&self, connect: bool) -> Result<()> {
        let net = format!("{}_{OUTSIDE_NETWORK}", self.spec.project);
        for s in &self.egress_services {
            let id = self.container_id(s).await?;
            let attached = self.container_networks(&id).await?.contains(&net);
            if attached != connect {
                let verb = if connect { "connect" } else { "disconnect" };
                self.docker(&["network", verb, &net, &id]).await?;
                self.log.line(&format!("{verb}ed {s} and {net}"));
            }
        }
        Ok(())
    }

    /// Fail closed: every network the services behind the sidecar are on must be internal.
    async fn check_isolated(&self) -> Result<()> {
        for s in &self.egress_services {
            let id = self.container_id(s).await?;
            for n in self.container_networks(&id).await? {
                if self.docker(&["network", "inspect", "-f", "{{.Internal}}", &n]).await? != "true" {
                    bail!("egress control: {s} is still on network {n}, which has a route out");
                }
            }
        }
        Ok(())
    }

    /// The method this environment enforces egress with, and what the allowlist refused so far.
    pub async fn egress_info(&self) -> Option<EgressInfo> {
        let method = self.egress?;
        let mut denied = vec![];
        // The last 20000 lines: older denials may be missed on a very chatty run.
        let logs = self.compose(&to_owned_args(&["logs", "--no-color", "--no-log-prefix", "--tail", "20000", EGRESS_SERVICE])).output();
        match tokio::time::timeout(QUICK_COMMAND_TIMEOUT, logs).await {
            Ok(Ok(o)) if o.status.success() => denied = parse_denials(&String::from_utf8_lossy(&o.stdout)),
            _ => self.log.line("could not read the egress sidecar's log"),
        }
        if !denied.is_empty() {
            self.log.line(&format!("egress refused: {}", denied.join(" ")));
        }
        Some(EgressInfo { enforced: true, method, denied })
    }

    /// Docker HEALTHCHECK semantics: failures during the start period don't count.
    pub async fn run_healthcheck(&self, hc: &Healthcheck) -> Result<()> {
        let start_end = tokio::time::Instant::now() + Duration::from_secs_f64(hc.start_period_sec);
        let mut failures = 0;
        loop {
            let in_start = tokio::time::Instant::now() < start_end;
            let r = self.exec(&hc.command, None, &BTreeMap::new(), Some(Duration::from_secs_f64(hc.timeout_sec.max(1.0)))).await;
            if matches!(&r, Ok(r) if r.code == 0) {
                return Ok(());
            }
            if in_start {
                tokio::time::sleep(Duration::from_secs_f64(hc.start_interval_sec)).await;
            } else {
                failures += 1;
                if failures >= hc.retries {
                    return Err(kind_err(
                        ErrorKind::Healthcheck,
                        format!("Healthcheck failed after {} consecutive retries: {}", hc.retries, hc.command),
                    ));
                }
                tokio::time::sleep(Duration::from_secs_f64(hc.interval_sec)).await;
            }
        }
    }

    /// Ensure dirs exist and are empty (keeping the dir roots); optionally chmod 777.
    pub async fn empty_dirs(&self, dirs: &[&str], chmod: bool) -> Result<()> {
        let mut cmd = String::new();
        for d in dirs {
            let q = quote(d);
            cmd.push_str(&format!("mkdir -p {q} && find {q} -mindepth 1 -maxdepth 1 -exec rm -rf {{}} + && "));
            if chmod {
                cmd.push_str(&format!("chmod 777 {q} && "));
            }
        }
        cmd.push_str("true");
        let r = self.exec_root(&cmd, Some(CLEAR_DIRS_TIMEOUT)).await?;
        if r.code != 0 {
            bail!("resetting {dirs:?} failed: {}", r.stderr);
        }
        Ok(())
    }

    /// The processes running in main now, as `pid:starttime` (starttime tells a reused pid apart).
    pub async fn process_snapshot(&self) -> Result<String> {
        let r = self.exec_from_root(&format!("{PROC_LIST}\nfor_each_proc echo")).await?;
        if r.code != 0 {
            bail!("listing processes failed: {}", r.stderr);
        }
        Ok(r.stdout.split_whitespace().collect::<Vec<_>>().join(" "))
    }

    /// Kill the processes the agent itself is still running: anything started since `snapshot` that still
    /// descends from a live `docker exec` or is the foreground job of a terminal. Processes it detached
    /// (reparented to PID 1, or a background job of its terminal) keep running. Returns how many were killed.
    pub async fn kill_agent_processes(&self, snapshot: &str) -> Result<usize> {
        let r = self.exec_from_root(&kill_agent_script(snapshot)).await?;
        if r.code != 0 {
            bail!("stopping processes failed: {}", r.stderr);
        }
        r.stdout.trim().parse().with_context(|| format!("stopping processes printed {:?}, not a count", r.stdout.trim()))
    }

    /// As root from `/`: the agent may have deleted or renamed the task's workdir, and `exec -w` would fail.
    async fn exec_from_root(&self, command: &str) -> Result<ExecResult> {
        self.exec_in(command, Some("root"), &BTreeMap::new(), Some(QUICK_COMMAND_TIMEOUT), Some("/")).await
    }

    pub async fn stop(&self) -> Result<()> {
        let args = if self.spec.keep_containers {
            to_owned_args(&["stop"])
        } else if self.spec.delete {
            to_owned_args(&["down", "--rmi", "local", "--volumes", "--remove-orphans"])
        } else {
            to_owned_args(&["down"])
        };
        self.run_ok(args, Some(TEARDOWN_TIMEOUT)).await.map(|_| ())
    }
}

/// Bash builtins only, so walking /proc forks nothing that would show up in it.
/// `for_each_proc cb` calls `cb pid:starttime` for every live process except PID 1 and this shell.
const PROC_LIST: &str = r#"for_each_proc() {
  local cb=$1 p s
  for p in /proc/[0-9]*; do
    case "${p#/proc/}" in 1 | "$$") continue ;; esac
    read -r s 2>/dev/null < "$p/stat" || continue
    set -- ${s##*) }
    [ "$1" = Z ] && continue
    "$cb" "${p#/proc/}:${20}"
  done
}"#;

/// Bash builtins only (see PROC_LIST). In /proc/<pid>/stat after the command name: $1 state, $2 ppid,
/// $3 pgrp, $4 session, $5 tty, $6 terminal foreground pgrp, $20 starttime. A `docker exec` process has
/// ppid 0 inside the container; an orphan is reparented to PID 1. Everything picked is stopped first, and
/// the scan repeats until it finds nothing new, so no child is orphaned (and so spared) mid-way.
/// Remove every symlink and non-regular file at or under `root` (links are not followed). Returns the count.
fn strip_links(root: &Path) -> usize {
    let Ok(meta) = root.symlink_metadata() else { return 0 };
    let t = meta.file_type();
    if t.is_dir() {
        let Ok(entries) = std::fs::read_dir(root) else { return 0 };
        entries.flatten().map(|e| strip_links(&e.path())).sum()
    } else if t.is_file() {
        0
    } else {
        usize::from(std::fs::remove_file(root).is_ok())
    }
}

fn kill_agent_script(snapshot: &str) -> String {
    format!(
        r#"keep=" {snapshot} "
scan() {{
  local p pid s
  ppid=() fg=() new=()
  for p in /proc/[0-9]*; do
    pid=${{p#/proc/}}
    read -r s 2>/dev/null < "$p/stat" || continue
    set -- ${{s##*) }}
    ppid[pid]=$2
    case "$pid" in 1 | "$$") continue ;; esac
    [ "$1" = Z ] && continue
    case "$keep" in *" $pid:${{20}} "*) continue ;; esac
    new+=("$pid")
    [ "$5" != 0 ] && [ "$3" = "$6" ] && [ "$4" != "$pid" ] && fg[pid]=1
  done
}}
owned() {{
  local p=$1 n=0
  while [ "$p" -gt 1 ] && [ "$p" != "$$" ] && [ $n -lt 1000 ]; do
    [ -n "${{fg[p]}}" ] && return 0
    [ "${{ppid[p]:-1}}" = 0 ] && return 0
    p=${{ppid[p]:-1}}
    n=$((n + 1))
  done
  return 1
}}
stopped=" "
for ((round = 0; round < 20; round++)); do
  scan
  more=
  for p in "${{new[@]}}"; do
    case "$stopped" in *" $p "*) continue ;; esac
    owned "$p" && kill -STOP "$p" 2>/dev/null && more=1 && stopped="$stopped$p "
  done
  [ -z "$more" ] && break
done
set -- $stopped
[ $# -gt 0 ] && kill -KILL "$@" 2>/dev/null
echo $#"#
    )
}

fn cache_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// Fail fast with a readable message when the docker daemon is unreachable.
pub async fn preflight() -> Result<()> {
    let out = tokio::time::timeout(
        DAEMON_CHECK_TIMEOUT,
        Command::new("docker").args(["version", "--format", "{{.Server.Version}}"]).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| anyhow!("`docker version` timed out after 30s: docker daemon unreachable (check DOCKER_CONTEXT / DOCKER_HOST)"))?
    .context("running `docker` (is it installed?)")?;
    if !out.status.success() {
        bail!("docker daemon unreachable: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let out = Command::new("docker").args(["compose", "version"]).output().await?;
    if !out.status.success() {
        bail!("`docker compose` (v2 plugin) is required");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn ssh_not_connected_only_before_a_session() {
        let r = |code, stderr: &str| ExecResult { code, stdout: String::new(), stderr: stderr.into() };
        let dial = "command [ssh -l u -T -- h docker system dial-stdio] has exited with exit status 255, ...: stderr=";
        assert!(ssh_not_connected(&r(1, &format!("{dial}kex_exchange_identification: read: Connection reset by peer"))));
        assert!(ssh_not_connected(&r(1, &format!("{dial}Connection closed by 192.0.2.1 port 22"))));
        assert!(ssh_not_connected(&r(1, &format!("{dial}mux_client_request_session: Session open refused by peer"))));
        assert!(!ssh_not_connected(&r(1, &format!("{dial}Connection to 192.0.2.1 closed by remote host."))));
        assert!(!ssh_not_connected(&r(1, "bash: line 1: foo: command not found")));
        assert!(!ssh_not_connected(&r(0, &format!("{dial}Connection closed by h port 22"))));
    }

    use super::*;

    #[test]
    fn strip_links_keeps_only_files_and_dirs() {
        let d = std::env::temp_dir().join(format!("hbench-strip-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/keep.txt"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/hosts", d.join("trajectory.json")).unwrap();
        std::os::unix::fs::symlink("/etc", d.join("sub/etc")).unwrap();
        std::os::unix::fs::symlink("keep.txt", d.join("sub/rel")).unwrap();
        assert!(std::process::Command::new("mkfifo").arg(d.join("reward.json")).status().unwrap().success());
        assert_eq!(strip_links(&d), 4);
        assert!(d.join("sub/keep.txt").is_file());
        for gone in ["trajectory.json", "sub/etc", "sub/rel", "reward.json"] {
            assert!(d.join(gone).symlink_metadata().is_err(), "{gone} survived");
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// What `docker compose exec ... main bash -c EXEC_FROM_STDIN` runs, with the same stdin.
    fn run_from_stdin(env: &[(&str, &str)], command: &str) -> (i32, String) {
        use std::io::Write as _;
        let env: BTreeMap<String, String> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let mut c = std::process::Command::new("bash")
            .args(["-c", EXEC_FROM_STDIN])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(&exec_stdin(&env, command)).unwrap();
        let out = c.wait_with_output().unwrap();
        (out.status.code().unwrap(), String::from_utf8(out.stdout).unwrap())
    }

    #[test]
    fn exec_passes_env_and_command_on_stdin() {
        let tricky = "sk-a=b 'c' \"d\" $(echo pwned) `e`\nline2\\";
        let (code, out) = run_from_stdin(
            &[("API_KEY", tricky), ("EMPTY", ""), ("PATH", &std::env::var("PATH").unwrap())],
            "printf '%s|' \"$API_KEY\" \"${EMPTY-unset}\" \"$0\" \"$#\"; env | grep -c '^__hb' || true; cat; set -o pipefail; false | true; exit 7",
        );
        assert_eq!(code, 7);
        assert_eq!(out, format!("{tricky}||bash|0|0\n"));
        assert_eq!(run_from_stdin(&[], "echo ok").1, "ok\n");
    }

    #[test]
    fn exec_keeps_secrets_off_the_command_line() {
        let env = BTreeMap::from([("OPENAI_API_KEY".to_string(), "sk-live-123456789".to_string())]);
        let input = exec_stdin(&env, "echo sk-live-123456789");
        assert!(!EXEC_FROM_STDIN.contains("sk-live"));
        assert_eq!(input, b"OPENAI_API_KEY=sk-live-123456789\0\0echo sk-live-123456789\0");
    }

    #[test]
    fn private_temp_files_are_mode_600() {
        let p = private_temp_file("hbench-test-private", b"secret").unwrap();
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&p).unwrap().permissions()) & 0o777, 0o600);
        assert_eq!(std::fs::read(&p).unwrap(), b"secret");
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn error_kind_names_round_trip() {
        for s in ["AgentTimeoutError", "CancelledError", "RateLimitError", "ApiUsageLimitError"] {
            assert_eq!(ErrorKind::from(s).as_str(), s);
        }
        assert_eq!(ErrorKind::from("AgentTimeoutError"), ErrorKind::AgentTimeout);
        assert_eq!(ErrorKind::from("ApiRateLimitError"), ErrorKind::ApiRateLimit);
        assert!(ErrorKind::ApiRateLimit.is_agent_run_error() && ErrorKind::ContextWindowExceeded.is_agent_run_error());
        assert!(!ErrorKind::Runtime.is_agent_run_error() && !ErrorKind::from("RateLimitError").is_agent_run_error());
        assert_eq!(error_kind(&anyhow!("plain")), ErrorKind::Runtime);
    }
    #[test]
    fn sanitizes_and_quotes() {
        assert_eq!(sanitize("Hello World__AbC", false), "hello-world__abc");
        assert_eq!(sanitize("_x.y", false), "0_x-y");
        assert_eq!(sanitize("hb__a.b", true), "hb__a.b");
        assert_eq!(tail("héllo", 3), "llo");
        assert_eq!(quote("/logs/agent"), "/logs/agent");
        assert_eq!(quote("a b'c"), r"'a b'\''c'");
    }

    #[tokio::test]
    async fn caps_command_output() {
        let data = format!("{}{}{}", "a".repeat(10), "b".repeat(170), "c".repeat(20));
        let s = read_capped(data.as_bytes(), 10, 20, None).await.unwrap();
        assert_eq!(s, format!("{}\n[hbench: 170 bytes of output dropped]\n{}", "a".repeat(10), "c".repeat(20)));
        assert_eq!(read_capped(&b"short"[..], 10, 20, None).await.unwrap(), "short");
    }

    #[test]
    fn classifies_docker_errors_and_env() {
        assert!(transient_docker_error("Error response from daemon: toomanyrequests: You have reached your pull rate limit"));
        assert!(transient_docker_error("net/http: TLS handshake timeout"));
        assert!(!transient_docker_error("service \"main\" refers to undefined network foo: invalid compose project"));
        assert!(transient_docker_error(
            "ERROR: failed to solve: rpc error: code = Unavailable desc = closing transport due to: graceful_stop"
        ));
        assert!(!transient_docker_error("failed to copy: write /var/lib/docker/tmp/x: no space left on device: unexpected EOF"));
    }

    #[test]
    fn picks_an_egress_method() {
        use EgressMethod::*;
        assert_eq!(choose_method(false, false, Some("proxy")).unwrap(), None);
        assert_eq!(choose_method(true, true, None).unwrap(), Some(NftProxy));
        assert_eq!(choose_method(true, false, None).unwrap(), Some(Proxy));
        assert_eq!(choose_method(true, true, Some("proxy")).unwrap(), Some(Proxy));
        assert_eq!(choose_method(true, true, Some("")).unwrap(), Some(NftProxy));
        assert!(choose_method(true, false, Some("nft+proxy")).is_err());
        assert!(choose_method(true, true, Some("iptables")).is_err());
        assert_eq!(serde_json::to_value(NftProxy).unwrap(), json!("nft+proxy"));
    }

    #[test]
    fn proxy_env_names_the_sidecar() {
        let services = ["api".to_string(), MAIN.to_string()];
        let e = proxy_env(EgressMethod::Proxy, &services);
        assert_eq!(e["HTTPS_PROXY"], "http://hbench-egress-sidecar:3128");
        assert_eq!(e["http_proxy"], e["HTTP_PROXY"]);
        assert_eq!(e["no_proxy"], "localhost,127.0.0.1,::1,api");
        assert_eq!(e.len(), 6);
        assert_eq!(proxy_env(EgressMethod::NftProxy, &services)["https_proxy"], "http://127.0.0.1:3128");
    }

    #[test]
    fn proxy_compose_isolates_the_services() {
        let allow = NetworkPolicy { mode: NetworkMode::Allowlist, allowed_hosts: vec!["pypi.org".into()] };
        let services = ["api".to_string(), MAIN.to_string()];
        let c = egress_compose(EgressMethod::Proxy, "img", "p1", &allow, &services, &["db".into()]).unwrap();
        assert_eq!(c["networks"]["default"]["internal"], json!(true));
        assert_eq!(c["networks"][OUTSIDE_NETWORK]["name"], json!("p1_hb-egress"));
        assert_eq!(c["services"][EGRESS_SERVICE]["networks"], json!(["default", OUTSIDE_NETWORK]));
        assert_eq!(c["services"][EGRESS_SERVICE]["environment"]["EGRESS_CONTROL_METHOD"], json!("proxy"));
        assert_eq!(c["services"]["main"]["networks"], json!(["default"]));
        assert!(c["services"]["main"].get("network_mode").is_none());
        assert_eq!(c["services"]["db"]["networks"], json!([OUTSIDE_NETWORK]));
        // A public baseline starts with the route out; the agent phase removes it.
        let c = egress_compose(EgressMethod::Proxy, "img", "p1", &NetworkPolicy::public(), &services, &[]).unwrap();
        assert_eq!(c["services"]["api"]["networks"], json!(["default", OUTSIDE_NETWORK]));
        let c = egress_compose(EgressMethod::NftProxy, "img", "p1", &allow, &services, &["db".into()]).unwrap();
        assert_eq!(c["services"]["main"]["network_mode"], json!("service:hbench-egress-sidecar"));
        assert!(c.get("networks").is_none());
        assert!(c["services"].get("db").is_none());
    }

    #[test]
    fn reads_denials_from_the_gost_log() {
        let log = r#"{"bypass":"allowlist","kind":"bypass","level":"debug","msg":"bypass: example.com:443, whitelist: true"}
{"bypass":"allowlist","kind":"bypass","level":"debug","msg":"proxy: pypi.org:80, whitelist: true"}
{"kind":"handler","level":"debug","msg":"bypass: example.com:443"}
not json
{"bypass":"allowlist","kind":"bypass","level":"debug","msg":"bypass: 1.1.1.1:53, whitelist: true"}
{"bypass":"allowlist","kind":"bypass","level":"debug","msg":"bypass: example.com:443, whitelist: true"}"#;
        assert_eq!(parse_denials(log), ["1.1.1.1:53", "example.com:443"]);
    }

    #[test]
    fn egress_services() {
        let d = std::env::temp_dir().join(format!("hbench-eg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("c.yaml");
        std::fs::write(
            &f,
            "services:\n  main: {}\n  api: {image: x}\n  db: {image: y, network_mode: host}\n  web: {networks: [default]}\n",
        )
        .unwrap();
        assert_eq!(egress_eligible_services(std::slice::from_ref(&f)).unwrap(), ["api", "main"]);
        assert_eq!(own_network_services(&[f]).unwrap(), ["web"]);
        assert_eq!(egress_eligible_services(&[]).unwrap(), ["main"]);
    }
}
