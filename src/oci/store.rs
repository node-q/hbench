//! Where a dataset's manifests and blobs come from: an OCI registry, pulled anonymously into the
//! content-addressed blob cache, or an OCI image layout on disk. Every manifest and blob is checked against its
//! digest before it is used.

use super::format::{MANIFEST_MEDIA_TYPE, MAX_MANIFEST_BYTES, REF_NAME};
use super::{LAYOUT_SCHEME, Sha256Writer, sha256_digest, verify_file};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use oci_client::Reference;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::secrets::RegistryAuth;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(120);

pub enum Store {
    Registry { client: oci_client::Client, registry: String, blobs: PathBuf },
    Layout { root: PathBuf },
}

fn is_loopback(registry: &str) -> bool {
    let host = match registry.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => registry.split(':').next().unwrap_or_default(),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

impl Store {
    /// The store for a task's or dataset's `registry` (a registry host, or `oci-layout:<dir>`), caching
    /// registry blobs under `cache_root/blobs/sha256`.
    pub fn open(registry: &str, cache_root: &Path) -> Result<Store> {
        if let Some(dir) = registry.strip_prefix(LAYOUT_SCHEME) {
            return Ok(Store::Layout { root: PathBuf::from(dir) });
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let plain_http = if is_loopback(registry) { vec![registry.to_string()] } else { vec![] };
        let config = ClientConfig {
            protocol: ClientProtocol::HttpsExcept(plain_http),
            platform_resolver: None,
            connect_timeout: Some(CONNECT_TIMEOUT),
            read_timeout: Some(READ_TIMEOUT),
            user_agent: concat!("hbench/", env!("CARGO_PKG_VERSION")),
            ..Default::default()
        };
        let client = oci_client::Client::try_from(config).context("creating the OCI registry client")?;
        Ok(Store::Registry { client, registry: registry.to_string(), blobs: cache_root.join("blobs/sha256") })
    }

    pub fn reference(&self, repository: &str, tag_or_digest: &str) -> String {
        let sep = if tag_or_digest.contains(':') { '@' } else { ':' };
        match self {
            Store::Registry { registry, .. } => format!("{registry}/{repository}{sep}{tag_or_digest}"),
            Store::Layout { root } => format!("{LAYOUT_SCHEME}{}/{repository}{sep}{tag_or_digest}", root.display()),
        }
    }

    /// The digest of the manifest `repository:tag` names now.
    pub async fn resolve_tag(&self, repository: &str, tag: &str) -> Result<String> {
        let reference = self.reference(repository, tag);
        match self {
            Store::Registry { client, registry, blobs } => {
                let image = Reference::with_tag(registry.clone(), repository.to_string(), tag.to_string());
                let (body, _) = client
                    .pull_manifest_raw(&image, &RegistryAuth::Anonymous, &[MANIFEST_MEDIA_TYPE])
                    .await
                    .map_err(|e| fetch_error(&reference, e))?;
                if body.len() as u64 > MAX_MANIFEST_BYTES {
                    bail!("{reference}: the manifest is over {MAX_MANIFEST_BYTES} bytes");
                }
                let digest = sha256_digest(&body);
                store_blob(blobs, &digest, &body)?;
                Ok(digest)
            }
            Store::Layout { root } => {
                let layout = root.join(repository);
                let marker = layout.join("oci-layout");
                if !marker.is_file() {
                    bail!("{reference}: {} is not an OCI image layout (no oci-layout file)", layout.display());
                }
                let text =
                    std::fs::read_to_string(layout.join("index.json")).with_context(|| format!("{reference}: reading index.json"))?;
                let index: oci_spec::image::ImageIndex =
                    serde_json::from_str(&text).with_context(|| format!("{reference}: parsing index.json"))?;
                let tagged: Vec<String> = index
                    .manifests()
                    .iter()
                    .filter(|m| m.annotations().as_ref().and_then(|a| a.get(REF_NAME)).is_some_and(|r| r == tag))
                    .map(|m| m.digest().to_string())
                    .collect();
                match tagged.as_slice() {
                    [d] => Ok(d.clone()),
                    [] => bail!("{reference} not found: no manifest in {} has {REF_NAME} {tag}", layout.display()),
                    _ => bail!("{reference}: several manifests in {} have {REF_NAME} {tag}", layout.display()),
                }
            }
        }
    }

    /// The manifest with `digest`, from the layout, the cache or the registry, checked against its digest.
    pub async fn manifest(&self, repository: &str, digest: &str) -> Result<Vec<u8>> {
        let reference = self.reference(repository, digest);
        let (client, registry, blobs) = match self {
            Store::Layout { root } => {
                let bytes = read_limited(&root.join(repository).join("blobs/sha256").join(hex(digest)?), MAX_MANIFEST_BYTES)
                    .with_context(|| reference.clone())?;
                if sha256_digest(&bytes) != digest {
                    bail!("{reference}: the manifest does not match its digest");
                }
                return Ok(bytes);
            }
            Store::Registry { client, registry, blobs } => (client, registry, blobs),
        };
        if let Ok(bytes) = read_limited(&blobs.join(hex(digest)?), MAX_MANIFEST_BYTES)
            && sha256_digest(&bytes) == digest
        {
            return Ok(bytes);
        }
        let image = Reference::with_digest(registry.clone(), repository.to_string(), digest.to_string());
        let (body, _) = client
            .pull_manifest_raw(&image, &RegistryAuth::Anonymous, &[MANIFEST_MEDIA_TYPE])
            .await
            .map_err(|e| fetch_error(&reference, e))?;
        if sha256_digest(&body) != digest {
            bail!("{reference}: the manifest does not match its digest");
        }
        store_blob(blobs, digest, &body)?;
        Ok(body.to_vec())
    }

    /// A small blob (at most `max` bytes) as bytes, checked against its digest and size.
    pub async fn blob_bytes(&self, repository: &str, digest: &str, size: u64, max: u64) -> Result<Vec<u8>> {
        if size > max {
            bail!("{}: {size} bytes, over the {max}-byte limit", self.reference(repository, digest));
        }
        let path = self.blob_file(repository, digest, size).await?;
        let bytes = std::fs::read(&path)?;
        if sha256_digest(&bytes) != digest {
            bail!("{}: the blob does not match its digest", self.reference(repository, digest));
        }
        Ok(bytes)
    }

    /// The path of a blob whose contents match `digest` and `size`, downloading it into the cache first when
    /// it comes from a registry.
    pub async fn blob_file(&self, repository: &str, digest: &str, size: u64) -> Result<PathBuf> {
        let reference = self.reference(repository, digest);
        match self {
            Store::Layout { root } => {
                let path = root.join(repository).join("blobs/sha256").join(hex(digest)?);
                verify_file(&path, digest, size).with_context(|| reference.clone())?;
                Ok(path)
            }
            Store::Registry { client, registry, blobs } => {
                let path = blobs.join(hex(digest)?);
                if verify_file(&path, digest, size).is_ok() {
                    return Ok(path);
                }
                std::fs::create_dir_all(blobs)?;
                let tmp = blobs.join(format!(".{}.tmp-{}", hex(digest)?, uuid::Uuid::new_v4().simple()));
                let image = Reference::with_digest(registry.clone(), repository.to_string(), digest.to_string());
                let fetched = async {
                    client.store_auth_if_needed(image.resolve_registry(), &RegistryAuth::Anonymous).await;
                    let mut stream = client.pull_blob_stream(&image, digest).await?;
                    let mut out = Sha256Writer::new(std::fs::File::create(&tmp)?);
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk?;
                        if out.len() + chunk.len() as u64 > size {
                            bail!("the registry sent more than the {size} bytes the manifest lists");
                        }
                        out.write_all(&chunk)?;
                    }
                    let (got, len) = out.finish()?;
                    if got != digest || len != size {
                        bail!("the blob does not match its digest ({got}, {len} bytes)");
                    }
                    Ok(())
                }
                .await
                .with_context(|| format!("fetching {reference}"));
                if let Err(e) = fetched {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                std::fs::rename(&tmp, &path)?;
                Ok(path)
            }
        }
    }
}

fn fetch_error(reference: &str, e: oci_client::errors::OciDistributionError) -> anyhow::Error {
    match e {
        oci_client::errors::OciDistributionError::UnauthorizedError { .. } => {
            anyhow::anyhow!("{reference}: not found, or not public (hbench pulls anonymously)")
        }
        e => anyhow::Error::new(e).context(format!("fetching {reference}")),
    }
}

fn hex(digest: &str) -> Result<&str> {
    match digest.strip_prefix("sha256:") {
        Some(h) if super::format::is_sha256_digest(digest) => Ok(h),
        _ => bail!("{digest:?} is not a sha256 digest"),
    }
}

fn read_limited(path: &Path, max: u64) -> Result<Vec<u8>> {
    let len = path.metadata().with_context(|| format!("reading {}", path.display()))?.len();
    if len > max {
        bail!("{} is {len} bytes, over the {max}-byte limit", path.display());
    }
    Ok(std::fs::read(path)?)
}

fn store_blob(blobs: &Path, digest: &str, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(blobs)?;
    let tmp = blobs.join(format!(".{}.tmp-{}", hex(digest)?, uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, blobs.join(hex(digest)?))?;
    Ok(())
}
