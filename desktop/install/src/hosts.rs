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

/// Whether Claude Desktop looks installed. Linux has no Claude Desktop build.
pub fn claude_present(home: Option<&Path>) -> bool {
    let mut c: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        c.push("/Applications/Claude.app".into());
        if let Some(h) = home {
            c.push(h.join("Applications/Claude.app"));
        }
    } else if cfg!(windows)
        && let Some(l) = std::env::var_os("LOCALAPPDATA")
    {
        c.push(Path::new(&l).join("AnthropicClaude"));
    }
    c.iter().any(|p| p.exists())
}

/// The hosts present, of `wanted`.
pub fn present(wanted: impl IntoIterator<Item = Target>, home: Option<&Path>) -> Vec<Target> {
    wanted
        .into_iter()
        .filter(|t| match t {
            Target::Claude => claude_present(home),
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

/// Run `orca plugin install <dir>`; its output (stdout then stderr, capped), and whether it succeeded.
pub fn run_orca_install(orca: &Path, dir: &Path) -> Result<(bool, String), String> {
    let (prog, args) = orca_install_argv(orca, dir)?;
    let out = Command::new(prog).args(args).stdin(Stdio::null()).output().map_err(|e| format!("couldn't run orca: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let text: String = text.chars().filter(|c| *c == '\n' || !c.is_control()).take(MAX_OUTPUT_CHARS).collect();
    Ok((out.status.success(), text))
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
        let (ok, out) = run_orca_install(&orca, &plugin).unwrap();
        assert!(ok);
        assert_eq!(out, format!("[plugin]\n[install]\n[{}]\n", plugin.display()));
        assert!(!Path::new("pwned").exists() && !d.join("pwned").exists());
        assert_eq!(find_orca(Some(d.clone().into_os_string()), None), Some(orca.clone()));
        assert_eq!(find_orca(Some("relative/dir".into()), Some(&d)).filter(|p| p.starts_with(&d)), None);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
