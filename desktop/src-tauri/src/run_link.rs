//! The run link, `actuaria://run/actuaria/<dataset>[?version=<x.y[.z]>]` (docs/REFERENCE.md, "Run links"): a
//! Marketplace page's "Open in Actuaria Studio". The link is parsed in the `actuaria-install` crate
//! (desktop/install/src/run.rs); this brings the window forward and hands the page the test set's name as
//! `window.__ACTUARIA_RUN__` with an `actuaria:run` event, as install.rs does for its dialog. The page looks the
//! name up in the catalogue it already has, opens New run with it picked, and answers `/__actuaria/run-link?do=taken`
//! so the next link may come. Nothing here starts a run, changes a setting or fetches anything; the page can't
//! make a run link of its own either: it only takes the one the OS handed over.

use actuaria_install::rate::Gate;
use actuaria_install::run;
use serde::Serialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

/// How long a link waits for the page to take it before the next link may replace it (a folder can take a while
/// to open, the first time).
const PENDING_TTL: Duration = Duration::from_secs(2 * 60);

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct View {
    /// none | open | error
    state: &'static str,
    nonce: String,
    dataset: Option<String>,
    version: Option<String>,
    message: Option<String>,
}

impl Default for View {
    fn default() -> View {
        View { state: "none", nonce: String::new(), dataset: None, version: None, message: None }
    }
}

#[derive(Default)]
struct Inner {
    gate: Gate,
    view: View,
    seq: u64,
}

#[derive(Default)]
pub struct RunLinks(Mutex<Inner>);

/// Send the current link to the page (also on each page load, so a link that came before the viewer loaded is
/// applied once it has).
pub fn push(app: &AppHandle) {
    let json = {
        let st = app.state::<RunLinks>();
        let i = st.0.lock().unwrap();
        serde_json::to_string(&i.view).unwrap_or_else(|_| "null".into())
    };
    if let Some(w) = app.get_webview_window(crate::MAIN) {
        let _ = w.eval(format!("window.__ACTUARIA_RUN__ = {json}; window.dispatchEvent(new Event(\"actuaria:run\"));"));
    }
}

fn nonce(seq: u64) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(seq);
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    format!("{:016x}{seq}", h.finish())
}

/// A run link from the OS. Dropped while the last one waits for the page or over the per-minute limit; otherwise
/// the page gets it, invalid links included ("This link is not valid").
pub fn on_link(app: &AppHandle, raw: &str) {
    let line = {
        let st = app.state::<RunLinks>();
        let mut i = st.0.lock().unwrap();
        let now = Instant::now();
        i.gate.expire(now, PENDING_TTL);
        if !i.gate.try_open(now) {
            return;
        }
        i.seq += 1;
        let nonce = nonce(i.seq);
        i.view = match run::parse(raw) {
            Ok(l) => View { state: "open", nonce, dataset: Some(l.dataset), version: l.version, message: None },
            Err(e) => View { state: "error", nonce, message: Some(format!("This link is not valid: {e}.")), ..View::default() },
        };
        match (&i.view.dataset, &i.view.version) {
            (Some(d), Some(v)) => format!("run link: open actuaria/{d}@{v}"),
            (Some(d), None) => format!("run link: open actuaria/{d}"),
            _ => format!("run link: error: {}", i.view.message.as_deref().unwrap_or_default()),
        }
    };
    crate::app_log(app, &line);
    if let Some(w) = app.get_webview_window(crate::MAIN) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
    crate::ensure_folder(app);
    push(app);
}

/// `/__actuaria/run-link?do=taken&nonce=..` from the page: it has applied the link (or shown why not).
pub fn action(app: &AppHandle, what: &str, nonce: Option<String>) {
    {
        let st = app.state::<RunLinks>();
        let mut i = st.0.lock().unwrap();
        if what != "taken" || i.view.state == "none" || nonce.as_deref() != Some(i.view.nonce.as_str()) {
            return;
        }
        i.gate.close();
        i.view = View::default();
    }
    push(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_view_is_no_link() {
        let v = serde_json::to_value(View::default()).unwrap();
        assert_eq!(v["state"], "none");
        assert!(v["dataset"].is_null());
    }
}
