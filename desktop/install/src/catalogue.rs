//! The catalogue (`index.json` on market.actuaria.io) and `bots.lock` (in node-q/hbench). A bot is offered only
//! for the targets whose artefact is valid in the catalogue and whose id, version, target and SHA-256 are also
//! in `bots.lock`. Any invalid entry for the bot refuses the bot.

use crate::link::{Target, valid_bot_id};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use url::Url;

pub const INDEX_URL: &str = "https://market.actuaria.io/bots/index.json";
pub const LOCK_URL: &str = "https://raw.githubusercontent.com/node-q/hbench/master/bots.lock";
pub const MAX_INDEX_BYTES: usize = 64 * 1024;
pub const MAX_LOCK_BYTES: usize = 64 * 1024;
pub const MAX_ARTEFACT_BYTES: u64 = 5 * 1024 * 1024;
pub const DOWNLOAD_HOST: &str = "market.actuaria.io";
const DOWNLOAD_PATH: &str = "/downloads/";
pub const MAX_NAME_CHARS: usize = 60;
pub const MAX_DESCRIPTION_CHARS: usize = 400;
const MAX_VERSION_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogueError {
    BadIndex(String),
    NotListed,
    BadEntry(String),
    BadLock(usize),
    LockUnavailable,
    NotLocked,
}

impl fmt::Display for CatalogueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogueError::BadIndex(e) => write!(f, "the catalogue couldn't be read ({e})"),
            CatalogueError::NotListed => f.write_str("this bot isn't in the catalogue"),
            CatalogueError::BadEntry(e) => write!(f, "the catalogue's entry for this bot isn't valid ({e})"),
            CatalogueError::BadLock(line) => write!(f, "bots.lock isn't valid (line {line})"),
            CatalogueError::LockUnavailable => f.write_str("bots.lock couldn't be fetched, so nothing can be installed now"),
            CatalogueError::NotLocked => f.write_str("this version isn't in bots.lock, so it can't be installed"),
        }
    }
}

#[derive(Deserialize)]
struct RawIndex {
    bots: Vec<RawBot>,
}

#[derive(Deserialize)]
struct RawBot {
    id: String,
    name: String,
    version: String,
    description: String,
    #[serde(default)]
    targets: RawTargets,
}

#[derive(Deserialize, Default)]
struct RawTargets {
    claude: Option<Artefact>,
    orca: Option<Artefact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Artefact {
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// A bot the user may install, already checked against the catalogue's rules and `bots.lock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Offer {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub artefacts: BTreeMap<Target, Artefact>,
}

/// `bots.lock`: one line per artefact, `actuaria/<id> <version> <target> sha256:<hex>`; `#` comments and blank
/// lines are ignored. Any other line makes the whole file invalid.
#[derive(Debug, Default)]
pub struct Lock(HashSet<(String, String, Target, String)>);

impl Lock {
    pub fn parse(text: &str) -> Result<Lock, CatalogueError> {
        let mut set = HashSet::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bad = || CatalogueError::BadLock(n + 1);
            let f: Vec<&str> = line.split_ascii_whitespace().collect();
            let [name, version, target, digest] = f[..] else { return Err(bad()) };
            let id = name.strip_prefix("actuaria/").filter(|id| valid_bot_id(id)).ok_or_else(bad)?;
            let target = Target::from_name(target).ok_or_else(bad)?;
            let hex = digest.strip_prefix("sha256:").filter(|h| valid_sha256(h)).ok_or_else(bad)?;
            if !valid_version(version) {
                return Err(bad());
            }
            set.insert((id.to_string(), version.to_string(), target, hex.to_string()));
        }
        Ok(Lock(set))
    }

    pub fn contains(&self, id: &str, version: &str, target: Target, sha256: &str) -> bool {
        self.0.contains(&(id.to_string(), version.to_string(), target, sha256.to_string()))
    }
}

pub fn valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// `1.2.3`, optionally with a `-pre` or `+build` tail of `[0-9A-Za-z.-]`.
pub fn valid_version(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_VERSION_BYTES {
        return false;
    }
    let (core, tail) = match s.find(['-', '+']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let nums: Vec<&str> = core.split('.').collect();
    nums.len() == 3
        && nums.iter().all(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
        && tail.is_none_or(|t| !t.is_empty() && t.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'+')))
}

/// An artefact URL must be `https://market.actuaria.io/downloads/<name>`: no port, userinfo, query, fragment,
/// percent-encoding or dot segments, and the name ends as the target expects.
pub fn valid_download_url(raw: &str, target: Target) -> bool {
    if raw.len() > 512 || !raw.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b':' | b'/' | b'.' | b'-' | b'_')) {
        return false;
    }
    let Ok(u) = Url::parse(raw) else { return false };
    let ext = match target {
        Target::Claude => ".mcpb",
        Target::Orca => ".zip",
    };
    u.as_str() == raw
        && u.scheme() == "https"
        && u.host_str() == Some(DOWNLOAD_HOST)
        && u.port().is_none()
        && u.username().is_empty()
        && u.password().is_none()
        && u.query().is_none()
        && u.fragment().is_none()
        && raw.starts_with(&format!("https://{DOWNLOAD_HOST}{DOWNLOAD_PATH}"))
        && u.path_segments().is_some_and(|s| s.skip(1).all(|seg| !seg.is_empty() && seg != "." && seg != ".."))
        && u.path().ends_with(ext)
}

/// Text from the catalogue, made safe to show as plain text: controls and bidi overrides removed, whitespace
/// collapsed, at most `max` characters.
pub fn plain_text(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control() && !matches!(*c as u32, 0x200b..=0x200f | 0x202a..=0x202e | 0x2066..=0x2069 | 0xfeff))
        .collect();
    let words: Vec<&str> = cleaned.split(' ').filter(|w| !w.is_empty()).collect();
    let joined = words.join(" ");
    if joined.chars().count() <= max {
        joined
    } else {
        let mut t: String = joined.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Look `id` up in the catalogue and check it against `bots.lock`. With no lock (it couldn't be fetched),
/// nothing is offered.
pub fn lookup(index_json: &[u8], lock: Option<&Lock>, id: &str) -> Result<Offer, CatalogueError> {
    if index_json.len() > MAX_INDEX_BYTES {
        return Err(CatalogueError::BadIndex("too large".into()));
    }
    let index: RawIndex = serde_json::from_slice(index_json).map_err(|e| CatalogueError::BadIndex(e.to_string()))?;
    let mut found = index.bots.into_iter().filter(|b| b.id == id);
    let bot = found.next().ok_or(CatalogueError::NotListed)?;
    if found.next().is_some() {
        return Err(CatalogueError::BadEntry("listed twice".into()));
    }
    let bad = |e: &str| CatalogueError::BadEntry(e.into());
    if !valid_bot_id(&bot.id) {
        return Err(bad("id"));
    }
    if !valid_version(&bot.version) {
        return Err(bad("version"));
    }
    let name = plain_text(&bot.name, MAX_NAME_CHARS);
    if name.is_empty() {
        return Err(bad("name"));
    }
    let description = plain_text(&bot.description, MAX_DESCRIPTION_CHARS);
    let mut listed = BTreeMap::new();
    for (t, a) in [(Target::Claude, bot.targets.claude), (Target::Orca, bot.targets.orca)] {
        let Some(a) = a else { continue };
        if !valid_download_url(&a.url, t) {
            return Err(bad("artefact URL"));
        }
        if !valid_sha256(&a.sha256) {
            return Err(bad("artefact SHA-256"));
        }
        if a.size == 0 || a.size > MAX_ARTEFACT_BYTES {
            return Err(bad("artefact size"));
        }
        listed.insert(t, a);
    }
    if listed.is_empty() {
        return Err(bad("no artefacts"));
    }
    let lock = lock.ok_or(CatalogueError::LockUnavailable)?;
    let artefacts: BTreeMap<Target, Artefact> =
        listed.into_iter().filter(|(t, a)| lock.contains(&bot.id, &bot.version, *t, &a.sha256)).collect();
    if artefacts.is_empty() {
        return Err(CatalogueError::NotLocked);
    }
    Ok(Offer { id: bot.id, name, version: bot.version, description, artefacts })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_A: &str = "d6d7975ce7d7ed3eecc46ae07529a1646dfc41adfffc0cb894619ffddbe68eb6";
    const SHA_B: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn index(claude_url: &str, extra: &str) -> String {
        format!(
            r#"{{"bots":[{{"id":"prompt-slim","name":"Prompt Slim","version":"0.1.0","description":"Slims prompts.",
            "targets":{{"claude":{{"url":"{claude_url}","sha256":"{SHA_A}","size":398242}},
                        "orca":{{"url":"https://market.actuaria.io/downloads/prompt-slim-0.1.0-orca.zip","sha256":"{SHA_B}","size":9000}}}}}}{extra}]}}"#
        )
    }
    const GOOD_URL: &str = "https://market.actuaria.io/downloads/prompt-slim-0.1.0.mcpb";

    fn lock() -> Lock {
        Lock::parse(&format!(
            "# bots.lock\n\nactuaria/prompt-slim 0.1.0 claude sha256:{SHA_A}\nactuaria/prompt-slim 0.1.0 orca sha256:{SHA_B}\n"
        ))
        .unwrap()
    }

    #[test]
    fn offers_locked_targets() {
        let o = lookup(index(GOOD_URL, "").as_bytes(), Some(&lock()), "prompt-slim").unwrap();
        assert_eq!(o.version, "0.1.0");
        assert_eq!(o.artefacts.keys().copied().collect::<Vec<_>>(), vec![Target::Claude, Target::Orca]);
        // Only the claude line locked: only claude offered.
        let l = Lock::parse(&format!("actuaria/prompt-slim 0.1.0 claude sha256:{SHA_A}")).unwrap();
        let o = lookup(index(GOOD_URL, "").as_bytes(), Some(&l), "prompt-slim").unwrap();
        assert_eq!(o.artefacts.keys().copied().collect::<Vec<_>>(), vec![Target::Claude]);
    }

    #[test]
    fn refuses_without_or_outside_the_lock() {
        let i = index(GOOD_URL, "");
        assert_eq!(lookup(i.as_bytes(), None, "prompt-slim"), Err(CatalogueError::LockUnavailable));
        assert_eq!(lookup(i.as_bytes(), Some(&Lock::default()), "prompt-slim"), Err(CatalogueError::NotLocked));
        // Same digest, other version: not locked.
        let l = Lock::parse(&format!("actuaria/prompt-slim 0.1.1 claude sha256:{SHA_A}")).unwrap();
        assert_eq!(lookup(i.as_bytes(), Some(&l), "prompt-slim"), Err(CatalogueError::NotLocked));
        // Same version, other target.
        let l = Lock::parse(&format!("actuaria/prompt-slim 0.1.0 orca sha256:{SHA_A}")).unwrap();
        assert_eq!(lookup(i.as_bytes(), Some(&l), "prompt-slim"), Err(CatalogueError::NotLocked));
        assert_eq!(lookup(i.as_bytes(), Some(&lock()), "other"), Err(CatalogueError::NotListed));
    }

    #[test]
    fn refuses_bad_index() {
        let l = lock();
        for url in [
            "http://market.actuaria.io/downloads/x.mcpb",
            "https://market.actuaria.io.evil.com/downloads/x.mcpb",
            "https://evil.com/downloads/x.mcpb",
            "https://market.actuaria.io:8443/downloads/x.mcpb",
            "https://u@market.actuaria.io/downloads/x.mcpb",
            "https://market.actuaria.io/download/x.mcpb",
            "https://market.actuaria.io/downloads/../x.mcpb",
            "https://market.actuaria.io/downloads/%2e%2e/x.mcpb",
            "https://market.actuaria.io/downloads/x.mcpb?a=1",
            "https://market.actuaria.io/downloads/x.mcpb#a",
            "https://market.actuaria.io/downloads//x.mcpb",
            "https://MARKET.actuaria.io/downloads/x.mcpb",
            "https://market.actuaria.io/downloads/x.exe",
        ] {
            assert!(matches!(lookup(index(url, "").as_bytes(), Some(&l), "prompt-slim"), Err(CatalogueError::BadEntry(_))), "{url}");
        }
        let dup = index(GOOD_URL, r#",{"id":"prompt-slim","name":"x","version":"9.9.9","description":"","targets":{}}"#);
        assert!(matches!(lookup(dup.as_bytes(), Some(&l), "prompt-slim"), Err(CatalogueError::BadEntry(_))));
        let big = vec![b' '; MAX_INDEX_BYTES + 1];
        assert!(matches!(lookup(&big, Some(&l), "prompt-slim"), Err(CatalogueError::BadIndex(_))));
        assert!(matches!(lookup(b"[]", Some(&l), "prompt-slim"), Err(CatalogueError::BadIndex(_))));
        let i = index(GOOD_URL, "").replace("398242", "6000000");
        assert!(matches!(lookup(i.as_bytes(), Some(&l), "prompt-slim"), Err(CatalogueError::BadEntry(_))));
        let i = index(GOOD_URL, "").replace("\"0.1.0\"", "\"latest\"");
        assert!(matches!(lookup(i.as_bytes(), Some(&l), "prompt-slim"), Err(CatalogueError::BadEntry(_))));
        let i = index(GOOD_URL, "").replace(SHA_A, &SHA_A.to_uppercase());
        assert!(matches!(lookup(i.as_bytes(), Some(&l), "prompt-slim"), Err(CatalogueError::BadEntry(_))));
    }

    #[test]
    fn text_is_plain_and_capped() {
        let i = index(GOOD_URL, "")
            .replace("Prompt Slim", "Prompt\\u202e Slim\\u0007 <b>x</b>")
            .replace("Slims prompts.", &"long ".repeat(200));
        let o = lookup(i.as_bytes(), Some(&lock()), "prompt-slim").unwrap();
        assert_eq!(o.name, "Prompt Slim <b>x</b>");
        assert_eq!(o.description.chars().count(), MAX_DESCRIPTION_CHARS);
        assert!(o.description.ends_with('…'));
    }

    #[test]
    fn lock_parsing() {
        assert!(Lock::parse("# only a header\n\n").is_ok());
        for bad in [
            "actuaria/prompt-slim 0.1.0 claude",
            format!("acme/prompt-slim 0.1.0 claude sha256:{SHA_A}").as_str(),
            format!("actuaria/prompt-slim 0.1.0 vscode sha256:{SHA_A}").as_str(),
            format!("actuaria/prompt-slim 0.1.0 claude sha512:{SHA_A}").as_str(),
            "actuaria/prompt-slim 0.1.0 claude sha256:abc",
            format!("actuaria/prompt-slim v0.1.0 claude sha256:{SHA_A}").as_str(),
            format!("actuaria/prompt-slim 0.1.0 claude sha256:{SHA_A} extra").as_str(),
            "<html>captive portal</html>",
        ] {
            assert!(Lock::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn versions() {
        for ok in ["0.1.0", "10.20.30", "1.0.0-rc.1", "1.0.0+build.5"] {
            assert!(valid_version(ok), "{ok}");
        }
        for bad in ["", "1", "1.0", "1.0.0.0", "v1.0.0", "1.0.0-", "1.0.x", "1.0.0 ", "1.0.0-<b>"] {
            assert!(!valid_version(bad), "{bad}");
        }
    }
}
