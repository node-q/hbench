//! Registering the `actuaria:` link handler on Windows and Linux as `<app> -- <link>`, so a link can never be
//! read as an option. (macOS hands links over as Apple events, from the bundle's Info.plist.)

/// The Windows `shell\open\command` value: `"<exe>" -- "%1"`.
pub fn windows_command(exe: &str) -> Result<String, String> {
    if exe.contains('"') {
        return Err("the app's path has a quote".into());
    }
    Ok(format!("\"{exe}\" -- \"%1\""))
}

/// `reg.exe` argument lists that register the handler for the current user.
pub fn windows_reg_args(exe: &str) -> Result<Vec<Vec<String>>, String> {
    let cmd = windows_command(exe)?;
    let key = r"HKCU\Software\Classes\actuaria";
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    Ok(vec![
        s(&["add", key, "/ve", "/d", "URL:Actuaria install link", "/f"]),
        s(&["add", key, "/v", "URL Protocol", "/d", "", "/f"]),
        s(&["add", &format!(r"{key}\shell\open\command"), "/ve", "/d", &cmd, "/f"]),
    ])
}

pub const LINUX_DESKTOP_FILE: &str = "io.actuaria.studio.install-link.desktop";

/// The `.desktop` file that handles `x-scheme-handler/actuaria` with `Exec="<exe>" -- %u`.
pub fn linux_desktop_entry(exe: &str) -> Result<String, String> {
    if exe.chars().any(|c| c.is_control()) || !exe.starts_with('/') {
        return Err("the app's path can't be used".into());
    }
    // Desktop Entry quoting: inside double quotes, escape " ` $ \ with a backslash; then double each backslash
    // for the string value, and `%` as `%%`.
    let mut q = String::new();
    for c in exe.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                q.push('\\');
                q.push(c)
            }
            '%' => q.push_str("%%"),
            c => q.push(c),
        }
    }
    let q = q.replace('\\', "\\\\");
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=Actuaria Studio Essentials install link\nExec=\"{q}\" -- %u\nNoDisplay=true\nTerminal=false\nMimeType=x-scheme-handler/actuaria;\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separator_before_the_link() {
        assert_eq!(
            windows_command(r"C:\Program Files\Actuaria\Actuaria.exe").unwrap(),
            r#""C:\Program Files\Actuaria\Actuaria.exe" -- "%1""#
        );
        assert!(windows_command("C:\\a\"b").is_err());
        assert_eq!(windows_reg_args("C:\\A.exe").unwrap()[2][4], r#""C:\A.exe" -- "%1""#);
        let e = linux_desktop_entry("/opt/Actuaria Studio/actuaria").unwrap();
        assert!(e.contains("Exec=\"/opt/Actuaria Studio/actuaria\" -- %u\n"), "{e}");
        let e = linux_desktop_entry("/a/$x`y\"%z").unwrap();
        assert!(e.contains(r#"Exec="/a/\\$x\\`y\\"%%z" -- %u"#), "{e}");
        assert!(linux_desktop_entry("relative").is_err());
        assert!(linux_desktop_entry("/a\nExec=evil").is_err());
    }
}
