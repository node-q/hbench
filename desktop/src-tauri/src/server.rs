//! The bundled `hbench view`, run on a free loopback port for the folder the app has open.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

pub struct Server {
    child: Child,
    /// The watchdog's lifeline (Unix): closing it, or the app dying, stops hbench.
    lifeline: Option<ChildStdin>,
    pub url: String,
    pub folder: PathBuf,
    /// Set once an exit nobody asked for has been noticed (and logged).
    exited: bool,
}

impl Server {
    /// The status hbench exited with, the first time it is seen to have exited on its own.
    pub fn exited_on_its_own(&mut self) -> Option<ExitStatus> {
        if self.exited {
            return None;
        }
        let status = self.child.try_wait().ok().flatten()?;
        self.exited = true;
        Some(status)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.lifeline.take().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if !self.url.is_empty() {
            tracing::info!(event = "server.stop");
        }
    }
}

/// Copy what hbench prints to its log, a line at a time, each redacted first. Says on `done` when the output ends.
fn pipe_to_log(from: impl Read + Send + 'static, log: Arc<locallog::Rotating>, home: Option<String>, done: mpsc::Sender<()>) {
    std::thread::spawn(move || {
        let mut r = BufReader::new(from);
        let mut buf = Vec::new();
        while matches!(r.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
            let line = String::from_utf8_lossy(&buf);
            let _ = log.append(&locallog::redact(line.trim_end_matches(['\r', '\n']), home.as_deref()));
            buf.clear();
        }
        let _ = done.send(());
    });
}

/// On Unix hbench runs under a small `sh` watchdog that kills it once its stdin, a pipe the app holds, closes.
/// The pipe closes when the app drops the server, quits, crashes or is killed, so hbench never outlives it.
/// The watchdog exits with hbench's status.
#[cfg(unix)]
fn command(bin: &Path) -> Command {
    let mut c = Command::new("/bin/sh");
    // Background jobs get /dev/null as stdin, so the lifeline reaches the reader on fd 3.
    // The reader's output goes nowhere, so hbench's ending ends the output the app reads.
    let script = r#"exec 3<&0; "$0" "$@" </dev/null & child=$!; (read _ <&3; kill "$child" 2>/dev/null) >/dev/null 2>&1 & wait "$child""#;
    c.arg("-c").arg(script).arg(bin);
    c
}

/// Elsewhere hbench runs directly and is killed when the server is dropped. On Windows it gets a console
/// without a window, which the docker and hbench processes it starts inherit, so none of them flashes one up.
#[cfg(not(unix))]
fn command(bin: &Path) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(bin);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000); // CREATE_NO_WINDOW
    c
}

/// hbench next to the app's own executable (where Tauri puts `externalBin`).
pub fn hbench_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("locating the app: {e}"))?;
    let dir = exe.parent().ok_or("the app has no parent directory")?;
    let name = if cfg!(windows) { "hbench.exe" } else { "hbench" };
    let p = dir.join(name);
    if p.is_file() { Ok(p) } else { Err(format!("hbench is missing from {}; run `npm run sidecar` in desktop/", dir.display())) }
}

/// Ports tried first, in order. The page's saved state (theme, side bar, tabs) is kept per origin, so a
/// stable port keeps it across launches; any free port is the fallback.
const PREFERRED_PORTS: std::ops::RangeInclusive<u16> = 47_810..=47_829;

fn free_port() -> Result<u16, String> {
    for p in PREFERRED_PORTS {
        if TcpListener::bind((Ipv4Addr::LOCALHOST, p)).is_ok() {
            return Ok(p);
        }
    }
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("finding a free port: {e}"))?;
    Ok(l.local_addr().map_err(|e| e.to_string())?.port())
}

/// The last `max` bytes of the log, for an error message.
fn log_tail(path: &Path, max: u64) -> String {
    let Ok(mut f) = File::open(path) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(max)));
    let mut s = String::new();
    let _ = f.read_to_string(&mut s);
    s.trim().to_string()
}

/// Wait a little for the output to reach the log, so an error can quote its end.
fn drain(done: &mpsc::Receiver<()>) {
    let until = Instant::now() + Duration::from_secs(2);
    for _ in 0..2 {
        if done.recv_timeout(until.saturating_duration_since(Instant::now())).is_err() {
            return;
        }
    }
}

/// Start `hbench view <folder>` and wait until it answers. Its output goes to `log` (redacted, size-capped; the
/// previous start's output becomes `log`.1), and its events, and those of the runs it starts, to `events`.
pub fn start(folder: &Path, env: &HashMap<String, String>, log: &Path, events: &Path) -> Result<Server, String> {
    let bin = hbench_path()?;
    let port = free_port()?;
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let out_log = Arc::new(locallog::Rotating::new(log));
    out_log.rotate().map_err(|e| format!("starting a new {}: {e}", log.display()))?;
    let home = env.get("HOME").or_else(|| env.get("USERPROFILE")).cloned();
    let mut cmd = command(&bin);
    cmd.arg("view").arg(folder).args(["--port", &port.to_string(), "--host", "127.0.0.1"]);
    // hbench can't tell what an empty folder is for; a new workspace collects jobs.
    let has_subdirs = std::fs::read_dir(folder).map(|d| d.flatten().any(|e| e.path().is_dir())).unwrap_or(false);
    if !has_subdirs {
        cmd.arg("--jobs");
    }
    cmd.envs(env)
        // The app updates as a whole; hbench's own update notice would only land in the log.
        .env("HBENCH_NO_UPDATE_CHECK", "1")
        .env("HBENCH_LOG_FILE", events)
        .current_dir(folder)
        .stdin(if cfg!(unix) { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("starting {}: {e}", bin.display()))?;
    let lifeline = child.stdin.take();
    let (done_tx, done) = mpsc::channel();
    if let Some(o) = child.stdout.take() {
        pipe_to_log(o, out_log.clone(), home.clone(), done_tx.clone());
    }
    if let Some(e) = child.stderr.take() {
        pipe_to_log(e, out_log.clone(), home, done_tx);
    }

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            drain(&done);
            return Err(format!("hbench view exited ({status}):\n{}", log_tail(log, 2000)));
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            break;
        }
        if Instant::now() > deadline {
            // Dropping the half-started server stops hbench (through its watchdog on Unix).
            drop(Server { child, lifeline, url: String::new(), folder: folder.to_path_buf(), exited: false });
            return Err(format!("hbench view did not start within 30s:\n{}", log_tail(log, 2000)));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(Server { child, lifeline, url: format!("http://127.0.0.1:{port}/"), folder: folder.to_path_buf(), exited: false })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A program run under the watchdog, with its lifeline held open as the app holds it: its output reaches the
    /// log redacted, and ends when it exits (so an early exit's message can quote it).
    #[test]
    fn output_is_redacted_into_the_log_and_ends_with_the_program() {
        let dir = std::env::temp_dir().join(format!("actuaria-server-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("prog.sh");
        std::fs::write(&script, "#!/bin/sh\necho \"listening, ANTHROPIC_API_KEY=sk-ant-api03-AbCdEf0123456789_xyz\"\necho \"/home/ann/jobs missing\" >&2\nexit 3\n")
            .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let log = Arc::new(locallog::Rotating::new(dir.join("view.log")));
        let mut child = command(&script).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let _lifeline = child.stdin.take();
        let (tx, done) = mpsc::channel();
        pipe_to_log(child.stdout.take().unwrap(), log.clone(), Some("/home/ann".into()), tx.clone());
        pipe_to_log(child.stderr.take().unwrap(), log.clone(), Some("/home/ann".into()), tx);
        assert_eq!(child.wait().unwrap().code(), Some(3));
        let started = Instant::now();
        drain(&done);
        assert!(started.elapsed() < Duration::from_millis(1500), "the output didn't end with the program");
        let text = log_tail(&dir.join("view.log"), 2000);
        assert!(text.contains("listening, ANTHROPIC_API_KEY=[REDACTED]"), "{text}");
        assert!(text.contains("~/jobs missing"), "{text}");
        assert!(!text.contains("sk-ant") && !text.contains("/home/ann"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
