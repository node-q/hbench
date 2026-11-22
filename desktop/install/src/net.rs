//! HTTPS fetches with a size cap: HTTPS only, no redirects at all, and only from market.actuaria.io.

use std::io::Read;
use std::time::Duration;
use url::Url;

const ALLOWED_HOST: &str = "market.actuaria.io";

/// Fetch `url` and return its body if it is a 200 of at most `cap` bytes.
pub fn get(url: &str, cap: u64) -> Result<Vec<u8>, String> {
    let u = Url::parse(url).map_err(|e| e.to_string())?;
    if u.scheme() != "https" || u.host_str() != Some(ALLOWED_HOST) || u.port().is_some() {
        return Err(format!("refusing to fetch {url}"));
    }
    // TLS through rustls with ring (no aws-lc build); a no-op if the app already installed a provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("ActuariaStudioEssentials-install/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(u).send().map_err(|e| e.to_string())?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(format!("{url} answered {}", resp.status()));
    }
    if resp.content_length().is_some_and(|n| n > cap) {
        return Err(format!("{url} is larger than {cap} bytes"));
    }
    read_capped(resp, cap)
}

/// Read at most `cap` bytes; more than that is an error, never a truncated body.
pub fn read_capped(r: impl Read, cap: u64) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    r.take(cap + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.len() as u64 > cap {
        return Err(format!("the download is larger than {cap} bytes"));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps() {
        assert_eq!(read_capped(&b"abcd"[..], 4).unwrap(), b"abcd");
        assert!(read_capped(&b"abcde"[..], 4).is_err());
    }

    #[test]
    fn refuses_other_urls_before_fetching() {
        for u in [
            "http://market.actuaria.io/x",
            "https://evil.com/x",
            "https://raw.githubusercontent.com/x",
            "https://market.actuaria.io:444/x",
            "file:///etc/passwd",
        ] {
            assert!(get(u, 10).unwrap_err().starts_with("refusing"), "{u}");
        }
    }
}
