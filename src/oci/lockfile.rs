//! `hbench.lock`, in the directory hbench runs in: the manifest digest each OCI dataset (`name@version`)
//! resolved to. A mirror serving the same dataset has the same digest, so the registry is not part of the key.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const FILE: &str = "hbench.lock";
const FORMAT_VERSION: u32 = 1;
const HEADER: &str = "# Written by hbench (--lock-write): the manifest digest of each OCI dataset, by name@version.\n";

/// How `hbench.lock` is used when a dataset's tag is resolved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LockMode {
    /// A locked dataset runs at its locked digest; a tag that moved only gets a warning.
    #[default]
    Read,
    /// Every OCI dataset must be locked, and its tag must still resolve to the locked digest.
    Locked,
    /// Record each dataset's digest, adding or updating entries.
    Write,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Lockfile {
    pub version: u32,
    #[serde(default)]
    pub datasets: BTreeMap<String, String>,
}

/// The lockfile at `path`; None when there is none.
pub fn read(path: &Path) -> Result<Option<Lockfile>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let table: toml::Table = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let lock: Lockfile = crate::config::strict(toml::Value::Table(table)).with_context(|| format!("parsing {}", path.display()))?;
    if lock.version != FORMAT_VERSION {
        bail!("{}: version {} is not supported (this hbench writes version {FORMAT_VERSION})", path.display(), lock.version);
    }
    if let Some((key, digest)) = lock.datasets.iter().find(|(_, d)| !super::format::is_sha256_digest(d)) {
        bail!("{}: {key} = {digest:?} is not a sha256 digest", path.display());
    }
    Ok(Some(lock))
}

pub fn write(path: &Path, datasets: &BTreeMap<String, String>) -> Result<()> {
    let lock = Lockfile { version: FORMAT_VERSION, datasets: datasets.clone() };
    crate::trial::write_atomic(path, format!("{HEADER}{}", toml::to_string(&lock)?)).with_context(|| format!("writing {}", path.display()))
}
