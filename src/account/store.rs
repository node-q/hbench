//! Where the refresh token lives: the OS keychain (macOS Keychain, Windows Credential Manager, the Secret
//! Service on Linux), never a file (D120: no plaintext fallback).

use anyhow::{Result, anyhow};

/// The keychain's service name for hbench's sign-in.
const SERVICE: &str = "io.actuaria.hbench.signin";

pub trait Store: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}

/// The OS keychain.
pub struct Keychain;

/// keyring's Secret Service backend blocks on its own executor, which must not run inside a tokio worker:
/// each call gets a thread of its own.
fn off_runtime<T: Send>(f: impl FnOnce() -> keyring::Result<T> + Send) -> Result<T> {
    std::thread::scope(|s| s.spawn(f).join())
        .map_err(|_| anyhow!("the keychain call panicked"))?
        .map_err(|e| anyhow!("the OS keychain: {e}"))
}

impl Store for Keychain {
    fn get(&self, key: &str) -> Result<Option<String>> {
        off_runtime(|| match keyring::Entry::new(SERVICE, key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e),
        })
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        off_runtime(|| keyring::Entry::new(SERVICE, key)?.set_password(value))
    }

    fn delete(&self, key: &str) -> Result<()> {
        off_runtime(|| match keyring::Entry::new(SERVICE, key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e),
        })
    }
}

/// An in-memory store, for tests.
#[cfg(test)]
#[derive(Default)]
pub struct Memory(pub std::sync::Mutex<std::collections::HashMap<String, String>>);

#[cfg(test)]
impl Store for Memory {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        self.0.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}
