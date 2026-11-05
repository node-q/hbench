//! A progress line on stderr while hbench downloads tasks, redrawn in place:
//! `Downloading terminal-bench@2.0.1  ████████░░░░  37/89 tasks  412 MB/1.1 GB`. Shown when stderr is a terminal,
//! unless `CI` or `HBENCH_NO_PROGRESS=1` is set or `--quiet` or `--observe` is on.

use crate::job::{bar, paint_if, use_color_on_stderr};
use std::ffi::OsStr;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

static QUIET: AtomicBool = AtomicBool::new(false);

const REDRAW_EVERY: Duration = Duration::from_millis(100);

/// `--quiet`: no progress lines from here on.
pub fn quiet() {
    QUIET.store(true, Ordering::Relaxed);
}

/// Whether progress lines are drawn on stderr.
pub fn enabled() -> bool {
    !QUIET.load(Ordering::Relaxed)
        && !crate::observe::enabled()
        && shown(std::env::var_os("HBENCH_NO_PROGRESS").as_deref(), std::env::var_os("CI").is_some(), std::io::stderr().is_terminal())
}

fn shown(no_progress: Option<&OsStr>, ci: bool, terminal: bool) -> bool {
    terminal && !ci && no_progress.is_none_or(|v| v == "0")
}

/// A download of `tasks` tasks and `bytes` bytes (0 when the size is not known), drawn while it runs when
/// [`enabled`]. Dropped unfinished, it erases its line.
pub struct Download {
    label: String,
    tasks: usize,
    bytes: u64,
    done: Mutex<Done>,
    live: bool,
}

#[derive(Default)]
struct Done {
    tasks: usize,
    bytes: u64,
    drawn: Option<Instant>,
}

impl Download {
    pub fn start(label: &str, tasks: usize, bytes: u64) -> Download {
        let d = Download { label: label.to_string(), tasks, bytes, done: Mutex::default(), live: enabled() };
        d.advance(0, 0);
        d
    }

    /// Whether the line is drawn; when not, the caller says what it downloads in a plain line.
    pub fn is_live(&self) -> bool {
        self.live
    }

    pub fn add_bytes(&self, n: u64) {
        self.advance(0, n);
    }

    pub fn add_tasks(&self, n: usize) {
        self.advance(n, 0);
    }

    /// Leaves `✔ Downloaded <label> · <tasks> · <bytes>` on screen.
    pub fn finish(mut self) {
        if self.live {
            eprintln!("\r\x1b[2K{}", self.finished_line(use_color_on_stderr()));
            self.live = false;
        }
    }

    /// (tasks, bytes) done so far.
    #[cfg(test)]
    pub fn counts(&self) -> (usize, u64) {
        let done = self.done();
        (done.tasks, done.bytes)
    }

    fn done(&self) -> MutexGuard<'_, Done> {
        self.done.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn advance(&self, tasks: usize, bytes: u64) {
        let mut done = self.done();
        done.tasks += tasks;
        done.bytes += bytes;
        if self.live && done.drawn.is_none_or(|t| t.elapsed() >= REDRAW_EVERY) {
            done.drawn = Some(Instant::now());
            eprint!("\r\x1b[2K{}", self.line(&done, use_color_on_stderr()));
        }
    }

    /// `Downloading terminal-bench@2.0.1  ████████░░░░  37/89 tasks  412 MB/1.1 GB`; the bar follows the bytes
    /// when their total is known, else the tasks.
    fn line(&self, done: &Done, color: bool) -> String {
        let frac = if self.bytes > 0 { done.bytes as f64 / self.bytes as f64 } else { done.tasks as f64 / self.tasks.max(1) as f64 };
        let (a, b) = bar(frac);
        let bytes = if self.bytes > 0 { format!("  {}/{}", size(done.bytes), size(self.bytes)) } else { String::new() };
        format!(
            "Downloading {}  {}{}  {}/{} tasks{bytes}",
            self.label,
            paint_if(color, "36", &a),
            paint_if(color, "2", &b),
            done.tasks,
            self.tasks
        )
    }

    fn finished_line(&self, color: bool) -> String {
        let bytes = if self.bytes > 0 { format!(" · {}", size(self.bytes)) } else { String::new() };
        format!(
            "{} {} {} {}",
            paint_if(color, "32", "✔"),
            paint_if(color, "1", &format!("Downloaded {}", self.label)),
            paint_if(color, "2", "·"),
            paint_if(color, "32", &format!("{} task(s){bytes}", self.tasks))
        )
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        if self.live {
            eprint!("\r\x1b[2K");
        }
    }
}

/// `n` bytes in decimal units, as docker prints them: `512 B`, `9.4 kB`, `412 MB`, `1.1 GB`.
fn size(n: u64) -> String {
    const UNITS: [&str; 4] = ["kB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let (mut v, mut unit) = (n as f64 / 1000.0, 0);
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if v < 10.0 { format!("{v:.1} {}", UNITS[unit]) } else { format!("{v:.0} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn download(tasks: usize, bytes: u64) -> Download {
        Download { label: "terminal-bench@2.0.1".into(), tasks, bytes, done: Mutex::default(), live: false }
    }

    #[test]
    fn shown_only_on_a_terminal_outside_ci_unless_turned_off() {
        assert!(shown(None, false, true));
        assert!(shown(Some(OsStr::new("0")), false, true));
        assert!(!shown(None, false, false));
        assert!(!shown(None, true, true));
        assert!(!shown(Some(OsStr::new("1")), false, true));
        assert!(!shown(Some(OsStr::new("")), false, true));
    }

    #[test]
    fn counts_tasks_and_bytes() {
        let d = download(89, 1_100_000_000);
        d.add_tasks(37);
        (0..4).for_each(|_| d.add_bytes(103_000_000));
        assert_eq!(d.line(&d.done(), false), "Downloading terminal-bench@2.0.1  █████████░░░░░░░░░░░░░░░  37/89 tasks  412 MB/1.1 GB");
        assert_eq!(d.finished_line(false), "✔ Downloaded terminal-bench@2.0.1 · 89 task(s) · 1.1 GB");
    }

    #[test]
    fn without_a_size_the_bar_follows_the_tasks() {
        let d = download(4, 0);
        d.add_tasks(2);
        assert_eq!(d.line(&d.done(), false), "Downloading terminal-bench@2.0.1  ████████████░░░░░░░░░░░░  2/4 tasks");
        assert_eq!(d.finished_line(false), "✔ Downloaded terminal-bench@2.0.1 · 4 task(s)");
    }

    #[test]
    fn sizes_in_decimal_units() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(999), "999 B");
        assert_eq!(size(9_400), "9.4 kB");
        assert_eq!(size(412_000_000), "412 MB");
        assert_eq!(size(1_100_000_000), "1.1 GB");
        assert_eq!(size(5_000_000_000_000_000), "5000 TB");
    }
}
