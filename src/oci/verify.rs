//! The signature check a pulled version passes before any of its tasks is unpacked (hbench-studio
//! `docs/design/market-platform.md` D116 and §7).
//!
//! The signing format is not decided yet, so the check that runs is [`Pending`], which accepts every version.
//! The real check implements [`Verifier`] and replaces it in [`configured`]; [`crate::oci::download`] already calls
//! it at the one place every OCI task is unpacked. Skipping the check takes the explicit `--skip-signature-check`
//! flag, and every version it lets through unchecked is logged.

use super::store::Store;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};

/// The version a check is asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    /// `<registry>/<repository>@<digest>`, for messages.
    pub reference: String,
    pub registry: String,
    pub repository: String,
    pub manifest_digest: String,
    /// The name the signatures are bound to: `<publisher>/<listing>:<tag>` as the version's own `listing.json`
    /// states it, never the repository it was pulled from (D116). A version without a listing has
    /// `<name>:<version>` from its config.
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

/// No check, by `--skip-signature-check`: each version it lets through is logged.
pub struct Skipped;

#[async_trait::async_trait]
impl Verifier for Skipped {
    async fn verify(&self, _: &Store, subject: &Subject) -> Result<()> {
        eprintln!("{}", skipped_line(subject));
        Ok(())
    }
}

fn skipped_line(subject: &Subject) -> String {
    format!("warning: --skip-signature-check: unpacking {} ({}) without checking its signatures", subject.canonical_name, subject.reference)
}

pub const SKIP_FLAG: &str = "--skip-signature-check";

static SKIP: AtomicBool = AtomicBool::new(false);

/// Turn the signature check off for this process, as `--skip-signature-check` asks; logged once here and again
/// for each version unpacked.
pub fn skip() {
    eprintln!("warning: {SKIP_FLAG}: pulled versions are unpacked without checking their signatures");
    SKIP.store(true, Ordering::SeqCst);
}

/// The check every pulled version passes before it is unpacked.
pub fn configured() -> &'static dyn Verifier {
    if SKIP.load(Ordering::SeqCst) { &Skipped } else { &Pending }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skipped_check_names_the_version_it_lets_through() {
        let s = Subject {
            reference: "registry.actuaria.io/acme/actuaria/starter@sha256:00".into(),
            registry: "registry.actuaria.io".into(),
            repository: "acme/actuaria/starter".into(),
            manifest_digest: "sha256:00".into(),
            canonical_name: "actuaria/starter:0.2".into(),
        };
        let line = skipped_line(&s);
        assert!(line.contains(SKIP_FLAG) && line.contains("actuaria/starter:0.2") && line.contains(&s.reference), "{line}");
    }
}
