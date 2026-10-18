//! Docker Compose environment.
//!
//! Files move with `docker compose cp` (never bind mounts for /logs), so the same
//! code works against remote daemons (ssh contexts). Network allowlists use
//! an egress-control sidecar (vendored under assets/egress-sidecar).

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

/// Error carrying an exception type name (used for result.json + retry policy).
#[derive(Debug)]
pub struct KindError {
    pub kind: &'static str,
    pub msg: String,
}
impl std::fmt::Display for KindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}
impl std::error::Error for KindError {}

pub fn kind_err(kind: &'static str, msg: impl Into<String>) -> anyhow::Error {
    KindError { kind, msg: msg.into() }.into()
}

pub fn error_kind(e: &anyhow::Error) -> &'static str {
    e.chain().find_map(|c| c.downcast_ref::<KindError>()).map(|k| k.kind).unwrap_or("RuntimeError")
}

/// Append-only per-trial log (trial.log).
pub struct TrialLog(Mutex<std::fs::File>);
impl TrialLog {
    pub fn create(path: &Path) -> Result<Arc<TrialLog>> {
        let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Arc::new(TrialLog(Mutex::new(f))))
    }
    pub fn line(&self, s: &str) {
        let ts = chrono::Utc::now().format("%H:%M:%S%.3f");
        let _ = writeln!(self.0.lock().unwrap(), "[{ts}] {s}");
    }
}

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

const EGRESS_SERVICE: &str = "hbench-egress-sidecar";
const EGRESS_READY: &str = "/tmp/hbench-egress-sidecar.ready";
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
    egress: bool,
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

fn sv(a: &[&str]) -> Vec<String> {
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

/// Services in task compose files that don't declare their own networking.
fn egress_eligible_services(files: &[PathBuf]) -> Result<Vec<String>> {
    let mut explicit: BTreeMap<String, bool> = BTreeMap::new();
    for f in files {
        let doc: Value = serde_yaml_ng::from_str(&std::fs::read_to_string(f)?).with_context(|| format!("parsing {}", f.display()))?;
        if let Some(services) = doc.get("services").and_then(Value::as_object) {
            for (name, cfg) in services {
                let e = cfg.get("network_mode").is_some() || cfg.get("networks").is_some();
                *explicit.entry(name.clone()).or_default() |= e;
            }
        }
    }
    explicit.entry(MAIN.into()).or_insert(false);
    Ok(explicit.into_iter().filter(|(n, e)| !e && n != EGRESS_SERVICE).map(|(n, _)| n).collect())
}

/// Whether the daemon's kernel supports the nftables rules the egress sidecar needs.
async fn egress_kernel_support() -> bool {
    static CACHE: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    *CACHE
        .get_or_init(|| async {
            let script =
                "if [ ! -f /proc/config.gz ]; then exit 0; fi; zcat /proc/config.gz 2>/dev/null | grep -qE '^CONFIG_NFT_FIB_INET=[ym]'";
            let out = tokio::time::timeout(
                Duration::from_secs(120),
                Command::new("docker").args(["container", "run", "--rm", "alpine:3.23", "sh", "-c", script]).kill_on_drop(true).output(),
            )
            .await;
            matches!(out, Ok(Ok(o)) if o.status.success())
        })
        .await
}

/// Build (once) the vendored egress sidecar image, content-addressed.
async fn egress_image(log: &TrialLog) -> Result<String> {
    static IMAGE: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    IMAGE
        .get_or_try_init(|| async {
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
            let exists = Command::new("docker").args(["image", "inspect", &image]).output().await?.status.success();
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
                let out = Command::new("docker").arg("build").arg("-t").arg(&image).arg(&dir).output().await?;
                if !out.status.success() {
                    bail!("building egress sidecar failed: {}", String::from_utf8_lossy(&out.stderr));
                }
            }
            Ok(image)
        })
        .await
        .cloned()
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
                let image = sanitize(&format!("hb__{name}-{}", &crate::task::hash_dir(env_dir)?[..12]), true);
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

        // Network: egress sidecar when any phase needs no-network/allowlist; plain
        // `network_mode: none` fallback only for a constant no-network policy.
        let policies: Vec<&NetworkPolicy> = std::iter::once(&spec.baseline).chain(spec.phase_policies.iter()).collect();
        let restricted = policies.iter().any(|p| p.mode != NetworkMode::Public);
        let mut egress = false;
        if restricted {
            if egress_kernel_support().await {
                egress = true;
            } else if policies.iter().all(|p| p.mode == NetworkMode::NoNetwork) {
                main.insert("network_mode".into(), json!("none"));
            } else {
                bail!("network allowlists need nftables (CONFIG_NFT_FIB_INET) on the docker host kernel");
            }
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
        if !startup_env.is_empty() {
            files.push(write("docker-compose-env.json", json!({"services": {"main": {"environment": startup_env}}}))?);
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
        if egress {
            let image = egress_image(&log).await?;
            let mut services = serde_json::Map::new();
            services.insert(
                EGRESS_SERVICE.into(),
                json!({
                    "image": image,
                    "entrypoint": ["/opt/egress-sidecar/entrypoint.sh"],
                    "init": true,
                    "environment": {
                        "EGRESS_CONTROL_INITIAL_NETWORK_MODE": serde_json::to_value(spec.baseline.mode)?,
                        "EGRESS_CONTROL_INITIAL_ALLOWED_HOSTS": spec.baseline.allowed_hosts.join(" "),
                    },
                    "cap_add": ["NET_ADMIN", "NET_RAW"],
                    "healthcheck": {"test": ["CMD", "test", "-f", EGRESS_READY], "interval": "1s", "timeout": "5s", "retries": 30},
                }),
            );
            for s in egress_eligible_services(&user_files)? {
                services.insert(s, json!({"network_mode": format!("service:{EGRESS_SERVICE}"), "depends_on": {EGRESS_SERVICE: {"condition": "service_healthy"}}}));
            }
            files.push(write("docker-compose-egress.json", json!({"services": services}))?);
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
        Ok(DockerEnv { spec, files, compose_env, build_image, egress, log })
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
        let mut c = self.compose(&args);
        c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
        self.log.line(&format!("$ {label}"));
        let mut child = c.spawn().context("spawning `docker compose` (is docker installed?)")?;
        if let Some(data) = stdin {
            let mut si = child.stdin.take().unwrap();
            si.write_all(&data).await?;
            drop(si);
        }
        let out = match timeout {
            Some(t) => match tokio::time::timeout(t, child.wait_with_output()).await {
                Ok(r) => r?,
                Err(_) => {
                    self.log.line(&format!("timed out after {t:?}"));
                    return Err(kind_err("TimeoutError", format!("`{label}` timed out after {t:?}")));
                }
            },
            None => child.wait_with_output().await?,
        };
        let r = ExecResult {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        };
        self.log.line(&format!("exit={} stdout:\n{}stderr:\n{}", r.code, tail(&r.stdout, 20000), tail(&r.stderr, 20000)));
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
        let started = async {
            if let Some(image) = &self.build_image {
                static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<bool>>>>> = OnceLock::new();
                let lock = LOCKS.get_or_init(Default::default).lock().unwrap().entry(image.clone()).or_default().clone();
                // Serialize builds of one image; later trials in this process reuse it.
                let mut built = lock.lock().await;
                if !*built {
                    self.run_ok(sv(&["build"]), None).await?;
                    *built = true;
                }
            } else if self.spec.env_dir.join("docker-compose.yaml").exists() {
                self.run_ok(sv(&["build"]), None).await?;
            }
            let _ = self.run(sv(&["down", "--remove-orphans"]), Some(Duration::from_secs(120)), "down (stale)", None).await;
            self.run_ok(sv(&["up", "--detach", "--wait"]), None).await?;
            anyhow::Ok(())
        };
        match tokio::time::timeout(self.spec.build_timeout, started).await {
            Ok(r) => r.map_err(|e| kind_err("EnvironmentStartError", format!("{e:#}")))?,
            Err(_) => {
                return Err(kind_err(
                    "EnvironmentStartTimeoutError",
                    format!("Environment start timed out after {} seconds", self.spec.build_timeout.as_secs_f64()),
                ));
            }
        }
        if !self.spec.log_dirs.is_empty() {
            let d = self.spec.log_dirs.join(" ");
            let r = self.exec_root(&format!("mkdir -p {d} && chmod 777 /logs {d}"), Some(Duration::from_secs(60))).await?;
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
        let mut args = sv(&["exec", "-T"]);
        if let Some(w) = cwd {
            args.extend(sv(&["-w", w]));
        }
        for (k, v) in &merged {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        if let Some(u) = user {
            args.extend(sv(&["-u", u]));
        }
        args.extend(sv(&["main", "bash", "-c", command]));
        let keys: Vec<&str> = merged.keys().map(String::as_str).collect();
        // Log env keys only: values may be secrets.
        let label = format!("exec user={} env={keys:?}: {}", user.unwrap_or("-"), tail(command, 2000));
        self.run(args, timeout, &label, None).await
    }

    pub async fn exec_root(&self, command: &str, timeout: Option<Duration>) -> Result<ExecResult> {
        self.exec(command, Some("root"), &BTreeMap::new(), timeout).await
    }

    /// Exec in any service: main uses bash + main defaults; sidecars use `sh -c`, no workdir/env.
    pub async fn service_exec(&self, service: &str, command: &str, user: Option<&str>, timeout: Option<Duration>) -> Result<ExecResult> {
        if service == MAIN {
            return self.exec(command, user, &BTreeMap::new(), timeout).await;
        }
        let mut args = sv(&["exec", "-T"]);
        if let Some(u) = user {
            args.extend(sv(&["-u", u]));
        }
        args.extend(sv(&[service, "sh", "-c", command]));
        let label = format!("exec [{service}] {}", tail(command, 2000));
        self.run(args, timeout, &label, None).await
    }

    pub async fn is_dir(&self, service: &str, path: &str) -> Result<bool> {
        Ok(self.service_exec(service, &format!("test -d {}", quote(path)), Some("root"), Some(Duration::from_secs(60))).await?.code == 0)
    }

    pub async fn upload_dir(&self, src: &Path, dst: &str) -> Result<()> {
        let r = self.exec_root(&format!("mkdir -p {}", quote(dst)), Some(Duration::from_secs(60))).await?;
        if r.code != 0 {
            bail!("mkdir {dst} failed: {}", r.stderr);
        }
        self.run_ok(vec!["cp".into(), format!("{}/.", src.display()), format!("main:{dst}")], Some(Duration::from_secs(600)))
            .await
            .map(|_| ())
    }

    pub async fn upload_file(&self, src: &Path, dst: &str) -> Result<()> {
        if let Some(parent) = Path::new(dst).parent().map(|p| p.display().to_string()).filter(|p| !p.is_empty() && p != "/") {
            self.exec_root(&format!("mkdir -p {}", quote(&parent)), Some(Duration::from_secs(60))).await?;
        }
        self.run_ok(vec!["cp".into(), src.display().to_string(), format!("main:{dst}")], Some(Duration::from_secs(600))).await.map(|_| ())
    }

    pub async fn download_dir(&self, service: &str, src: &str, dst: &Path) -> Result<()> {
        std::fs::create_dir_all(dst)?;
        self.run_ok(
            vec!["cp".into(), format!("{service}:{}/.", src.trim_end_matches('/')), dst.display().to_string()],
            Some(Duration::from_secs(600)),
        )
        .await
        .map(|_| ())
    }

    pub async fn download_file(&self, service: &str, src: &str, dst: &Path) -> Result<()> {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        self.run_ok(vec!["cp".into(), format!("{service}:{src}"), dst.display().to_string()], Some(Duration::from_secs(600)))
            .await
            .map(|_| ())
    }

    /// Tar inside the service with `--exclude` patterns, copy the archive out, extract locally.
    pub async fn download_dir_excluding(&self, service: &str, src: &str, dst: &Path, exclude: &[String]) -> Result<()> {
        let tmp = format!("/tmp/hbench-transfer-{}.tar.gz", uuid::Uuid::new_v4().simple());
        let flags: Vec<String> = exclude.iter().map(|p| format!("--exclude={}", quote(p))).collect();
        let r = self
            .service_exec(
                service,
                &format!("tar czf {tmp} {} -C {} .", flags.join(" "), quote(src)),
                Some("root"),
                Some(Duration::from_secs(600)),
            )
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
            Ok(())
        }
        .await;
        let _ = std::fs::remove_file(&local);
        let _ = self.service_exec(service, &format!("rm -f {tmp}"), Some("root"), Some(Duration::from_secs(60))).await;
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
        self.run_ok(sv(&["stop", service]), Some(Duration::from_secs(120))).await.map(|_| ())
    }

    pub async fn set_network_policy(&self, p: &NetworkPolicy) -> Result<()> {
        if !self.egress {
            if p.mode == NetworkMode::Public && self.spec.baseline.mode == NetworkMode::Public {
                return Ok(());
            }
            bail!("Docker egress control was not enabled when the environment started, so it cannot enforce this network policy.");
        }
        let mut args = sv(&["exec", "--no-TTY", EGRESS_SERVICE, "network-policy"]);
        match p.mode {
            NetworkMode::Public => args.push("allow-all".into()),
            NetworkMode::NoNetwork => args.push("deny-all".into()),
            NetworkMode::Allowlist => {
                args.push("allow".into());
                args.extend(p.allowed_hosts.iter().cloned());
            }
        }
        self.run_ok(args, Some(Duration::from_secs(120))).await.map(|_| ())
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
                        "HealthcheckError",
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
        let r = self.exec_root(&cmd, Some(Duration::from_secs(120))).await?;
        if r.code != 0 {
            bail!("resetting {dirs:?} failed: {}", r.stderr);
        }
        Ok(())
    }

    /// The processes running in main now, as `pid:starttime` (starttime tells a reused pid apart).
    pub async fn process_snapshot(&self) -> Result<String> {
        let r = self.exec_root(&format!("{PROC_LIST}\nfor_each_proc echo"), Some(Duration::from_secs(60))).await?;
        if r.code != 0 {
            bail!("listing processes failed: {}", r.stderr);
        }
        Ok(r.stdout.split_whitespace().collect::<Vec<_>>().join(" "))
    }

    /// Kill every process in main that is not in `snapshot`. Returns how many were killed.
    pub async fn kill_processes_since(&self, snapshot: &str) -> Result<usize> {
        let r = self.exec_root(&kill_since_script(snapshot), Some(Duration::from_secs(60))).await?;
        if r.code != 0 {
            bail!("stopping processes failed: {}", r.stderr);
        }
        Ok(r.stdout.trim().parse().unwrap_or(0))
    }

    pub async fn stop(&self) -> Result<()> {
        let args = if self.spec.keep_containers {
            sv(&["stop"])
        } else if self.spec.delete {
            sv(&["down", "--rmi", "local", "--volumes", "--remove-orphans"])
        } else {
            sv(&["down"])
        };
        self.run_ok(args, Some(Duration::from_secs(300))).await.map(|_| ())
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

/// Stop (so nothing forks while we look) then kill everything not in `snapshot`, until none is left.
fn kill_since_script(snapshot: &str) -> String {
    format!(
        r#"{PROC_LIST}
keep=" {snapshot} "
pick() {{ case "$keep" in *" $1 "*) ;; *) kill -STOP "${{1%%:*}}" 2>/dev/null && new="$new ${{1%%:*}}";; esac; }}
total=0
for round in 1 2 3 4 5; do
  new=
  for_each_proc pick
  [ -z "$new" ] && break
  kill -KILL $new 2>/dev/null
  set -- $new
  total=$((total + $#))
done
echo $total"#
    )
}

fn cache_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// Fail fast with a readable message when the docker daemon is unreachable.
pub async fn preflight() -> Result<()> {
    let out = tokio::time::timeout(
        Duration::from_secs(30),
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
    use super::*;
    #[test]
    fn sanitizes_and_quotes() {
        assert_eq!(sanitize("Hello World__AbC", false), "hello-world__abc");
        assert_eq!(sanitize("_x.y", false), "0_x-y");
        assert_eq!(sanitize("hb__a.b", true), "hb__a.b");
        assert_eq!(tail("héllo", 3), "llo");
        assert_eq!(quote("/logs/agent"), "/logs/agent");
        assert_eq!(quote("a b'c"), r"'a b'\''c'");
    }

    #[test]
    fn egress_services() {
        let d = std::env::temp_dir().join(format!("hbench-eg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("c.yaml");
        std::fs::write(&f, "services:\n  main: {}\n  api: {image: x}\n  db: {image: y, network_mode: host}\n").unwrap();
        assert_eq!(egress_eligible_services(&[f]).unwrap(), ["api", "main"]);
        assert_eq!(egress_eligible_services(&[]).unwrap(), ["main"]);
    }
}
