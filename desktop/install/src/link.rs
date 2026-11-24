//! The install link: `actuaria://install/actuaria/<bot-id>`, optionally `?target=claude` or `?target=orca`.
//!
//! Parsed byte by byte rather than through a URL parser, so nothing is normalised first: the accepted form is
//! exact, and anything else (another host or path, userinfo, a port, percent-encoding, a fragment, extra or
//! repeated parameters, over 105 bytes) is refused.

use serde::Serialize;
use std::fmt;

pub const SCHEME: &str = "actuaria";
pub const MAX_LINK_BYTES: usize = 105;
const PREFIX: &str = "actuaria://install/actuaria/";

/// The host an artefact installs into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Claude,
    Orca,
}

impl Target {
    pub const ALL: [Target; 2] = [Target::Claude, Target::Orca];

    pub fn as_str(self) -> &'static str {
        match self {
            Target::Claude => "claude",
            Target::Orca => "orca",
        }
    }

    pub fn from_name(s: &str) -> Option<Target> {
        match s {
            "claude" => Some(Target::Claude),
            "orca" => Some(Target::Orca),
            _ => None,
        }
    }

    /// The host's name as the user knows it.
    pub fn host_name(self) -> &'static str {
        match self {
            Target::Claude => "Claude Desktop",
            Target::Orca => "Orca",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The bot's id; its publisher is always `actuaria`.
    pub bot: String,
    pub target: Option<Target>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    TooLong,
    BadCharacter,
    NotAnInstallLink,
    BadBotId,
    BadQuery,
    NotARunLink,
    BadDatasetName,
    BadVersion,
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LinkError::TooLong => "the link is too long",
            LinkError::BadCharacter => "the link has a character that isn't allowed",
            LinkError::NotAnInstallLink => "it isn't an Actuaria install link",
            LinkError::BadBotId => "the bot name isn't valid",
            LinkError::BadQuery => "the link has parameters that aren't allowed",
            LinkError::NotARunLink => "it isn't an Actuaria link to a test set",
            LinkError::BadDatasetName => "the test set's name isn't valid",
            LinkError::BadVersion => "the version isn't valid",
        })
    }
}

/// `^[a-z][a-z0-9-]{0,62}$`
pub fn valid_bot_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// The checks every `actuaria:` link passes first: at most `max` bytes of printable ASCII (no spaces, controls
/// or non-ASCII), and no percent-encoding, fragment or backslash at all.
pub(crate) fn check_raw(raw: &str, max: usize) -> Result<(), LinkError> {
    if raw.len() > max {
        return Err(LinkError::TooLong);
    }
    if !raw.bytes().all(|c| (0x21..=0x7e).contains(&c)) || raw.contains(['%', '#', '\\']) {
        return Err(LinkError::BadCharacter);
    }
    Ok(())
}

pub fn parse(raw: &str) -> Result<Link, LinkError> {
    check_raw(raw, MAX_LINK_BYTES)?;
    let rest = raw.strip_prefix(PREFIX).ok_or(LinkError::NotAnInstallLink)?;
    let (id, query) = match rest.split_once('?') {
        Some((id, q)) => (id, Some(q)),
        None => (rest, None),
    };
    if !valid_bot_id(id) {
        return Err(LinkError::BadBotId);
    }
    let target = match query {
        None => None,
        // Exactly one parameter, spelt exactly: no empty query, no repeats, nothing else.
        Some(q) => Some(q.strip_prefix("target=").and_then(Target::from_name).ok_or(LinkError::BadQuery)?),
    };
    Ok(Link { bot: id.to_string(), target })
}

/// The link from a launch's arguments. The OS handler is registered as `<app> -- <link>`, so the link is the
/// one argument after `--`; anything else (no `--`, nothing or more than one argument after it) is no link.
pub fn from_args<S: AsRef<str>>(args: &[S]) -> Option<&str> {
    let i = args.iter().position(|a| a.as_ref() == "--")?;
    match &args[i + 1..] {
        [one] => Some(one.as_ref()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn accepts() {
        let ok = [
            ("actuaria://install/actuaria/prompt-slim", "prompt-slim", None),
            ("actuaria://install/actuaria/a", "a", None),
            ("actuaria://install/actuaria/b0t-9?target=claude", "b0t-9", Some(Target::Claude)),
            ("actuaria://install/actuaria/prompt-slim?target=orca", "prompt-slim", Some(Target::Orca)),
        ];
        for (raw, id, target) in ok {
            assert_eq!(parse(raw), Ok(Link { bot: id.into(), target }), "{raw}");
        }
        let max_id = format!("{PREFIX}a{}", "b".repeat(62));
        assert!(parse(&max_id).is_ok());
    }

    #[test]
    fn refuses() {
        let long_id = format!("{PREFIX}a{}", "b".repeat(63));
        let too_long = format!("{PREFIX}a?target=orca{}", "x".repeat(300));
        let bad = [
            "",
            "actuaria:",
            "actuaria://install/actuaria/",
            "actuaria://install/actuaria",
            "actuaria://install/actuaria/prompt-slim/",
            "actuaria://install/actuaria/prompt-slim/x",
            "actuaria://install/acme/prompt-slim",
            "actuaria://install/Actuaria/prompt-slim",
            "actuaria://install//actuaria/prompt-slim",
            "actuaria://Install/actuaria/prompt-slim",
            "actuaria://open/actuaria/prompt-slim",
            "ACTUARIA://install/actuaria/prompt-slim",
            "https://install/actuaria/prompt-slim",
            "actuaria:install/actuaria/prompt-slim",
            "actuaria:///install/actuaria/prompt-slim",
            "actuaria://user@install/actuaria/prompt-slim",
            "actuaria://user:pw@install/actuaria/prompt-slim",
            "actuaria://install:80/actuaria/prompt-slim",
            "actuaria://install/actuaria/Prompt-slim",
            "actuaria://install/actuaria/1bot",
            "actuaria://install/actuaria/-bot",
            "actuaria://install/actuaria/bot_x",
            "actuaria://install/actuaria/bot.x",
            "actuaria://install/actuaria/../bot",
            "actuaria://install/actuaria/a%2fb",
            "actuaria://install/actuaria/a%2Fb",
            "actuaria://install/actuaria%2Fprompt-slim",
            "actuaria://install/actuaria/a\\b",
            "actuaria://install/actuaria/bot#x",
            "actuaria://install/actuaria/bot?",
            "actuaria://install/actuaria/bot?target=",
            "actuaria://install/actuaria/bot?target=vscode",
            "actuaria://install/actuaria/bot?target=Claude",
            "actuaria://install/actuaria/bot?Target=claude",
            "actuaria://install/actuaria/bot?target=claude&target=orca",
            "actuaria://install/actuaria/bot?target=claude&target=claude",
            "actuaria://install/actuaria/bot?target=claude&x=1",
            "actuaria://install/actuaria/bot?x=1",
            "actuaria://install/actuaria/bot?target=claude?",
            "actuaria://install/actuaria/bot?&target=claude",
            "actuaria://install/actuaria/bot?target=claude;x",
            "actuaria://install/actuaria/bot ?target=claude",
            "actuaria://install/actuaria/bot\n",
            " actuaria://install/actuaria/bot",
            "actuaria://install/actuaria/bøt",
            "actuaria://install/actuaria/bot\u{202e}",
            "--actuaria://install/actuaria/bot",
            &long_id,
            &too_long,
        ];
        for raw in bad {
            assert!(parse(raw).is_err(), "accepted {raw:?}");
        }
        assert_eq!(parse(&too_long), Err(LinkError::TooLong));
    }

    #[test]
    fn length_cap_is_105_bytes() {
        // The longest valid link: a 63-byte id and `?target=claude`.
        let at_cap = format!("{PREFIX}a{}?target=claude", "b".repeat(62));
        assert_eq!(at_cap.len(), 105);
        assert_eq!(parse(&at_cap), Ok(Link { bot: format!("a{}", "b".repeat(62)), target: Some(Target::Claude) }));
        let over = format!("{at_cap}x");
        assert_eq!(over.len(), 106);
        assert_eq!(parse(&over), Err(LinkError::TooLong));
    }

    #[test]
    fn args_take_only_the_one_after_the_separator() {
        let link = "actuaria://install/actuaria/bot";
        assert_eq!(from_args(&["/app", "--", link]), Some(link));
        assert_eq!(from_args(&["/app", link]), None);
        assert_eq!(from_args(&["/app", "--"]), None);
        assert_eq!(from_args(&["/app", "--", link, "--x"]), None);
        assert_eq!(from_args::<&str>(&[]), None);
    }

    proptest! {
        // Whatever the input, the parser never panics, and anything it accepts is exactly the canonical form.
        #[test]
        fn fuzz_any_string(s in "\\PC{0,300}") {
            if let Ok(l) = parse(&s) {
                let canon = match l.target {
                    Some(t) => format!("{PREFIX}{}?target={}", l.bot, t.as_str()),
                    None => format!("{PREFIX}{}", l.bot),
                };
                prop_assert_eq!(canon, s);
            }
        }

        #[test]
        fn fuzz_near_misses(id in "[a-zA-Z0-9._~%/:@?&=#-]{0,80}", q in proptest::option::of("[a-z=&?%#]{0,30}")) {
            let s = match &q { Some(q) => format!("{PREFIX}{id}?{q}"), None => format!("{PREFIX}{id}") };
            match parse(&s) {
                Ok(l) => {
                    prop_assert!(valid_bot_id(&l.bot));
                    prop_assert_eq!(&l.bot, &id);
                    prop_assert!(matches!(q.as_deref(), None | Some("target=claude") | Some("target=orca")));
                }
                Err(_) => prop_assert!(!valid_bot_id(&id) || !matches!(q.as_deref(), None | Some("target=claude") | Some("target=orca"))),
            }
        }
    }
}
