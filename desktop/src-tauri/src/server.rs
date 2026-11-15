//! The bundled `hbench view`, run on a free loopback port for the folder the app has open.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Server {
    child: Child,
    /// The watchdog's lifeline (Unix): closing it, or the app dying, stops hbench.
    lifeline: Option<ChildStdin>,
    pub url: String,
    pub folder: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.lifeline.take().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// On Unix hbench runs under a small `sh` watchdog that kills it once its stdin, a pipe the app holds, closes.
/// The pipe closes when the app drops the server, quits, crashes or is killed, so hbench never outlives it.
/// The watchdog exits with hbench's status.
#[cfg(unix)]
fn command(bin: &Path) -> Command {
    let mut c = Command::new("/bin/sh");
    // Background jobs get /dev/null as stdin, so the lifeline reaches the reader on fd 3.
    let script = r#"exec 3<&0; "$0" "$@" </dev/null & child=$!; (read _ <&3; kill "$child" 2>/dev/null) & wait "$child""#;
    c.arg("-c").arg(script).arg(bin);
    c
}

#[cfg(not(unix))]
fn command(bin: &Path) -> Command {
    Command::new(bin)
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

/// Start `hbench view <folder>` and wait until it answers.
pub fn start(folder: &Path, env: &HashMap<String, String>, log: &Path) -> Result<Server, String> {
    let bin = hbench_path()?;
    let port = free_port()?;
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let out = File::create(log).map_err(|e| format!("creating {}: {e}", log.display()))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
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
        .current_dir(folder)
        .stdin(if cfg!(unix) { Stdio::piped() } else { Stdio::null() })
        .stdout(out)
        .stderr(err);
    let mut child = cmd.spawn().map_err(|e| format!("starting {}: {e}", bin.display()))?;
    let lifeline = child.stdin.take();

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("hbench view exited ({status}):\n{}", log_tail(log, 2000)));
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            break;
        }
        if Instant::now() > deadline {
            // Dropping the half-started server stops hbench (through its watchdog on Unix).
            drop(Server { child, lifeline, url: String::new(), folder: folder.to_path_buf() });
            return Err(format!("hbench view did not start within 30s:\n{}", log_tail(log, 2000)));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(Server { child, lifeline, url: format!("http://127.0.0.1:{port}/"), folder: folder.to_path_buf() })
}
