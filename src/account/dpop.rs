//! DPoP (RFC 9449) for the browser sign-in: the refresh token is bound to a P-256 key made at sign-in and kept in
//! the OS keychain next to it, so a refresh token copied off the machine is useless without the key. Keycloak's
//! policy for the public clients binds the refresh token only; access tokens stay Bearer.
//!
//! Neither the key nor a proof is ever logged or put in an error message.

use super::store::Store;
use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use serde_json::json;
use sha2::{Digest, Sha256};

/// The sign-in's proof-of-possession key. Its `Debug` shows the thumbprint only.
pub struct Key(SigningKey);

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dpop::Key({})", self.thumbprint())
    }
}

/// The keychain entry of the key, next to the refresh token's `store_key`.
pub fn store_key(refresh_key: &str) -> String {
    format!("dpop:{refresh_key}")
}

impl Key {
    /// A new key from the OS random source.
    pub fn generate() -> Key {
        loop {
            let mut b = [0u8; 32];
            getrandom::fill(&mut b).expect("the OS random source failed");
            // Zero or a value past the group order is refused; the chance is about 2^-32.
            if let Ok(k) = SigningKey::from_slice(&b) {
                return Key(k);
            }
        }
    }

    /// The key stored for `refresh_key`; `None` when it is missing or is not a P-256 PKCS#8 key (a damaged key
    /// is as unusable as a missing one). A keychain that can't be read is an error.
    pub fn load(store: &dyn Store, refresh_key: &str) -> Result<Option<Key>> {
        let Some(pem) = store.get(&store_key(refresh_key))? else { return Ok(None) };
        Ok(SigningKey::from_pkcs8_pem(&pem).ok().map(Key))
    }

    /// Keep the key (PKCS#8, PEM) in the store.
    pub fn save(&self, store: &dyn Store, refresh_key: &str) -> Result<()> {
        let Ok(pem) = self.0.to_pkcs8_pem(LineEnding::LF) else { bail!("encoding the sign-in key failed") };
        store.set(&store_key(refresh_key), &pem)
    }

    /// The public key as a JWK, its members in the order RFC 7638 hashes them.
    pub fn jwk(&self) -> serde_json::Value {
        let point = self.0.verifying_key().to_sec1_point(false);
        let (x, y) = (point.x().expect("an uncompressed point"), point.y().expect("an uncompressed point"));
        json!({"crv": "P-256", "kty": "EC", "x": URL_SAFE_NO_PAD.encode(x), "y": URL_SAFE_NO_PAD.encode(y)})
    }

    /// The JWK SHA-256 thumbprint (RFC 7638), base64url: what Keycloak binds the refresh token to (`cnf.jkt`).
    pub fn thumbprint(&self) -> String {
        let j = self.jwk();
        let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#, j["x"].as_str().unwrap(), j["y"].as_str().unwrap());
        URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
    }

    /// A DPoP proof for one request: `htm` the method, `htu` the URL without query or fragment, `nonce` the
    /// server's `DPoP-Nonce` when it asked for one.
    pub fn proof(&self, htm: &str, htu: &str, nonce: Option<&str>) -> String {
        let header = json!({"typ": "dpop+jwt", "alg": "ES256", "jwk": self.jwk()});
        let iat = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let mut claims = json!({"jti": super::random_b64(16), "htm": htm, "htu": htu_of(htu), "iat": iat});
        if let Some(n) = nonce {
            claims["nonce"] = json!(n);
        }
        let input = format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()));
        let sig: Signature = self.0.sign(input.as_bytes());
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
    }
}

/// The `htu` of a URL: no query, no fragment (RFC 9449 section 4.2).
fn htu_of(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}
