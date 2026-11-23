//! The app's local logs and Help's "Copy Diagnostics" / "Save Diagnostics…".
//!
//! Three logs sit in the app's log folder: `essentials.log`, the app's own events (see the `locallog` crate for
//! the line format); `hbench-view.log`, what the bundled `hbench view` prints; and `hbench.log`, the events of
//! that server and of the runs it starts (`HBENCH_LOG_FILE`). Each is redacted line by line before it is
//! written, and capped at 5 files of 1 MB.
//!
//! Diagnostics are the app's version, the system, Docker's status and the last lines of each log, redacted
//! again. They go to the clipboard or to a zip the user picks, and nowhere else: nothing is sent.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

/// Lines of each log in the copied diagnostics.
pub const TAIL_LINES: usize = 200;
const DOCKER_TIMEOUT: Duration = Duration::from_secs(15);

/// The app's log folder. None when the system gives none: then nothing is logged to a file (never to a shared
/// temporary folder).
fn log_dir(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_log_dir().ok()
}

fn named(app: &AppHandle, stem: &str) -> Option<PathBuf> {
    log_dir(app).map(|d| d.join(if crate::DEV { format!("{stem}.dev.log") } else { format!("{stem}.log") }))
}

/// The app's own events.
pub fn app_log(app: &AppHandle) -> Option<PathBuf> {
    named(app, "essentials")
}

/// What `hbench view` prints.
pub fn view_log(app: &AppHandle) -> Option<PathBuf> {
    named(app, "hbench-view")
}

/// The events of `hbench view` and the runs it starts.
pub fn cli_log(app: &AppHandle) -> Option<PathBuf> {
    named(app, "hbench")
}

/// `text` with the open folder's path written as its name: folder paths stay out of the logs.
pub fn hide_folder(text: &str, folder: &Path) -> String {
    FolderMask::new(folder).apply(text)
}

/// The ways a folder's path appears in text: as given and resolved (symlinks), without a trailing separator, and
/// with `\` doubled (JSON on Windows). Each becomes `…/<name>`.
pub struct FolderMask {
    forms: Vec<String>,
    name: String,
}

impl FolderMask {
    pub fn new(folder: &Path) -> FolderMask {
        let trim = |p: &Path| p.display().to_string().trim_end_matches(['/', '\\']).to_string();
        let mut forms = vec![trim(folder)];
        if let Ok(real) = folder.canonicalize() {
            // Windows' verbatim prefix (`\\?\C:\…`) as well as without it.
            let real = trim(&real);
            forms.push(real.trim_start_matches(r"\\?\").to_string());
            forms.push(real);
        }
        for f in forms.clone() {
            forms.push(f.replace('\\', "\\\\"));
        }
        forms.retain(|f| f.len() > 1);
        forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
        forms.dedup();
        // The last component, whichever separator the path uses.
        let name = trim(folder).rsplit(['/', '\\']).next().unwrap_or_default().to_string();
        FolderMask { forms, name }
    }

    pub fn apply(&self, text: &str) -> String {
        let mut s = text.to_string();
        for f in &self.forms {
            s = s.replace(f, &format!("…/{}", self.name));
        }
        s
    }
}

pub fn home(app: &AppHandle) -> Option<String> {
    app.path().home_dir().ok().map(|h| h.to_string_lossy().into_owned()).filter(|h| h.len() > 1)
}

/// Send the app's events to `essentials.log`, from now on.
pub fn init(app: &AppHandle) {
    let Some(path) = app_log(app) else { return };
    let sink =
        locallog::Sink::new(locallog::Rotating::new(path).own_folder(), format!("essentials/{}", env!("CARGO_PKG_VERSION")), home(app));
    locallog::init(sink);
    tracing::info!(event = "app.start", beta = app.state::<crate::AppState>().settings.lock().unwrap().beta_updates);
}

/// An error the user is shown, in the log too.
pub fn shown(title: &str, message: &str) {
    tracing::warn!(event = "ui.error", title = %title, error = %message);
}

#[derive(Debug, PartialEq)]
pub enum Docker {
    Available(String),
    /// Docker is there, but its engine doesn't answer (Docker Desktop not started).
    NotRunning(String),
    NotFound,
}

impl Docker {
    pub fn describe(&self) -> String {
        match self {
            Docker::Available(v) => format!("available, version {v}"),
            Docker::NotRunning(why) if why.is_empty() => "installed, but not running".into(),
            Docker::NotRunning(why) => format!("installed, but not running ({why})"),
            Docker::NotFound => "not found".into(),
        }
    }
}

/// What `docker version` says, with the environment hbench gets.
pub fn docker_status(env: &HashMap<String, String>) -> Docker {
    let mut cmd = Command::new("docker");
    cmd.args(["version", "--format", "{{.Server.Version}}"]).envs(env).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000); // CREATE_NO_WINDOW
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Docker::NotFound,
        Err(e) => return Docker::NotRunning(e.to_string()),
    };
    let deadline = Instant::now() + DOCKER_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                // Only the process started here.
                let _ = child.kill();
                let _ = child.wait();
                return Docker::NotRunning(format!("no answer within {}s", DOCKER_TIMEOUT.as_secs()));
            }
        }
    };
    let read = |r: Option<&mut dyn Read>| {
        let mut s = String::new();
        if let Some(r) = r {
            let _ = r.read_to_string(&mut s);
        }
        s.trim().to_string()
    };
    let out = read(child.stdout.as_mut().map(|r| r as &mut dyn Read));
    let err = read(child.stderr.as_mut().map(|r| r as &mut dyn Read));
    if status.success() && !out.is_empty() {
        Docker::Available(out)
    } else {
        Docker::NotRunning(err.lines().next().unwrap_or_default().chars().take(200).collect())
    }
}

/// Check Docker and log what was found.
pub fn check_docker(app: &AppHandle) -> Docker {
    let d = docker_status(&app.state::<crate::AppState>().env);
    match &d {
        Docker::Available(v) => tracing::info!(event = "docker.check", status = "available", version = %v),
        Docker::NotRunning(why) => tracing::warn!(event = "docker.check", status = "not-running", error = %why),
        Docker::NotFound => tracing::warn!(event = "docker.check", status = "not-found"),
    }
    d
}

/// The diagnostics' first lines.
fn header(app: &AppHandle, docker: &Docker, at: SystemTime) -> String {
    let beta = app.state::<crate::AppState>().settings.lock().unwrap().beta_updates;
    format!(
        "{} diagnostics\nCreated: {}\nApp version: {}{}\nSystem: {} {} ({})\nDocker: {}\nUpdates: {}\n",
        crate::APP_NAME,
        locallog::rfc3339(at),
        env!("CARGO_PKG_VERSION"),
        if crate::DEV { " (development build)" } else { "" },
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
        docker.describe(),
        if beta { "beta" } else { "stable" },
    )
}

fn logs(app: &AppHandle) -> Vec<(String, PathBuf)> {
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    [app_log(app), view_log(app), cli_log(app)].into_iter().flatten().map(|p| (name(&p), p)).collect()
}

/// The text "Copy Diagnostics" puts on the clipboard: the header and the last lines of each log, redacted again.
pub fn text(header: &str, logs: &[(String, PathBuf)], home: Option<&str>) -> String {
    let mut s = header.to_string();
    for (name, path) in logs {
        let lines = locallog::tail_redacted(path, TAIL_LINES, home);
        s.push_str(&format!("\n--- {name}: last {} lines ---\n", lines.len()));
        if lines.is_empty() {
            s.push_str("(empty)\n");
        }
        for l in lines {
            s.push_str(&l);
            s.push('\n');
        }
    }
    // The header and lines are already redacted; once more over the whole, in case a line was cut mid-secret.
    s.lines().map(|l| locallog::redact(l, home)).collect::<Vec<_>>().join("\n") + "\n"
}

/// Write the zip: `diagnostics.txt`, and every file of each log (rotated ones too), each line redacted again.
pub fn write_zip(dest: &Path, header: &str, logs: &[(String, PathBuf)], home: Option<&str>) -> Result<(), String> {
    let file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("diagnostics.txt", opts).map_err(|e| e.to_string())?;
    zip.write_all(text(header, logs, home).as_bytes()).map_err(|e| e.to_string())?;
    for (_, path) in logs {
        for f in locallog::files(path, locallog::FILES) {
            let Ok(bytes) = std::fs::read(&f) else { continue };
            let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            zip.start_file(format!("logs/{name}"), opts).map_err(|e| e.to_string())?;
            for line in String::from_utf8_lossy(&bytes).lines() {
                zip.write_all(locallog::redact(line, home).as_bytes()).map_err(|e| e.to_string())?;
                zip.write_all(b"\n").map_err(|e| e.to_string())?;
            }
        }
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}

fn tell(app: &AppHandle, title: &str, text: &str, kind: MessageDialogKind) {
    if kind == MessageDialogKind::Error {
        shown(title, text);
    }
    app.dialog().message(text).title(title).kind(kind).blocking_show();
}

/// Help → "Copy Diagnostics". Blocks; call off the main thread.
pub fn copy(app: &AppHandle) {
    let docker = check_docker(app);
    let text = text(&header(app, &docker, SystemTime::now()), &logs(app), home(app).as_deref());
    match app.clipboard().write_text(text) {
        Ok(()) => {
            tracing::info!(event = "diagnostics.copy");
            tell(
                app,
                "Diagnostics copied",
                "Paste them in your message to us. They hold the app's version, your system, whether Docker is running, \
                 and the app's recent log lines, with keys, emails and your user name taken out. Nothing was sent.",
                MessageDialogKind::Info,
            );
        }
        Err(e) => tell(
            app,
            "Couldn't copy the diagnostics",
            &format!("{e}\n\nUse Help → Save Diagnostics… to save them to a file instead."),
            MessageDialogKind::Error,
        ),
    }
}

/// Help → "Save Diagnostics…". Blocks; call off the main thread.
pub fn save(app: &AppHandle) {
    let at = SystemTime::now();
    let day = locallog::rfc3339(at).get(..10).unwrap_or_default().to_string();
    let mut dialog = app
        .dialog()
        .file()
        .set_title("Save diagnostics")
        .set_file_name(format!("actuaria-diagnostics-{day}.zip"))
        .add_filter("Zip", &["zip"]);
    if let Ok(d) = app.path().desktop_dir().or_else(|_| app.path().download_dir()) {
        dialog = dialog.set_directory(d);
    }
    let Some(dest) = dialog.blocking_save_file().and_then(|p| p.into_path().ok()) else { return };
    let docker = check_docker(app);
    match write_zip(&dest, &header(app, &docker, at), &logs(app), home(app).as_deref()) {
        Ok(()) => {
            tracing::info!(event = "diagnostics.save");
            let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            tell(
                app,
                "Diagnostics saved",
                &format!(
                    "Saved as {name}. Attach the file to your message to us. Keys, emails and your user name are taken out. Nothing was sent."
                ),
                MessageDialogKind::Info,
            );
        }
        Err(e) => tell(app, "Couldn't save the diagnostics", &e, MessageDialogKind::Error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("actuaria-diag-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Lines as an older build (or a process that skipped redaction) may have left them.
    const DIRTY: &str = "\
2026-11-23T08:00:00.000Z essentials/0.5.0 macos-aarch64 update.check error=\"401 Bearer abcdefghijkl\"
listening at /Users/ann/Actuaria with ANTHROPIC_API_KEY=sk-ant-api03-AbCdEf0123456789_xyzXYZ
signed in as ann@example.com, cookie: sid=99887766
token ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 secret 0123456789abcdef0123456789abcdef
";

    const LEAKS: &[&str] =
        &["abcdefghijkl", "/Users/ann", "sk-ant", "ann@example.com", "99887766", "ghp_", "0123456789abcdef0123456789abcdef"];

    #[test]
    fn copied_diagnostics_hold_no_secret_email_or_home() {
        let d = temp("copy");
        std::fs::write(d.join("essentials.log"), DIRTY).unwrap();
        let logs = vec![("essentials.log".to_string(), d.join("essentials.log")), ("hbench-view.log".to_string(), d.join("none.log"))];
        let out = text("Header\nDocker: not found\n", &logs, Some("/Users/ann"));
        for leak in LEAKS {
            assert!(!out.contains(leak), "{leak} reached the diagnostics:\n{out}");
        }
        assert!(out.contains("--- essentials.log: last 4 lines ---"), "{out}");
        assert!(out.contains("--- hbench-view.log: last 0 lines ---\n(empty)"), "{out}");
        assert!(out.contains("listening at ~/Actuaria"), "{out}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The review's blocking cases (#131), as lines already in a log: named secrets in JSON or quotes.
    #[test]
    fn named_secrets_in_json_or_quotes_never_reach_the_diagnostics() {
        let d = temp("json");
        let lines = [
            r#"2026-11-23T08:00:00.000Z essentials/0.5.0 linux-x86_64 ui.error error="bad config {\"api_key\": \"hunter2xx\", \"password\": \"pw2\"}""#,
            r#"HTTP 401 body={"x_api_key":"short"}"#,
            r#"auth failed: token: "hunter2xx" rejected"#,
        ];
        std::fs::write(d.join("a.log"), lines.join("\n")).unwrap();
        let out = text("", &[("a.log".into(), d.join("a.log"))], None);
        for leak in ["hunter2xx", "pw2", "short"] {
            assert!(!out.contains(leak), "{leak} reached the diagnostics:\n{out}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_folder_path_becomes_its_name() {
        let f = Path::new("/home/ann/clients/acme");
        assert_eq!(hide_folder("Couldn't open /home/ann/clients/acme\n\nno access", f), "Couldn't open …/acme\n\nno access");
        // A trailing separator, and the path inside an error chain.
        assert_eq!(hide_folder("x", Path::new("/home/ann/clients/acme/")), "x");
        assert_eq!(
            hide_folder("Error: reading /home/ann/clients/acme/job-1/result.json: caused by: denied", Path::new("/home/ann/clients/acme/")),
            "Error: reading …/acme/job-1/result.json: caused by: denied"
        );
        // JSON-doubled Windows separators.
        assert_eq!(hide_folder(r#"{"p":"C:\\Users\\Ann\\acme\\x"}"#, Path::new(r"C:\Users\Ann\acme")), r#"{"p":"…/acme\\x"}"#);
        // Through a symlink: the resolved path goes too.
        let d = temp("link");
        let real = d.join("real-client");
        std::fs::create_dir_all(&real).unwrap();
        let link = d.join("acme");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let resolved = real.canonicalize().unwrap();
            let out = hide_folder(&format!("opened {}/x", resolved.display()), &link);
            assert_eq!(out, "opened …/acme/x");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_the_last_lines_are_copied() {
        let d = temp("tail");
        let lines: String = (0..500).map(|i| format!("line {i}\n")).collect();
        std::fs::write(d.join("a.log"), lines).unwrap();
        let out = text("", &[("a.log".into(), d.join("a.log"))], None);
        assert!(out.contains(&format!("last {TAIL_LINES} lines")));
        assert!(!out.contains("line 299\n") && out.contains("line 300\n") && out.ends_with("line 499\n"), "{out}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_saved_zip_holds_no_secret_email_or_home() {
        let d = temp("zip");
        std::fs::write(d.join("hbench-view.log"), DIRTY).unwrap();
        std::fs::write(d.join("hbench-view.log.1"), DIRTY).unwrap();
        let dest = d.join("out.zip");
        let logs = vec![("hbench-view.log".to_string(), d.join("hbench-view.log"))];
        write_zip(&dest, "Header\n", &logs, Some("/Users/ann")).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&dest).unwrap()).unwrap();
        let mut names = vec![];
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).unwrap();
            let name = f.name().unwrap().to_string();
            names.push(name.clone());
            let mut s = String::new();
            f.read_to_string(&mut s).unwrap();
            for leak in LEAKS {
                assert!(!s.contains(leak), "{leak} reached {name} in the zip:\n{s}");
            }
        }
        assert_eq!(names, ["diagnostics.txt", "logs/hbench-view.log.1", "logs/hbench-view.log"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn docker_is_described_plainly() {
        assert_eq!(Docker::Available("27.3.1".into()).describe(), "available, version 27.3.1");
        assert_eq!(Docker::NotRunning(String::new()).describe(), "installed, but not running");
        assert_eq!(Docker::NotFound.describe(), "not found");
        let none = HashMap::from([("PATH".to_string(), "/nonexistent".to_string())]);
        assert_eq!(docker_status(&none), Docker::NotFound);
    }
}
