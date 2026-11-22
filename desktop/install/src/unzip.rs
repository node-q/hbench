//! A safe unzip for Orca plugin archives: at most 200 entries and 20 MB in all (counted as written, not as the
//! archive claims), and no absolute paths, drive letters, backslashes, `..`, symlinks or other special files,
//! nor names Windows can't hold (reserved device names, or a trailing dot or space).

use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};

pub const MAX_ENTRIES: usize = 200;
pub const MAX_TOTAL_BYTES: u64 = 20 * 1024 * 1024;

/// The archive path as a relative path of plain parts, or `None` if it is anything else.
fn safe_rel(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.len() > 512 || name.starts_with('/') || name.contains(['\\', ':', '\0']) {
        return None;
    }
    let p = Path::new(name.trim_end_matches('/'));
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(s) if s != "." && s != ".." && !windows_unsafe(&s.to_string_lossy()) => out.push(s),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// A path part Windows reserves or silently alters: `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`
/// (any case, with or without an extension), or anything ending in a dot or a space.
fn windows_unsafe(part: &str) -> bool {
    if part.ends_with(['.', ' ']) {
        return true;
    }
    let stem = part.split('.').next().unwrap_or(part).trim_end_matches(' ').to_ascii_uppercase();
    match stem.as_bytes() {
        b"CON" | b"PRN" | b"AUX" | b"NUL" => true,
        [b'C', b'O', b'M', d] | [b'L', b'P', b'T', d] => (b'1'..=b'9').contains(d),
        _ => false,
    }
}

/// Extract `zip` into `dest` (an existing, empty folder).
pub fn extract(zip: &[u8], dest: &Path) -> Result<(), String> {
    let mut ar = zip::ZipArchive::new(Cursor::new(zip)).map_err(|e| format!("not a valid zip: {e}"))?;
    if ar.len() > MAX_ENTRIES {
        return Err(format!("the archive has {} entries; at most {MAX_ENTRIES} are allowed", ar.len()));
    }
    let mut total: u64 = 0;
    for i in 0..ar.len() {
        let mut f = ar.by_index(i).map_err(|e| e.to_string())?;
        let name = f.name().map_err(|e| e.to_string())?.to_string();
        let rel = safe_rel(&name).ok_or_else(|| format!("refusing the archive: unsafe path {name:?}"))?;
        // Unix mode: only regular files and folders (no symlinks, devices or fifos).
        if let Some(mode) = f.unix_mode() {
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                return Err(format!("refusing the archive: {name:?} is not a plain file"));
            }
        }
        if f.is_symlink() {
            return Err(format!("refusing the archive: {name:?} is a symlink"));
        }
        let out = dest.join(&rel);
        if f.is_dir() {
            fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut w = fs::OpenOptions::new().write(true).create_new(true).open(&out).map_err(|e| format!("{name:?}: {e}"))?;
        let left = MAX_TOTAL_BYTES - total;
        let mut buf = Vec::new();
        (&mut f).take(left + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
        if buf.len() as u64 > left {
            return Err(format!("refusing the archive: over {MAX_TOTAL_BYTES} bytes unpacked"));
        }
        total += buf.len() as u64;
        w.write_all(&buf).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The plugin's folder in an extracted archive: `dest` itself if it has `orca-plugin.json`, or its one subfolder
/// that has it.
pub fn plugin_root(dest: &Path) -> Option<PathBuf> {
    const MANIFEST: &str = "orca-plugin.json";
    if dest.join(MANIFEST).is_file() {
        return Some(dest.to_path_buf());
    }
    let entries: Vec<_> = fs::read_dir(dest).ok()?.filter_map(|e| e.ok()).collect();
    match &entries[..] {
        [one] if one.file_type().ok()?.is_dir() && one.path().join(MANIFEST).is_file() => Some(one.path()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    fn build(entries: &[(&str, &[u8])], symlink: Option<(&str, &str)>) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let o = SimpleFileOptions::default();
        for (n, b) in entries {
            w.start_file(*n, o).unwrap();
            w.write_all(b).unwrap();
        }
        if let Some((n, t)) = symlink {
            w.add_symlink(n, t, o).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ai-unzip-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn extracts_a_plugin() {
        let d = dir("ok");
        let z = build(&[("p/orca-plugin.json", b"{}"), ("p/src/a.js", b"x")], None);
        extract(&z, &d).unwrap();
        assert_eq!(plugin_root(&d), Some(d.join("p")));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn refuses_traversal_and_specials() {
        for name in ["../evil", "a/../../evil", "/etc/evil", "C:/evil", "a\\..\\evil", "..", "./../x"] {
            let d = dir("trav");
            let z = build(&[("ok.txt", b"x"), (name, b"x")], None);
            assert!(extract(&z, &d).is_err(), "{name}");
            assert!(!d.parent().unwrap().join("evil").exists());
            fs::remove_dir_all(&d).unwrap();
        }
        let d = dir("link");
        assert!(extract(&build(&[], Some(("l", "/etc/passwd"))), &d).unwrap_err().contains("plain file"));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn refuses_windows_reserved_and_trailing_dot_or_space() {
        let bad = [
            "CON",
            "con",
            "Prn",
            "aux.txt",
            "NUL.tar.gz",
            "com1",
            "COM9.js",
            "lpt1",
            "LPT9.md",
            "a/con/b.txt",
            "nul /x",
            "end.",
            "end ",
            "dir./x",
            "dir /x",
            "con .txt",
        ];
        for name in bad {
            let d = dir("win");
            assert!(extract(&build(&[(name, b"x")], None), &d).is_err(), "{name}");
            fs::remove_dir_all(&d).unwrap();
        }
        for name in ["console.txt", "com0", "com10", "lpt", "auxiliary/x", "a.con", "nul-x", ".hidden"] {
            assert!(safe_rel(name).is_some(), "{name}");
        }
    }

    #[test]
    fn refuses_too_many_or_too_big() {
        let d = dir("many");
        let names: Vec<String> = (0..=MAX_ENTRIES).map(|i| format!("f{i}")).collect();
        let entries: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
        assert!(extract(&build(&entries, None), &d).unwrap_err().contains("entries"));
        fs::remove_dir_all(&d).unwrap();
        let d = dir("big");
        let big = vec![0u8; (MAX_TOTAL_BYTES / 2 + 1) as usize];
        assert!(extract(&build(&[("a", &big), ("b", &big)], None), &d).unwrap_err().contains("unpacked"));
        fs::remove_dir_all(&d).unwrap();
    }
}
