//! Self-update. The app checks the latest release's `latest.json` (see `tauri.conf.json`) a little after launch
//! and then every few hours, and the File/App menu checks on demand. An update is offered in the page: a prompt
//! the user can dismiss, after which the status bar keeps an "Update" action. Installing downloads and verifies
//! the signed package, swaps it in, and asks to restart.
//!
//! The app owns the state and pushes it to the page as `window.__ACTUARIA_UPDATE__` plus an `actuaria:update`
//! event, again after every page load (opening a folder loads another viewer). The page answers by navigating to
//! `/__actuaria/update?do=<install|dismiss|show|restart|notes>`.
//!
//! Debug builds never check. `ACTUARIA_UPDATE_DEMO=<version>` (debug builds only) fakes an update to that version,
//! with a simulated download, to try the flow without a release.

use crate::{APP_NAME, AppState, DEV, main_window, save_settings};
use serde::Serialize;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::{Update, UpdaterExt};

const FIRST_CHECK: Duration = Duration::from_secs(8);
const EVERY: Duration = Duration::from_secs(4 * 60 * 60);
const RELEASES: &str = "https://github.com/node-q/hbench/releases";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// What the page shows.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    /// `none`, `available`, `downloading`, `ready` (installed, restart to finish) or `error`.
    state: &'static str,
    current: &'static str,
    version: Option<String>,
    /// Download progress, 0 to 1; `None` while the size is unknown.
    progress: Option<f64>,
    error: Option<String>,
    /// Whether the prompt is up. Once dismissed, the status bar alone offers the update.
    prompt: bool,
    /// Installing quits and relaunches the app at once (Windows), rather than waiting for a restart.
    install_restarts: bool,
}

impl Default for View {
    fn default() -> Self {
        View { state: "none", current: CURRENT, version: None, progress: None, error: None, prompt: false, install_restarts: cfg!(windows) }
    }
}

#[derive(Default)]
pub struct Updates {
    view: Mutex<View>,
    update: Mutex<Option<Update>>,
    /// A check or a download is under way.
    busy: Mutex<bool>,
}

fn demo() -> Option<String> {
    if DEV { std::env::var("ACTUARIA_UPDATE_DEMO").ok().filter(|v| !v.is_empty()) } else { None }
}

/// Send the state to the page.
pub fn push(app: &AppHandle) {
    let view = app.state::<Updates>().view.lock().unwrap().clone();
    if let Some(w) = main_window(app) {
        let json = serde_json::to_string(&view).unwrap_or_default();
        let _ = w.eval(format!("window.__ACTUARIA_UPDATE__ = {json}; window.dispatchEvent(new Event(\"actuaria:update\"));"));
    }
}

fn set(app: &AppHandle, f: impl FnOnce(&mut View)) {
    f(&mut app.state::<Updates>().view.lock().unwrap());
    push(app);
}

/// Claim the one check-or-download slot; `false` if something holds it.
fn claim(app: &AppHandle) -> bool {
    let updates = app.state::<Updates>();
    let mut busy = updates.busy.lock().unwrap();
    !std::mem::replace(&mut *busy, true)
}

fn release(app: &AppHandle) {
    *app.state::<Updates>().busy.lock().unwrap() = false;
}

/// Check after launch, then every few hours.
pub fn start(app: &AppHandle) {
    if DEV && demo().is_none() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK).await;
        loop {
            check(&app, false).await;
            tokio::time::sleep(EVERY).await;
        }
    });
}

/// The menu's "Check for Updates…": like the periodic check, but it always answers.
pub fn check_now(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move { check(&app, true).await });
}

async fn check(app: &AppHandle, manual: bool) {
    let state = app.state::<Updates>().view.lock().unwrap().state;
    // Already found (or being installed): a manual check brings the prompt back.
    if state != "none" && state != "error" {
        if manual {
            set(app, |v| v.prompt = true);
        }
        return;
    }
    if !claim(app) {
        return;
    }
    let found = match demo() {
        Some(v) => Ok(Some(v)),
        None if DEV => Ok(None),
        None => {
            // Windows: the installer takes over and this process exits inside `install`, so stop hbench view
            // first (on Windows nothing else stops it when the app goes away).
            let stopper = app.clone();
            let updater =
                app.updater_builder().on_before_exit(move || drop(stopper.state::<AppState>().server.lock().unwrap().take())).build();
            match updater {
                Ok(u) => match u.check().await {
                    Ok(u) => {
                        let v = u.as_ref().map(|u| u.version.clone());
                        *app.state::<Updates>().update.lock().unwrap() = u;
                        Ok(v)
                    }
                    // The endpoint answered, but not with a manifest: the latest release has no `latest.json`
                    // (one published before the updater, or without the signing key). Nothing to offer.
                    Err(tauri_plugin_updater::Error::ReleaseNotFound) => {
                        eprintln!("update check: the latest release has no latest.json");
                        Ok(None)
                    }
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            }
        }
    };
    release(app);
    match found {
        Ok(Some(version)) => {
            let skipped = app.state::<AppState>().settings.lock().unwrap().skip_update.as_deref() == Some(version.as_str());
            set(app, |v| {
                *v = View { state: "available", version: Some(version), prompt: manual || !skipped, ..View::default() };
            });
        }
        Ok(None) => {
            set(app, |v| *v = View::default());
            if manual {
                message(app, &format!("{APP_NAME} is up to date"), &format!("Version {CURRENT} is the latest."), MessageDialogKind::Info);
            }
        }
        Err(e) => {
            eprintln!("update check failed: {e}");
            if manual {
                message(app, "Couldn't check for updates", &e.to_string(), MessageDialogKind::Error);
            }
        }
    }
}

fn message(app: &AppHandle, title: &str, text: &str, kind: MessageDialogKind) {
    app.dialog().message(text).title(title).kind(kind).show(|_| {});
}

/// The page's `/__actuaria/update?do=..`.
pub fn action(app: &AppHandle, what: &str) {
    match what {
        "install" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move { install(&app).await });
        }
        "dismiss" => {
            let view = app.state::<Updates>().view.lock().unwrap().clone();
            // Not asked again about this version at launch; the status bar still offers it.
            if view.state == "available" {
                let state = app.state::<AppState>();
                let mut settings = state.settings.lock().unwrap();
                settings.skip_update = view.version.clone();
                save_settings(app, &settings);
            }
            set(app, |v| {
                v.prompt = false;
                if v.state == "error" {
                    // Back to offering the update, if there is one to offer.
                    v.state = if v.version.is_some() { "available" } else { "none" };
                    v.error = None;
                }
            });
        }
        "show" => set(app, |v| v.prompt = v.state != "none"),
        "notes" => {
            let version = app.state::<Updates>().view.lock().unwrap().version.clone();
            let url = version.map_or_else(|| format!("{RELEASES}/latest"), |v| format!("{RELEASES}/tag/v{v}"));
            let _ = app.opener().open_url(url, None::<&str>);
        }
        "restart" if app.state::<Updates>().view.lock().unwrap().state == "ready" => {
            if demo().is_some() {
                set(app, |v| *v = View::default());
            } else {
                // The exit handler stops hbench view.
                app.restart();
            }
        }
        _ => {}
    }
}

async fn install(app: &AppHandle) {
    let state = app.state::<Updates>().view.lock().unwrap().state;
    if state != "available" && state != "error" {
        return;
    }
    if !claim(app) {
        return;
    }
    set(app, |v| {
        v.state = "downloading";
        v.progress = Some(0.0);
        v.error = None;
    });
    let result = if demo().is_some() { fake_download(app).await } else { download_and_install(app).await };
    release(app);
    match result {
        Ok(()) => set(app, |v| {
            v.state = "ready";
            v.progress = None;
            // Ask to restart even if the prompt was dismissed while downloading: the update is half done.
            v.prompt = true;
        }),
        Err(e) => {
            eprintln!("update failed: {e}");
            set(app, |v| {
                v.state = "error";
                v.progress = None;
                v.error = Some(e);
                v.prompt = true;
            });
        }
    }
}

async fn download_and_install(app: &AppHandle) -> Result<(), String> {
    let update = app.state::<Updates>().update.lock().unwrap().clone().ok_or("No update to install.")?;
    let mut got = 0u64;
    let mut shown = -1i64;
    let progress = app.clone();
    let bytes = update
        .download(
            |chunk, total| {
                got += chunk as u64;
                let p = total.filter(|t| *t > 0).map(|t| (got as f64 / t as f64).min(1.0));
                // One push per percent is plenty.
                let pct = p.map_or(-1, |p| (p * 100.0) as i64);
                if pct != shown {
                    shown = pct;
                    set(&progress, |v| v.progress = p);
                }
            },
            || {},
        )
        .await
        .map_err(|e| format!("Download failed: {e}"))?;
    // Swapping the bundle is blocking file work, and may wait on an administrator prompt.
    tauri::async_runtime::spawn_blocking(move || update.install(bytes))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("Couldn't install the update: {e}"))
}

async fn fake_download(app: &AppHandle) -> Result<(), String> {
    for i in 1..=40 {
        tokio::time::sleep(Duration::from_millis(75)).await;
        set(app, |v| v.progress = Some(i as f64 / 40.0));
    }
    if std::env::var("ACTUARIA_UPDATE_DEMO_FAIL").is_ok() {
        return Err("Couldn't install the update: demo failure".into());
    }
    Ok(())
}
