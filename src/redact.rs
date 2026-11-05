//! Scrubbing known secret values out of everything hbench writes.
//!
//! Every secret hbench handles is registered here: the host's sensitive environment variables at
//! startup, and any sensitive value a trial passes to a container or reads from its config. Logs are
//! scrubbed line by line as they are written; each trial's directory (trajectories, agent logs,
//! downloaded artifacts, result.json) is swept when the trial ends, and the job directory when the job ends.
//!
//! A value is matched raw, JSON-escaped, URL-encoded and base64-encoded (standard and URL-safe, alone or
//! at any offset inside a longer base64 string, as in `Authorization: Basic ...`).

use aho_corasick::{AhoCorasick, MatchKind};
use base64::Engine;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

pub const MASK: &str = "[REDACTED]";
/// Shorter values are too likely to be ordinary words or numbers to scrub everywhere.
const MIN_LEN: usize = 8;

#[derive(Default)]
struct Registry {
    values: BTreeSet<String>,
    matcher: Option<Arc<AhoCorasick>>,
}

fn registry() -> &'static RwLock<Registry> {
    static R: OnceLock<RwLock<Registry>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Whether `v` is worth treating as a secret: long enough, and not a path or a `${VAR}` template.
fn eligible(v: &str) -> bool {
    v.len() >= MIN_LEN && !v.starts_with('/') && !(v.starts_with("${") && v.ends_with('}'))
}

/// Remember a secret value, so that it is scrubbed from everything written from now on.
pub fn register(v: &str) {
    let v = v.trim();
    if !eligible(v) {
        return;
    }
    let mut r = registry().write().unwrap();
    if r.values.insert(v.to_string()) {
        let patterns: BTreeSet<String> = r.values.iter().flat_map(|v| encodings(v)).collect();
        r.matcher = AhoCorasick::builder().match_kind(MatchKind::LeftmostLongest).build(&patterns).ok().map(Arc::new);
    }
}

/// Whether a variable's value is a secret, by its name: `*_KEY`, `*TOKEN`, `*SECRET*`, ... but not
/// `GIT_AUTHOR_NAME` or `TOKENIZER`, which would scrub names and model ids everywhere. Model endpoints
/// (`*_BASE_URL`, `base_url`, ...) count too: they can reveal private infrastructure.
fn secret_name(k: &str) -> bool {
    let u = k.to_ascii_uppercase();
    (crate::config::is_sensitive_key(k) && !u.contains("AUTHOR") && !u.contains("TOKENIZER")) || crate::config::is_endpoint_key(k)
}

/// Register the values of the sensitive keys in an env map.
pub fn register_env<'a>(env: impl IntoIterator<Item = (&'a String, &'a String)>) {
    for (k, v) in env {
        if secret_name(k) {
            register(v);
        }
    }
}

/// Register the host's sensitive environment variables: model API keys, cloud credentials, tokens.
pub fn register_host_env() {
    for (k, v) in std::env::vars() {
        if secret_name(&k) {
            register(&v);
        }
    }
}

/// Register string values under sensitive keys anywhere in a JSON document (agent kwargs, MCP configs).
pub fn register_json(v: &serde_json::Value) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, v) in m {
                match v {
                    serde_json::Value::String(s) if secret_name(k) => register(s),
                    _ => register_json(v),
                }
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(register_json),
        _ => {}
    }
}

/// Every form a value is likely to take in a log or a file.
fn encodings(v: &str) -> Vec<String> {
    let mut out = vec![v.to_string()];
    let json = serde_json::to_string(v).unwrap();
    out.push(json[1..json.len() - 1].to_string());
    out.push(url_encode(v, false, true));
    out.push(url_encode(v, false, false));
    out.push(url_encode(v, true, true));
    let std = base64::engine::general_purpose::STANDARD;
    let full = std.encode(v);
    let mut b64 = vec![full.clone(), full.trim_end_matches('=').to_string()];
    // Inside a longer base64 string the value can start at any of three byte offsets. The characters
    // that depend only on the value's bytes are the same wherever it sits, so match those.
    let n = v.len();
    for k in 0..3 {
        let mut bytes = vec![0u8; k];
        bytes.extend_from_slice(v.as_bytes());
        let enc = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&bytes);
        let (start, end) = ((8 * k).div_ceil(6), (8 * (k + n)) / 6);
        b64.push(enc[start..end].to_string());
    }
    for s in b64 {
        out.push(s.replace('+', "-").replace('/', "_"));
        out.push(s);
    }
    out.retain(|s| s.len() >= MIN_LEN);
    out
}

fn url_encode(v: &str, lower: bool, plus_for_space: bool) -> String {
    let mut s = String::new();
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => s.push(b as char),
            b' ' if plus_for_space => s.push('+'),
            _ if lower => s.push_str(&format!("%{b:02x}")),
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

fn matcher() -> Option<Arc<AhoCorasick>> {
    registry().read().unwrap().matcher.clone()
}

/// `s` with every registered secret replaced by [`MASK`].
pub fn text(s: &str) -> String {
    match matcher() {
        Some(m) if m.is_match(s) => m.replace_all(s, &vec![MASK; m.patterns_len()]),
        _ => s.to_string(),
    }
}

fn bytes(b: &[u8]) -> Option<Vec<u8>> {
    let m = matcher()?;
    m.is_match(b).then(|| m.replace_all_bytes(b, &vec![MASK.as_bytes(); m.patterns_len()]))
}

/// A serializable value with every registered secret scrubbed from its JSON form.
pub fn value<T: serde::Serialize + serde::de::DeserializeOwned>(v: T) -> T {
    let Ok(s) = serde_json::to_string(&v) else { return v };
    let r = text(&s);
    if r == s { v } else { serde_json::from_str(&r).unwrap_or(v) }
}

/// Rewrite every regular file under `dir` (symlinks are not followed) that holds a registered secret.
/// Returns how many files were changed.
pub fn scrub_dir(dir: &Path) -> usize {
    if matcher().is_none() {
        return 0;
    }
    let mut changed = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let Ok(t) = e.file_type() else { continue };
            let p = e.path();
            if t.is_dir() {
                stack.push(p);
            } else if t.is_file()
                && let Ok(b) = std::fs::read(&p)
                && let Some(clean) = bytes(&b)
                && std::fs::write(&p, clean).is_ok()
            {
                changed += 1;
            }
        }
    }
    changed
}

/// Copy the files under `src` into `dst`, scrubbed, each replaced in one step: a reader of `dst` sees the
/// old file or the clean new one, never raw bytes. Files are not read back from `dst`, so a read-only
/// one can't stay raw the way an in-place scrub would leave it.
pub fn publish_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (t, to) = (e.file_type()?, dst.join(e.file_name()));
        if t.is_dir() {
            publish_dir(&e.path(), &to)?;
        } else if t.is_file() {
            let raw = std::fs::read(e.path())?;
            let tmp = dst.join(format!(".{}.publish", e.file_name().to_string_lossy()));
            let _ = std::fs::remove_file(&tmp);
            std::fs::write(&tmp, bytes(&raw).unwrap_or(raw))?;
            std::fs::set_permissions(&tmp, e.metadata()?.permissions())?;
            std::fs::rename(&tmp, to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn reset() {
    *registry().write().unwrap() = Registry::default();
}

/// The registry is process-wide, so tests that use it hold this lock.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

    const KEY: &str = "sk-ant-api03-Zq8/x+Y w=p&k";

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let g = test_lock();
        reset();
        register(KEY);
        g
    }

    #[test]
    fn scrubs_raw_json_and_url_encoded_values() {
        let _g = setup();
        let json = serde_json::to_string(&serde_json::json!({"k": "a\"b"})).unwrap();
        assert_eq!(text("x-api-key: sk-ant-api03-Zq8/x+Y w=p&k done"), "x-api-key: [REDACTED] done");
        assert_eq!(text("?key=sk-ant-api03-Zq8%2Fx%2BY%20w%3Dp%26k&a=1"), "?key=[REDACTED]&a=1");
        assert_eq!(text("key=sk-ant-api03-Zq8%2fx%2bY+w%3dp%26k"), "key=[REDACTED]");
        assert_eq!(text("key=sk-ant-api03-Zq8%2Fx%2BY+w%3Dp%26k"), "key=[REDACTED]");
        assert_eq!(text(&json), json);
    }

    #[test]
    fn scrubs_base64_alone_and_inside_longer_strings() {
        let _g = setup();
        assert_eq!(text(&STANDARD.encode(KEY)), MASK);
        assert!(!text(&URL_SAFE_NO_PAD.encode(KEY)).contains(&URL_SAFE_NO_PAD.encode(KEY)[..12]));
        for prefix in ["", "u", "us", "user:", "x-user:"] {
            let blob = STANDARD.encode(format!("{prefix}{KEY}:tail"));
            let out = text(&format!("Authorization: Basic {blob}"));
            assert!(out.contains(MASK), "{prefix:?}: {out}");
            // What is left on either side of the mask carries less than one byte of the value.
            let rest = out.trim_start_matches("Authorization: Basic ").replace(MASK, "");
            assert!(rest.len() <= STANDARD.encode(format!("{prefix}:tail")).len() + 4, "{prefix:?}: {out}");
        }
    }

    #[test]
    fn ignores_short_values_paths_and_templates() {
        let _g = test_lock();
        reset();
        for v in ["true", "1234567", "/tmp/ssh-agent.sock", "${OPENAI_API_KEY}"] {
            register(v);
        }
        assert_eq!(text("true 1234567 /tmp/ssh-agent.sock ${OPENAI_API_KEY}"), "true 1234567 /tmp/ssh-agent.sock ${OPENAI_API_KEY}");
    }

    #[test]
    fn registers_only_sensitive_keys_and_endpoints() {
        let _g = test_lock();
        reset();
        let env: std::collections::BTreeMap<String, String> = [
            ("OPENAI_API_KEY", "sk-proj-abcdefgh"),
            ("OPENAI_BASE_URL", "https://api.example.com/v1"),
            ("GIT_AUTHOR_NAME", "Ada Lovelace"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        register_env(&env);
        register_json(
            &serde_json::json!({"model": {"api_key": "nested-secret-123", "name": "gpt-long-name", "base_url": "http://10.9.8.7/v1"}}),
        );
        assert_eq!(
            text("sk-proj-abcdefgh https://api.example.com/v1/chat Ada Lovelace nested-secret-123 gpt-long-name http://10.9.8.7/v1"),
            "[REDACTED] [REDACTED]/chat Ada Lovelace [REDACTED] gpt-long-name [REDACTED]"
        );
    }

    #[test]
    fn scrubs_files_and_serializable_values() {
        let _g = setup();
        let dir = std::env::temp_dir().join(format!("hbench-redact-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("agent/sessions")).unwrap();
        std::fs::write(dir.join("agent/sessions/trajectory.json"), format!("{{\"auth\": {}}}", serde_json::json!(KEY))).unwrap();
        std::fs::write(dir.join("trial.log"), format!("token={}\n", STANDARD.encode(KEY))).unwrap();
        std::fs::write(dir.join("clean.txt"), "nothing here").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", dir.join("link")).unwrap();
        assert_eq!(scrub_dir(&dir), 2);
        assert_eq!(std::fs::read_to_string(dir.join("agent/sessions/trajectory.json")).unwrap(), "{\"auth\": \"[REDACTED]\"}");
        assert_eq!(std::fs::read_to_string(dir.join("trial.log")).unwrap(), "token=[REDACTED]\n");
        assert_eq!(scrub_dir(&dir), 0);
        std::fs::remove_dir_all(&dir).unwrap();

        let v = value(serde_json::json!({"message": format!("401 for key {KEY}")}));
        assert_eq!(v["message"], "401 for key [REDACTED]");
    }

    #[test]
    fn publish_dir_never_writes_a_raw_byte_to_the_destination() {
        use std::os::unix::fs::PermissionsExt;
        let _g = test_lock();
        reset();
        register("hunter2-long-secret");
        let root = std::env::temp_dir().join(format!("hbench-publish-{}", uuid::Uuid::new_v4()));
        let (src, dst) = (root.join("src"), root.join("dst"));
        std::fs::create_dir_all(src.join("sessions")).unwrap();
        std::fs::write(src.join("sessions/a.jsonl"), "key hunter2-long-secret\n").unwrap();
        std::fs::write(src.join("plain.txt"), "nothing here").unwrap();
        std::fs::write(src.join("locked.txt"), "hunter2-long-secret").unwrap();
        std::fs::set_permissions(src.join("locked.txt"), std::fs::Permissions::from_mode(0o400)).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", src.join("link")).unwrap();
        for _ in 0..2 {
            publish_dir(&src, &dst).unwrap();
        }
        assert_eq!(std::fs::read_to_string(dst.join("sessions/a.jsonl")).unwrap(), format!("key {MASK}\n"));
        assert_eq!(std::fs::read_to_string(dst.join("plain.txt")).unwrap(), "nothing here");
        assert_eq!(std::fs::read_to_string(dst.join("locked.txt")).unwrap(), MASK);
        assert_eq!(std::fs::metadata(dst.join("locked.txt")).unwrap().permissions().mode() & 0o777, 0o400);
        let names: Vec<_> = std::fs::read_dir(&dst).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(!names.iter().any(|n| n.ends_with(".publish") || n == "link"), "{names:?}");
        std::fs::remove_dir_all(&root).unwrap();
        reset();
    }
}
