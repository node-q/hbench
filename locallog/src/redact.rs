//! The one redaction every local log line goes through, before it is written and again when diagnostics are
//! copied or saved: keys and tokens, cookies and auth headers, credentials in URLs, emails, long secret-looking
//! strings, and the home folder (as `~`).

use regex::{Captures, Regex};
use std::sync::OnceLock;

pub const MASK: &str = "[REDACTED]";
pub const EMAIL: &str = "[email]";

struct Rules {
    encoded_home: Regex,
    percent: Regex,
    home_generic: Regex,
    cli_flag: Regex,
    key_quoted: Regex,
    key_word: Regex,
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

/// A secret's name: `api_key`, `ANTHROPIC_KEY`, `x-auth-token`, `"auth"` (Docker's config), `passphrase`, ...
const KEY: &str = concat!(
    r"[A-Za-z0-9_.\-]*(?:api[_\-]?key|[_\-]key|secret|token|passw(?:or)?d|passphrase|pwd|credential|auth",
    r"|session[_\-]?id|private[_\-]?key|access[_\-]?key)[A-Za-z0-9_\-]*|key"
);

/// Whether a name the key rules matched is a secret's: not `author`, `tokenizer`, `keyboard`, or a count.
fn secret_key(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    !(n.contains("author") || n.contains("tokenizer") || n.ends_with("tokens") || n.contains("keyboard"))
}

/// Whether a matched value is one to keep: a number, or already masked.
fn keep_value(v: &str) -> bool {
    v.is_empty() || v.bytes().all(|b| b.is_ascii_digit() || b == b'.') || v.starts_with(MASK.trim_end_matches(']')) || v == "~"
}

/// `s` with `%XX` escapes decoded (invalid UTF-8 kept lossily), so encoded tokens and emails are seen.
fn percent_decode(s: &str, re: &Regex) -> String {
    if !re.is_match(s) {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn rules() -> &'static Rules {
    static R: OnceLock<Rules> = OnceLock::new();
    R.get_or_init(|| {
        let re = |s: &str| Regex::new(s).expect("a valid redaction pattern");
        Rules {
            // A home folder still URL-encoded (`%2Fhome%2Fann`, `C%3A%5CUsers%5CAnn%20Lee`): the user's name goes whole.
            encoded_home: re(r"(?i)(?:%2F(?:home|Users)%2F|[a-z]%3A(?:%5C|%2F)Users(?:%5C|%2F))(?:[^%\s&\x22'/\\]|%20|%2E|%2D|%5F)+"),
            percent: re(r"%[0-9A-Fa-f]{2}"),
            // Another account's home, or this one's when the caller doesn't know it: the user's name goes, whole
            // when it has spaces and a folder follows (`C:\Users\Ann Lee\x`).
            home_generic: re(concat!(
                r#"(?i)(?:^|(?<lead>[\s"'=(:,\[]|//))(?:/home/|/Users/|[a-z]:\\{1,4}Users\\{1,4}|[a-z]:/Users/)"#,
                r#"(?:[^/\\\s"']+(?: [^/\\\s"']+)*(?<sep>[/\\])|[^/\\\s"']+)"#
            )),
            // `--password x`, `--token=x`, `-p x` (docker login).
            cli_flag: re(r"(?i)(?:^|(?<lead>\s))(?<flag>--(?:password|passphrase|pass|token|api-key|secret|auth)[= ]|-p )\S+"),
            cookie: re(r"(?i)\b((?:set-)?cookie)(\s*[:=]\s*).*"),
            authorization: re(r#"(?i)\b((?:proxy-)?authorization)(["']?\s*[:=]\s*["']?)[^"'\r\n]*"#),
            bearer: re(r"(?i)\b(bearer)\s+[A-Za-z0-9._~+/=\-]{6,}"),
            // `user:pass@`, `:pass@`, `token@`.
            url_userinfo: re(r"(?i)\b([a-z][a-z0-9+.\-]*://)[^/\s@]+@"),
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
                r"|(?:sk|rk|pk)_(?:live|test)_[A-Za-z0-9]{12,}",
                r"|dckr_pat_[A-Za-z0-9_\-]{16,}|glsa_[A-Za-z0-9_]{20,})"
            )),
            jwt: re(r"\beyJ[A-Za-z0-9_\-]{6,}\.[A-Za-z0-9_\-]{6,}\.[A-Za-z0-9_\-]*"),
            // `"password": "correct horse"`, `{\"api_key\": \"x\"}`, `token: 'x'`: the whole quoted value.
            key_quoted: re(&format!(r#"(?i)\b({KEY})(\\?["']?\s*[:=]\s*\\?["'])([^"'\r\n]*?)(\\?["'])"#)),
            // `api_key=…`, `ANTHROPIC_KEY=…`, `?access_token=…`, `password: …`.
            key_value: re(&format!(r#"(?i)\b({KEY})(\\?["']?\s*[:=]\s*\\?["']?)([^\s"'\\,;&}}\]]+)"#)),
            // `token abcdef…`: a long run after a secret's name.
            key_word: re(r"(?i)\b(token|key|secret|password|passphrase|apikey)(\s+)[A-Za-z0-9_]{20,}\b"),
            email: re(r"[\p{L}\p{N}._%+\-]+@[\p{L}\p{N}\-]+(?:\.[\p{L}\p{N}\-]+)*\.\p{L}{2,}\b"),
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
    let mut forms = vec![home.replace('\\', "\\\\\\\\"), home.replace('\\', "\\\\"), home.to_string(), home.replace('\\', "/")];
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
    let mut s = r.encoded_home.replace_all(line, "~").into_owned();
    s = percent_decode(&s, &r.percent);
    for form in home.map(home_forms).unwrap_or_default() {
        s = replace_folder(&s, &form);
    }
    let around = |c: &Captures| format!("{}~{}", c.name("lead").map_or("", |m| m.as_str()), c.name("sep").map_or("", |m| m.as_str()));
    s = r.home_generic.replace_all(&s, around).into_owned();
    s = r
        .cli_flag
        .replace_all(&s, |c: &Captures| format!("{}{}{MASK}", c.name("lead").map_or("", |m| m.as_str()), &c["flag"]))
        .into_owned();
    s = r.cookie.replace_all(&s, format!("${{1}}${{2}}{MASK}")).into_owned();
    s = r.authorization.replace_all(&s, format!("${{1}}${{2}}{MASK}")).into_owned();
    s = r.bearer.replace_all(&s, format!("${{1}} {MASK}")).into_owned();
    s = r.url_userinfo.replace_all(&s, format!("${{1}}{MASK}@")).into_owned();
    s = r.prefixed.replace_all(&s, MASK).into_owned();
    s = r.jwt.replace_all(&s, MASK).into_owned();
    // Token counts (`input_tokens=1200`) and values already masked stay.
    let named = |c: &Captures, close: &str| {
        if !secret_key(&c[1]) || keep_value(&c[3]) { c[0].to_string() } else { format!("{}{}{MASK}{close}", &c[1], &c[2]) }
    };
    s = r.key_quoted.replace_all(&s, |c: &Captures| named(c, &c[4])).into_owned();
    s = r.key_value.replace_all(&s, |c: &Captures| named(c, "")).into_owned();
    s = r.key_word.replace_all(&s, format!("${{1}}${{2}}{MASK}")).into_owned();
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

    /// The security review's cases (#131): each `leak` must be gone from the redacted line.
    pub(crate) const REVIEW: &[(&str, &[&str])] = &[
        (r#"bad config {"api_key": "hunter2xx", "password": "pw2"}"#, &["hunter2xx", "pw2"]),
        (r#"HTTP 401 body={"x_api_key":"short"}"#, &["short"]),
        (r#"auth failed: token: "hunter2xx" rejected"#, &["hunter2xx"]),
        (r#"escaped {\"api_key\": \"hunter2xx\", \"password\": \"pw2\"}"#, &["hunter2xx", "pw2"]),
        ("ANTHROPIC_KEY=plainsecret1 AZURE_OPENAI_KEY=plainkey", &["plainsecret1", "plainkey"]),
        (r#"{"auths": {"r.io": {"auth": "dXNlcjpwYXNzd29yZA=="}}}"#, &["dXNlcjpwYXNzd29yZA"]),
        ("passphrase=opensesame1", &["opensesame1"]),
        ("ran docker login --password hunter2xx -u ann", &["hunter2xx"]),
        ("ran tool --token=abc12345", &["abc12345"]),
        ("ran docker login -p hunter2xx registry", &["hunter2xx"]),
        ("connect redis://:onlypass@cache:6379", &["onlypass"]),
        ("clone https://ghp_short@github.com/x", &["ghp_short"]),
        (r#"{"password": "correct horse battery"}"#, &["correct", "horse", "battery"]),
        ("GET /cb?token%3Dabcdef123456&x=1", &["abcdef123456"]),
        ("GET /cb?user=ann%40example.com", &["ann@", "ann%40"]),
        ("GET /open?p=%2Fhome%2Fann%2Fjobs", &["ann"]),
        ("GET /open?p=C%3A%5CUsers%5CAnn%20Lee%5Cjobs", &["Ann", "Lee"]),
        ("from anné@example.com and josé.garcía@empresa.es", &["anné", "josé", "garcía"]),
        ("open file:///home/bob/x", &["bob"]),
        (r"open C:\Users\Ann Lee\x", &["Ann", "Lee"]),
        ("token abcdefghijklmnopqrstuvwxyz0123", &["abcdefghijklmnopqrstuvwxyz0123"]),
        ("key abcdefghijklmnopqrstuvwxyz", &["abcdefghijklmnopqrstuvwxyz"]),
        ("dckr_pat_AbCdEfGhIjKlMnOp12 and glsa_abcdefghijklmnopqrstuvwxyz_12345678", &["dckr_pat_", "glsa_"]),
    ];

    #[test]
    fn the_reviews_cases_are_redacted() {
        for (line, leaks) in REVIEW {
            let out = redact(line, Some("/home/ann"));
            for leak in *leaks {
                assert!(!out.contains(leak), "{leak:?} survived: {line} -> {out}");
            }
            assert_eq!(redact(&out, Some("/home/ann")), out, "not idempotent: {line}");
        }
        // The whole quoted value goes, the quotes stay.
        assert_eq!(redact(r#"{"password": "correct horse battery"}"#, None), r#"{"password": "[REDACTED]"}"#);
        // Doubly escaped home, known.
        assert_eq!(redact(r"p=C:\\Users\\Ann\\x", Some(r"C:\Users\Ann")), r"p=~\\x");
        assert_eq!(redact(r"p=C:\\\\Users\\\\Ann\\\\x", Some(r"C:\Users\Ann")), r"p=~\\\\x");
        assert_eq!(redact(r"p=C:\\\\Users\\\\Bob\\\\x", None), r"p=~\\\\x");
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
