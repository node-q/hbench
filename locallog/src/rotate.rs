//! A log file capped by size: `name.log` takes lines until the next one would pass the cap, then becomes
//! `name.log.1` (the older ones move up, the oldest goes) and a new `name.log` starts. At most `files` files of
//! at most `max_bytes` each, so never more than `files × max_bytes` on disk.
//!
//! tracing-appender rotates by time only, hence this. Each line is appended through a fresh open, so other
//! processes writing the same file (runs the viewer starts) see its current size; two of them rotating at the
//! same moment can lose a file's worth of lines, never pass the cap.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const MAX_BYTES: u64 = 1024 * 1024;
pub const FILES: usize = 5;

pub struct Rotating {
    path: PathBuf,
    max_bytes: u64,
    files: usize,
    lock: Mutex<()>,
}

impl Rotating {
    /// 5 files of 1 MB.
    pub fn new(path: impl Into<PathBuf>) -> Rotating {
        Rotating::with_cap(path, MAX_BYTES, FILES)
    }

    pub fn with_cap(path: impl Into<PathBuf>, max_bytes: u64, files: usize) -> Rotating {
        Rotating { path: path.into(), max_bytes: max_bytes.max(2), files: files.max(1), lock: Mutex::new(()) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn numbered(&self, n: usize) -> PathBuf {
        numbered(&self.path, n)
    }

    /// Append one line (a newline is added; a line too long for a file is cut).
    pub fn append(&self, line: &str) -> io::Result<()> {
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let line = cut(line, (self.max_bytes - 1) as usize);
        let size = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if size > 0 && size + line.len() as u64 + 1 > self.max_bytes {
            self.rotate_locked()?;
        }
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut f = OpenOptions::new().create(true).append(true).open(&self.path)?;
        f.write_all(format!("{line}\n").as_bytes())
    }

    /// Start a new file now (the current one becomes `.1`).
    pub fn rotate(&self) -> io::Result<()> {
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.rotate_locked()
    }

    fn rotate_locked(&self) -> io::Result<()> {
        if self.files == 1 {
            return match fs::remove_file(&self.path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let _ = fs::remove_file(self.numbered(self.files - 1));
        for n in (1..self.files - 1).rev() {
            let from = self.numbered(n);
            if from.exists() {
                fs::rename(&from, self.numbered(n + 1))?;
            }
        }
        match fs::rename(&self.path, self.numbered(1)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// The files there are, oldest first.
    pub fn files(&self) -> Vec<PathBuf> {
        files(&self.path, self.files)
    }
}

fn numbered(path: &Path, n: usize) -> PathBuf {
    if n == 0 {
        return path.to_path_buf();
    }
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

/// `path` and its rotated files that exist, oldest first.
pub fn files(path: &Path, max: usize) -> Vec<PathBuf> {
    (0..max.max(1)).rev().map(|n| numbered(path, n)).filter(|p| p.is_file()).collect()
}

/// At most `max` bytes of `s`, on a character boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The last `n` lines of a log and its rotated files.
pub fn tail(path: &Path, n: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in files(path, FILES).iter().rev() {
        if out.len() >= n {
            break;
        }
        let Ok(bytes) = fs::read(f) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        let take = lines.len().min(n - out.len());
        let mut newer = std::mem::take(&mut out);
        out = lines.split_off(lines.len() - take);
        out.append(&mut newer);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("locallog-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_files_never_pass_the_cap() {
        let d = dir("cap");
        let log = Rotating::with_cap(d.join("a.log"), 200, 5);
        for i in 0..1000 {
            log.append(&format!("line {i} {}", "x".repeat(i % 50))).unwrap();
        }
        let files = log.files();
        assert_eq!(files.len(), 5);
        let mut total = 0;
        for f in &files {
            let len = fs::metadata(f).unwrap().len();
            assert!(len <= 200, "{} is {len} bytes", f.display());
            total += len;
        }
        assert!(total <= 5 * 200);
        assert_eq!(fs::read_dir(&d).unwrap().count(), 5, "no sixth file");
        // The newest line is last in the current file, and lines stay whole.
        let last = tail(&d.join("a.log"), 1);
        assert_eq!(last, vec![format!("line 999 {}", "x".repeat(999 % 50))]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_line_longer_than_a_file_is_cut_to_fit() {
        let d = dir("long");
        let log = Rotating::with_cap(d.join("a.log"), 64, 2);
        log.append(&"é".repeat(100)).unwrap();
        log.append("next").unwrap();
        for f in log.files() {
            assert!(fs::metadata(&f).unwrap().len() <= 64);
        }
        assert_eq!(tail(&d.join("a.log"), 5).last().map(String::as_str), Some("next"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn tail_reads_across_rotated_files_in_order() {
        let d = dir("tail");
        let log = Rotating::with_cap(d.join("a.log"), 30, 5);
        for i in 0..20 {
            log.append(&format!("line {i:02}")).unwrap();
        }
        let got = tail(&d.join("a.log"), 7);
        let want: Vec<String> = (13..20).map(|i| format!("line {i:02}")).collect();
        assert_eq!(got, want);
        assert!(tail(&d.join("missing.log"), 5).is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn rotate_starts_a_new_file() {
        let d = dir("rotate");
        let log = Rotating::with_cap(d.join("a.log"), 1000, 3);
        log.append("old").unwrap();
        log.rotate().unwrap();
        log.append("new").unwrap();
        assert_eq!(fs::read_to_string(d.join("a.log")).unwrap(), "new\n");
        assert_eq!(fs::read_to_string(d.join("a.log.1")).unwrap(), "old\n");
        let _ = fs::remove_dir_all(&d);
    }
}
