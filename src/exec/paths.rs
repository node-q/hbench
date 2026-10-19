//! Path helpers: `~` expansion, lexical POSIX normalization, and recursive glob matching.

use std::path::{Path, PathBuf};

pub(super) fn expanduser(s: &str) -> String {
    if (s == "~" || s.starts_with("~/"))
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{}{}", home.trim_end_matches('/'), &s[1..]);
    }
    s.to_string()
}

/// Lexical POSIX normalization: repeated `/` and `.` segments dropped, a leading `//` kept, empty -> `.`.
pub(super) fn posix_normalize(s: &str) -> String {
    if s.is_empty() {
        return ".".into();
    }
    let root = if s.starts_with("//") && !s.starts_with("///") {
        "//"
    } else if s.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    let joined = format!("{root}{}", parts.join("/"));
    if joined.is_empty() { ".".into() } else { joined }
}

/// Lexical POSIX normalization (`a/./b/../c` -> `a/c`), keeping leading `..` on relative paths and a
/// leading `//`.
pub(super) fn normpath(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let initial = if p.starts_with("//") && !p.starts_with("///") {
        2
    } else if p.starts_with('/') {
        1
    } else {
        0
    };
    let mut out: Vec<&str> = vec![];
    for c in p.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c != ".." || (initial == 0 && out.is_empty()) || out.last() == Some(&"..") {
            out.push(c);
        } else if !out.is_empty() {
            out.pop();
        }
    }
    let s = format!("{}{}", "/".repeat(initial), out.join("/"));
    if s.is_empty() { ".".into() } else { s }
}

pub(super) fn posix_join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

pub(super) fn posix_split(p: &str) -> (String, String) {
    let i = p.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (head, tail) = p.split_at(i);
    let head = if !head.is_empty() && head.chars().any(|c| c != '/') { head.trim_end_matches('/') } else { head };
    (head.to_string(), tail.to_string())
}

pub(super) fn has_magic(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// The system temp directory, normalized.
pub(super) fn temp_root() -> PathBuf {
    PathBuf::from(posix_normalize(&std::env::temp_dir().to_string_lossy()))
}

/// `p` with `~` expanded, made absolute and normalized, with symlinks resolved as far as the path exists.
pub(super) fn resolve(p: &str) -> PathBuf {
    let p = PathBuf::from(expanduser(p));
    let abs = if p.is_absolute() { p } else { std::env::current_dir().unwrap_or_default().join(p) };
    let abs = PathBuf::from(normpath(&abs.to_string_lossy()));
    if let Ok(c) = abs.canonicalize() {
        return c;
    }
    // Canonicalize the longest existing ancestor, keep the rest as-is.
    let mut tail = vec![];
    let mut cur = abs.as_path();
    while let Some(parent) = cur.parent() {
        tail.push(cur.file_name().unwrap_or_default().to_owned());
        if let Ok(c) = parent.canonicalize() {
            return tail.iter().rev().fold(c, |acc, t| acc.join(t));
        }
        cur = parent;
    }
    abs
}

/// Paths matching `pattern`, with `**` spanning directories (unsorted, hidden entries skipped unless the pattern
/// component starts with '.').
pub(super) fn glob_paths(pattern: &str) -> Vec<String> {
    let mut out = iglob(pattern, false);
    if (pattern.is_empty() || pattern.starts_with("**")) && out.first().is_some_and(String::is_empty) {
        out.remove(0);
    }
    out
}

fn iglob(pathname: &str, dironly: bool) -> Vec<String> {
    let (dirname, basename) = posix_split(pathname);
    if !has_magic(pathname) {
        let p = Path::new(pathname);
        let hit = if !basename.is_empty() { p.symlink_metadata().is_ok() } else { Path::new(&dirname).is_dir() };
        return if hit { vec![pathname.to_string()] } else { vec![] };
    }
    if dirname.is_empty() {
        return if basename == "**" { glob2("", &basename, dironly) } else { glob1("", &basename, dironly) };
    }
    let dirs = if dirname != pathname && has_magic(&dirname) { iglob(&dirname, true) } else { vec![dirname.clone()] };
    let mut out = vec![];
    for d in dirs {
        let names = if !has_magic(&basename) {
            glob0(&d, &basename)
        } else if basename == "**" {
            glob2(&d, &basename, dironly)
        } else {
            glob1(&d, &basename, dironly)
        };
        out.extend(names.into_iter().map(|n| posix_join(&d, &n)));
    }
    out
}

fn listdir(dirname: &str, dironly: bool) -> Vec<String> {
    let dir = if dirname.is_empty() { "." } else { dirname };
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    rd.flatten().filter(|e| !dironly || e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()
}

fn glob0(dirname: &str, basename: &str) -> Vec<String> {
    let hit = if !basename.is_empty() {
        Path::new(&posix_join(dirname, basename)).symlink_metadata().is_ok()
    } else {
        Path::new(dirname).is_dir()
    };
    if hit { vec![basename.to_string()] } else { vec![] }
}

fn glob1(dirname: &str, pattern: &str, dironly: bool) -> Vec<String> {
    listdir(dirname, dironly)
        .into_iter()
        .filter(|n| (pattern.starts_with('.') || !n.starts_with('.')) && crate::glob_match(pattern, n))
        .collect()
}

fn glob2(dirname: &str, _pattern: &str, dironly: bool) -> Vec<String> {
    let mut out = vec![];
    if dirname.is_empty() || Path::new(dirname).is_dir() {
        out.push(String::new());
    }
    rlistdir(dirname, dironly, &mut out, "");
    out
}

fn rlistdir(dirname: &str, dironly: bool, out: &mut Vec<String>, prefix: &str) {
    for x in listdir(dirname, dironly) {
        if x.starts_with('.') {
            continue;
        }
        let rel = if prefix.is_empty() { x.clone() } else { posix_join(prefix, &x) };
        out.push(rel.clone());
        let path = if dirname.is_empty() { x.clone() } else { posix_join(dirname, &x) };
        rlistdir(&path, dironly, out, &rel);
    }
}
