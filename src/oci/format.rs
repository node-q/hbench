//! The dataset artifact: its media types and annotations, the config blob, and the checks a manifest and its
//! config pass before any of its layers is used.

use anyhow::{Context, Result, bail};
use oci_spec::image::{Descriptor, ImageManifest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const ARTIFACT_TYPE: &str = "application/vnd.hbench.dataset.v1";
pub const CONFIG_MEDIA_TYPE: &str = "application/vnd.hbench.dataset.config.v1+json";
pub const LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
pub const INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";

pub const TITLE: &str = "org.opencontainers.image.title";
pub const DESCRIPTION: &str = "org.opencontainers.image.description";
pub const PACKAGE_DIGEST: &str = "dev.hbench.task.package-digest";
pub const REF_NAME: &str = "org.opencontainers.image.ref.name";
pub const VERSION: &str = "org.opencontainers.image.version";
pub const LICENSES: &str = "org.opencontainers.image.licenses";
pub const SOURCE: &str = "org.opencontainers.image.source";
pub const REVISION: &str = "org.opencontainers.image.revision";
pub const CREATED: &str = "org.opencontainers.image.created";

/// A registry's list of its datasets: the manifest tagged `latest` in `<namespace>/index`.
pub const DATASET_INDEX_REPOSITORY: &str = "index";
pub const DATASET_INDEX_TAG: &str = "latest";
pub const DATASET_INDEX_ARTIFACT_TYPE: &str = "application/vnd.hbench.dataset-index.v1";
pub const DATASET_INDEX_MEDIA_TYPE: &str = "application/vnd.hbench.dataset-index.v1+json";

const MIB: u64 = 1024 * 1024;
pub const MAX_MANIFEST_BYTES: u64 = 4 * MIB;
pub const MAX_CONFIG_BYTES: u64 = 4 * MIB;
pub const MAX_DATASET_INDEX_BYTES: u64 = 4 * MIB;

/// The config blob, as `assets/schemas/dataset-config.v1.json` defines it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigBlob {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub license: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<Upstream>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canary: Option<String>,
    pub tasks: Vec<TaskEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEntry {
    pub name: String,
    pub layer_digest: String,
    pub package_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<Timeouts>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timeouts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_sec: Option<f64>,
}

/// One task's layer, as the manifest describes it and the config confirms.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub name: String,
    pub digest: String,
    pub size: u64,
    pub package_digest: String,
}

/// A dataset whose manifest and config agree, task by task.
#[derive(Debug, Clone)]
pub struct Dataset {
    pub config: ConfigBlob,
    pub layers: Vec<Layer>,
}

/// The JSON layer of a dataset index, as `assets/schemas/dataset-index.v1.json` defines it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetIndex {
    pub schema_version: u32,
    pub datasets: Vec<IndexEntry>,
}

/// A dataset version an index lists: `<namespace>/<name>@<digest>`, tagged `<version>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexEntry {
    pub name: String,
    pub version: String,
    pub digest: String,
}

/// The layer that holds a dataset index, refused unless `bytes` is an OCI image manifest of the index artifact
/// type with exactly one layer of the index media type.
pub fn parse_index_manifest(bytes: &[u8]) -> Result<Descriptor> {
    let m: ImageManifest = serde_json::from_slice(bytes).context("not an OCI image manifest")?;
    let media_type = m.media_type().as_ref().map(ToString::to_string);
    if m.schema_version() != 2 || media_type.as_deref() != Some(MANIFEST_MEDIA_TYPE) {
        bail!("not an OCI image manifest (schemaVersion 2, mediaType {MANIFEST_MEDIA_TYPE})");
    }
    let artifact_type = m.artifact_type().as_ref().map(ToString::to_string);
    if artifact_type.as_deref() != Some(DATASET_INDEX_ARTIFACT_TYPE) {
        bail!(
            "not an hbench dataset index: artifactType is {}, not {DATASET_INDEX_ARTIFACT_TYPE}",
            artifact_type.as_deref().unwrap_or("unset")
        );
    }
    match m.layers().as_slice() {
        [layer] if layer.media_type().to_string() == DATASET_INDEX_MEDIA_TYPE => Ok(layer.clone()),
        _ => bail!("a dataset index has exactly one layer, of mediaType {DATASET_INDEX_MEDIA_TYPE}"),
    }
}

/// A dataset index layer, refused unless its schema version is 1 and every entry names a valid repository
/// component, version and digest, with no name@version twice.
pub fn parse_dataset_index(bytes: &[u8]) -> Result<DatasetIndex> {
    let index: DatasetIndex = serde_json::from_slice(bytes).context("not a dataset index")?;
    if index.schema_version != 1 {
        bail!("dataset index schema_version is {}, not 1", index.schema_version);
    }
    let mut seen = BTreeSet::new();
    for e in &index.datasets {
        if !super::is_repository_component(&e.name) || e.name == DATASET_INDEX_REPOSITORY {
            bail!("dataset index: {:?} is not a dataset name", e.name);
        }
        if !super::is_tag(&e.version) {
            bail!("dataset index: {}'s version {:?} is not a tag", e.name, e.version);
        }
        if !is_sha256_digest(&e.digest) {
            bail!("dataset index: {}@{}'s digest {:?} is not a sha256 digest", e.name, e.version, e.digest);
        }
        if !seen.insert((e.name.as_str(), e.version.as_str())) {
            bail!("dataset index lists {}@{} twice", e.name, e.version);
        }
    }
    Ok(index)
}

pub fn is_sha256_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// A task name usable as a directory name: what the config schema allows.
pub fn is_task_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && s.len() <= 255
}

fn annotation<'a>(d: &'a Descriptor, key: &str) -> Option<&'a str> {
    d.annotations().as_ref().and_then(|a| a.get(key)).map(String::as_str)
}

/// A dataset manifest, refused unless it is an OCI image manifest of the dataset artifact type whose layers
/// are all task layers with a valid, unique title and a package digest.
pub fn parse_manifest(bytes: &[u8]) -> Result<ImageManifest> {
    let m: ImageManifest = serde_json::from_slice(bytes).context("not an OCI image manifest")?;
    let media_type = m.media_type().as_ref().map(ToString::to_string);
    if m.schema_version() != 2 || media_type.as_deref() != Some(MANIFEST_MEDIA_TYPE) {
        bail!("not an OCI image manifest (schemaVersion 2, mediaType {MANIFEST_MEDIA_TYPE})");
    }
    let artifact_type = m.artifact_type().as_ref().map(ToString::to_string);
    if artifact_type.as_deref() != Some(ARTIFACT_TYPE) {
        bail!("not an hbench dataset: artifactType is {}, not {ARTIFACT_TYPE}", artifact_type.as_deref().unwrap_or("unset"));
    }
    if m.config().media_type().to_string() != CONFIG_MEDIA_TYPE {
        bail!("config mediaType is {}, not {CONFIG_MEDIA_TYPE}", m.config().media_type());
    }
    if m.config().size() > MAX_CONFIG_BYTES {
        bail!("config blob is {} bytes, over the {MAX_CONFIG_BYTES}-byte limit", m.config().size());
    }
    if m.layers().is_empty() {
        bail!("a dataset manifest needs at least one task layer");
    }
    let mut names = BTreeSet::new();
    for (i, layer) in m.layers().iter().enumerate() {
        let title = annotation(layer, TITLE).with_context(|| format!("layer {i} has no {TITLE} annotation"))?;
        if !is_task_name(title) {
            bail!("layer {i}: {title:?} is not a valid task name");
        }
        if !names.insert(title) {
            bail!("two layers are named {title:?}");
        }
        if layer.media_type().to_string() != LAYER_MEDIA_TYPE {
            bail!("task {title}: layer mediaType is {}, not {LAYER_MEDIA_TYPE}", layer.media_type());
        }
        if !is_sha256_digest(layer.digest().as_ref()) {
            bail!("task {title}: layer digest {} is not sha256", layer.digest());
        }
        if !annotation(layer, PACKAGE_DIGEST).is_some_and(is_sha256_digest) {
            bail!("task {title}: missing or invalid {PACKAGE_DIGEST} annotation");
        }
    }
    Ok(m)
}

impl Dataset {
    /// The dataset `manifest` describes, once its config blob validates against the schema and lists the
    /// manifest's layers, in order, with the same names, layer digests and package digests.
    pub fn new(manifest: &ImageManifest, config: &[u8]) -> Result<Dataset> {
        let value: serde_json::Value = serde_json::from_slice(config).context("config blob is not JSON")?;
        let schema: serde_json::Value = serde_json::from_str(crate::schemas::DATASET_CONFIG).expect("vendored dataset config schema");
        let problems = crate::schemas::violations(&schema, &value)?;
        if !problems.is_empty() {
            bail!("config blob does not match the dataset config schema:\n  {}", problems.join("\n  "));
        }
        let config: ConfigBlob = serde_json::from_value(value).context("config blob")?;
        if config.tasks.len() != manifest.layers().len() {
            bail!("config lists {} task(s), the manifest {} layer(s)", config.tasks.len(), manifest.layers().len());
        }
        if let Some(v) = manifest.annotations().as_ref().and_then(|a| a.get(VERSION))
            && *v != config.version
        {
            bail!("manifest {VERSION} is {v}, config version {}", config.version);
        }
        let mut layers = vec![];
        for (layer, task) in manifest.layers().iter().zip(&config.tasks) {
            let layer = Layer {
                name: annotation(layer, TITLE).unwrap_or_default().to_string(),
                digest: layer.digest().to_string(),
                size: layer.size(),
                package_digest: annotation(layer, PACKAGE_DIGEST).unwrap_or_default().to_string(),
            };
            if (task.name.as_str(), task.layer_digest.as_str(), task.package_digest.as_str())
                != (layer.name.as_str(), layer.digest.as_str(), layer.package_digest.as_str())
            {
                bail!(
                    "config task {} ({}, package {}) does not match its manifest layer {} ({}, package {})",
                    task.name,
                    task.layer_digest,
                    task.package_digest,
                    layer.name,
                    layer.digest,
                    layer.package_digest
                );
            }
            layers.push(layer);
        }
        Ok(Dataset { config, layers })
    }
}
