//! Finding the hosts and calling their own installers. Essentials never approves anything for the user: Claude
//! Desktop shows its own install dialog for a `.mcpb`, and Orca asks the user to approve the plugin's access.

use crate::link::Target;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const CLAUDE_DOWNLOAD_URL: &str = "https://claude.ai/download";
pub const MAX_OUTPUT_CHARS: usize = 4000;

#[cfg(windows)]
const ORCA_EXE: &str = "orca.exe";
#[cfg(not(windows))]
const ORCA_EXE: &str = "orca";

/// `orca` on PATH (absolute entries only, so never the current folder), then the usual install locations.
pub fn find_orca(path_var: Option<OsString>, home: Option<&Path>) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = path_var.map(|p| std::env::split_paths(&p).filter(|d| d.is_absolute()).collect()).unwrap_or_default();
    if let Some(h) = home {
        dirs.extend([h.join(".local/bin"), h.join(".orca/bin"), h.join("AppData/Local/Programs/orca")]);
    }
    dirs.extend(["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin"].map(PathBuf::from));
    dirs.into_iter().map(|d| d.join(ORCA_EXE)).find(|p| p.is_file())
}

/// Claude Desktop itself: the app on macOS, the exe its installer puts in `%LOCALAPPDATA%` on Windows. Also what a
/// `.mcpb` is opened with, so it goes to Claude and not to whatever else claims the extension. Linux has none.
pub fn find_claude(home: Option<&Path>) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        let mut c = vec![PathBuf::from("/Applications/Claude.app")];
        c.extend(home.map(|h| h.join("Applications/Claude.app")));
        return c.into_iter().find(|p| p.is_dir());
    }
    if cfg!(windows) {
        let exe = Path::new(&std::env::var_os("LOCALAPPDATA")?).join("AnthropicClaude").join("claude.exe");
        return (exe.is_absolute() && exe.is_file()).then_some(exe);
    }
    None
}

/// The hosts present, of `wanted`.
pub fn present(wanted: impl IntoIterator<Item = Target>, home: Option<&Path>) -> Vec<Target> {
    wanted
        .into_iter()
        .filter(|t| match t {
            Target::Claude => find_claude(home).is_some(),
            Target::Orca => find_orca(std::env::var_os("PATH"), home).is_some(),
        })
        .collect()
}

/// `orca plugin install <dir>` as a program and its arguments: run directly, never through a shell. `dir` must
/// be absolute, so it can never be read as an option.
pub fn orca_install_argv(orca: &Path, dir: &Path) -> Result<(PathBuf, Vec<OsString>), String> {
    if !orca.is_absolute() || !dir.is_absolute() {
        return Err("orca and the plugin folder must be absolute paths".into());
    }
    Ok((orca.to_path_buf(), vec!["plugin".into(), "install".into(), dir.as_os_str().to_owned()]))
}

/// How `orca plugin install` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrcaResult {
    Installed,
    /// Exit 3: the plugin asks for access, so Orca wrote nothing; its output has the command that approves it,
    /// which the user runs (on the same folder).
    NeedsApproval,
    Failed,
}

impl OrcaResult {
    fn from_code(code: Option<i32>) -> Self {
        match code {
            Some(0) => OrcaResult::Installed,
            Some(3) => OrcaResult::NeedsApproval,
            _ => OrcaResult::Failed,
        }
    }
}

/// Run `orca plugin install <dir>`; how it ended, and its output (stdout then stderr, capped).
pub fn run_orca_install(orca: &Path, dir: &Path) -> Result<(OrcaResult, String), String> {
    let (prog, args) = orca_install_argv(orca, dir)?;
    let out = Command::new(prog).args(args).stdin(Stdio::null()).output().map_err(|e| format!("couldn't run orca: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let text: String = text.chars().filter(|c| *c == '\n' || !c.is_control()).take(MAX_OUTPUT_CHARS).collect();
    Ok((OrcaResult::from_code(out.status.code()), text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_has_no_shell() {
        let orca = Path::new(if cfg!(windows) { "C:\\o\\orca.exe" } else { "/opt/orca/orca" });
        let dir = std::env::temp_dir().join("x; rm -rf ~ $(id) `id` | && \"'");
        let (prog, args) = orca_install_argv(orca, &dir).unwrap();
        assert_eq!(prog, orca);
        assert_eq!(args, vec![OsString::from("plugin"), "install".into(), dir.clone().into_os_string()]);
        assert!(orca_install_argv(Path::new("orca"), &dir).is_err());
        assert!(orca_install_argv(orca, Path::new("--evil")).is_err());
    }

    #[test]
    fn orca_exit_codes() {
        assert_eq!(OrcaResult::from_code(Some(0)), OrcaResult::Installed);
        assert_eq!(OrcaResult::from_code(Some(3)), OrcaResult::NeedsApproval);
        for c in [Some(1), Some(2), None] {
            assert_eq!(OrcaResult::from_code(c), OrcaResult::Failed);
        }
    }

    #[cfg(unix)]
    #[test]
    fn runs_without_a_shell() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("ai-orca-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        // A fake orca that prints its arguments one per line.
        let orca = d.join("orca");
        std::fs::write(&orca, "#!/bin/sh\nfor a in \"$@\"; do echo \"[$a]\"; done\n").unwrap();
        std::fs::set_permissions(&orca, std::fs::Permissions::from_mode(0o755)).unwrap();
        let plugin = d.join("p $(touch pwned); x");
        let (r, out) = run_orca_install(&orca, &plugin).unwrap();
        assert_eq!(r, OrcaResult::Installed);
        assert_eq!(out, format!("[plugin]\n[install]\n[{}]\n", plugin.display()));
        assert!(!Path::new("pwned").exists() && !d.join("pwned").exists());
        assert_eq!(find_orca(Some(d.clone().into_os_string()), None), Some(orca.clone()));
        assert_eq!(find_orca(Some("relative/dir".into()), Some(&d)).filter(|p| p.starts_with(&d)), None);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
