//! upstream-compatible task format: task.toml + instruction.md + environment/ + tests/ (+ solution/, steps/).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

pub const MAIN: &str = "main";
pub const CONVENTION_ARTIFACTS: &str = "/logs/artifacts";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TaskConfig {
    pub schema_version: Option<String>,
    pub task: Option<PackageInfo>,
    pub metadata: toml::Table,
    pub verifier: VerifierConfig,
    pub agent: PhaseConfig,
    pub environment: EnvironmentConfig,
    pub solution: SolutionConfig,
    pub source: Option<String>,
    pub multi_step_reward_strategy: Option<RewardStrategy>,
    pub steps: Option<Vec<StepConfig>>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RewardStrategy {
    Mean,
    Final,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PackageInfo {
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
}

/// `[agent]` / `[steps.agent]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PhaseConfig {
    pub timeout_sec: Option<f64>,
    pub user: Option<User>,
    pub network_mode: Option<NetworkMode>,
    pub allowed_hosts: Option<Vec<String>>,
}

impl PhaseConfig {
    pub fn explicit_policy(&self) -> Option<NetworkPolicy> {
        self.network_mode.map(|m| NetworkPolicy { mode: m, allowed_hosts: self.allowed_hosts.clone().unwrap_or_default() })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifierMode {
    Shared,
    Separate,
}

/// `[verifier]` / `[steps.verifier]`. Step-level timeout_sec is optional (inherits).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct VerifierConfig {
    pub timeout_sec: Option<f64>,
    pub env: BTreeMap<String, String>,
    pub user: Option<User>,
    pub environment_mode: Option<VerifierMode>,
    pub environment: Option<Box<EnvironmentConfig>>,
    pub collect: Vec<CollectHook>,
    pub network_mode: Option<NetworkMode>,
    pub allowed_hosts: Option<Vec<String>>,
}

impl VerifierConfig {
    fn mode(&self) -> Option<VerifierMode> {
        self.environment_mode.or(self.environment.as_ref().map(|_| VerifierMode::Separate))
    }
    pub fn explicit_policy(&self) -> Option<NetworkPolicy> {
        self.network_mode.map(|m| NetworkPolicy { mode: m, allowed_hosts: self.allowed_hosts.clone().unwrap_or_default() })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CollectHook {
    pub command: String,
    #[serde(default = "main_service")]
    pub service: String,
    #[serde(default = "sixty")]
    pub timeout_sec: f64,
    #[serde(default)]
    pub user: Option<User>,
}
fn main_service() -> String {
    MAIN.into()
}
fn sixty() -> f64 {
    60.0
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SolutionConfig {
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StepConfig {
    pub name: String,
    #[serde(default)]
    pub agent: PhaseConfig,
    #[serde(default)]
    pub verifier: VerifierConfig,
    #[serde(default)]
    pub min_reward: Option<MinReward>,
    #[serde(default)]
    pub healthcheck: Option<Healthcheck>,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MinReward {
    One(f64),
    Keys(BTreeMap<String, f64>),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Healthcheck {
    pub command: String,
    #[serde(default = "five")]
    pub interval_sec: f64,
    #[serde(default = "thirty")]
    pub timeout_sec: f64,
    #[serde(default)]
    pub start_period_sec: f64,
    #[serde(default = "five")]
    pub start_interval_sec: f64,
    #[serde(default = "three")]
    pub retries: u32,
}
fn five() -> f64 {
    5.0
}
fn thirty() -> f64 {
    30.0
}
fn three() -> u32 {
    3
}

/// Artifact entry: `"/path"` or `{ source, destination?, exclude?, service? }`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum Artifact {
    Path(String),
    Full {
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactSpec {
    pub source: String,
    pub destination: Option<String>,
    pub exclude: Vec<String>,
    pub service: Option<String>,
}

impl Artifact {
    pub fn spec(&self) -> ArtifactSpec {
        match self {
            Artifact::Path(s) => ArtifactSpec { source: s.clone(), destination: None, exclude: vec![], service: None },
            Artifact::Full { source, destination, exclude, service } => ArtifactSpec {
                source: source.clone(),
                destination: destination.clone().filter(|d| !d.is_empty()),
                exclude: exclude.clone(),
                service: service.clone(),
            },
        }
    }
}

impl ArtifactSpec {
    pub fn service(&self) -> &str {
        self.service.as_deref().unwrap_or(MAIN)
    }
    pub fn is_convention(&self) -> bool {
        self.source.trim_end_matches('/') == CONVENTION_ARTIFACTS && self.service() == MAIN
    }
    pub fn validate(&self) -> Result<()> {
        if Path::new(&self.source).components().any(|c| c == Component::ParentDir) {
            bail!("Artifact source must not contain '..' components, got: {:?}", self.source);
        }
        if self.service() != MAIN && !self.source.starts_with('/') {
            bail!("Artifact source {:?} from service {:?} must be an absolute path.", self.source, self.service());
        }
        if let Some(d) = &self.destination {
            let p = Path::new(d);
            if d.contains('\\') || p.is_absolute() || p.components().any(|c| c == Component::ParentDir) {
                bail!("Artifact destination must be a relative forward-slash path without '..', got: {d:?}");
            }
            if d.trim_end_matches('/') == "manifest.json" {
                bail!("Artifact destination 'manifest.json' is reserved for the collection manifest.");
            }
        }
        Ok(())
    }
}

/// Merge entries, prepending the implicit `/logs/artifacts` convention entry unless declared.
pub fn with_convention(entries: &[Artifact]) -> Vec<ArtifactSpec> {
    let mut v: Vec<ArtifactSpec> = entries.iter().map(Artifact::spec).collect();
    if !v.iter().any(ArtifactSpec::is_convention) {
        v.insert(0, ArtifactSpec { source: CONVENTION_ARTIFACTS.into(), destination: None, exclude: vec![], service: None });
    }
    v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkMode {
    Public,
    NoNetwork,
    Allowlist,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkPolicy {
    #[serde(rename = "network_mode")]
    pub mode: NetworkMode,
    pub allowed_hosts: Vec<String>,
}

impl NetworkPolicy {
    pub fn public() -> NetworkPolicy {
        NetworkPolicy { mode: NetworkMode::Public, allowed_hosts: vec![] }
    }
}

/// Users may be given by name or uid.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum User {
    Name(String),
    Id(i64),
}

impl std::fmt::Display for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            User::Name(s) => f.write_str(s),
            User::Id(i) => write!(f, "{i}"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct EnvironmentConfig {
    pub build_timeout_sec: f64,
    pub docker_image: Option<String>,
    pub os: Option<String>,
    pub cpus: Option<u32>,
    pub memory_mb: Option<u64>,
    /// Deprecated "2G"/"512M" form, migrated into memory_mb.
    pub memory: Option<String>,
    pub storage_mb: Option<u64>,
    pub storage: Option<String>,
    pub gpus: Option<u32>,
    pub gpu_types: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub workdir: Option<String>,
    pub network_mode: Option<NetworkMode>,
    pub allowed_hosts: Option<Vec<String>>,
    /// Deprecated; false == network_mode "no-network".
    pub allow_internet: Option<bool>,
    pub healthcheck: Option<Healthcheck>,
    pub mcp_servers: Vec<McpServer>,
    pub skills_dir: Option<String>,
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        Self {
            build_timeout_sec: 600.0,
            docker_image: None,
            os: None,
            cpus: None,
            memory_mb: None,
            memory: None,
            storage_mb: None,
            storage: None,
            gpus: None,
            gpu_types: None,
            env: BTreeMap::new(),
            workdir: None,
            network_mode: None,
            allowed_hosts: None,
            allow_internet: None,
            healthcheck: None,
            mcp_servers: vec![],
            skills_dir: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct McpServer {
    pub name: String,
    #[serde(default = "sse")]
    pub transport: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}
fn sse() -> String {
    "sse".into()
}

impl McpServer {
    pub fn transport(&self) -> &str {
        if self.transport == "http" { "streamable-http" } else { &self.transport }
    }
}

fn parse_size_mb(s: &str) -> Result<u64> {
    let u = s.trim().to_uppercase();
    let (num, mult) = match u.chars().last() {
        Some('G') => (&u[..u.len() - 1], 1024.0),
        Some('M') => (&u[..u.len() - 1], 1.0),
        Some('K') => (&u[..u.len() - 1], 1.0 / 1024.0),
        _ => bail!("Invalid size format: {s}. Expected format like '1G', '512M', etc."),
    };
    Ok((num.parse::<f64>()? * mult) as u64)
}

impl EnvironmentConfig {
    pub fn effective_memory_mb(&self) -> Result<Option<u64>> {
        if self.memory_mb.is_some() {
            return Ok(self.memory_mb);
        }
        self.memory.as_deref().map(parse_size_mb).transpose()
    }

    /// `[environment]` baseline network policy (legacy allow_internet honored).
    pub fn baseline(&self) -> NetworkPolicy {
        let mode = match (self.network_mode, self.allow_internet) {
            (Some(m), _) => m,
            (None, Some(false)) => NetworkMode::NoNetwork,
            _ => NetworkMode::Public,
        };
        NetworkPolicy { mode, allowed_hosts: self.allowed_hosts.clone().unwrap_or_default() }
    }

    pub fn is_windows(&self) -> bool {
        self.os.as_deref().is_some_and(|o| o.eq_ignore_ascii_case("windows"))
    }
}

/// Validate + normalize allowlist hosts (hostnames, `*.suffix`, IPs, CIDRs).
pub fn normalize_hosts(hosts: &[String]) -> Result<Vec<String>> {
    let label = regex::Regex::new(r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$").unwrap();
    let mut out = vec![];
    for h in hosts {
        let h = h.trim().to_lowercase();
        let h = h.trim_end_matches('.').to_string();
        if h.is_empty() {
            bail!("allowed_hosts entries must be non-empty hostnames.");
        }
        if let Some((addr, bits)) = h.split_once('/') {
            let ip: std::net::IpAddr = addr.parse().map_err(|_| anyhow::anyhow!("invalid allowed_hosts entry {h:?}"))?;
            let bits: u8 = bits.parse().map_err(|_| anyhow::anyhow!("invalid CIDR {h:?}"))?;
            if bits > if ip.is_ipv4() { 32 } else { 128 } {
                bail!("invalid CIDR {h:?}");
            }
            out.push(h);
            continue;
        }
        if let Ok(ip) = h.parse::<std::net::IpAddr>() {
            out.push(ip.to_string());
            continue;
        }
        if h.contains(':') || h.contains('[') {
            bail!("allowed_hosts entries must be hostnames, IP addresses, or IP CIDR ranges, not URLs, ports, or paths.");
        }
        let name = match h.strip_prefix("*.") {
            Some(rest) => {
                if rest.parse::<std::net::IpAddr>().is_ok() || rest.is_empty() {
                    bail!("allowed_hosts wildcard entries must target hostnames");
                }
                rest
            }
            None if h.contains('*') => bail!("allowed_hosts wildcard entries must use a leading '*.' prefix."),
            None => &h,
        };
        if !name.split('.').all(|l| label.is_match(l)) {
            bail!("invalid allowed_hosts entry {h:?}");
        }
        out.push(h.clone());
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct Task {
    pub dir: PathBuf,
    pub config: TaskConfig,
    pub instruction: String,
}

impl Task {
    pub fn load(dir: &Path) -> Result<Task> {
        let dir = dir.canonicalize().with_context(|| format!("task dir {}", dir.display()))?;
        let toml_path = dir.join("task.toml");
        let raw = std::fs::read_to_string(&toml_path).with_context(|| format!("reading {}", toml_path.display()))?;
        let mut config: TaskConfig = toml::from_str(&raw).with_context(|| format!("parsing {}", toml_path.display()))?;
        if config.environment.is_windows() {
            bail!("{}: windows tasks are not supported", dir.display());
        }
        normalize_env_config(&mut config.environment)?;
        if let Some(v) = config.verifier.environment.as_mut() {
            normalize_env_config(v)?;
        }
        let steps = config.steps.clone().unwrap_or_default();
        let mut names = std::collections::BTreeSet::new();
        for s in &steps {
            if s.name.is_empty() || s.name.contains(['/', '\\']) || s.name == "." || s.name == ".." {
                bail!("{}: invalid step name {:?}", dir.display(), s.name);
            }
            if !names.insert(&s.name) {
                bail!("{}: duplicate step name {:?}", dir.display(), s.name);
            }
            if let Some(v) = &s.verifier.environment
                && s.verifier.environment_mode == Some(VerifierMode::Shared)
            {
                let _ = v;
                bail!("step {}: environment_mode='shared' is incompatible with [steps.verifier.environment]", s.name);
            }
        }
        if config.verifier.environment.is_some() && config.verifier.environment_mode == Some(VerifierMode::Shared) {
            bail!("[verifier].environment_mode='shared' is incompatible with [verifier.environment]");
        }
        for a in config.artifacts.iter().chain(steps.iter().flat_map(|s| s.artifacts.iter())) {
            a.spec().validate()?;
        }
        let instruction = if steps.is_empty() {
            strip_canary(&std::fs::read_to_string(dir.join("instruction.md")).with_context(|| format!("reading {}/instruction.md", dir.display()))?)
        } else {
            for s in &steps {
                let p = dir.join("steps").join(&s.name).join("instruction.md");
                if !p.is_file() {
                    bail!("missing {}", p.display());
                }
            }
            String::new()
        };
        let task = Task { dir, config, instruction };
        let env_dir = task.env_dir();
        if !env_dir.join("Dockerfile").exists()
            && !env_dir.join("docker-compose.yaml").exists()
            && task.config.environment.docker_image.is_none()
        {
            bail!("{}: needs environment/Dockerfile, environment/docker-compose.yaml or [environment].docker_image", task.dir.display());
        }
        Ok(task)
    }

    pub fn is_task_dir(dir: &Path) -> bool {
        dir.join("task.toml").is_file()
    }

    /// `[task].name` when present, else the directory name.
    pub fn name(&self) -> String {
        self.config.task.as_ref().and_then(|t| t.name.clone()).unwrap_or_else(|| self.dir_name())
    }
    pub fn dir_name(&self) -> String {
        self.dir.file_name().unwrap().to_string_lossy().into_owned()
    }
    pub fn steps(&self) -> &[StepConfig] {
        self.config.steps.as_deref().unwrap_or(&[])
    }
    pub fn has_steps(&self) -> bool {
        !self.steps().is_empty()
    }
    pub fn env_dir(&self) -> PathBuf {
        self.dir.join("environment")
    }
    pub fn tests_dir(&self) -> PathBuf {
        self.dir.join("tests")
    }
    pub fn solution_dir(&self) -> PathBuf {
        self.dir.join("solution")
    }
    pub fn step_dir(&self, step: &str) -> PathBuf {
        self.dir.join("steps").join(step)
    }
    pub fn step_instruction(&self, step: &str) -> Result<String> {
        let p = self.step_dir(step).join("instruction.md");
        Ok(strip_canary(&std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?))
    }

    /// Host-side test scripts the verifier will need (upstream's `_validate_tests`).
    pub fn validate_tests(&self) -> Result<()> {
        if !self.has_steps() {
            if self.verifier_env_definition(None).is_some_and(|(_, _, bundled)| bundled) {
                return Ok(());
            }
            if !self.tests_dir().join("test.sh").is_file() {
                bail!("{} does not contain tests/test.sh", self.dir.display());
            }
            return Ok(());
        }
        for s in self.steps() {
            if self.verifier_env_definition(Some(s)).is_some_and(|(_, _, bundled)| bundled) {
                continue;
            }
            if !self.step_dir(&s.name).join("tests/test.sh").is_file() && !self.tests_dir().join("test.sh").is_file() {
                bail!("step {:?} has neither steps/{0}/tests/test.sh nor tests/test.sh", s.name);
            }
        }
        Ok(())
    }
    pub fn checksum(&self) -> Result<String> {
        hash_dir(&self.dir)
    }

    pub fn verifier_mode(&self, step: Option<&StepConfig>) -> VerifierMode {
        step.and_then(|s| s.verifier.mode()).or(self.config.verifier.mode()).unwrap_or(VerifierMode::Shared)
    }

    /// Where the separate verifier environment comes from: (env config, build dir, tests bundled in image).
    pub fn verifier_env_definition(&self, step: Option<&StepConfig>) -> Option<(EnvironmentConfig, PathBuf, bool)> {
        if self.verifier_mode(step) != VerifierMode::Separate {
            return None;
        }
        let base: EnvironmentConfig = step
            .and_then(|s| s.verifier.environment.as_deref().cloned())
            .or_else(|| self.config.verifier.environment.as_deref().cloned())
            .unwrap_or_else(|| self.config.environment.clone());
        let mut candidates: Vec<(Option<&EnvironmentConfig>, PathBuf)> = vec![];
        if let Some(s) = step {
            candidates.push((s.verifier.environment.as_deref(), self.step_dir(&s.name).join("tests")));
        }
        candidates.push((self.config.verifier.environment.as_deref(), self.tests_dir()));
        for (env, dir) in candidates {
            if let Some(img) = env.and_then(|e| e.docker_image.clone()) {
                return Some((EnvironmentConfig { docker_image: Some(img), ..base }, dir, true));
            }
            if dir.join("Dockerfile").is_file() || dir.join("docker-compose.yaml").is_file() {
                return Some((EnvironmentConfig { docker_image: None, ..base }, dir, true));
            }
        }
        let os = base.os.clone().or(self.config.environment.os.clone());
        Some((EnvironmentConfig { docker_image: self.config.environment.docker_image.clone(), os, ..base }, self.env_dir(), false))
    }
}

fn normalize_env_config(e: &mut EnvironmentConfig) -> Result<()> {
    if let Some(m) = e.memory.take() {
        let mb = parse_size_mb(&m)?;
        if e.memory_mb.is_some_and(|x| x != mb) {
            bail!("Conflicting 'memory' and 'memory_mb' values");
        }
        e.memory_mb.get_or_insert(mb);
    }
    if let Some(s) = e.storage.take() {
        e.storage_mb.get_or_insert(parse_size_mb(&s)?);
    }
    if e.network_mode.is_none()
        && let Some(false) = e.allow_internet
    {
        e.network_mode = Some(NetworkMode::NoNetwork);
    }
    e.allow_internet = None;
    if let Some(h) = &e.allowed_hosts {
        e.allowed_hosts = Some(normalize_hosts(h)?);
        if e.network_mode != Some(NetworkMode::Allowlist) {
            bail!("allowed_hosts is only valid when network_mode='allowlist'.");
        }
    }
    Ok(())
}

/// Drop leading canary comment lines (`<!-- ...canary... -->` / `# ...canary...`) and blank lines after them.
pub fn strip_canary(text: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"(?i)^(<!--.*canary.*-->|#.*canary.*)$").unwrap());
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    while i < lines.len() && re.is_match(lines[i].trim()) {
        i += 1;
    }
    if i > 0 {
        while i < lines.len() && lines[i].trim().is_empty() {
            i += 1;
        }
    }
    lines[i..].join("\n")
}

/// Stable sha256 over relative paths + contents of every file under `dir`.
pub fn hash_dir(dir: &Path) -> Result<String> {
    let mut files = Vec::new();
    if dir.exists() {
        walk(dir, &mut files)?;
    }
    files.sort();
    let mut h = Sha256::new();
    for f in files {
        let rel = f.strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
        h.update(rel.as_bytes());
        h.update([0]);
        h.update(std::fs::read(&f)?);
        h.update([0]);
    }
    Ok(hex(&h.finalize()))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_dir() {
            walk(&p, out)?;
        } else if p.is_file() {
            out.push(p);
        }
    }
    Ok(())
}

/// Resolve `${VAR}` / `${VAR:-default}` templates against the host env (upstream semantics).
pub fn resolve_env_vars(env: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (k, v) in env {
        out.insert(k.clone(), resolve_template(k, v)?);
    }
    Ok(out)
}

pub fn resolve_template(k: &str, v: &str) -> Result<String> {
    Ok(match v.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        Some(inner) => {
            let (var, default) = match inner.split_once(":-") {
                Some((a, b)) => (a, Some(b)),
                None => (inner, None),
            };
            match (std::env::var(var), default) {
                (Ok(val), _) => val,
                (Err(_), Some(d)) => d.to_string(),
                (Err(_), None) => bail!("Environment variable {var} not found (needed for {k})"),
            }
        }
        None => v.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_legacy_and_current_task_toml() {
        let mut legacy: TaskConfig = toml::from_str(
            "version = \"1.0\"\n[metadata]\ndifficulty='easy'\nexpert_time_estimate_min = 45\n\
             [verifier]\ntimeout_sec = 900.0\n[agent]\ntimeout_sec = 900.0\n\
             [environment]\ndocker_image = \"x:1\"\nmemory = \"2G\"\nallow_internet = false\n",
        )
        .unwrap();
        normalize_env_config(&mut legacy.environment).unwrap();
        assert_eq!(legacy.verifier.timeout_sec, Some(900.0));
        assert_eq!(legacy.environment.memory_mb, Some(2048));
        assert_eq!(legacy.environment.baseline().mode, NetworkMode::NoNetwork);
        let cur: TaskConfig = toml::from_str(
            "schema_version='1.4'\nartifacts=['/a', {source='/b', service='api', exclude=['*.log']}]\n\
             [task]\nname='hbench/hello'\n[agent]\nuser = 1000\n[environment]\nworkdir='/w'\n\
             [verifier]\nenvironment_mode='separate'\n[[verifier.collect]]\ncommand='x'\nservice='api'\n\
             [[steps]]\nname='one'\nmin_reward=1.0\n[steps.healthcheck]\ncommand='true'\n\
             [[steps]]\nname='two'\nmin_reward={a=0.5}\n[steps.verifier]\ntimeout_sec=5\n",
        )
        .unwrap();
        assert_eq!(cur.task.unwrap().name.unwrap(), "hbench/hello");
        assert_eq!(cur.agent.user.unwrap().to_string(), "1000");
        assert_eq!(cur.verifier.environment_mode, Some(VerifierMode::Separate));
        assert_eq!(cur.verifier.collect[0].timeout_sec, 60.0);
        let steps = cur.steps.unwrap();
        assert_eq!(steps[1].verifier.timeout_sec, Some(5.0));
        assert!(matches!(steps[1].min_reward, Some(MinReward::Keys(_))));
        assert_eq!(steps[0].healthcheck.as_ref().unwrap().retries, 3);
        let specs = with_convention(&cur.artifacts);
        assert!(specs[0].is_convention() && specs[2].service() == "api" && specs.len() == 3);
    }

    #[test]
    fn canary_stripping() {
        assert_eq!(strip_canary("# BENCHMARK canary GUID x\n<!-- canary -->\n\nDo it\n"), "Do it\n");
        assert_eq!(strip_canary("Do it\n# canary\n"), "Do it\n# canary\n");
        assert_eq!(strip_canary("\nDo it"), "\nDo it");
    }

    #[test]
    fn host_normalization() {
        let ok = normalize_hosts(&["Example.COM.".into(), "*.pypi.org".into(), "10.0.0.0/8".into(), "::1".into()]).unwrap();
        assert_eq!(ok, ["example.com", "*.pypi.org", "10.0.0.0/8", "::1"]);
        for bad in ["https://x.com", "x.com:443", "a*.b", "*.1.2.3.4", ""] {
            assert!(normalize_hosts(&[bad.into()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn artifact_validation() {
        let s = |src: &str, dst: Option<&str>, svc: Option<&str>| ArtifactSpec {
            source: src.into(),
            destination: dst.map(String::from),
            exclude: vec![],
            service: svc.map(String::from),
        };
        assert!(s("/a/../b", None, None).validate().is_err());
        assert!(s("rel", None, Some("api")).validate().is_err());
        assert!(s("/a", Some("manifest.json"), None).validate().is_err());
        assert!(s("/a", Some("/abs"), None).validate().is_err());
        assert!(s("/a", Some("x/y"), Some("api")).validate().is_ok());
    }

    #[test]
    fn env_templates() {
        // SAFETY: single-threaded test touching a unique var.
        unsafe { std::env::set_var("HBENCH_TEST_VAR", "abc") };
        let m: BTreeMap<String, String> = [("A", "${HBENCH_TEST_VAR}"), ("B", "${HBENCH_MISSING:-dflt}"), ("C", "lit")]
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect();
        let r = resolve_env_vars(&m).unwrap();
        assert_eq!((r["A"].as_str(), r["B"].as_str(), r["C"].as_str()), ("abc", "dflt", "lit"));
        let bad: BTreeMap<String, String> = [("X".into(), "${HBENCH_MISSING}".into())].into();
        assert!(resolve_env_vars(&bad).is_err());
    }
}
