//! Self-update. The app checks the latest release's `latest.json` (and, on the beta channel, the beta feed's) a little
//! after launch and then every few hours, and the File/App menu checks on demand. An update is offered in the page: a prompt
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
use semver::Version;
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
/// Per feed: a feed that hangs must not hold back the other one. Downloads have no time limit.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// The beta channel's feed: the fixed release `beta-channel`, whose latest.json release.yml replaces after a beta or
/// stable release newer than the version there. It can be behind stable (when that job failed), so the app reads
/// both. Its updates are signed with the same key as stable's.
const BETA_FEED: &str = "https://github.com/node-q/hbench/releases/download/beta-channel/latest.json";
const STABLE_FEED: &str = "https://github.com/node-q/hbench/releases/latest/download/latest.json";
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
    /// A check or a download is under way. Locked while one starts, while a check's result is kept or dropped, and
    /// while the channel changes, so that those never interleave. Lock it before the others.
    busy: Mutex<bool>,
}

impl Updates {
    /// Back to nothing on offer.
    fn clear(&self) {
        *self.update.lock().unwrap() = None;
        *self.view.lock().unwrap() = View::default();
    }
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

/// Start a check or a download: run `begin` with the one slot claimed, if nothing holds it and the state is `none`
/// or `error`, or also `available` for a download (`offered`). `false` if not.
fn claim(app: &AppHandle, offered: bool, begin: impl FnOnce(&mut View)) -> bool {
    let updates = app.state::<Updates>();
    let mut busy = updates.busy.lock().unwrap();
    let mut view = updates.view.lock().unwrap();
    let ok = match view.state {
        "available" => offered,
        "error" => true,
        "none" => !offered,
        _ => false,
    };
    if *busy || !ok {
        return false;
    }
    *busy = true;
    begin(&mut view);
    true
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

/// Whether the app is on the beta channel.
fn on_beta(app: &AppHandle) -> bool {
    app.state::<AppState>().settings.lock().unwrap().beta_updates
}

async fn check(app: &AppHandle, mut manual: bool) {
    if !claim(app, false, |_| {}) {
        // Already found (or being installed): a manual check brings the prompt back.
        if manual {
            set(app, |v| v.prompt |= !matches!(v.state, "none" | "error"));
        }
        return;
    }
    let found = loop {
        let beta = on_beta(app);
        let found = find(app, beta).await;
        let updates = app.state::<Updates>();
        let mut busy = updates.busy.lock().unwrap();
        if on_beta(app) != beta {
            // The channel changed meanwhile: what the old one offers is not wanted. Look again, and answer, since
            // the user is the one who switched.
            updates.clear();
            manual = true;
            continue;
        }
        *busy = false;
        break found.map(|found| {
            let Some(Found { version, update }) = found else {
                updates.clear();
                return None;
            };
            *updates.update.lock().unwrap() = update;
            let skipped = app.state::<AppState>().settings.lock().unwrap().skip_update.as_deref() == Some(version.as_str());
            *updates.view.lock().unwrap() =
                View { state: "available", version: Some(version.clone()), prompt: manual || !skipped, ..View::default() };
            Some(version)
        });
    };
    push(app);
    match found {
        Ok(Some(_)) => {}
        Ok(None) => {
            if manual {
                message(
                    app,
                    &format!("{APP_NAME} is up to date"),
                    &format!("You have the latest version, {CURRENT}."),
                    MessageDialogKind::Info,
                );
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

/// An update on offer (none to install in the demo).
struct Found {
    version: String,
    update: Option<Update>,
}

/// The update on offer, if any: on the beta channel, the higher of the two feeds'.
async fn find(app: &AppHandle, beta: bool) -> Result<Option<Found>, tauri_plugin_updater::Error> {
    if let Some(version) = demo() {
        return Ok(Some(Found { version, update: None }));
    }
    if DEV {
        return Ok(None);
    }
    let mut offers = Vec::new();
    for &feed in feeds(beta) {
        let offer = read_feed(app, feed).await;
        if let Err(e) = &offer {
            eprintln!("update check: {}: {e}", feed.url());
        }
        offers.push(offer);
    }
    Ok(highest(offers)?.map(|(_, update)| Found { version: update.version.clone(), update: Some(update) }))
}

/// The update one feed offers this app, if any.
async fn read_feed(app: &AppHandle, feed: Feed) -> Result<Option<(Version, Update)>, tauri_plugin_updater::Error> {
    // Windows: the installer takes over and this process exits inside `install`, so stop hbench view
    // first (on Windows nothing else stops it when the app goes away).
    let stopper = app.clone();
    let updater = app
        .updater_builder()
        .endpoints(vec![feed.url().parse()?])?
        .timeout(CHECK_TIMEOUT)
        .version_comparator(move |current, remote| feed.offers(&current, &remote.version))
        .on_before_exit(move || drop(stopper.state::<AppState>().server.lock().unwrap().take()))
        .build()?;
    match updater.check().await {
        Ok(Some(update)) => Ok(Some((update.version.parse()?, update))),
        Ok(None) => Ok(None),
        // The endpoint answered, but not with a manifest: the latest release has no `latest.json` (one published
        // before the updater, or without the signing key), or there is no beta feed, or it is just being replaced.
        // Nothing to offer.
        Err(tauri_plugin_updater::Error::ReleaseNotFound) => {
            eprintln!("update check: no latest.json at {}", feed.url());
            Ok(None)
        }
        Err(e) => Err(e),
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
    let mut beta = false;
    let claimed = claim(app, true, |v| {
        beta = on_beta(app);
        v.state = "downloading";
        v.progress = Some(0.0);
        v.error = None;
    });
    if !claimed {
        return;
    }
    push(app);
    let result = if demo().is_some() { fake_download(app).await } else { download_and_install(app).await };
    let switched = {
        let updates = app.state::<Updates>();
        let mut busy = updates.busy.lock().unwrap();
        *busy = false;
        let mut view = updates.view.lock().unwrap();
        view.progress = None;
        // Ask to restart even if the prompt was dismissed while downloading: the update is half done.
        view.prompt = true;
        match &result {
            Ok(()) => view.state = "ready",
            Err(e) => {
                view.state = "error";
                view.error = Some(e.clone());
            }
        }
        drop(view);
        // Don't offer to retry an update of the channel the user left during the download.
        let switched = result.is_err() && on_beta(app) != beta;
        if switched {
            updates.clear();
        }
        switched
    };
    push(app);
    if let Err(e) = result {
        eprintln!("update failed: {e}");
    }
    if switched {
        check(app, true).await;
    }
}

async fn download_and_install(app: &AppHandle) -> Result<(), String> {
    let update = app.state::<Updates>().update.lock().unwrap().clone().ok_or("There is no update to install.")?;
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
        .map_err(|e| format!("The download didn't finish: {e}"))?;
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum Feed {
    Beta,
    Stable,
}

impl Feed {
    fn url(self) -> &'static str {
        match self {
            Feed::Beta => BETA_FEED,
            Feed::Stable => STABLE_FEED,
        }
    }

    /// Whether `remote`, read from this feed, is an update to `current`. Updates only ever go up, so leaving the beta
    /// channel never downgrades. The stable feed never offers a prerelease, even one mis-marked as the latest release.
    fn offers(self, current: &Version, remote: &Version) -> bool {
        remote > current && (self == Feed::Beta || remote.pre.is_empty())
    }
}

/// Where to look for updates. On the beta channel, both feeds, each on its own: the beta feed can be behind stable,
/// or lack this platform, and must not hide a newer stable.
fn feeds(beta: bool) -> &'static [Feed] {
    if beta { &[Feed::Beta, Feed::Stable] } else { &[Feed::Stable] }
}

/// The highest version the feeds offer (the last one's on a tie). A feed that failed counts only when no other
/// offers anything: then "up to date" might be wrong, so it is an error.
fn highest<T, E>(offers: Vec<Result<Option<(Version, T)>, E>>) -> Result<Option<(Version, T)>, E> {
    let mut best: Option<(Version, T)> = None;
    let mut failed = None;
    for offer in offers {
        match offer {
            Ok(Some(o)) if best.as_ref().is_none_or(|b| o.0 >= b.0) => best = Some(o),
            Ok(_) => {}
            Err(e) => failed = failed.or(Some(e)),
        }
    }
    match (best, failed) {
        (None, Some(e)) => Err(e),
        (best, _) => Ok(best),
    }
}

/// What changing the channel does to the update at hand.
#[derive(Debug, PartialEq)]
enum OnSwitch {
    /// Forget the update the old channel offered (or failed to install), then check the new one.
    ClearAndCheck,
    Check,
    /// A download goes on and an installed update stays ready. A check in flight sees the change by itself, drops its
    /// result and checks again.
    Leave,
}

fn on_switch(state: &str, busy: bool) -> OnSwitch {
    match state {
        _ if busy => OnSwitch::Leave,
        "available" | "error" => OnSwitch::ClearAndCheck,
        "none" => OnSwitch::Check,
        _ => OnSwitch::Leave,
    }
}

/// Help/App menu "Receive Beta Updates": flip the setting, keep it, and check at once on the new channel.
pub fn toggle_beta(app: &AppHandle) {
    let updates = app.state::<Updates>();
    let (on, then) = {
        let busy = updates.busy.lock().unwrap();
        let on = {
            let state = app.state::<AppState>();
            let mut s = state.settings.lock().unwrap();
            s.beta_updates = !s.beta_updates;
            save_settings(app, &s);
            s.beta_updates
        };
        let then = on_switch(updates.view.lock().unwrap().state, *busy);
        if then == OnSwitch::ClearAndCheck {
            updates.clear();
        }
        (on, then)
    };
    if let Some(menu) = app.menu()
        && let Some(tauri::menu::MenuItemKind::Check(item)) = menu.get("beta-updates").or_else(|| find_in_submenus(&menu, "beta-updates"))
    {
        let _ = item.set_checked(on);
    }
    eprintln!("updates: beta channel {}", if on { "on" } else { "off" });
    if then != OnSwitch::Leave {
        push(app);
        check_now(app);
    }
}

fn find_in_submenus(menu: &tauri::menu::Menu<tauri::Wry>, id: &str) -> Option<tauri::menu::MenuItemKind<tauri::Wry>> {
    menu.items().ok()?.into_iter().find_map(|k| match k {
        tauri::menu::MenuItemKind::Submenu(sub) => sub.get(id),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn updates_only_go_up() {
        for feed in [Feed::Beta, Feed::Stable] {
            assert!(feed.offers(&v("0.4.1"), &v("0.5.0")));
            assert!(feed.offers(&v("0.5.0-beta.2"), &v("0.5.0")));
            assert!(!feed.offers(&v("0.5.0"), &v("0.5.0")));
            assert!(!feed.offers(&v("0.5.1-beta.1"), &v("0.5.0")));
        }
        assert!(Feed::Beta.offers(&v("0.5.0-beta.9"), &v("0.5.0-beta.10")));
        assert!(!Feed::Beta.offers(&v("0.5.0"), &v("0.5.0-beta.10")));
    }

    #[test]
    fn the_stable_feed_never_offers_a_prerelease() {
        assert!(Feed::Beta.offers(&v("0.4.1"), &v("0.5.0-beta.1")));
        assert!(!Feed::Stable.offers(&v("0.4.1"), &v("0.5.0-beta.1")));
        assert!(!Feed::Stable.offers(&v("0.5.0-beta.1"), &v("0.5.0-beta.2")));
        assert!(!Feed::Stable.offers(&v("0.4.1"), &v("0.5.0-rc.1")));
    }

    #[test]
    fn stable_reads_only_its_feed() {
        assert_eq!(feeds(false), [Feed::Stable]);
        assert_eq!(feeds(true), [Feed::Beta, Feed::Stable]);
    }

    /// The version offered, given the beta feed's and the stable feed's ("" for none, "!" for a failed read).
    fn offered(beta: &str, stable: &str) -> Result<Option<String>, Feed> {
        let offer = |version: &str, feed| match version {
            "" => Ok(None),
            "!" => Err(feed),
            s => Ok(Some((v(s), feed))),
        };
        Ok(highest(vec![offer(beta, Feed::Beta), offer(stable, Feed::Stable)])?.map(|(v, feed)| format!("{v} {feed:?}")))
    }

    #[test]
    fn the_higher_feed_wins() {
        let some = |s: &str| Ok(Some(s.to_string()));
        assert_eq!(offered("0.5.0-beta.2", "0.4.2"), some("0.5.0-beta.2 Beta"));
        assert_eq!(offered("0.5.0-beta.2", "0.5.0"), some("0.5.0 Stable"));
        assert_eq!(offered("0.5.0-beta.10", ""), some("0.5.0-beta.10 Beta"));
        assert_eq!(offered("0.5.10-beta.1", "0.5.9"), some("0.5.10-beta.1 Beta"));
        assert_eq!(offered("0.5.0", "0.5.0"), some("0.5.0 Stable"));
        assert_eq!(offered("", "0.5.0"), some("0.5.0 Stable"));
        assert_eq!(offered("", ""), Ok(None));
    }

    #[test]
    fn a_failed_feed_does_not_hide_the_other() {
        assert_eq!(offered("!", "0.5.0"), Ok(Some("0.5.0 Stable".into())));
        assert_eq!(offered("0.5.0-beta.1", "!"), Ok(Some("0.5.0-beta.1 Beta".into())));
        assert_eq!(offered("!", ""), Err(Feed::Beta));
        assert_eq!(offered("", "!"), Err(Feed::Stable));
        assert_eq!(offered("!", "!"), Err(Feed::Beta));
    }

    #[test]
    fn switching_channel_forgets_the_offer_but_not_a_download() {
        assert_eq!(on_switch("available", false), OnSwitch::ClearAndCheck);
        assert_eq!(on_switch("error", false), OnSwitch::ClearAndCheck);
        assert_eq!(on_switch("none", false), OnSwitch::Check);
        assert_eq!(on_switch("ready", false), OnSwitch::Leave);
        for state in ["none", "error", "downloading"] {
            assert_eq!(on_switch(state, true), OnSwitch::Leave);
        }
    }

    #[test]
    fn clearing_forgets_the_update() {
        let updates = Updates::default();
        *updates.view.lock().unwrap() = View { state: "available", version: Some("0.5.0-beta.1".into()), prompt: true, ..View::default() };
        updates.clear();
        let view = updates.view.lock().unwrap();
        assert_eq!((view.state, view.version.as_deref(), view.prompt), ("none", None, false));
    }
}
