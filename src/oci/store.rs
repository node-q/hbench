//! Where a dataset's manifests and blobs come from and go to: an OCI registry, pulled into the content-addressed
//! blob cache (anonymously, unless the store was opened with credentials), or OCI image layouts on disk,
//! `<root>/<repository>`. Every manifest and blob is checked against its digest before it is used.

use super::format::{ARTIFACT_TYPE, INDEX_MEDIA_TYPE, MANIFEST_MEDIA_TYPE, MAX_MANIFEST_BYTES, REF_NAME};
use super::{LAYOUT_SCHEME, Sha256Writer, sha256_digest, verify_file};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use oci_client::Reference;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::errors::{OciDistributionError, OciErrorCode};
use oci_client::secrets::RegistryAuth;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(120);
const UPLOAD_CHUNK_BYTES: usize = 1024 * 1024;
const LAYOUT_VERSION: &str = "1.0.0";

pub enum Store {
    Registry { client: oci_client::Client, registry: String, blobs: PathBuf, auth: RegistryAuth },
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
        Store::open_with_auth(registry, cache_root, RegistryAuth::Anonymous)
    }

    /// [`Store::open`], authenticating to a registry with `auth`.
    pub fn open_with_auth(registry: &str, cache_root: &Path, auth: RegistryAuth) -> Result<Store> {
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
            use_monolithic_push: true,
            ..Default::default()
        };
        let client = oci_client::Client::try_from(config).context("creating the OCI registry client")?;
        Ok(Store::Registry { client, registry: registry.to_string(), blobs: cache_root.join("blobs/sha256"), auth })
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
            Store::Registry { client, registry, blobs, auth } => {
                let image = Reference::with_tag(registry.clone(), repository.to_string(), tag.to_string());
                let (body, _) =
                    client.pull_manifest_raw(&image, auth, &[MANIFEST_MEDIA_TYPE]).await.map_err(|e| fetch_error(&reference, auth, e))?;
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
                match layout_tagged(&layout, tag).with_context(|| reference.clone())?.as_slice() {
                    [d] => Ok(d.clone()),
                    [] => bail!("{reference} not found: no manifest in {} has {REF_NAME} {tag}", layout.display()),
                    _ => bail!("{reference}: several manifests in {} have {REF_NAME} {tag}", layout.display()),
                }
            }
        }
    }

    /// Every tag in `repository`.
    pub async fn tags(&self, repository: &str) -> Result<Vec<String>> {
        match self {
            Store::Registry { client, registry, auth, .. } => {
                let image = Reference::with_tag(registry.clone(), repository.to_string(), "latest".into());
                let r =
                    client.list_tags(&image, auth, None, None).await.map_err(|e| fetch_error(&self.reference(repository, "*"), auth, e))?;
                Ok(r.tags)
            }
            Store::Layout { root } => {
                let text = match std::fs::read_to_string(root.join(repository).join("index.json")) {
                    Ok(t) => t,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                    Err(e) => return Err(e).context("reading index.json"),
                };
                let index: oci_spec::image::ImageIndex = serde_json::from_str(&text).context("parsing index.json")?;
                Ok(index.manifests().iter().filter_map(|m| m.annotations().as_ref()?.get(REF_NAME).cloned()).collect())
            }
        }
    }

    /// The digest of the manifest `repository:tag` names now, or None when the tag names none.
    pub async fn tagged(&self, repository: &str, tag: &str) -> Result<Option<String>> {
        let reference = self.reference(repository, tag);
        match self {
            Store::Registry { client, registry, auth, .. } => {
                let image = Reference::with_tag(registry.clone(), repository.to_string(), tag.to_string());
                match client.fetch_manifest_digest(&image, auth).await {
                    Ok(digest) => Ok(Some(digest)),
                    Err(e) if is_not_found(&e) => Ok(None),
                    Err(e) => Err(fetch_error(&reference, auth, e)),
                }
            }
            Store::Layout { root } => match layout_tagged(&root.join(repository), tag).with_context(|| reference.clone())?.as_slice() {
                [] => Ok(None),
                [d] => Ok(Some(d.clone())),
                _ => bail!("{reference}: several manifests have {REF_NAME} {tag}"),
            },
        }
    }

    /// The digest `repository:tag` names now, or None when the tag names none or, pulling anonymously, the repository
    /// is not public: an anonymous client can't tell those apart.
    pub async fn visible_tag(&self, repository: &str, tag: &str) -> Result<Option<String>> {
        if let Store::Registry { client, registry, auth: RegistryAuth::Anonymous, .. } = self {
            let image = Reference::with_tag(registry.clone(), repository.to_string(), tag.to_string());
            return match client.fetch_manifest_digest(&image, &RegistryAuth::Anonymous).await {
                Ok(digest) => Ok(Some(digest)),
                Err(e) if is_not_found(&e) || matches!(e, OciDistributionError::UnauthorizedError { .. }) => Ok(None),
                Err(e) => Err(fetch_error(&self.reference(repository, tag), &RegistryAuth::Anonymous, e)),
            };
        }
        self.tagged(repository, tag).await
    }

    /// Put the blob at `path` (`digest`, `size` bytes) into `repository`, unless it is there already; whether it
    /// was copied.
    pub async fn put_blob(&self, repository: &str, digest: &str, size: u64, path: &Path) -> Result<bool> {
        let reference = self.reference(repository, digest);
        match self {
            Store::Layout { root } => {
                let blobs = root.join(repository).join("blobs/sha256");
                let target = blobs.join(hex(digest)?);
                if verify_file(&target, digest, size).is_ok() {
                    return Ok(false);
                }
                std::fs::create_dir_all(&blobs)?;
                let tmp = blobs.join(format!(".{}.tmp-{}", hex(digest)?, uuid::Uuid::new_v4().simple()));
                let copied = std::fs::copy(path, &tmp).map_err(anyhow::Error::from).and_then(|_| verify_file(&tmp, digest, size));
                if let Err(e) = copied {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e.context(format!("copying {} to {reference}", path.display())));
                }
                std::fs::rename(&tmp, &target)?;
                Ok(true)
            }
            Store::Registry { client, registry, auth, .. } => {
                let image = Reference::with_digest(registry.clone(), repository.to_string(), digest.to_string());
                client.store_auth_if_needed(image.resolve_registry(), auth).await;
                if client.blob_exists(&image, digest).await.map_err(|e| push_error(&reference, auth, e))? {
                    return Ok(false);
                }
                let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
                client
                    .push_blob_stream(&image, file_chunks(file), digest, Some(size))
                    .await
                    .map_err(|e| push_error(&reference, auth, e))?;
                Ok(true)
            }
        }
    }

    /// Put `manifest` into `repository` and tag it `tag`, moving the tag if it names another manifest; the
    /// manifest's digest.
    pub async fn put_manifest(&self, repository: &str, tag: &str, manifest: &[u8]) -> Result<String> {
        let digest = sha256_digest(manifest);
        let reference = self.reference(repository, tag);
        match self {
            Store::Layout { root } => {
                let layout = root.join(repository);
                let blobs = layout.join("blobs/sha256");
                std::fs::create_dir_all(&blobs)?;
                store_blob(&blobs, &digest, manifest)?;
                tag_in_layout(&layout, tag, &digest, manifest.len() as u64).with_context(|| reference.clone())?;
            }
            Store::Registry { client, registry, auth, .. } => {
                let image = Reference::with_tag(registry.clone(), repository.to_string(), tag.to_string());
                let media_type = http::HeaderValue::from_static(MANIFEST_MEDIA_TYPE);
                client.push_manifest_raw(&image, manifest.to_vec(), media_type).await.map_err(|e| push_error(&reference, auth, e))?;
            }
        }
        Ok(digest)
    }

    /// The manifest with `digest`, from the layout, the cache or the registry, checked against its digest.
    pub async fn manifest(&self, repository: &str, digest: &str) -> Result<Vec<u8>> {
        let reference = self.reference(repository, digest);
        let (client, registry, blobs, auth) = match self {
            Store::Layout { root } => {
                let bytes = read_limited(&root.join(repository).join("blobs/sha256").join(hex(digest)?), MAX_MANIFEST_BYTES)
                    .with_context(|| reference.clone())?;
                if sha256_digest(&bytes) != digest {
                    bail!("{reference}: the manifest does not match its digest");
                }
                return Ok(bytes);
            }
            Store::Registry { client, registry, blobs, auth } => (client, registry, blobs, auth),
        };
        if let Ok(bytes) = read_limited(&blobs.join(hex(digest)?), MAX_MANIFEST_BYTES)
            && sha256_digest(&bytes) == digest
        {
            return Ok(bytes);
        }
        let image = Reference::with_digest(registry.clone(), repository.to_string(), digest.to_string());
        let (body, _) =
            client.pull_manifest_raw(&image, auth, &[MANIFEST_MEDIA_TYPE]).await.map_err(|e| fetch_error(&reference, auth, e))?;
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
            Store::Registry { client, registry, blobs, auth } => {
                let path = blobs.join(hex(digest)?);
                if verify_file(&path, digest, size).is_ok() {
                    return Ok(path);
                }
                std::fs::create_dir_all(blobs)?;
                let tmp = blobs.join(format!(".{}.tmp-{}", hex(digest)?, uuid::Uuid::new_v4().simple()));
                let image = Reference::with_digest(registry.clone(), repository.to_string(), digest.to_string());
                let fetched = async {
                    client.store_auth_if_needed(image.resolve_registry(), auth).await;
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

fn fetch_error(reference: &str, auth: &RegistryAuth, e: OciDistributionError) -> anyhow::Error {
    match (e, auth) {
        (OciDistributionError::UnauthorizedError { .. }, RegistryAuth::Anonymous) => {
            anyhow::anyhow!("{reference}: not found, or not public (hbench pulls anonymously)")
        }
        (OciDistributionError::UnauthorizedError { .. }, _) => anyhow::anyhow!("{reference}: not found, or the credentials can't read it"),
        (e, _) => anyhow::Error::new(e).context(format!("fetching {reference}")),
    }
}

fn push_error(reference: &str, auth: &RegistryAuth, e: OciDistributionError) -> anyhow::Error {
    let denied = match &e {
        OciDistributionError::UnauthorizedError { .. } | OciDistributionError::AuthenticationFailure(_) => true,
        OciDistributionError::RegistryError { envelope, .. } => {
            envelope.errors.iter().any(|e| matches!(e.code, OciErrorCode::Unauthorized | OciErrorCode::Denied))
        }
        _ => false,
    };
    match (denied, auth) {
        (true, RegistryAuth::Anonymous) => anyhow::anyhow!(
            "{reference}: the registry refused an anonymous push; log in with `docker login` or `oras login` (hbench reads $DOCKER_CONFIG/config.json, else ~/.docker/config.json)"
        ),
        (true, _) => anyhow::Error::new(e).context(format!("{reference}: the registry refused the push with these credentials")),
        (false, _) => anyhow::Error::new(e).context(format!("pushing {reference}")),
    }
}

fn is_not_found(e: &OciDistributionError) -> bool {
    match e {
        OciDistributionError::ImageManifestNotFoundError(_) => true,
        OciDistributionError::ServerError { code, .. } => *code == 404,
        OciDistributionError::RegistryError { envelope, .. } => envelope
            .errors
            .iter()
            .any(|e| matches!(e.code, OciErrorCode::ManifestUnknown | OciErrorCode::NameUnknown | OciErrorCode::NotFound)),
        _ => false,
    }
}

fn file_chunks(file: std::fs::File) -> impl futures_util::Stream<Item = oci_client::errors::Result<bytes::Bytes>> + Send + 'static {
    futures_util::stream::try_unfold(file, |mut file| async move {
        let mut chunk = vec![0; UPLOAD_CHUNK_BYTES];
        let n = file.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        chunk.truncate(n);
        Ok(Some((bytes::Bytes::from(chunk), file)))
    })
}

/// The digests of the manifests in the layout at `layout` tagged `tag` (none when there is no layout).
fn layout_tagged(layout: &Path, tag: &str) -> Result<Vec<String>> {
    let text = match std::fs::read_to_string(layout.join("index.json")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).context("reading index.json"),
    };
    let index: oci_spec::image::ImageIndex = serde_json::from_str(&text).context("parsing index.json")?;
    Ok(index
        .manifests()
        .iter()
        .filter(|m| m.annotations().as_ref().and_then(|a| a.get(REF_NAME)).is_some_and(|r| r == tag))
        .map(|m| m.digest().to_string())
        .collect())
}

/// Tag the dataset manifest `digest` (`size` bytes, already in the layout's blobs) `tag` in the OCI image layout
/// at `layout`, replacing any manifest that had the tag.
pub(super) fn tag_in_layout(layout: &Path, tag: &str, digest: &str, size: u64) -> Result<()> {
    let index_path = layout.join("index.json");
    let mut manifests: Vec<Value> = match std::fs::read_to_string(&index_path) {
        Ok(s) => serde_json::from_str::<Value>(&s).context("parsing index.json")?["manifests"].as_array().cloned().unwrap_or_default(),
        Err(_) => vec![],
    };
    manifests.retain(|m| m["annotations"][REF_NAME] != tag);
    manifests.push(json!({
        "mediaType": MANIFEST_MEDIA_TYPE, "artifactType": ARTIFACT_TYPE, "digest": digest, "size": size,
        "annotations": {REF_NAME: tag},
    }));
    manifests.sort_by(|a, b| a["annotations"][REF_NAME].as_str().cmp(&b["annotations"][REF_NAME].as_str()));
    let index = json!({"schemaVersion": 2, "mediaType": INDEX_MEDIA_TYPE, "manifests": manifests});
    std::fs::write(layout.join("oci-layout"), json!({"imageLayoutVersion": LAYOUT_VERSION}).to_string())?;
    Ok(crate::trial::write_atomic(&index_path, serde_json::to_vec_pretty(&index)?)?)
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
