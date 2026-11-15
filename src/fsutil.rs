//! Small filesystem helpers shared across modules.

use serde_json::Value;
use std::path::Path;

/// A JSON file's value; None when it is missing or not valid JSON.
pub fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// The user's home directory: `HOME`, else Windows's `USERPROFILE`.
pub fn home_dir() -> Option<std::path::PathBuf> {
    ["HOME", "USERPROFILE"].iter().find_map(|k| std::env::var_os(k).filter(|v| !v.is_empty())).map(Into::into)
}

/// Set a file's Unix permission bits. Windows files have none, so there this does nothing.
pub fn set_mode(p: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (p, mode);
        Ok(())
    }
}

/// Whether a regular file is executable: any execute bit on Unix. Windows has no such bit, so there a file
/// counts as executable when it starts with `#!`, which is what makes a script runnable inside a container.
pub fn is_executable(p: &Path, meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = p;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        use std::io::Read;
        let _ = meta;
        let mut head = [0u8; 2];
        std::fs::File::open(p).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"#!"
    }
}

/// Add the execute bits to a file (nothing to add on Windows).
pub fn make_executable(p: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(p)?.permissions().mode();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode | 0o111))
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        Ok(())
    }
}

/// Create a symlink at `link` pointing to `target`. Windows needs to know whether the target is a directory,
/// and lets an unprivileged user create symlinks only in Developer Mode.
pub fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        let resolved = link.parent().unwrap_or(Path::new(".")).join(target);
        let made = if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        };
        made.map_err(|e| {
            std::io::Error::new(e.kind(), format!("{e} (creating a symlink on Windows needs Developer Mode or an elevated shell)"))
        })
    }
}

/// A relative path with `/` separators, whatever the host uses: the form archives, digests and containers take.
pub fn slash_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' { s.into_owned() } else { s.replace(std::path::MAIN_SEPARATOR, "/") }
}

/// The `tar` to run on this host. Windows ships bsdtar in System32; name it outright, since a GNU tar earlier
/// on PATH (Git's) would read the `C:` of a path as a remote host.
pub fn tar_program() -> std::ffi::OsString {
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        let bundled = std::path::Path::new(&root).join("System32").join("tar.exe");
        if bundled.is_file() {
            return bundled.into();
        }
    }
    "tar".into()
}
