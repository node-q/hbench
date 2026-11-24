//! The run link: `actuaria://run/actuaria/<dataset>`, optionally `?version=<x.y>` or `?version=<x.y.z>`.
//!
//! It opens New run with that test set picked, and nothing more: no run starts, nothing downloads. Parsed as
//! strictly as the install link (`link.rs`, whose first checks it shares): the accepted form is exact, and
//! anything else is refused. The name is only looked up in the catalogue the app already has; nothing in the
//! link is ever fetched.

use crate::link::{LinkError, check_raw};

pub const PREFIX: &str = "actuaria://run/actuaria/";
pub const MAX_DATASET_BYTES: usize = 64;
/// The prefix, the longest name and `?version=` with three six-digit numbers.
pub const MAX_RUN_LINK_BYTES: usize = PREFIX.len() + MAX_DATASET_BYTES + "?version=".len() + 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLink {
    /// The test set's name in the catalogue; its publisher is always `actuaria`.
    pub dataset: String,
    pub version: Option<String>,
}

/// Whether a link is meant as a run link (it still has to parse): the app sends it here rather than to the
/// install dialog.
pub fn is_run_link(raw: &str) -> bool {
    raw.starts_with("actuaria://run/")
}

/// `^[a-z0-9]([a-z0-9._-]{0,62}[a-z0-9])?$`, without `..`: the dataset index's names, lowercase.
pub fn valid_dataset_name(s: &str) -> bool {
    let b = s.as_bytes();
    let alnum = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !b.is_empty()
        && b.len() <= MAX_DATASET_BYTES
        && alnum(&b[0])
        && alnum(&b[b.len() - 1])
        && b.iter().all(|c| alnum(c) || matches!(c, b'.' | b'_' | b'-'))
        && !s.contains("..")
}

/// `x.y` or `x.y.z`, each part 1 to 6 digits.
pub fn valid_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    (2..=3).contains(&parts.len()) && parts.iter().all(|p| (1..=6).contains(&p.len()) && p.bytes().all(|c| c.is_ascii_digit()))
}

pub fn parse(raw: &str) -> Result<RunLink, LinkError> {
    check_raw(raw, MAX_RUN_LINK_BYTES)?;
    let rest = raw.strip_prefix(PREFIX).ok_or(LinkError::NotARunLink)?;
    let (name, query) = match rest.split_once('?') {
        Some((n, q)) => (n, Some(q)),
        None => (rest, None),
    };
    if !valid_dataset_name(name) {
        return Err(LinkError::BadDatasetName);
    }
    let version = match query {
        None => None,
        // Exactly one parameter, spelt exactly: no empty query, no repeats, nothing else.
        Some(q) => {
            let v = q.strip_prefix("version=").ok_or(LinkError::BadQuery)?;
            if !valid_version(v) {
                return Err(if v.contains(['&', '=', '?', ';']) { LinkError::BadQuery } else { LinkError::BadVersion });
            }
            Some(v.to_string())
        }
    };
    Ok(RunLink { dataset: name.to_string(), version })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ok(name: &str, version: Option<&str>) -> Result<RunLink, LinkError> {
        Ok(RunLink { dataset: name.into(), version: version.map(Into::into) })
    }

    #[test]
    fn accepts() {
        assert_eq!(parse("actuaria://run/actuaria/terminal-bench"), ok("terminal-bench", None));
        assert_eq!(parse("actuaria://run/actuaria/terminal-bench?version=2.0"), ok("terminal-bench", Some("2.0")));
        assert_eq!(parse("actuaria://run/actuaria/swebench-verified?version=1.0.12"), ok("swebench-verified", Some("1.0.12")));
        assert_eq!(parse("actuaria://run/actuaria/bigcodebench_hard"), ok("bigcodebench_hard", None));
        assert_eq!(parse("actuaria://run/actuaria/aime.2025"), ok("aime.2025", None));
        assert_eq!(parse("actuaria://run/actuaria/a"), ok("a", None));
        assert_eq!(parse("actuaria://run/actuaria/9"), ok("9", None));
    }

    #[test]
    fn refuses() {
        let long_name = format!("{PREFIX}a{}", "b".repeat(64));
        let bad: &[(&str, LinkError)] = &[
            ("", LinkError::NotARunLink),
            ("actuaria://run/actuaria/", LinkError::BadDatasetName),
            ("actuaria://run/actuaria", LinkError::NotARunLink),
            ("actuaria://run/acme/terminal-bench", LinkError::NotARunLink),
            ("actuaria://run/Actuaria/terminal-bench", LinkError::NotARunLink),
            ("actuaria://run//actuaria/terminal-bench", LinkError::NotARunLink),
            ("actuaria://Run/actuaria/terminal-bench", LinkError::NotARunLink),
            ("ACTUARIA://run/actuaria/terminal-bench", LinkError::NotARunLink),
            ("actuaria:run/actuaria/terminal-bench", LinkError::NotARunLink),
            ("actuaria://user@run/actuaria/terminal-bench", LinkError::NotARunLink),
            ("actuaria://run:80/actuaria/terminal-bench", LinkError::NotARunLink),
            ("actuaria://install/actuaria/terminal-bench", LinkError::NotARunLink),
            ("https://market.actuaria.io/run/actuaria/x", LinkError::NotARunLink),
            ("actuaria://run/actuaria/terminal-bench/", LinkError::BadDatasetName),
            ("actuaria://run/actuaria/terminal-bench/x", LinkError::BadDatasetName),
            ("actuaria://run/actuaria/Terminal-bench", LinkError::BadDatasetName),
            ("actuaria://run/actuaria/-bench", LinkError::BadDatasetName),
            ("actuaria://run/actuaria/bench-", LinkError::BadDatasetName),
            ("actuaria://run/actuaria/.bench", LinkError::BadDatasetName),
            ("actuaria://run/actuaria/a..b", LinkError::BadDatasetName),
            ("actuaria://run/actuaria/../x", LinkError::BadDatasetName),
            ("actuaria://run/actuaria/a:b", LinkError::BadDatasetName),
            ("actuaria://run/actuaria/a@b", LinkError::BadDatasetName),
            ("actuaria://run/actuaria/a%2fb", LinkError::BadCharacter),
            ("actuaria://run/actuaria/a\\b", LinkError::BadCharacter),
            ("actuaria://run/actuaria/x#y", LinkError::BadCharacter),
            ("actuaria://run/actuaria/x y", LinkError::BadCharacter),
            ("actuaria://run/actuaria/x\n", LinkError::BadCharacter),
            (" actuaria://run/actuaria/x", LinkError::BadCharacter),
            ("actuaria://run/actuaria/bénch", LinkError::BadCharacter),
            ("actuaria://run/actuaria/x?", LinkError::BadQuery),
            ("actuaria://run/actuaria/x?version=", LinkError::BadVersion),
            ("actuaria://run/actuaria/x?version=2", LinkError::BadVersion),
            ("actuaria://run/actuaria/x?version=2.", LinkError::BadVersion),
            ("actuaria://run/actuaria/x?version=.2", LinkError::BadVersion),
            ("actuaria://run/actuaria/x?version=1.2.3.4", LinkError::BadVersion),
            ("actuaria://run/actuaria/x?version=v1.2", LinkError::BadVersion),
            ("actuaria://run/actuaria/x?version=1.2-beta", LinkError::BadVersion),
            ("actuaria://run/actuaria/x?version=latest", LinkError::BadVersion),
            ("actuaria://run/actuaria/x?version=1234567.0", LinkError::BadVersion),
            ("actuaria://run/actuaria/x?Version=1.0", LinkError::BadQuery),
            ("actuaria://run/actuaria/x?target=claude", LinkError::BadQuery),
            ("actuaria://run/actuaria/x?run=1", LinkError::BadQuery),
            ("actuaria://run/actuaria/x?url=https://evil.example", LinkError::BadQuery),
            ("actuaria://run/actuaria/x?version=1.0&version=2.0", LinkError::BadQuery),
            ("actuaria://run/actuaria/x?version=1.0&x=1", LinkError::BadQuery),
            ("actuaria://run/actuaria/x?&version=1.0", LinkError::BadQuery),
            ("actuaria://run/actuaria/x?version=1.0?", LinkError::BadQuery),
            ("actuaria://run/actuaria/x?version=1.0;x", LinkError::BadQuery),
        ];
        for (raw, want) in bad {
            assert_eq!(parse(raw).as_ref(), Err(want), "{raw:?}");
        }
        assert_eq!(parse(&long_name), Err(LinkError::BadDatasetName));
    }

    #[test]
    fn length_cap() {
        let name = format!("a{}", "b".repeat(MAX_DATASET_BYTES - 1));
        let at_cap = format!("{PREFIX}{name}?version=123456.123456.123456");
        assert_eq!(at_cap.len(), MAX_RUN_LINK_BYTES);
        assert_eq!(parse(&at_cap), ok(&name, Some("123456.123456.123456")));
        assert_eq!(parse(&format!("{at_cap}0")), Err(LinkError::TooLong));
    }

    #[test]
    fn only_run_links_are_routed_here() {
        assert!(is_run_link("actuaria://run/actuaria/x"));
        assert!(is_run_link("actuaria://run/whatever"));
        assert!(!is_run_link("actuaria://install/actuaria/x"));
        assert!(!is_run_link("ACTUARIA://run/actuaria/x"));
        assert!(!is_run_link(" actuaria://run/actuaria/x"));
    }

    proptest! {
        // Whatever the input, the parser never panics, and anything it accepts is exactly the canonical form.
        #[test]
        fn fuzz_any_string(s in "\\PC{0,300}") {
            if let Ok(l) = parse(&s) {
                let canon = match &l.version {
                    Some(v) => format!("{PREFIX}{}?version={v}", l.dataset),
                    None => format!("{PREFIX}{}", l.dataset),
                };
                prop_assert_eq!(canon, s);
            }
        }

        #[test]
        fn fuzz_near_misses(name in "[a-zA-Z0-9._~%/:@?&=#-]{0,80}", q in proptest::option::of("[a-z0-9.=&?%#]{0,30}")) {
            let s = match &q { Some(q) => format!("{PREFIX}{name}?{q}"), None => format!("{PREFIX}{name}") };
            let good_q = |q: Option<&str>| q.is_none_or(|q| q.strip_prefix("version=").is_some_and(valid_version));
            match parse(&s) {
                Ok(l) => {
                    prop_assert!(valid_dataset_name(&l.dataset));
                    prop_assert_eq!(&l.dataset, &name);
                    prop_assert!(good_q(q.as_deref()));
                }
                Err(_) => prop_assert!(!valid_dataset_name(&name) || !good_q(q.as_deref())),
            }
        }
    }
}
