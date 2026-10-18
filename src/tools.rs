//! Local utilities: `tasks start-env`, `cache clean`, `trials handoff`.

use crate::agents::{self, AgentCtx};
use crate::config::AgentConfig;
use crate::env::{DockerEnv, EnvSpec, LOG_DIRS, TrialLog, cache_root};
use crate::task::{Task, resolve_env_vars};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Start a task's environment (force-built), optionally with solution/tests and an installed agent,
/// then attach an interactive shell; the environment is torn down on exit.
pub async fn start_env(path: &Path, all: bool, interactive: bool, agent: Option<AgentConfig>) -> Result<()> {
    let task = Arc::new(Task::load(path)?);
    let tmp = std::env::temp_dir().join(format!("hbench-env-{}", uuid::Uuid::new_v4().simple()));
    for d in ["agent", "verifier", "artifacts"] {
        std::fs::create_dir_all(tmp.join(d))?;
    }
    let log = TrialLog::create(&tmp.join("trial.log"))?;
    let session = format!("{}__{}__env", task.dir_name(), &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let ec = task.config.environment.clone();
    let spec = EnvSpec {
        project: crate::env::sanitize(&session, false),
        env_dir: task.env_dir(),
        compose_dir: tmp.join("compose"),
        force_build: true,
        build_timeout: Duration::from_secs_f64(ec.build_timeout_sec),
        baseline: ec.baseline(),
        config: ec,
        persistent_env: Default::default(),
        override_cpus: None,
        override_memory_mb: None,
        override_gpus: None,
        mounts: vec![],
        extra_compose: vec![],
        phase_policies: vec![],
        upload_env_dir: true,
        keep_containers: false,
        delete: true,
        log_dirs: LOG_DIRS.to_vec(),
    };
    let env = DockerEnv::new(spec, log).await?;
    let r = async {
        eprintln!("starting environment {session} ...");
        env.start().await?;
        if all {
            if task.solution_dir().is_dir() {
                env.upload_dir(&task.solution_dir(), "/solution").await?;
            }
            if task.tests_dir().is_dir() {
                env.upload_dir(&task.tests_dir(), "/tests").await?;
            }
        }
        if let Some(hc) = &task.config.environment.healthcheck {
            eprintln!("running healthcheck...");
            env.run_healthcheck(hc).await?;
        }
        if let Some(a) = agent {
            eprintln!("setting up agent {} ...", a.name());
            let mut ag = agents::create(&a, tmp.join("agent"), task.clone(), None)?;
            let overlay = ag.extra_env().map(Ok).unwrap_or_else(|| resolve_env_vars(&a.env))?;
            let cx = AgentCtx {
                env: &env,
                user: task.config.agent.user.as_ref().map(|u| u.to_string()),
                overlay,
                logs_dir: tmp.join("agent"),
                step: None,
            };
            ag.setup(&cx).await?;
            eprintln!("agent setup complete");
        }
        if interactive {
            env.attach().await?;
        }
        anyhow::Ok(())
    }
    .await;
    let _ = env.stop().await;
    let _ = std::fs::remove_dir_all(&tmp);
    r
}

/// Remove the hbench cache dir and hbench-built images (`hb__*`, `hbench-egress-sidecar`).
pub async fn cache_clean(force: bool, dry: bool, no_docker: bool, no_cache_dir: bool) -> Result<()> {
    let dir = cache_root();
    let size: u64 = if dir.exists() && !no_cache_dir { walk_size(&dir) } else { 0 };
    let mut images = vec![];
    if !no_docker {
        match tokio::process::Command::new("docker").args(["images", "--format", "{{.Repository}}:{{.Tag}}"]).output().await {
            Ok(o) if o.status.success() => {
                images = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|i| i.starts_with("hb__") || i.starts_with("hbench-egress-sidecar"))
                    .map(String::from)
                    .collect();
            }
            _ => eprintln!("could not list Docker images; skipping image cleanup"),
        }
    }
    if dir.exists() && !no_cache_dir {
        println!("Cache directory: {} ({:.1} MB)", dir.display(), size as f64 / 1048576.0);
    }
    println!("Docker images: {}", images.len());
    for i in &images {
        println!("  {i}");
    }
    if dry || (images.is_empty() && (no_cache_dir || !dir.exists())) {
        return Ok(());
    }
    if !force {
        eprint!("Proceed with cleanup? [y/N] ");
        let mut a = String::new();
        std::io::stdin().read_line(&mut a)?;
        if !matches!(a.trim(), "y" | "Y" | "yes") {
            bail!("aborted");
        }
    }
    if !images.is_empty() {
        let out = tokio::process::Command::new("docker").arg("rmi").arg("-f").args(&images).output().await?;
        if !out.status.success() {
            eprintln!("docker rmi: {}", String::from_utf8_lossy(&out.stderr));
        }
    }
    if dir.exists() && !no_cache_dir {
        std::fs::remove_dir_all(&dir)?;
    }
    println!("cleaned");
    Ok(())
}

fn walk_size(p: &Path) -> u64 {
    std::fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| if e.path().is_dir() { walk_size(&e.path()) } else { e.metadata().map(|m| m.len()).unwrap_or(0) })
        .sum()
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// Install a finished trial's native session for the local agent CLI; returns the resume command.
pub fn handoff_command(trial_dir: &Path, cwd: &Path) -> Result<Vec<String>> {
    let cfg: Value = serde_json::from_str(
        &std::fs::read_to_string(trial_dir.join("config.json"))
            .with_context(|| format!("{} is not a trial directory (missing config.json).", trial_dir.display()))?,
    )?;
    let agent = cfg["agent"]["name"].as_str().unwrap_or("oracle");
    match agent {
        "claude-code" => {
            if !which("claude") {
                bail!("claude CLI not found on PATH; install Claude Code first: npm install -g @anthropic-ai/claude-code");
            }
            let mut sessions: Vec<PathBuf> = vec![];
            let projects = trial_dir.join("agent/sessions/projects");
            for d in std::fs::read_dir(&projects).into_iter().flatten().flatten() {
                for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                    if f.path().extension().is_some_and(|e| e == "jsonl") {
                        sessions.push(f.path());
                    }
                }
            }
            if sessions.len() != 1 {
                bail!(
                    "Expected exactly 1 session in {}, found {}; handoff supports single-session trials only",
                    trial_dir.join("agent/sessions").display(),
                    sessions.len()
                );
            }
            let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude"));
            let slug: String =
                cwd.display().to_string().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect();
            let project = config_dir.join("projects").join(slug);
            std::fs::create_dir_all(&project)?;
            std::fs::copy(&sessions[0], project.join(sessions[0].file_name().unwrap()))?;
            Ok(vec!["claude".into(), "--resume".into(), sessions[0].file_stem().unwrap().to_string_lossy().into_owned()])
        }
        "muse-code" => {
            if !which("muse") {
                bail!("muse CLI not found on PATH; install Muse Code first");
            }
            let logs = trial_dir.join("agent");
            let manifest: Value =
                serde_json::from_str(&std::fs::read_to_string(logs.join(crate::agents::installed::muse_code::MANIFEST))?)?;
            let sid = uuid::Uuid::parse_str(manifest["session_id"].as_str().unwrap_or_default())?.to_string();
            let mut found = vec![];
            for e in walkdir(&logs.join("sessions")) {
                if e.ends_with(format!("{sid}/session.jsonl")) {
                    found.push(e);
                }
            }
            if found.len() != 1 {
                bail!("Expected exactly one matching Muse session for handoff");
            }
            let data = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share"));
            let store = data.join("muse/sessions");
            if walkdir(&store).iter().any(|p| p.to_string_lossy().contains(&format!("/{sid}/"))) {
                bail!("Muse session {sid} already exists locally");
            }
            let source = found[0].parent().unwrap();
            let target = store.join(source.strip_prefix(logs.join("sessions"))?);
            copy_tree(source, &target)?;
            let ws = cwd.canonicalize()?.display().to_string();
            for f in walkdir(&target).into_iter().filter(|p| p.ends_with("session.jsonl")) {
                let t = std::fs::read_to_string(&f)?;
                std::fs::write(&f, crate::agents::installed::muse_code::relocate_session(&t, &ws))?;
            }
            Ok(vec!["muse".into(), "--workspace".into(), ws, "resume".into(), sid])
        }
        other => bail!("agent '{other}' does not support handoff."),
    }
}

use serde_json::Value;

fn walkdir(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
        if e.path().is_dir() {
            out.extend(walkdir(&e.path()));
        } else {
            out.push(e.path());
        }
    }
    out
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        if e.path().is_dir() {
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        } else {
            std::fs::copy(e.path(), dst.join(e.file_name()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handoff_rejects_unsupported_agents() {
        let d = std::env::temp_dir().join(format!("hbench-ho-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("config.json"), r#"{"agent": {"name": "oracle"}}"#).unwrap();
        assert!(handoff_command(&d, &d).unwrap_err().to_string().contains("does not support handoff"));
        assert!(handoff_command(&d.join("missing"), &d).is_err());
    }
}
