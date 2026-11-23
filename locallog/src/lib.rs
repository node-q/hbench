//! Local event logs for hbench and Actuaria Studio Essentials, to trace a user's error after the fact.
//!
//! One line per event: `<RFC 3339 UTC time> <app>/<version> <os>-<arch> <event> <message> [key=value…] [error="…"]`,
//! for example
//! `2026-11-23T08:15:02.120Z essentials/0.5.0 macos-aarch64 install.state error actuaria/prompt-slim@0.1.0 error="Couldn't reach the catalogue: timed out"`.
//!
//! Events are `tracing` events that carry an `event` field (`tracing::info!(event = "server.start", port, "…")`);
//! nothing else is recorded, so no dependency's logging lands here. Every line is redacted ([`redact()`]) before
//! it is written, and the files are size-capped ([`Rotating`]). Callers never pass task content, prompts or
//! answers. Nothing here sends anything anywhere.

mod redact;
mod rotate;

pub use redact::{EMAIL, MASK, redact};
pub use rotate::{FILES, MAX_BYTES, Rotating, files, tail};

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// The longest line written; an error longer than this is cut.
pub const MAX_LINE: usize = 4096;

/// `2026-11-23T08:15:02.120Z`.
pub fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, rem % 3600 / 60, rem % 60, d.subsec_millis())
}

/// `macos-aarch64`, `windows-x86_64`, `linux-x86_64`.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// One event, before it becomes a line.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Entry {
    pub event: String,
    pub message: String,
    pub fields: Vec<(String, String)>,
    pub error: Option<String>,
}

/// A field's value as written: bare when it is one plain word, else quoted.
fn value(v: &str) -> String {
    let plain = !v.is_empty() && v.chars().all(|c| !c.is_whitespace() && c != '"' && c != '\\' && !c.is_control());
    if plain {
        return v.to_string();
    }
    let mut s = String::from("\"");
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(s, "\\u{{{:x}}}", c as u32);
            }
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

/// One line, never more: newlines and control characters in the message are escaped.
fn one_line(s: &str) -> String {
    s.trim().replace('\r', "\\r").replace('\n', "\\n").chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// The line for `e`, unredacted (the [`Sink`] redacts it). `app` is `essentials/0.5.0` or `hbench/0.5.0`.
pub fn format_line(at: SystemTime, app: &str, e: &Entry) -> String {
    let event = if e.event.is_empty() { "event" } else { e.event.as_str() };
    let mut line = format!("{} {app} {} {}", rfc3339(at), platform(), one_line(event).replace(' ', "_"));
    if !e.message.trim().is_empty() {
        line.push(' ');
        line.push_str(&one_line(&e.message));
    }
    for (k, v) in &e.fields {
        let _ = write!(line, " {k}={}", value(v));
    }
    if let Some(err) = e.error.as_deref() {
        let _ = write!(line, " error={}", value(err.trim()));
    }
    line
}

#[derive(Default)]
struct Collect(Entry);

impl Visit for Collect {
    fn record_str(&mut self, f: &Field, v: &str) {
        match f.name() {
            "event" => self.0.event = v.to_string(),
            "message" => self.0.message = v.to_string(),
            "error" => self.0.error = Some(v.to_string()),
            k => self.0.fields.push((k.to_string(), v.to_string())),
        }
    }

    fn record_debug(&mut self, f: &Field, v: &dyn fmt::Debug) {
        self.record_str(f, &format!("{v:?}"));
    }
}

/// Where events go: a size-capped file, each line redacted first.
pub struct Sink {
    log: Rotating,
    app: String,
    home: Option<String>,
    /// More scrubbing on top of [`redact()`]: hbench's registry of the secret values it handles.
    extra: Option<fn(&str) -> String>,
}

impl Sink {
    /// `app`: `essentials/0.5.0`. `home`: the user's home folder, written as `~`.
    pub fn new(log: Rotating, app: impl Into<String>, home: Option<String>) -> Sink {
        Sink { log, app: app.into(), home, extra: None }
    }

    pub fn with_scrubber(mut self, f: fn(&str) -> String) -> Sink {
        self.extra = Some(f);
        self
    }

    pub fn path(&self) -> &Path {
        self.log.path()
    }

    /// The line `e` becomes, redacted and cut to [`MAX_LINE`].
    pub fn line(&self, at: SystemTime, e: &Entry) -> String {
        let home = self.home.as_deref();
        // Each part before it is quoted and escaped (a `"` escaped as `\"` would hide `"password": "x"`), then the
        // whole line again.
        let clean = |s: &str| {
            let s = redact(s, home);
            self.extra.map_or(s.clone(), |f| f(&s))
        };
        let e = Entry {
            event: e.event.clone(),
            message: clean(&e.message),
            fields: e
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), clean(&format!("{k}={v}")).split_once('=').map_or_else(String::new, |(_, v)| v.to_string())))
                .collect(),
            error: e.error.as_deref().map(clean),
        };
        let mut line = redact(&format_line(at, &self.app, &e), home);
        if let Some(f) = self.extra {
            line = f(&line);
        }
        if line.len() > MAX_LINE {
            let mut end = MAX_LINE;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
        }
        line
    }

    /// Write `e` now. Best effort: a log that can't be written never stops the app.
    pub fn write(&self, e: &Entry) {
        let _ = self.log.append(&self.line(SystemTime::now(), e));
    }
}

impl Subscriber for Sink {
    fn enabled(&self, m: &Metadata<'_>) -> bool {
        m.is_event() && m.fields().field("event").is_some()
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, e: &Event<'_>) {
        let mut c = Collect::default();
        e.record(&mut c);
        if c.0.event.is_empty() {
            c.0.event = e.metadata().target().to_string();
        }
        self.write(&c.0);
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// Send this process's events to `sink`. Once per process; `false` if a subscriber was already set.
pub fn init(sink: Sink) -> bool {
    tracing::subscriber::set_global_default(sink).is_ok()
}

/// The user's data folder for `app`: `%LOCALAPPDATA%\<app>` (Windows), `~/Library/Application Support/<app>`
/// (macOS), `$XDG_DATA_HOME/<app>` or `~/.local/share/<app>` (elsewhere).
pub fn data_dir(app: &str) -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = || var("HOME").or_else(|| var("USERPROFILE"));
    let base = if cfg!(windows) {
        var("LOCALAPPDATA").or_else(|| home().map(|h| h.join("AppData").join("Local")))
    } else if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library").join("Application Support"))
    } else {
        var("XDG_DATA_HOME").or_else(|| home().map(|h| h.join(".local").join("share")))
    };
    base.map(|b| b.join(app))
}

/// The last `n` lines of a log (with its rotated files), each redacted again, as diagnostics show them.
pub fn tail_redacted(path: &Path, n: usize, home: Option<&str>) -> Vec<String> {
    tail(path, n).iter().map(|l| redact(l, home)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64, millis: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis)
    }

    #[test]
    fn times_are_rfc3339_utc() {
        assert_eq!(rfc3339(at(0, 0)), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(at(951_782_400, 5)), "2000-02-29T00:00:00.005Z");
        assert_eq!(rfc3339(at(1_795_420_502, 120)), "2026-11-23T07:55:02.120Z");
        assert_eq!(rfc3339(at(4_102_444_799, 999)), "2099-12-31T23:59:59.999Z");
    }

    #[test]
    fn a_line_has_time_app_version_os_event_and_error() {
        let e = Entry {
            event: "install.state".into(),
            message: "error actuaria/prompt-slim@0.1.0".into(),
            fields: vec![("port".into(), "47810".into()), ("folder".into(), "my jobs".into())],
            error: Some("Couldn't reach the catalogue:\ntimed out".into()),
        };
        let line = format_line(at(1_795_420_502, 120), "essentials/0.5.0", &e);
        assert_eq!(
            line,
            format!(
                "2026-11-23T07:55:02.120Z essentials/0.5.0 {} install.state error actuaria/prompt-slim@0.1.0 port=47810 folder=\"my jobs\" error=\"Couldn't reach the catalogue:\\ntimed out\"",
                platform()
            )
        );
        assert!(!line.contains('\n'));
        let bare = format_line(at(0, 0), "hbench/0.5.0", &Entry { event: "command.start".into(), ..Entry::default() });
        assert_eq!(bare, format!("1970-01-01T00:00:00.000Z hbench/0.5.0 {} command.start", platform()));
    }

    #[test]
    fn a_message_with_newlines_stays_one_line() {
        let e = Entry { event: "ui.error".into(), message: "Couldn't open\n\nthe folder\r".into(), ..Entry::default() };
        let line = format_line(at(0, 0), "a/1", &e);
        assert!(line.ends_with("ui.error Couldn't open\\n\\nthe folder"), "{line}");
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("locallog-lib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn events_are_redacted_before_they_are_written() {
        let d = temp("sink");
        let sink = Sink::new(Rotating::new(d.join("x.log")), "essentials/0.5.0", Some("/home/ann".into()));
        let path = sink.path().to_path_buf();
        tracing::subscriber::with_default(sink, || {
            tracing::error!(
                event = "update.check",
                url = "https://ann:pw12345@example.com/latest.json",
                error = "401 for sk-ant-api03-AbCdEf0123456789_xyzXYZ from ann@example.com in /home/ann/x",
                "Couldn't check for updates"
            );
            // Not an app event: never written.
            tracing::info!("a dependency's own log line");
            tracing::info!(event = "server.start", port = 47810_u16, folder = "/home/ann/Actuaria");
        });
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].contains(" update.check Couldn't check for updates url="), "{}", lines[0]);
        for leaked in ["pw12345", "sk-ant", "ann@example.com", "/home/ann"] {
            assert!(!text.contains(leaked), "{leaked} reached the log: {text}");
        }
        assert!(lines[1].ends_with("server.start port=47810 folder=~/Actuaria"), "{}", lines[1]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_scrubber_runs_after_redaction_and_lines_are_capped() {
        let d = temp("scrub");
        let sink = Sink::new(Rotating::new(d.join("x.log")), "hbench/0.5.0", None).with_scrubber(|s| s.replace("plainword", "[REDACTED]"));
        let line =
            sink.line(at(0, 0), &Entry { event: "e".into(), message: format!("plainword {}", "y ".repeat(5000)), ..Entry::default() });
        assert!(!line.contains("plainword"));
        assert!(line.len() <= MAX_LINE);
    }

    /// The review's blocking cases (#131): a named secret in JSON or quotes, in the message, a field or the error,
    /// survives neither the escaping nor the line.
    #[test]
    fn named_secrets_in_json_or_quotes_never_reach_a_line() {
        let sink = Sink::new(Rotating::new(temp("json").join("x.log")), "essentials/0.5.0", None);
        let cases = [
            (r#"bad config {"api_key": "hunter2xx", "password": "pw2"}"#, ["hunter2xx", "pw2"]),
            (r#"HTTP 401 body={"x_api_key":"short"}"#, ["short", "short"]),
            (r#"auth failed: token: "hunter2xx" rejected"#, ["hunter2xx", "hunter2xx"]),
        ];
        for (text, leaks) in cases {
            for e in [
                Entry { event: "e".into(), error: Some(text.into()), ..Entry::default() },
                Entry { event: "e".into(), message: text.into(), ..Entry::default() },
                Entry { event: "e".into(), fields: vec![("detail".into(), text.into())], ..Entry::default() },
            ] {
                let line = sink.line(at(0, 0), &e);
                for leak in leaks {
                    assert!(!line.contains(leak), "{leak} reached the line: {line}");
                }
            }
        }
        // A field named for a secret is masked whatever its value.
        let e = Entry { event: "e".into(), fields: vec![("api_key".into(), "plain value".into())], ..Entry::default() };
        assert!(!sink.line(at(0, 0), &e).contains("plain"));
    }

    #[test]
    fn diagnostics_redact_again() {
        let d = temp("tail");
        std::fs::create_dir_all(&d).unwrap();
        // A line written before this redaction existed, or by a process that skipped it.
        std::fs::write(d.join("old.log"), "token=abc123secret at /home/ann/x by ann@example.com\n").unwrap();
        let got = tail_redacted(&d.join("old.log"), 200, Some("/home/ann"));
        assert_eq!(got, vec!["token=[REDACTED] at ~/x by [email]".to_string()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_data_dir_is_the_platforms() {
        let d = data_dir("hbench").unwrap();
        assert!(d.ends_with("hbench"));
    }
}
