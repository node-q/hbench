//! `--observe`: every trial's log lines and command output on stderr as they happen, each line under the
//! trial's name, redacted like trial.log.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static ON: AtomicBool = AtomicBool::new(false);

/// A line longer than this is printed in pieces.
const LONGEST_LINE: usize = 16 * 1024;

pub fn enable() {
    ON.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

pub fn print(who: &str, text: &str) {
    let prefix = crate::job::paint_if(crate::job::use_color_on_stderr(), "2", &format!("{who} |"));
    let mut err = std::io::stderr().lock();
    for line in crate::redact::text(text).lines() {
        let _ = writeln!(err, "{prefix} {line}");
    }
}

/// A command as it starts: its first line, since hbench's own helper scripts run to dozens.
pub fn command(who: &str, label: &str) {
    let mut lines = label.lines();
    let first = lines.next().unwrap_or_default();
    match lines.count() {
        0 => print(who, &format!("$ {first}")),
        more => print(who, &format!("$ {first} … ({more} more lines in trial.log)")),
    }
}

/// A command's exit status, when it failed.
pub fn exit(who: &str, code: i32) {
    if code != 0 {
        print(who, &format!("exit {code}"));
    }
}

/// A command's output as it arrives, printed a whole line at a time.
pub struct Lines {
    who: String,
    partial: Vec<u8>,
}

impl Lines {
    pub fn new(who: &str) -> Lines {
        Lines { who: who.to_string(), partial: Vec::new() }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for line in self.split(bytes) {
            print(&self.who, &line);
        }
    }

    pub fn finish(&mut self) {
        if !self.partial.is_empty() {
            print(&self.who, &String::from_utf8_lossy(&std::mem::take(&mut self.partial)));
        }
    }

    fn split(&mut self, bytes: &[u8]) -> Vec<String> {
        self.partial.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(end) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            out.push(String::from_utf8_lossy(&line[..end]).trim_end_matches('\r').to_string());
        }
        while self.partial.len() > LONGEST_LINE {
            let piece: Vec<u8> = self.partial.drain(..LONGEST_LINE).collect();
            out.push(String::from_utf8_lossy(&piece).into_owned());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_across_reads_come_out_whole() {
        let mut l = Lines::new("t");
        assert_eq!(l.split(b"first li"), Vec::<String>::new());
        assert_eq!(l.split(b"ne\r\nsecond\nthi"), vec!["first line", "second"]);
        assert_eq!(l.split(b"rd"), Vec::<String>::new());
        assert_eq!(l.partial, b"third");
    }

    #[test]
    fn a_line_without_an_end_is_printed_in_pieces() {
        let mut l = Lines::new("t");
        let out = l.split(&vec![b'x'; LONGEST_LINE * 2 + 5]);
        assert_eq!(out.len(), 2);
        assert_eq!(l.partial.len(), 5);
    }
}
