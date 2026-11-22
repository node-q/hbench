//! Downloads go to a private folder (0700 on Unix) under the app's data folder, are capped at the catalogue's
//! size (at most 5 MB), and are written only once their SHA-256 matches the catalogue's.

use crate::catalogue::{Artefact, MAX_ARTEFACT_BYTES};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const TEMP_DIR_NAME: &str = "install-tmp";

/// A private temp folder, removed (with what is in it) when dropped.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn path(&self) -> &Path {
        &self.0
    }
    /// Keep the folder (the caller removes it later).
    pub fn keep(self) -> PathBuf {
        let p = self.0.clone();
        std::mem::forget(self);
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn private_dir(p: &Path, recursive: bool) -> std::io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(recursive);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(p)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// A new, empty private folder under `<data_dir>/install-tmp`.
pub fn temp_dir(data_dir: &Path) -> std::io::Result<TempDir> {
    static N: AtomicU32 = AtomicU32::new(0);
    let base = data_dir.join(TEMP_DIR_NAME);
    private_dir(&base, true)?;
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let dir = base.join(format!("{}-{t}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    // `create` (not recursive) fails if it exists, so the folder is always new.
    private_dir(&dir, false)?;
    Ok(TempDir(dir))
}

/// Remove what earlier runs left in `<data_dir>/install-tmp`.
pub fn sweep(data_dir: &Path) {
    let _ = fs::remove_dir_all(data_dir.join(TEMP_DIR_NAME));
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The download's cap: the catalogue's size, and never over 5 MB.
pub fn cap(a: &Artefact) -> u64 {
    a.size.min(MAX_ARTEFACT_BYTES)
}

/// Check `bytes` against the catalogue's size and SHA-256, then write them to `dir/name` (a new file).
pub fn verify_and_write(bytes: &[u8], a: &Artefact, dir: &Path, name: &str) -> Result<PathBuf, String> {
    if bytes.len() as u64 != a.size || bytes.len() as u64 > MAX_ARTEFACT_BYTES {
        return Err(format!("the download is {} bytes; the catalogue says {}", bytes.len(), a.size));
    }
    let got = sha256_hex(bytes);
    if got != a.sha256 {
        return Err(format!("the download's SHA-256 is {got}; the catalogue says {}", a.sha256));
    }
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        return Err("bad file name".into());
    }
    let path = dir.join(name);
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&path).map_err(|e| e.to_string())?;
    f.write_all(bytes).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artefact(bytes: &[u8]) -> Artefact {
        Artefact { url: String::new(), sha256: sha256_hex(bytes), size: bytes.len() as u64 }
    }

    #[test]
    fn verifies_and_refuses_mismatch() {
        let data = std::env::temp_dir().join(format!("ai-test-{}", std::process::id()));
        let t = temp_dir(&data).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(t.path()).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(fs::metadata(data.join(TEMP_DIR_NAME)).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let good = b"the artefact";
        let a = artefact(good);
        let p = verify_and_write(good, &a, t.path(), "bot.mcpb").unwrap();
        assert_eq!(fs::read(&p).unwrap(), good);
        // One byte changed, same size.
        let err = verify_and_write(b"the artefacT", &a, t.path(), "bot2.mcpb").unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        assert!(!t.path().join("bot2.mcpb").exists());
        assert!(verify_and_write(b"the artefact!", &a, t.path(), "bot3.mcpb").is_err());
        assert!(verify_and_write(good, &a, t.path(), "../x").is_err());
        let dir = t.path().to_path_buf();
        drop(t);
        assert!(!dir.exists(), "dropped temp dirs are removed");
        sweep(&data);
        let _ = fs::remove_dir_all(&data);
    }
}
