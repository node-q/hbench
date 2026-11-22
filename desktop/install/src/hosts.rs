//! Finding the hosts and calling their own installers. Essentials never approves anything for the user: Claude
//! Desktop shows its own install dialog for a `.mcpb`, and Orca asks the user to approve the plugin's access.

use crate::link::Target;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const CLAUDE_DOWNLOAD_URL: &str = "https://claude.ai/download";
pub const MAX_OUTPUT_CHARS: usize = 4000;

/// Orca's command. On Linux it is `orca-ide`: `orca` there is GNOME's screen reader, which is never run.
const ORCA_EXE: &str = if cfg!(windows) {
    "orca.exe"
} else if cfg!(target_os = "linux") {
    "orca-ide"
} else {
    "orca"
};

/// Whether `p` can be Orca's command. On Linux that is a script; the app's own binary has the same name, and would
/// open a window rather than install.
fn is_orca_cli(p: &Path) -> bool {
    if !cfg!(target_os = "linux") {
        return p.is_file();
    }
    let mut head = [0; 2];
    p.is_file() && std::fs::File::open(p).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"#!"
}

/// Orca's command on PATH (absolute entries only, so never the current folder), then the usual install locations.
pub fn find_orca(path_var: Option<OsString>, home: Option<&Path>) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = path_var.map(|p| std::env::split_paths(&p).filter(|d| d.is_absolute()).collect()).unwrap_or_default();
    if let Some(h) = home {
        dirs.extend([h.join(".local/bin"), h.join(".orca/bin"), h.join("AppData/Local/Programs/orca")]);
    }
    dirs.extend(["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin"].map(PathBuf::from));
    dirs.into_iter().map(|d| d.join(ORCA_EXE)).find(|p| is_orca_cli(p))
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

/// `<claude> <mcpb>`, run directly: the path is one argument, spaces included.
pub fn claude_command(claude: &Path, mcpb: &Path) -> Command {
    let mut c = Command::new(claude);
    c.arg(mcpb).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    c
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

/// Variables that would send the command to another computer's Orca.
const ORCA_REMOTE_VARS: [&str; 3] = ["ORCA_ENVIRONMENT", "ORCA_PAIRING_CODE", "ORCA_REMOTE_PAIRING"];

/// `orca plugin install <dir> --environment local`: this computer's Orca, run directly and never through a shell.
/// `dir` must be absolute, so it can never be read as an option.
pub fn orca_install_command(orca: &Path, dir: &Path) -> Result<Command, String> {
    if !orca.is_absolute() || !dir.is_absolute() {
        return Err("orca and the plugin folder must be absolute paths".into());
    }
    let mut c = Command::new(orca);
    c.args(["plugin", "install"]).arg(dir).args(["--environment", "local"]).stdin(Stdio::null());
    for v in ORCA_REMOTE_VARS {
        c.env_remove(v);
    }
    Ok(c)
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
    let out = orca_install_command(orca, dir)?.output().map_err(|e| format!("couldn't run orca: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let text: String = text.chars().filter(|c| *c == '\n' || !c.is_control()).take(MAX_OUTPUT_CHARS).collect();
    Ok((OrcaResult::from_code(out.status.code()), text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn argv_has_no_shell() {
        let orca = Path::new(if cfg!(windows) { "C:\\o\\orca.exe" } else { "/opt/orca/orca" });
        let dir = std::env::temp_dir().join("x; rm -rf ~ $(id) `id` | && \"'");
        let c = orca_install_command(orca, &dir).unwrap();
        assert_eq!(c.get_program(), orca);
        let args: Vec<OsString> = c.get_args().map(|a| a.to_owned()).collect();
        assert_eq!(args, vec![OsString::from("plugin"), "install".into(), dir.clone().into(), "--environment".into(), "local".into()]);
        let envs: Vec<_> = c.get_envs().collect();
        for v in ["ORCA_ENVIRONMENT", "ORCA_PAIRING_CODE", "ORCA_REMOTE_PAIRING"] {
            assert!(envs.contains(&(OsStr::new(v), None)), "{v}");
        }
        assert_eq!(envs.len(), 3);
        assert!(orca_install_command(Path::new("orca"), &dir).is_err());
        assert!(orca_install_command(orca, Path::new("--evil")).is_err());
    }

    #[test]
    fn claude_gets_the_path_as_one_argument() {
        let mcpb = Path::new("/Users/Ann Other/data dir/prompt-slim-0.1.0.mcpb");
        let c = claude_command(Path::new("/x/claude.exe"), mcpb);
        assert_eq!(c.get_program(), "/x/claude.exe");
        assert_eq!(c.get_args().collect::<Vec<_>>(), [mcpb.as_os_str()]);
        assert!(claude_command(Path::new("/no/such/claude.exe"), mcpb).spawn().is_err());
    }

    #[cfg(unix)]
    fn script(p: &Path, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(p, text).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_gnome_screen_reader_is_not_orca() {
        let d = std::env::temp_dir().join(format!("ai-gnome-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let found = || find_orca(Some(d.clone().into_os_string()), None).filter(|p| p.starts_with(&d));
        script(&d.join("orca"), "#!/usr/bin/python3\n");
        assert_eq!(found(), None);
        // Nor is Orca's window binary, which is not a script.
        script(&d.join("orca-ide"), "\x7fELF");
        assert_eq!(found(), None);
        script(&d.join("orca-ide"), "#!/usr/bin/env bash\n");
        assert_eq!(found(), Some(d.join("orca-ide")));
        std::fs::remove_dir_all(&d).unwrap();
        for dir in ["/usr/bin", "/usr/local/bin", "/bin"] {
            assert_ne!(find_orca(Some(dir.into()), None), Some(Path::new(dir).join("orca")));
        }
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
        let d = std::env::temp_dir().join(format!("ai-orca-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        // A fake orca that prints its arguments one per line.
        let orca = d.join(ORCA_EXE);
        script(&orca, "#!/bin/sh\nfor a in \"$@\"; do echo \"[$a]\"; done\n");
        let plugin = d.join("p $(touch pwned); x");
        let (r, out) = run_orca_install(&orca, &plugin).unwrap();
        assert_eq!(r, OrcaResult::Installed);
        assert_eq!(out, format!("[plugin]\n[install]\n[{}]\n[--environment]\n[local]\n", plugin.display()));
        assert!(!Path::new("pwned").exists() && !d.join("pwned").exists());
        assert_eq!(find_orca(Some(d.clone().into_os_string()), None), Some(orca.clone()));
        assert_eq!(find_orca(Some("relative/dir".into()), Some(&d)).filter(|p| p.starts_with(&d)), None);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
