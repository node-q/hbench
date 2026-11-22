//! A native yes/no dialog that the page's script can't answer: modal to the main window, with Cancel the default,
//! so a stray Enter (or a key held from the page's own button) cancels. Anything but its "Install" is no.
//!
//! Not tauri-plugin-dialog: it has no default button, makes the first button the default, reports Esc and the close
//! box as the second button (so the two can't be swapped), and on Linux drops the parent.

use std::sync::mpsc;
use tauri::{AppHandle, Manager};

const INSTALL: &str = "Install";
const CANCEL: &str = "Cancel";

/// Ask, and wait for the answer. Not from the main thread: the dialog runs there.
pub fn ask(app: &AppHandle, title: String, text: String) -> bool {
    let (tx, rx) = mpsc::channel();
    let h = app.clone();
    // If the dialog can't be shown, or goes with its window, `tx` is dropped and the answer is no.
    let _ = app.run_on_main_thread(move || show(&h, &title, &text, tx));
    rx.recv().unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn show(app: &AppHandle, title: &str, text: &str, tx: mpsc::Sender<bool>) {
    use gtk::prelude::*;
    let Some(parent) = app.get_webview_window(crate::MAIN).and_then(|w| w.gtk_window().ok()) else { return };
    let flags = gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT;
    let d = gtk::MessageDialog::new(Some(&parent), flags, gtk::MessageType::Question, gtk::ButtonsType::None, title);
    d.set_title(title);
    d.set_secondary_text(Some(text));
    d.add_button(CANCEL, gtk::ResponseType::Cancel);
    d.add_button(INSTALL, gtk::ResponseType::Accept);
    d.set_default_response(gtk::ResponseType::Cancel);
    if let Some(b) = d.widget_for_response(gtk::ResponseType::Cancel) {
        b.grab_focus();
    }
    d.connect_response(move |d, r| {
        let _ = tx.send(r == gtk::ResponseType::Accept);
        // SAFETY: on the main thread, and nothing else holds the dialog.
        unsafe { d.destroy() };
    });
    d.show_all();
    d.present();
}

/// rfd makes the first button the default, so Cancel is first. Only the button labelled Install gives
/// `Custom("Install")`: Esc and the close box give `Cancel`.
#[cfg(any(windows, target_os = "macos"))]
fn show(app: &AppHandle, title: &str, text: &str, tx: mpsc::Sender<bool>) {
    let Some(parent) = app.get_webview_window(crate::MAIN) else { return };
    let answer = rfd::AsyncMessageDialog::new()
        .set_title(title)
        .set_description(text)
        .set_level(rfd::MessageLevel::Warning)
        .set_buttons(rfd::MessageButtons::OkCancelCustom(CANCEL.into(), INSTALL.into()))
        .set_parent(&parent)
        .show();
    std::thread::spawn(move || {
        let r = tauri::async_runtime::block_on(answer);
        let _ = tx.send(matches!(r, rfd::MessageDialogResult::Custom(s) if s == INSTALL));
    });
}
