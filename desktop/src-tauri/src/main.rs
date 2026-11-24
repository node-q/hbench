//! Actuaria desktop: a native window over hbench's viewer. The app runs the bundled `hbench view` on a free
//! loopback port for the open folder and shows it. The page gets no IPC: menus reach it through
//! `window.__actuaria.command(..)`, and it reaches the app by navigating to `/__actuaria/<action>`, which is
//! intercepted here and never loaded.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod confirm;
mod install;
mod run_link;
mod server;
mod shell_env;
mod update;

use serde::{Deserialize, Serialize};
use server::Server;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::menu::{AboutMetadataBuilder, CheckMenuItemBuilder, Menu, MenuItemBuilder, Submenu, SubmenuBuilder};
use tauri::webview::{DownloadEvent, PageLoadEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, Wry};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use url::Url;

const MAIN: &str = "main";
/// The app's name, as `productName` in tauri.conf.json (the bundle, the installers) has it.
const APP_NAME: &str = "Actuaria Studio Essentials";
const MAX_RECENT: usize = 8;
const DOCS_URL: &str = "https://github.com/node-q/hbench#readme";
const SITE_URL: &str = "https://actuaria.io/";

#[derive(Default, Serialize, Deserialize)]
struct Settings {
    folder: Option<PathBuf>,
    #[serde(default)]
    recent: Vec<PathBuf>,
    /// The update the user dismissed: not prompted for again at launch (the status bar still offers it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skip_update: Option<String>,
    /// Receive beta updates (the `beta-channel` feed, see update.rs). Off by default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    beta_updates: bool,
}

struct AppState {
    server: Mutex<Option<Server>>,
    env: HashMap<String, String>,
    settings: Mutex<Settings>,
    zoom: Mutex<f64>,
    recent_menu: Mutex<Option<Submenu<Wry>>>,
}

/// Debug builds keep their own settings and log, and run beside the installed app instead of handing over
/// to it, so developing the app never disturbs the one in use.
const DEV: bool = cfg!(debug_assertions);

fn settings_path(app: &AppHandle) -> Option<PathBuf> {
    let name = if DEV { "settings.dev.json" } else { "settings.json" };
    app.path().app_config_dir().ok().map(|d| d.join(name))
}

fn log_path(app: &AppHandle) -> PathBuf {
    let name = if DEV { "hbench-view.dev.log" } else { "hbench-view.log" };
    app.path().app_log_dir().unwrap_or_else(|_| std::env::temp_dir()).join(name)
}

/// Add a line to the app's log, the file Help's log item opens (the server's output goes there too). Best effort.
pub(crate) fn app_log(app: &AppHandle, line: &str) {
    use std::io::Write;
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path(app)) {
        let _ = writeln!(f, "[essentials {secs}] {line}");
    }
}

fn load_settings(app: &AppHandle) -> Settings {
    settings_path(app).and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_settings(app: &AppHandle, s: &Settings) {
    if let Some(p) = settings_path(app) {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(b) = serde_json::to_vec_pretty(s) {
            let _ = std::fs::write(p, b);
        }
    }
}

/// The folder opened on first launch: ~/Actuaria, created if needed.
fn default_folder(app: &AppHandle) -> PathBuf {
    let dir = app.path().home_dir().unwrap_or_else(|_| PathBuf::from(".")).join("Actuaria");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn folder_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(MAIN)
}

/// Run a viewer command (`palette`, `sidebar`, `theme`, `go:/path`, `back`, `forward`) in the page.
fn page_command(app: &AppHandle, cmd: &str) {
    if let Some(w) = main_window(app) {
        let arg = serde_json::to_string(cmd).unwrap_or_default();
        let _ = w.eval(format!("window.__actuaria && window.__actuaria.command({arg})"));
    }
}

fn show_error(app: &AppHandle, title: &str, message: &str) {
    app.dialog().message(message).title(title).kind(MessageDialogKind::Error).blocking_show();
}

/// Open `folder`: start a viewer for it, then swap it in, so a folder that fails leaves the current one up.
/// Blocks; call off the main thread.
fn open_folder(app: &AppHandle, folder: PathBuf) {
    let _one_at_a_time = FOLDER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    open_folder_now(app, folder, Remember::Yes);
}

/// Folder opens run one at a time, so a run link's default folder can't replace a folder the user picked.
static FOLDER_LOCK: Mutex<()> = Mutex::new(());

/// Whether the folder opened at start has been tried: until then, that open is the one a run link waits for.
static STARTED: AtomicBool = AtomicBool::new(false);

/// Whether an open goes into the settings (the folder reopened at the next start, and Open Recent).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Remember {
    Yes,
    No,
}

/// Under `lock`, call `open` unless `is_open` says a folder is open by then. Whether it called it.
fn open_unless_open(lock: &Mutex<()>, is_open: impl Fn() -> bool, open: impl FnOnce()) -> bool {
    let _one_at_a_time = lock.lock().unwrap_or_else(|e| e.into_inner());
    if is_open() {
        return false;
    }
    open();
    true
}

/// A run link needs New run: with no folder open, open the default one (~/Actuaria), as the first launch does,
/// without saving it as the folder to reopen. Nothing while the folder opened at start is still being tried.
pub(crate) fn ensure_folder(app: &AppHandle) {
    if !STARTED.load(Ordering::SeqCst) || current_folder(app).is_some() {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        open_unless_open(&FOLDER_LOCK, || current_folder(&app).is_some(), || open_folder_now(&app, default_folder(&app), Remember::No));
    });
}

/// An `actuaria:` link from the OS, normalised as macOS hands links over (`link::normalise`), so every OS reads
/// the same link the same way: a run link opens New run, anything else goes to the install dialog (which says
/// "This link is not valid" when it isn't an install link either).
fn open_link(app: &AppHandle, raw: &str) {
    let raw = actuaria_install::link::normalise(raw);
    if actuaria_install::run::is_run_link(&raw) {
        run_link::on_link(app, &raw);
    } else {
        install::on_link(app, &raw);
    }
}

/// The per-minute budget install and run links share (`rate::Budget`).
#[derive(Default)]
pub(crate) struct LinkBudget(pub(crate) Mutex<actuaria_install::rate::Budget>);

/// The link in a launch's arguments (after `--`), if any.
fn link_from_args(app: &AppHandle, args: &[String]) {
    if let Some(raw) = actuaria_install::link::from_args(args) {
        open_link(app, raw);
    }
}

/// Record an opened folder in the settings (reopened at the next start, first in Open Recent), unless the open
/// isn't to be remembered. Whether the settings changed.
fn remember_folder(st: &mut Settings, folder: &Path, remember: Remember) -> bool {
    if remember == Remember::No {
        return false;
    }
    st.folder = Some(folder.to_path_buf());
    st.recent.retain(|p| p != folder);
    st.recent.insert(0, folder.to_path_buf());
    st.recent.truncate(MAX_RECENT);
    true
}

/// Open a folder; call with `FOLDER_LOCK` held.
fn open_folder_now(app: &AppHandle, folder: PathBuf, remember: Remember) {
    let state = app.state::<AppState>();
    let first = state.server.lock().unwrap().is_none();
    match server::start(&folder, &state.env, &log_path(app)) {
        Ok(s) => {
            let url = s.url.clone();
            // Dropping the previous server stops it.
            *state.server.lock().unwrap() = Some(s);
            let remembered = {
                let mut st = state.settings.lock().unwrap();
                let changed = remember_folder(&mut st, &folder, remember);
                if changed {
                    save_settings(app, &st);
                }
                changed
            };
            if remembered {
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || rebuild_recent(&handle));
            }
            if let Some(w) = main_window(app) {
                let _ = w.set_title(&format!("{} — {APP_NAME}", folder_name(&folder)));
                if let Ok(u) = Url::parse(&url) {
                    let _ = w.navigate(u);
                }
            }
        }
        Err(e) if first => {
            // Still on the start screen: show the error there, with a way to pick another folder.
            if let Some(w) = main_window(app) {
                let msg = serde_json::to_string(&format!("Couldn't open {}\n\n{e}", folder.display())).unwrap_or_default();
                let _ = w.eval(format!("window.showError && window.showError({msg})"));
            }
        }
        Err(e) => show_error(app, "Couldn't open the folder", &format!("{}\n\n{e}", folder.display())),
    }
}

fn pick_folder(app: &AppHandle) {
    let current = app.state::<AppState>().settings.lock().unwrap().folder.clone();
    let mut dialog = app.dialog().file().set_title("Open a jobs or tasks folder");
    if let Some(c) = current {
        dialog = dialog.set_directory(c);
    }
    if let Some(p) = dialog.blocking_pick_folder().and_then(|p| p.into_path().ok()) {
        open_folder(app, p);
    }
}

fn current_folder(app: &AppHandle) -> Option<PathBuf> {
    app.state::<AppState>().server.lock().unwrap().as_ref().map(|s| s.folder.clone())
}

/// Handle `/__actuaria/<action>` from the page. Runs on its own thread (dialogs block).
fn page_action(app: AppHandle, url: Url) {
    std::thread::spawn(move || {
        let action = url.path().trim_start_matches("/__actuaria/").to_string();
        let param = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        match action.as_str() {
            "open-folder" => pick_folder(&app),
            "open-recent" => {
                if let Some(p) = param("path") {
                    open_folder(&app, PathBuf::from(p));
                }
            }
            "reveal" => {
                // The open folder, or one entry in it (`path`: a single name, never a path out of the folder).
                let Some(folder) = current_folder(&app) else { return };
                let target = match param("path") {
                    Some(name) => {
                        let mut parts = Path::new(&name).components();
                        match (parts.next(), parts.next()) {
                            (Some(std::path::Component::Normal(n)), None) => folder.join(n),
                            _ => return,
                        }
                    }
                    None => folder,
                };
                if target.exists() {
                    let _ = app.opener().reveal_item_in_dir(target);
                }
            }
            "notify" => {
                let clip = |k: &str, max: usize| param(k).unwrap_or_default().chars().take(max).collect::<String>();
                let _ = app.notification().builder().title(clip("title", 80)).body(clip("body", 300)).show();
            }
            "theme" => {
                // The title bar shows the window's background colour: match the tab strip.
                let (theme, bg) = match param("mode").as_deref() {
                    Some("light") => (tauri::Theme::Light, tauri::window::Color(0xeb, 0xe5, 0xf0, 0xff)),
                    _ => (tauri::Theme::Dark, tauri::window::Color(0x0c, 0x06, 0x12, 0xff)),
                };
                if let Some(w) = main_window(&app) {
                    let _ = w.set_theme(Some(theme));
                    let _ = w.set_background_color(Some(bg));
                }
            }
            "server-log" => {
                let _ = app.opener().open_path(log_path(&app).to_string_lossy(), None::<&str>);
            }
            "update" => update::action(&app, param("do").as_deref().unwrap_or_default()),
            "run-link" => run_link::action(&app, param("do").as_deref().unwrap_or_default(), param("nonce")),
            "install" => install::action(&app, param("do").as_deref().unwrap_or_default(), param("nonce"), param("target")),
            "open-external" => {
                if let Some(u) = param("url").and_then(|u| Url::parse(&u).ok())
                    && matches!(u.scheme(), "http" | "https" | "mailto")
                {
                    let _ = app.opener().open_url(u.as_str(), None::<&str>);
                }
            }
            _ => {}
        }
    });
}

/// Whether the webview may load `url`: the start screen and the running viewer. Anything else is either a
/// page action or a link that belongs in the browser.
fn allow_navigation(app: &AppHandle, url: &Url) -> bool {
    if url.path().starts_with("/__actuaria/") {
        page_action(app.clone(), url.clone());
        // The page sends its actions one at a time; tell it this one arrived.
        if let Some(w) = main_window(app) {
            let _ = w.eval("window.__actuaria && window.__actuaria.ack && window.__actuaria.ack()");
        }
        return false;
    }
    let local_app = matches!(url.scheme(), "tauri" | "asset") || url.host_str() == Some("tauri.localhost");
    if local_app || url.scheme() == "about" {
        return true;
    }
    let server = app.state::<AppState>().server.lock().unwrap().as_ref().map(|s| s.url.clone());
    if let Some(origin) = server.and_then(|s| Url::parse(&s).ok())
        && url.origin() == origin.origin()
    {
        return true;
    }
    if matches!(url.scheme(), "http" | "https" | "mailto") {
        let _ = app.opener().open_url(url.as_str(), None::<&str>);
    }
    false
}

/// Marks the page as the desktop app and sends links that would leave the viewer (other sites, new tabs)
/// to the default browser.
fn init_script() -> String {
    let info = serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "platform": std::env::consts::OS });
    format!(
        r#"window.__ACTUARIA_DESKTOP__ = {info};
(function () {{
  var out = function (u) {{ location.href = "/__actuaria/open-external?url=" + encodeURIComponent(u); }};
  document.addEventListener("click", function (e) {{
    var a = e.target && e.target.closest ? e.target.closest("a[href]") : null;
    if (!a || a.hasAttribute("download") || e.defaultPrevented) return;
    var u = new URL(a.href, location.href);
    var external = u.origin !== location.origin && /^(https?|mailto):$/.test(u.protocol);
    if (external || (a.target === "_blank" && /^https?:$/.test(u.protocol))) {{ e.preventDefault(); out(u.href); }}
  }}, true);
  window.open = function (u) {{ if (u) out(new URL(u, location.href).href); return null; }};
}})();"#
    )
}

fn rebuild_recent(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some(menu) = state.recent_menu.lock().unwrap().clone() else { return };
    if let Ok(items) = menu.items() {
        for it in items {
            let _ = menu.remove(&it);
        }
    }
    let recent = state.settings.lock().unwrap().recent.clone();
    if recent.is_empty() {
        if let Ok(i) = MenuItemBuilder::with_id("none", "No Recent Folders").enabled(false).build(app) {
            let _ = menu.append(&i);
        }
        return;
    }
    for (n, p) in recent.iter().enumerate() {
        let label = format!("{}  —  {}", folder_name(p), p.parent().map(|d| d.display().to_string()).unwrap_or_default());
        if let Ok(i) = MenuItemBuilder::with_id(format!("recent:{n}"), label).build(app) {
            let _ = menu.append(&i);
        }
    }
}

fn build_menu(app: &AppHandle) -> tauri::Result<(Menu<Wry>, Submenu<Wry>)> {
    let item = |id: &str, text: &str, acc: Option<&str>| {
        let mut b = MenuItemBuilder::with_id(id, text);
        if let Some(a) = acc {
            b = b.accelerator(a);
        }
        b.build(app)
    };
    let about = AboutMetadataBuilder::new()
        .name(Some(APP_NAME))
        .version(Some(env!("CARGO_PKG_VERSION")))
        .copyright(Some("© 2026 NODEQ"))
        .comments(Some("Expert-verified AI agent benchmarking studio.\nBuilt on hbench by node-q."))
        .website(Some(SITE_URL))
        .build();

    let recent = SubmenuBuilder::new(app, "Open Recent").build()?;
    let reveal = if cfg!(target_os = "macos") { "Reveal Folder in Finder" } else { "Show Folder in File Manager" };
    let mut file = SubmenuBuilder::new(app, "File")
        .item(&item("go:/run", "New Run…", Some("CmdOrCtrl+N"))?)
        .separator()
        .item(&item("open-folder", "Open Folder…", Some("CmdOrCtrl+O"))?)
        .item(&recent)
        .separator()
        .item(&item("reveal", reveal, Some("CmdOrCtrl+Shift+R"))?)
        .separator()
        // As in an editor, Cmd+W closes the tab; the window takes Shift.
        .item(&item("close-tab", "Close Tab", Some("CmdOrCtrl+W"))?)
        .item(&item("close-other-tabs", "Close Other Tabs", Some("Alt+CmdOrCtrl+T"))?)
        .item(&item("close-window", "Close Window", Some("CmdOrCtrl+Shift+W"))?);
    if !cfg!(target_os = "macos") {
        file = file.separator().quit();
    }
    let edit = SubmenuBuilder::new(app, "Edit").undo().redo().separator().cut().copy().paste().select_all().build()?;
    let view = SubmenuBuilder::new(app, "View")
        .item(&item("palette", "Command Palette…", Some("CmdOrCtrl+Shift+P"))?)
        .separator()
        .item(&item("go:/", "Jobs", Some("CmdOrCtrl+1"))?)
        .item(&item("go:/compare", "Compare", Some("CmdOrCtrl+2"))?)
        .item(&item("go:/run", "New Run", Some("CmdOrCtrl+3"))?)
        .separator()
        .item(&item("sidebar", "Toggle Side Bar", Some("CmdOrCtrl+B"))?)
        .item(&item("theme", "Toggle Light / Dark Theme", Some("CmdOrCtrl+Shift+L"))?)
        .separator()
        .item(&item("zoom:in", "Zoom In", Some("CmdOrCtrl+="))?)
        .item(&item("zoom:out", "Zoom Out", Some("CmdOrCtrl+-"))?)
        .item(&item("zoom:reset", "Actual Size", Some("CmdOrCtrl+0"))?)
        .separator()
        .item(&item("reload", "Reload", Some("CmdOrCtrl+R"))?)
        .fullscreen()
        .build()?;
    let go = SubmenuBuilder::new(app, "Go")
        .item(&item("back", "Back", Some("CmdOrCtrl+["))?)
        .item(&item("forward", "Forward", Some("CmdOrCtrl+]"))?)
        .separator()
        .item(&item("tab-next", "Next Tab", Some("CmdOrCtrl+Shift+]"))?)
        .item(&item("tab-prev", "Previous Tab", Some("CmdOrCtrl+Shift+["))?)
        .separator()
        .item(&item("palette", "Go to Job, Trial or Page…", Some("CmdOrCtrl+P"))?)
        .build()?;
    let window = SubmenuBuilder::new(app, "Window").minimize().maximize().build()?;
    let mut help = SubmenuBuilder::new(app, "Help")
        .item(&item("help:docs", "hbench Documentation", None)?)
        .item(&item("help:site", "Actuaria.io Website", None)?)
        .separator()
        .item(&item("help:log", "Show Server Log", None)?);
    if cfg!(debug_assertions) {
        help = help.separator().item(&item("devtools", "Toggle Developer Tools", Some("Alt+CmdOrCtrl+I"))?);
    }
    if !cfg!(target_os = "macos") {
        help = help
            .separator()
            .item(&item("check-update", "Check for Updates…", None)?)
            .item(
                &CheckMenuItemBuilder::with_id("beta-updates", "Receive Beta Updates")
                    .checked(app.state::<AppState>().settings.lock().unwrap().beta_updates)
                    .build(app)?,
            )
            .about(Some(about.clone()));
    }

    let menu = Menu::new(app)?;
    if cfg!(target_os = "macos") {
        let app_menu = SubmenuBuilder::new(app, APP_NAME)
            .about(Some(about))
            .item(&item("check-update", "Check for Updates…", None)?)
            .item(
                &CheckMenuItemBuilder::with_id("beta-updates", "Receive Beta Updates")
                    .checked(app.state::<AppState>().settings.lock().unwrap().beta_updates)
                    .build(app)?,
            )
            .separator()
            .services()
            .separator()
            .hide()
            .hide_others()
            .show_all()
            .separator()
            .quit()
            .build()?;
        menu.append(&app_menu)?;
    }
    menu.append(&file.build()?)?;
    menu.append(&edit)?;
    menu.append(&view)?;
    menu.append(&go)?;
    menu.append(&window)?;
    menu.append(&help.build()?)?;
    Ok((menu, recent))
}

fn on_menu(app: &AppHandle, id: &str) {
    let zoom = |delta: Option<f64>| {
        let state = app.state::<AppState>();
        let mut z = state.zoom.lock().unwrap();
        *z = delta.map_or(1.0, |d| (*z + d).clamp(0.5, 2.0));
        if let Some(w) = main_window(app) {
            let _ = w.set_zoom(*z);
        }
    };
    match id {
        "open-folder" => {
            let app = app.clone();
            std::thread::spawn(move || pick_folder(&app));
        }
        "reveal" | "help:log" => {
            let action = if id == "reveal" { "reveal" } else { "server-log" };
            if let Ok(u) = Url::parse(&format!("http://local/__actuaria/{action}")) {
                page_action(app.clone(), u);
            }
        }
        "check-update" => update::check_now(app),
        "beta-updates" => update::toggle_beta(app),
        "help:docs" => {
            let _ = app.opener().open_url(DOCS_URL, None::<&str>);
        }
        "help:site" => {
            let _ = app.opener().open_url(SITE_URL, None::<&str>);
        }
        "zoom:in" => zoom(Some(0.1)),
        "zoom:out" => zoom(Some(-0.1)),
        "zoom:reset" => zoom(None),
        "reload" => {
            if let Some(w) = main_window(app) {
                let _ = w.eval("location.reload()");
            }
        }
        "close-window" => {
            if let Some(w) = main_window(app) {
                let _ = w.close();
            }
        }
        "devtools" =>
        {
            #[cfg(debug_assertions)]
            if let Some(w) = main_window(app) {
                if w.is_devtools_open() { w.close_devtools() } else { w.open_devtools() }
            }
        }
        id if id.starts_with("recent:") => {
            let n: usize = id["recent:".len()..].parse().unwrap_or(usize::MAX);
            let path = app.state::<AppState>().settings.lock().unwrap().recent.get(n).cloned();
            if let Some(p) = path {
                let app = app.clone();
                std::thread::spawn(move || open_folder(&app, p));
            }
        }
        cmd => page_command(app, cmd),
    }
}

fn main() {
    let mut builder = tauri::Builder::default();
    if !DEV {
        // A second launch focuses the running app, and hands over its `actuaria:` link (`-- <link>`), which is
        // parsed only here, in the running instance.
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if let Some(w) = main_window(app) {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
            link_from_args(app, &args);
        }));
    }
    builder
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_deep_link::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let settings = load_settings(&handle);
            app.manage(AppState {
                server: Mutex::new(None),
                env: shell_env::resolve(),
                settings: Mutex::new(settings),
                zoom: Mutex::new(1.0),
                recent_menu: Mutex::new(None),
            });
            app.manage(update::Updates::default());
            app.manage(install::Installs::default());
            app.manage(run_link::RunLinks::default());
            app.manage(LinkBudget::default());
            let (menu, recent) = build_menu(&handle)?;
            app.set_menu(menu)?;
            *app.state::<AppState>().recent_menu.lock().unwrap() = Some(recent);
            rebuild_recent(&handle);
            app.on_menu_event(|app, e| on_menu(app, e.id().0.as_str()));

            let nav = handle.clone();
            let dl = handle.clone();
            WebviewWindowBuilder::new(app, MAIN, WebviewUrl::App("index.html".into()))
                .title(APP_NAME)
                .inner_size(1440.0, 900.0)
                .min_inner_size(960.0, 600.0)
                .background_color(tauri::window::Color(0x13, 0x0a, 0x1c, 0xff))
                .initialization_script(init_script())
                .on_navigation(move |url| allow_navigation(&nav, url))
                // Each page (the start screen, a folder's viewer, a reload) starts without the update state.
                .on_page_load(|w, p| {
                    if matches!(p.event(), PageLoadEvent::Finished) {
                        update::push(w.app_handle());
                        install::push(w.app_handle());
                        run_link::push(w.app_handle());
                    }
                })
                // The title bar names the open tab, then the folder: "Jobs — my-evals".
                .on_document_title_changed(|w, title| {
                    // The viewer titles its pages "<page> · <app name>" (useTitle in viewer/src/shell.tsx).
                    let Some(page) = title.strip_suffix(&format!(" · {APP_NAME}")) else { return };
                    let folder = current_folder(w.app_handle()).map(|f| folder_name(&f)).unwrap_or_default();
                    let _ = w.set_title(&if folder.is_empty() { page.to_string() } else { format!("{page} — {folder}") });
                })
                .on_download(move |_, event| {
                    if let DownloadEvent::Requested { url, destination } = event {
                        let name = destination
                            .file_name()
                            .map(|n| n.to_owned())
                            .or_else(|| url.path_segments().and_then(|mut s| s.next_back()).map(Into::into))
                            .unwrap_or_else(|| "download".into());
                        if let Ok(dir) = dl.path().download_dir() {
                            *destination = dir.join(name);
                        }
                    }
                    true
                })
                .build()?;
            // The title bar takes the window's background colour (the theme action keeps it on the tab strip's)
            // instead of the system grey. macOS 26 does that by itself only for apps built with its SDK; the
            // release's runners build with an older one.
            #[cfg(target_os = "macos")]
            if let Some(w) = main_window(&handle) {
                let _ = w.set_title_bar_style(tauri::TitleBarStyle::Transparent);
            }

            let folder = app.state::<AppState>().settings.lock().unwrap().folder.clone().filter(|p| p.is_dir());
            let folder = folder.unwrap_or_else(|| default_folder(&handle));
            update::start(&handle);
            install::setup(&handle);
            // A link that launched the app: on Windows and Linux in the arguments (after `--`), on macOS as an
            // Apple event. Either waits for the page's first load to show (install::push and run_link::push on page load).
            let args: Vec<String> = std::env::args().collect();
            link_from_args(&handle, &args);
            #[cfg(target_os = "macos")]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                if let Ok(Some(urls)) = app.deep_link().get_current() {
                    for u in urls {
                        open_link(&handle, u.as_str());
                    }
                }
                let h = handle.clone();
                app.deep_link().on_open_url(move |e| {
                    for u in e.urls() {
                        open_link(&h, u.as_str());
                    }
                });
            }
            // A run link that came meanwhile waits for this open (ensure_folder); if it failed, the link opens the
            // default folder instead.
            std::thread::spawn(move || {
                open_folder(&handle, folder);
                STARTED.store(true, Ordering::SeqCst);
                if run_link::waiting(&handle) {
                    ensure_folder(&handle);
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("building the Actuaria app")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // Stop hbench view with the app.
                app.state::<AppState>().server.lock().unwrap().take();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn a_link_open_is_not_remembered_and_the_users_pick_stays() {
        let mut st = Settings::default();
        let mine = PathBuf::from("/home/ann/evals");
        assert!(remember_folder(&mut st, &mine, Remember::Yes));
        assert_eq!(st.folder.as_deref(), Some(mine.as_path()));
        assert_eq!(st.recent, vec![mine.clone()]);
        // A run link's default folder opens with Remember::No: the saved folder and Open Recent stay the user's,
        // and nothing is saved.
        let before = serde_json::to_string(&st).unwrap();
        assert!(!remember_folder(&mut st, Path::new("/home/ann/Actuaria"), Remember::No));
        assert_eq!(serde_json::to_string(&st).unwrap(), before);
        // Reopening keeps one entry per folder, newest first.
        remember_folder(&mut st, Path::new("/b"), Remember::Yes);
        remember_folder(&mut st, &mine, Remember::Yes);
        assert_eq!(st.recent, vec![mine.clone(), PathBuf::from("/b")]);
    }

    #[test]
    fn a_link_open_waits_for_the_users_pick_and_then_does_nothing() {
        let lock = Arc::new(Mutex::new(()));
        let current: Arc<Mutex<Option<&str>>> = Arc::new(Mutex::new(None));
        // The user's pick holds the lock while its viewer starts.
        let guard = lock.lock().unwrap();
        let (l, c) = (lock.clone(), current.clone());
        let link =
            std::thread::spawn(move || open_unless_open(&l, || c.lock().unwrap().is_some(), || *c.lock().unwrap() = Some("~/Actuaria")));
        std::thread::sleep(Duration::from_millis(50));
        *current.lock().unwrap() = Some("/home/ann/evals");
        drop(guard);
        assert!(!link.join().unwrap(), "the link's open saw the user's folder and did nothing");
        assert_eq!(*current.lock().unwrap(), Some("/home/ann/evals"));
        // With nothing open, the link opens the default.
        let c = Arc::new(Mutex::new(None::<&str>));
        assert!(open_unless_open(&lock, || c.lock().unwrap().is_some(), || *c.lock().unwrap() = Some("~/Actuaria")));
        assert_eq!(*c.lock().unwrap(), Some("~/Actuaria"));
    }
}
