//! Signing in to Actuaria (hbench-studio docs/design/sign-in-products.md 7.2, D133). The page asks by navigating
//! to `/__actuaria/login`, `/__actuaria/logout` or `/__actuaria/account`; the app runs the bundled
//! `hbench login --browser` or `hbench logout`, or opens the account site in the system browser. No Tauri
//! command, no capability: the page gets only progress, as `window.__ACTUARIA_SIGNIN__` with an
//! `actuaria:signin` event. **Tokens never enter the window**: hbench keeps them in the OS keychain, and the page
//! learns who is signed in from the local viewer's `/api/account`.

use crate::{main_window, server};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

/// The account site, opened in the system browser.
const ACCOUNT_URL: &str = "https://account.actuaria.io";
/// hbench prints these (src/account/mod.rs, `NO_BROWSER_LINE` and `URL_LINE_PREFIX`): the address is shown
/// in the page only when hbench could not open a browser.
const NO_BROWSER_LINE: &str = "Couldn't open a browser.";
const URL_LINE_PREFIX: &str = "Sign in at: ";

#[derive(Clone, Default, Serialize)]
pub struct View {
    /// `idle` or `waiting` (a sign-in is open in the browser).
    state: &'static str,
    last: Option<&'static str>,
    ok: Option<bool>,
    /// The authorization address, only when no browser opened. It carries no code and no token.
    url: Option<String>,
    error: Option<String>,
    seq: u64,
}

#[derive(Default)]
pub struct Signin {
    view: Mutex<View>,
    /// The running `hbench login --browser`, with its generation.
    login: Mutex<Option<(u64, Child)>>,
}

/// Send the current state to the page (also after each page load, as the update state is).
pub fn push(app: &AppHandle) {
    let view = app.state::<Signin>().view.lock().unwrap().clone();
    if let Some(w) = main_window(app) {
        let json = serde_json::to_string(&view).unwrap_or_default();
        let _ = w.eval(format!("window.__ACTUARIA_SIGNIN__ = {json}; window.dispatchEvent(new Event(\"actuaria:signin\"));"));
    }
}

fn set(app: &AppHandle, f: impl FnOnce(&mut View)) -> u64 {
    let seq = {
        let st = app.state::<Signin>();
        let mut v = st.view.lock().unwrap();
        if v.state.is_empty() {
            v.state = "idle";
        }
        f(&mut v);
        v.seq += 1;
        v.seq
    };
    push(app);
    seq
}

/// hbench, with the login shell's environment, and no console window on Windows.
fn hbench(env: &HashMap<String, String>, args: &[&str]) -> Result<Command, String> {
    let mut c = Command::new(server::hbench_path()?);
    c.args(args).envs(env).env("HBENCH_NO_UPDATE_CHECK", "1").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000); // CREATE_NO_WINDOW
    Ok(c)
}

/// The last line hbench wrote to stderr, without its `Error: ` prefix: what to tell the person.
fn last_error(stderr: &str) -> Option<String> {
    let line = stderr.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    Some(line.trim_start_matches("Error: ").chars().take(300).collect())
}

/// Handle `/__actuaria/login`, `/__actuaria/logout` and `/__actuaria/account`. Called off the main thread.
pub fn action(app: &AppHandle, action: &str) {
    match action {
        "login" => login(app),
        "logout" => logout(app),
        "account" => {
            let _ = app.opener().open_url(ACCOUNT_URL, None::<&str>);
        }
        _ => {}
    }
}

/// Start `hbench login --browser`. A second request while one waits starts over: the person may have closed
/// the browser tab, and the first one would otherwise hold them for five minutes.
fn login(app: &AppHandle) {
    stop_login(app);
    let env = app.state::<crate::AppState>().env.clone();
    let mut child = match hbench(&env, &["login", "--browser"]).and_then(|mut c| c.spawn().map_err(|e| e.to_string())) {
        Ok(c) => c,
        Err(e) => {
            set(app, |v| *v = View { state: "idle", last: Some("login"), ok: Some(false), error: Some(e), seq: v.seq, url: None });
            return;
        }
    };
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let generation = set(app, |v| *v = View { state: "waiting", seq: v.seq, ..View::default() });
    *app.state::<Signin>().login.lock().unwrap() = Some((generation, child));

    // The address goes to the page only when hbench says no browser opened.
    let out_app = app.clone();
    let out = std::thread::spawn(move || {
        let mut no_browser = false;
        for line in stdout.into_iter().flat_map(|s| BufReader::new(s).lines().map_while(Result::ok)) {
            if line.starts_with(NO_BROWSER_LINE) {
                no_browser = true;
            } else if let Some(url) = line.strip_prefix(URL_LINE_PREFIX)
                && no_browser
                && (url.starts_with("https://") || (crate::DEV && url.starts_with("http://")))
            {
                let url = url.to_string();
                set(&out_app, |v| v.url = Some(url));
            }
        }
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(e) = stderr {
            let _ = std::io::Read::read_to_string(&mut BufReader::new(e), &mut s);
        }
        s
    });

    // Wait for it, unless a newer login replaced it.
    let status = loop {
        {
            let st = app.state::<Signin>();
            let mut slot = st.login.lock().unwrap();
            match slot.as_mut() {
                Some((g, c)) if *g == generation => {
                    if let Ok(Some(st)) = c.try_wait() {
                        slot.take();
                        break Some(st);
                    }
                }
                _ => break None,
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let _ = out.join();
    let stderr = err.join().unwrap_or_default();
    let Some(status) = status else { return };
    let ok = status.success();
    let error = if ok { None } else { last_error(&stderr).or_else(|| Some(format!("hbench exited with {status}"))) };
    set(app, |v| *v = View { state: "idle", last: Some("login"), ok: Some(ok), error, url: None, seq: v.seq });
}

/// `hbench logout`: revokes the refresh token at Keycloak and clears the keychain. The browser's own session
/// stays signed in, and the page says so.
fn logout(app: &AppHandle) {
    stop_login(app);
    let env = app.state::<crate::AppState>().env.clone();
    let (ok, error) = match hbench(&env, &["logout"]).and_then(|mut c| c.output().map_err(|e| e.to_string())) {
        Ok(out) if out.status.success() => (true, None),
        Ok(out) => (false, last_error(&String::from_utf8_lossy(&out.stderr))),
        Err(e) => (false, Some(e)),
    };
    set(app, |v| *v = View { state: "idle", last: Some("logout"), ok: Some(ok), error, url: None, seq: v.seq });
}

/// Stop a waiting `hbench login --browser` (a new request, sign-out, or the app quitting).
pub fn stop_login(app: &AppHandle) {
    if let Some((_, mut c)) = app.state::<Signin>().login.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::last_error;

    #[test]
    fn the_last_stderr_line_is_the_message() {
        assert_eq!(last_error("noise\nError: sign-in was not completed: x\n\n").as_deref(), Some("sign-in was not completed: x"));
        assert_eq!(last_error(""), None);
    }
}
