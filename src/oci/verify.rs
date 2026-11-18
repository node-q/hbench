//! The signature check a pulled version passes before any of its tasks is unpacked (hbench-studio
//! `docs/design/market-platform.md` D116 and §7).
//!
//! The signing format is not decided yet, so the check that runs is [`Pending`], which accepts every version.
//! The real check implements [`Verifier`] and replaces it in [`configured`]; [`crate::oci::download`] already calls
//! it at the one place every OCI task is unpacked.
//!
//! Skipping the check takes the explicit `--insecure-skip-signature` flag (hbench-studio
//! `docs/design/threat-model-signing.md` §6, V-1): it holds for one run only, since no environment variable and no
//! config key sets it; it is refused for the market's registry; and every version it lets through is logged.

use super::store::Store;
use anyhow::{Result, bail};
use std::sync::atomic::{AtomicBool, Ordering};

/// The version a check is asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    /// `<registry>/<repository>@<digest>`, for messages.
    pub reference: String,
    pub registry: String,
    pub repository: String,
    pub manifest_digest: String,
    /// The name the signatures must be bound to: for a market version the `<publisher>/<listing>:<tag>` that was
    /// asked for, never a name the artifact states about itself or the repository it was pulled from (D116).
    /// Any other dataset has `<name>:<version>`.
    pub canonical_name: String,
}

/// A signature check. `store` is where `subject` was pulled from, for reading its referrers.
#[async_trait::async_trait]
pub trait Verifier: Send + Sync {
    async fn verify(&self, store: &Store, subject: &Subject) -> Result<()>;
}

/// The check until the signing format is decided: it accepts every version.
pub struct Pending;

#[async_trait::async_trait]
impl Verifier for Pending {
    async fn verify(&self, _: &Store, _: &Subject) -> Result<()> {
        Ok(())
    }
}

/// No check, by `--insecure-skip-signature`: each version it lets through is logged, and a version from the
/// market's registry is refused, since the bypass never applies to market content.
pub struct Skipped;

#[async_trait::async_trait]
impl Verifier for Skipped {
    async fn verify(&self, _: &Store, subject: &Subject) -> Result<()> {
        if super::market::is_market(&subject.registry) {
            bail!(
                "{SKIP_FLAG} does not apply to the market's registry, {}: a market version is never unpacked unchecked. Run again without {SKIP_FLAG}.",
                subject.registry
            );
        }
        eprintln!("{}", skipped_line(subject));
        Ok(())
    }
}

fn skipped_line(subject: &Subject) -> String {
    format!("warning: {SKIP_FLAG}: unpacking {} ({}) without checking its signatures", subject.canonical_name, subject.reference)
}

pub const SKIP_FLAG: &str = "--insecure-skip-signature";

static SKIP: AtomicBool = AtomicBool::new(false);

/// Turn the signature check off for this process, as `--insecure-skip-signature` asks; logged once here and again
/// for each version unpacked. Only that flag calls this: no environment variable or config key does.
pub fn skip() {
    eprintln!("warning: {SKIP_FLAG}: pulled versions are unpacked without checking their signatures (refused for the market's registry)");
    SKIP.store(true, Ordering::SeqCst);
}

/// The check every pulled version passes before it is unpacked.
pub fn configured() -> &'static dyn Verifier {
    if SKIP.load(Ordering::SeqCst) { &Skipped } else { &Pending }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject(registry: &str) -> Subject {
        Subject {
            reference: format!("{registry}/acme/actuaria/starter@sha256:00"),
            registry: registry.into(),
            repository: "acme/actuaria/starter".into(),
            manifest_digest: "sha256:00".into(),
            canonical_name: "actuaria/starter:0.2".into(),
        }
    }

    #[test]
    fn a_skipped_check_names_the_version_it_lets_through() {
        let s = subject("registry.example");
        let line = skipped_line(&s);
        assert!(
            line.contains("--insecure-skip-signature") && line.contains("actuaria/starter:0.2") && line.contains(&s.reference),
            "{line}"
        );
    }

    #[tokio::test]
    async fn the_bypass_is_refused_for_the_markets_registry_under_any_spelling() {
        // The check reads nothing from the store, so any store does.
        let store = Store::open("oci-layout:/nonexistent", std::path::Path::new("/nonexistent")).unwrap();
        for market in ["registry.actuaria.io", "registry.actuaria.io:443", "REGISTRY.ACTUARIA.IO", "registry.actuaria.io."] {
            let err = Skipped.verify(&store, &subject(market)).await.unwrap_err().to_string();
            assert!(err.contains("--insecure-skip-signature does not apply to the market's registry"), "{market}: {err}");
        }
        Skipped.verify(&store, &subject("registry.example")).await.unwrap();
        Skipped.verify(&store, &subject("oci-layout:/somewhere")).await.unwrap();
    }

    #[test]
    fn only_the_flag_turns_the_check_off() {
        // No environment variable or config key reaches `skip`: main.rs calls it for the flag, and nothing else does.
        let sources = [include_str!("../main.rs"), include_str!("../config.rs"), include_str!("../env.rs"), include_str!("mod.rs")];
        let calls: usize = sources.iter().map(|s| s.matches("verify::skip()").count()).sum();
        assert_eq!(calls, 1, "verify::skip() is called from one place, the flag");
        let cli = include_str!("../main.rs");
        let flag = cli.find("insecure_skip_signature: bool").expect("the flag");
        let arg = &cli[cli[..flag].rfind("#[arg(").unwrap()..flag];
        assert!(!arg.contains("env"), "the flag reads no environment variable: {arg}");
    }
}
