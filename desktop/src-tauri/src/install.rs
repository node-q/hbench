//! The one-click install link, `actuaria://install/actuaria/<bot>[?target=claude|orca]` (docs/REFERENCE.md,
//! "Install links"). The checks live in the `actuaria-install` crate (desktop/install); this wires them to the
//! app: the link arrives (single-instance handoff, or Apple event on macOS), the page shows the confirm screen
//! from `window.__ACTUARIA_INSTALL__` (an `actuaria:install` event), and the user's choice comes back as
//! `/__actuaria/install?do=..`. The page can't start an install: it can only confirm the one a link opened.

use crate::confirm;
use actuaria_install::catalogue::{self, Offer};
use actuaria_install::{Target, download, hosts, link, net, rate, rate::Gate, unzip};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

/// How long a handed-over `.mcpb` stays on disk for Claude Desktop to read it.
const CLAUDE_GRACE: Duration = Duration::from_secs(10 * 60);

const EXPIRED: &str = "This was open for over 5 minutes, so the catalogue may have changed. Open the link again. Nothing was installed.";

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct View {
    /// none | loading | confirm | working | approval | done | error
    state: &'static str,
    nonce: String,
    bot: Option<BotView>,
    /// The hosts the user can pick (present and offered).
    targets: Vec<Target>,
    message: Option<String>,
    output: Option<String>,
    /// A host to download ("claude" links claude.ai/download).
    get_host: Option<Target>,
}

#[derive(Serialize, Clone)]
struct BotView {
    id: String,
    name: String,
    version: String,
    description: String,
    source: &'static str,
}

#[derive(Default)]
struct Inner {
    gate: Gate,
    view: View,
    /// The offer, and when the catalogue was read for it.
    offer: Option<(Offer, Instant)>,
    seq: u64,
}

#[derive(Default)]
pub struct Installs(Mutex<Inner>);

fn set_view(app: &AppHandle, f: impl FnOnce(&mut View)) {
    {
        let st = app.state::<Installs>();
        let mut i = st.0.lock().unwrap();
        f(&mut i.view);
    }
    push(app);
}

/// Send the current state to the page (also on each page load).
pub fn push(app: &AppHandle) {
    let json = {
        let st = app.state::<Installs>();
        let i = st.0.lock().unwrap();
        serde_json::to_string(&i.view).unwrap_or_else(|_| "null".into())
    };
    if let Some(w) = app.get_webview_window(crate::MAIN) {
        let _ = w.eval(format!("window.__ACTUARIA_INSTALL__ = {json}; window.dispatchEvent(new Event(\"actuaria:install\"));"));
    }
}

/// Show an error for dialog `seq`, unless that dialog was closed or replaced meanwhile.
fn error(app: &AppHandle, seq: u64, message: String, get_host: Option<Target>) {
    {
        let st = app.state::<Installs>();
        let mut i = st.0.lock().unwrap();
        if i.seq != seq || !i.gate.is_open() {
            return;
        }
        i.view.state = "error";
        i.view.message = Some(message);
        i.view.get_host = get_host;
    }
    push(app);
}

fn nonce(seq: u64) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(seq);
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    format!("{:016x}{seq}", h.finish())
}

/// A link from the OS. Dropped while a dialog is open or over the per-minute limit; otherwise it opens the
/// dialog, invalid links included ("This link is not valid").
pub fn on_link(app: &AppHandle, raw: &str) {
    let seq = {
        let st = app.state::<Installs>();
        let mut i = st.0.lock().unwrap();
        if !i.gate.try_open(Instant::now()) {
            return;
        }
        i.seq += 1;
        i.offer = None;
        i.view = View { state: "loading", nonce: nonce(i.seq), ..View::default() };
        i.seq
    };
    if let Some(w) = app.get_webview_window(crate::MAIN) {
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
    let parsed = match link::parse(raw) {
        Ok(l) => l,
        Err(e) => return error(app, seq, format!("This link is not valid: {e}."), None),
    };
    push(app);
    let app = app.clone();
    std::thread::spawn(move || prepare(&app, seq, parsed));
}

/// Look the bot up, check it against bots.lock, and show the confirm screen with the hosts present.
fn prepare(app: &AppHandle, seq: u64, l: link::Link) {
    let read_at = Instant::now();
    let index = match net::get(catalogue::INDEX_URL, catalogue::MAX_INDEX_BYTES as u64) {
        Ok(b) => b,
        Err(e) => return error(app, seq, format!("Couldn't reach the catalogue: {e}"), None),
    };
    let lock = net::get(catalogue::LOCK_URL, catalogue::MAX_LOCK_BYTES as u64)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .map(|t| catalogue::Lock::parse(&t));
    let lock = match lock {
        Some(Ok(l)) => Some(l),
        Some(Err(e)) => return error(app, seq, format!("Can't install: {e}."), None),
        None => None,
    };
    let offer = match catalogue::lookup(&index, lock.as_ref(), &l.bot) {
        Ok(o) => o,
        Err(e) => return error(app, seq, format!("Can't install actuaria/{}: {e}.", l.bot), None),
    };
    let home = app.path().home_dir().ok();
    let offered: Vec<Target> = offer.artefacts.keys().copied().filter(|t| l.target.is_none_or(|w| w == *t)).collect();
    if offered.is_empty() {
        let t = l.target.map(|t| t.host_name()).unwrap_or("this host");
        return error(app, seq, format!("{} isn't available for {t}.", offer.name), None);
    }
    let targets = hosts::present(offered.clone(), home.as_deref());
    if targets.is_empty() {
        let want = if offered.len() == 1 { offered[0].host_name() } else { "Claude Desktop or Orca" };
        let get = offered.contains(&Target::Claude).then_some(Target::Claude);
        return error(app, seq, format!("{} needs {want}, which isn't installed on this computer.", offer.name), get);
    }
    let st = app.state::<Installs>();
    let mut i = st.0.lock().unwrap();
    if i.seq != seq || !i.gate.is_open() {
        return;
    }
    i.view.state = "confirm";
    i.view.bot = Some(BotView {
        id: offer.id.clone(),
        name: offer.name.clone(),
        version: offer.version.clone(),
        description: offer.description.clone(),
        source: catalogue::DOWNLOAD_HOST,
    });
    i.view.targets = targets;
    i.offer = Some((offer, read_at));
    drop(i);
    push(app);
}

/// `/__actuaria/install?do=..` from the page.
pub fn action(app: &AppHandle, what: &str, nonce: Option<String>, target: Option<String>) {
    let st = app.state::<Installs>();
    let mut i = st.0.lock().unwrap();
    match what {
        "cancel" | "close" => {
            if i.view.state == "working" {
                return;
            }
            i.gate.close();
            i.offer = None;
            i.view = View { state: "none", ..View::default() };
        }
        "get-host" => {
            if i.view.get_host == Some(Target::Claude) {
                let _ = app.opener().open_url(hosts::CLAUDE_DOWNLOAD_URL, None::<&str>);
            }
            return;
        }
        "confirm" => {
            let t = target.as_deref().and_then(Target::from_name);
            let ok = i.view.state == "confirm" && nonce.as_deref() == Some(i.view.nonce.as_str());
            let (Some(t), true) = (t, ok) else { return };
            if !i.view.targets.contains(&t) {
                return;
            }
            let Some((offer, read_at)) = i.offer.take() else { return };
            if rate::offer_expired(read_at, Instant::now()) {
                i.view.state = "error";
                i.view.message = Some(EXPIRED.into());
                drop(i);
                return push(app);
            }
            i.view.state = "working";
            i.view.message = Some(format!("Confirm in the dialog to install {} {}…", offer.name, offer.version));
            drop(i);
            push(app);
            let app = app.clone();
            std::thread::spawn(move || {
                // A native dialog the page's script can't answer: only its "Install" goes on.
                let text = format!(
                    "Install {} {} (actuaria/{}) into {}?\n\nDownloaded from {} and checked against the catalogue.",
                    offer.name,
                    offer.version,
                    offer.id,
                    t.host_name(),
                    catalogue::DOWNLOAD_HOST
                );
                let yes = confirm::ask(&app, "Install a bot".into(), text);
                if !yes {
                    let st = app.state::<Installs>();
                    let mut i = st.0.lock().unwrap();
                    i.gate.close();
                    i.view = View { state: "none", ..View::default() };
                    drop(i);
                    push(&app);
                    return;
                }
                // The native dialog may have been left open.
                if rate::offer_expired(read_at, Instant::now()) {
                    return set_view(&app, |v| {
                        v.state = "error";
                        v.message = Some(EXPIRED.into());
                    });
                }
                set_view(&app, |v| v.message = Some(format!("Downloading and checking {} {}…", offer.name, offer.version)));
                let r = run(&app, &offer, t);
                set_view(&app, |v| match r {
                    Ok((state, message, output)) => {
                        v.state = state;
                        v.message = Some(message);
                        v.output = output;
                    }
                    Err((message, output)) => {
                        let no_handler = output.as_deref() == Some(NO_HANDLER);
                        v.state = "error";
                        v.message = Some(message);
                        v.output = output.filter(|_| !no_handler);
                        v.get_host = no_handler.then_some(Target::Claude);
                    }
                });
            });
            return;
        }
        _ => return,
    }
    drop(i);
    push(app);
}

/// The Claude hand-off's marker for "Claude Desktop couldn't be opened": the screen links claude.ai/download.
const NO_HANDLER: &str = "no-handler";

/// The page's state, message and output.
type Outcome = Result<(&'static str, String, Option<String>), (String, Option<String>)>;

/// Download, verify and hand over to the host's installer.
fn run(app: &AppHandle, offer: &Offer, t: Target) -> Outcome {
    let fail = |m: String| Err((m, None));
    let Some(a) = offer.artefacts.get(&t) else { return fail("Nothing to install for that host.".into()) };
    let Ok(data_dir) = app.path().app_data_dir() else { return fail("No app data folder.".into()) };
    let tmp = download::temp_dir(&data_dir).map_err(|e| (format!("Couldn't make a private folder: {e}"), None))?;
    let bytes = net::get(&a.url, download::cap(a)).map_err(|e| (format!("The download failed: {e}"), None))?;
    let file = match t {
        Target::Claude => format!("{}-{}.mcpb", offer.id, offer.version),
        Target::Orca => format!("{}-{}.zip", offer.id, offer.version),
    };
    let path =
        download::verify_and_write(&bytes, a, tmp.path(), &file).map_err(|e| (format!("Refused: {e}. Nothing was installed."), None))?;
    match t {
        Target::Claude => {
            // Open it with Claude explicitly; nothing else gets to claim the `.mcpb`.
            let home = app.path().home_dir().ok();
            let opened = match hosts::find_claude(home.as_deref()) {
                Some(c) => open_with_claude(app, &c, &path),
                None => Err("it wasn't found on this computer".into()),
            };
            if let Err(e) = opened {
                return Err((
                    format!("Couldn't open the bot in Claude Desktop: {e}. Install or update Claude Desktop, then try again."),
                    Some(NO_HANDLER.into()),
                ));
            }
            // Claude Desktop reads the file once it opens: remove it later.
            let dir: PathBuf = tmp.keep();
            std::thread::spawn(move || {
                std::thread::sleep(CLAUDE_GRACE);
                let _ = std::fs::remove_dir_all(dir);
            });
            Ok((
                "done",
                format!("Started Claude Desktop with {}. Claude shows its own install dialog: review and approve it there.", offer.name),
                None,
            ))
        }
        Target::Orca => {
            let unpacked = tmp.path().join("plugin");
            std::fs::create_dir(&unpacked).map_err(|e| (e.to_string(), None))?;
            unzip::extract(&bytes, &unpacked).map_err(|e| (format!("Refused: {e}. Nothing was installed."), None))?;
            let no_root = || ("Refused: the archive has no orca-plugin.json.".to_string(), None);
            unzip::plugin_root(&unpacked).ok_or_else(no_root)?;
            let home = app.path().home_dir().ok();
            let orca =
                hosts::find_orca(std::env::var_os("PATH"), home.as_deref()).ok_or_else(|| ("Orca isn't installed.".to_string(), None))?;
            // Kept after this returns: the command Orca gives the user to approve the plugin reads it again.
            let kept = download::keep_plugin(&data_dir, &offer.id, &unpacked)
                .map_err(|e| (format!("Couldn't keep the plugin's folder: {e}"), None))?;
            let root = unzip::plugin_root(&kept).ok_or_else(no_root)?;
            let (r, out) = hosts::run_orca_install(&orca, &root).map_err(|e| (e, None))?;
            match r {
                hosts::OrcaResult::Installed => Ok(("done", format!("Orca installed {}.", offer.name), Some(out))),
                hosts::OrcaResult::NeedsApproval => Ok((
                    "approval",
                    "Orca hasn't installed it yet: it needs your approval. Review the access it asks for below, and if you agree, run the command Orca shows.".into(),
                    Some(out),
                )),
                hosts::OrcaResult::Failed => Err(("Orca couldn't install the plugin.".into(), Some(out))),
            }
        }
    }
}

/// Open a `.mcpb` with Claude Desktop itself.
#[cfg(target_os = "macos")]
fn open_with_claude(app: &AppHandle, claude: &Path, mcpb: &Path) -> Result<(), String> {
    // `open -a <Claude.app> -- <file>`, whose exit status is checked.
    app.opener().open_path(mcpb.to_string_lossy(), Some(claude.to_string_lossy())).map_err(|e| e.to_string())
}

#[cfg(not(target_os = "macos"))]
fn open_with_claude(_app: &AppHandle, claude: &Path, mcpb: &Path) -> Result<(), String> {
    hosts::claude_command(claude, mcpb).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Register the `actuaria:` handler as `<app> -- <link>` on Windows and Linux (macOS uses the bundle's
/// Info.plist), and remove downloads an earlier run left.
pub fn setup(app: &AppHandle) {
    if let Ok(d) = app.path().app_data_dir() {
        download::sweep(&d);
    }
    #[cfg(any(windows, target_os = "linux"))]
    if !crate::DEV {
        register();
    }
}

#[cfg(windows)]
fn register() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(exe) = std::env::current_exe() else { return };
    let Ok(cmds) = actuaria_install::register::windows_reg_args(&exe.to_string_lossy()) else { return };
    for args in cmds {
        let _ = std::process::Command::new("reg").args(args).creation_flags(CREATE_NO_WINDOW).output();
    }
}

#[cfg(target_os = "linux")]
fn register() {
    // An AppImage runs from a temporary mount: register the AppImage itself.
    let exe = std::env::var_os("APPIMAGE").map(PathBuf::from).or_else(|| std::env::current_exe().ok());
    let Some(exe) = exe else { return };
    let Ok(entry) = actuaria_install::register::linux_desktop_entry(&exe.to_string_lossy()) else { return };
    let dir = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|d| d.join("applications"));
    let Some(dir) = dir else { return };
    let _ = std::fs::create_dir_all(&dir);
    if std::fs::write(dir.join(actuaria_install::register::LINUX_DESKTOP_FILE), entry).is_ok() {
        let _ = std::process::Command::new("xdg-mime")
            .args(["default", actuaria_install::register::LINUX_DESKTOP_FILE, "x-scheme-handler/actuaria"])
            .output();
    }
}

/// The link in a launch's arguments (after `--`), if any.
pub fn from_args(app: &AppHandle, args: &[String]) {
    if let Some(raw) = link::from_args(args) {
        on_link(app, raw);
    }
}
