//! The one redaction every local log line goes through, before it is written and again when diagnostics are
//! copied or saved: keys and tokens, cookies and auth headers, credentials in URLs, emails, long secret-looking
//! strings, and the home folder (as `~`).

use regex::{Captures, Regex};
use std::sync::OnceLock;

pub const MASK: &str = "[REDACTED]";
pub const EMAIL: &str = "[email]";

struct Rules {
    home_generic: Regex,
    cookie: Regex,
    authorization: Regex,
    bearer: Regex,
    url_userinfo: Regex,
    prefixed: Regex,
    jwt: Regex,
    key_value: Regex,
    email: Regex,
    hex: Regex,
    opaque: Regex,
}

fn rules() -> &'static Rules {
    static R: OnceLock<Rules> = OnceLock::new();
    R.get_or_init(|| {
        let re = |s: &str| Regex::new(s).expect("a valid redaction pattern");
        Rules {
            // Another account's home, or this one's when the caller doesn't know it: the user's name goes.
            home_generic: re(r#"(?i)(?:^|(?<lead>[\s"'=(:,\[]))(?:/home/|/Users/|[a-z]:\\{1,2}Users\\{1,2}|[a-z]:/Users/)[^/\\\s"']+"#),
            cookie: re(r"(?i)\b((?:set-)?cookie)(\s*[:=]\s*).*"),
            authorization: re(r#"(?i)\b((?:proxy-)?authorization)(["']?\s*[:=]\s*["']?)[^"'\r\n]*"#),
            bearer: re(r"(?i)\b(bearer)\s+[A-Za-z0-9._~+/=\-]{6,}"),
            url_userinfo: re(r"(?i)\b([a-z][a-z0-9+.\-]*://)[^/\s:@]+:[^/\s@]+@"),
            // Keys with a known shape: OpenAI/Anthropic (sk-, sk-ant-, sk-proj-), GitHub, GitLab, Slack, AWS,
            // Google, Hugging Face, Stripe.
            prefixed: re(concat!(
                r"\b(?:(?:sk|pk|rk)-[A-Za-z0-9_\-]{12,}",
                r"|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}",
                r"|glpat-[A-Za-z0-9_\-]{16,}",
                r"|xox[abprse]-[A-Za-z0-9\-]{10,}",
                r"|(?:AKIA|ASIA)[0-9A-Z]{16}",
                r"|AIza[0-9A-Za-z_\-]{30,}",
                r"|hf_[A-Za-z0-9]{20,}",
                r"|(?:sk|rk|pk)_(?:live|test)_[A-Za-z0-9]{12,})"
            )),
            jwt: re(r"\beyJ[A-Za-z0-9_\-]{6,}\.[A-Za-z0-9_\-]{6,}\.[A-Za-z0-9_\-]*"),
            // `api_key=…`, `"token": "…"`, `ANTHROPIC_API_KEY=…`, `?access_token=…`, `password: …`.
            key_value: re(concat!(
                r#"(?i)\b([A-Za-z0-9_.\-]*(?:api[_\-]?key|secret|token|passw(?:or)?d|pwd|credential|session[_\-]?id|private[_\-]?key|access[_\-]?key)[A-Za-z0-9_\-]*)"#,
                r#"(["']?\s*[:=]\s*["']?)([^\s"',;&}\]]+)"#
            )),
            email: re(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9\-]+(?:\.[A-Za-z0-9\-]+)*\.[A-Za-z]{2,}\b"),
            hex: re(r"\b(sha(?:1|224|256|384|512):)?([0-9a-fA-F]{32,})\b"),
            opaque: re(r"[A-Za-z0-9+_\-]{32,}={0,2}"),
        }
    })
}

/// `home` as it may appear in a line: as is, with `\` doubled (JSON), and with `/` for `\` (Windows).
fn home_forms(home: &str) -> Vec<String> {
    let home = home.trim_end_matches(['/', '\\']);
    if home.len() < 2 {
        return vec![];
    }
    let mut forms = vec![home.replace('\\', "\\\\"), home.to_string(), home.replace('\\', "/")];
    forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
    forms.dedup();
    forms
}

/// `s` with the folder `dir` written as `~`, where `dir` is a whole path component: not `/home/ann` in `/home/anna`.
fn replace_folder(s: &str, dir: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(dir) {
        let after = &rest[i + dir.len()..];
        let whole = after.chars().next().is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')));
        out.push_str(&rest[..i]);
        out.push_str(if whole { "~" } else { dir });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Whether a long run of letters and digits looks like a secret rather than a word, a name or a target triple:
/// upper and lower case letters and digits, all three.
fn looks_random(s: &str) -> bool {
    let s = s.trim_end_matches('=');
    s.bytes().any(|b| b.is_ascii_uppercase()) && s.bytes().any(|b| b.is_ascii_lowercase()) && s.bytes().any(|b| b.is_ascii_digit())
}

/// The line with secrets, emails and the home folder taken out. `home` is the user's home folder, if known.
/// Idempotent: redacting a redacted line changes nothing.
pub fn redact(line: &str, home: Option<&str>) -> String {
    let r = rules();
    let mut s = line.to_string();
    for form in home.map(home_forms).unwrap_or_default() {
        s = replace_folder(&s, &form);
    }
    s = r.home_generic.replace_all(&s, |c: &Captures| format!("{}~", c.name("lead").map_or("", |m| m.as_str()))).into_owned();
    s = r.cookie.replace_all(&s, format!("${{1}}${{2}}{MASK}")).into_owned();
    s = r.authorization.replace_all(&s, format!("${{1}}${{2}}{MASK}")).into_owned();
    s = r.bearer.replace_all(&s, format!("${{1}} {MASK}")).into_owned();
    s = r.url_userinfo.replace_all(&s, format!("${{1}}{MASK}@")).into_owned();
    s = r.prefixed.replace_all(&s, MASK).into_owned();
    s = r.jwt.replace_all(&s, MASK).into_owned();
    s = r
        .key_value
        .replace_all(&s, |c: &Captures| {
            let v = &c[3];
            // Token counts (`input_tokens=1200`) and values already masked stay.
            if v.bytes().all(|b| b.is_ascii_digit() || b == b'.') || v.starts_with(MASK.trim_end_matches(']')) || v == "~" {
                c[0].to_string()
            } else {
                format!("{}{}{MASK}", &c[1], &c[2])
            }
        })
        .into_owned();
    s = r.email.replace_all(&s, EMAIL).into_owned();
    // A digest (`sha256:…`) names content, not a secret: it stays.
    s = r.hex.replace_all(&s, |c: &Captures| if c.get(1).is_some() { c[0].to_string() } else { MASK.to_string() }).into_owned();
    s = r.opaque.replace_all(&s, |c: &Captures| if looks_random(&c[0]) { MASK.to_string() } else { c[0].to_string() }).into_owned();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Secrets that must never reach a log line. Each is checked on its own and inside a sentence.
    const SECRETS: &[&str] = &[
        "sk-ant-api03-AbCdEf0123456789_xyzXYZ-abcdef",
        "sk-proj-Zx9Yw8Vu7Ts6Rq5Po4Nm3Lk2",
        "sk-1234567890abcdefABCDEF",
        "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789",
        "github_pat_11ABCDEFG0123456789_abcdefghijklmnop",
        "glpat-AbCdEfGhIjKlMnOpQrSt",
        "xoxb-123456789012-abcdefghijkl",
        "AKIAIOSFODNN7EXAMPLE",
        "AIzaSyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q",
        "hf_AbCdEfGhIjKlMnOpQrStUvWxYz",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        "0123456789abcdef0123456789abcdef0123456789abcdef",
        "Qm9iJ3Mgc2VjcmV0IGtleSB3aXRoIDEyMyBkaWdpdHM9PQ==",
    ];

    fn clean(line: &str, secret: &str) {
        let out = redact(line, Some("/home/ann"));
        assert!(!out.contains(secret), "{secret:?} reached the log: {out}");
    }

    #[test]
    fn keys_and_tokens_never_reach_a_line() {
        for s in SECRETS {
            clean(s, s);
            clean(&format!("request failed with key {s}: 401"), s);
            clean(&format!("{{\"key\":\"{s}\"}}"), s);
        }
    }

    #[test]
    fn named_secrets_are_masked_whatever_their_shape() {
        for line in [
            "ANTHROPIC_API_KEY=hunter2hunter2",
            "api_key: short1",
            "\"password\": \"correct horse\"",
            "GET /x?access_token=abc.def&page=2",
            "client_secret=s3cr3t",
            "OPENAI_API_KEY='plainsecret'",
        ] {
            let out = redact(line, None);
            assert!(out.contains(MASK), "{line} -> {out}");
            for leaked in ["hunter2", "short1", "correct", "abc.def", "s3cr3t", "plainsecret"] {
                assert!(!out.contains(leaked), "{line} -> {out}");
            }
        }
        // Counts and the page parameter are not secrets.
        assert_eq!(redact("input_tokens=1200 output_tokens: 33", None), "input_tokens=1200 output_tokens: 33");
        assert!(redact("GET /x?access_token=abc&page=2", None).ends_with("&page=2"));
    }

    #[test]
    fn auth_headers_and_cookies_go_whole() {
        for (line, leaked) in [
            ("Authorization: Bearer abc.def.ghi", "abc.def"),
            ("authorization: Basic dXNlcjpwYXNz", "dXNlcjpwYXNz"),
            ("sent Bearer s3cr3tvalue to the proxy", "s3cr3tvalue"),
            ("Cookie: session=abc123; theme=dark", "abc123"),
            ("Set-Cookie: sid=xyz789; HttpOnly", "xyz789"),
            ("cloning https://ann:pa55word@example.com/repo.git", "pa55word"),
        ] {
            let out = redact(line, None);
            assert!(!out.contains(leaked), "{line} -> {out}");
            assert!(out.contains(MASK), "{line} -> {out}");
        }
    }

    #[test]
    fn emails_go() {
        let out = redact("signed in as ann.lee+test@example.co.uk, cc bob@corp.example", None);
        assert_eq!(out, "signed in as [email], cc [email]");
        // A bot or package at a version is not an email.
        assert_eq!(redact("install: done actuaria/prompt-slim@0.1.0", None), "install: done actuaria/prompt-slim@0.1.0");
    }

    #[test]
    fn the_home_folder_becomes_a_tilde() {
        assert_eq!(redact("opened /home/ann/Actuaria/jobs", Some("/home/ann")), "opened ~/Actuaria/jobs");
        assert_eq!(redact("opened /home/ann/x", Some("/home/ann/")), "opened ~/x");
        assert_eq!(redact(r"C:\Users\Ann\AppData\x", Some(r"C:\Users\Ann")), r"~\AppData\x");
        assert_eq!(redact(r#"{"p":"C:\\Users\\Ann\\x"}"#, Some(r"C:\Users\Ann")), r#"{"p":"~\\x"}"#);
        assert_eq!(redact("C:/Users/Ann/x", Some(r"C:\Users\Ann")), "~/x");
        assert_eq!(redact("/home/ann.bak/x /home/annabel", Some("/home/ann")), "~/x ~");
        // Without knowing the home folder, or for another account, the user's name still goes.
        assert_eq!(redact("read /Users/bob/Library/x and /home/carol", None), "read ~/Library/x and ~");
        assert_eq!(redact("path=/home/dan/x", None), "path=~/x");
        // Other folders stay.
        assert_eq!(redact("/usr/local/bin/docker", Some("/home/ann")), "/usr/local/bin/docker");
    }

    #[test]
    fn ordinary_text_stays() {
        for line in [
            "server.start port=47810 folder=~/Actuaria",
            "x86_64-unknown-linux-musl aarch64-apple-darwin",
            "dataset swe-bench-verified-mini@1.2.0 digest sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "Couldn't reach the catalogue: error sending request for url (https://actuaria.io/index.json)",
            "job 2026-11-23__10-01-02 finished: 12 trials, 1 errored",
            "a-very-long-dataset-name-without-any-secret-in-it",
        ] {
            assert_eq!(redact(line, Some("/home/ann")), line);
        }
    }

    #[test]
    fn redacting_twice_changes_nothing() {
        for s in SECRETS {
            let line = format!("token={s} Authorization: Bearer {s} mail ann@example.com at /home/ann/x");
            let once = redact(&line, Some("/home/ann"));
            assert_eq!(redact(&once, Some("/home/ann")), once);
        }
    }
}
