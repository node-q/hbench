//! The source descriptor (`source.yaml`) a dataset is packed from, as `assets/schemas/dataset-source.v1.json`
//! defines it: what the dataset's config blob and manifest say beyond its tasks, and what its task directories
//! must be. `source.path`, `keywords`, `attribution`, `rai` and `environment.planned` are checked against the
//! schema and otherwise not used.

use super::format::Upstream;
use super::pack::{DatasetInfo, Provenance, task_name};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Descriptor {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub license: String,
    #[serde(default)]
    pub upstream: Option<Upstream>,
    #[serde(default)]
    pub canary: Option<String>,
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    pub environment: Option<Environment>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Source {
    pub git_url: String,
    pub commit: String,
    pub task_count: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Environment {
    pub policy: ImagePolicy,
    #[serde(default)]
    pub declared_image: Option<String>,
    #[serde(default)]
    pub images: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImagePolicy {
    AsDeclared,
    PinnedByDigest,
}

/// The descriptor at `path`, refused unless it is YAML that matches the source descriptor schema.
pub fn load(path: &Path) -> Result<Descriptor> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text).with_context(|| format!("descriptor {}", path.display()))
}

pub fn parse(text: &str) -> Result<Descriptor> {
    let yaml: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).context("not YAML")?;
    let value = serde_json::to_value(yaml).context("not a mapping with string keys")?;
    let schema: serde_json::Value = serde_json::from_str(crate::schemas::DATASET_SOURCE).expect("vendored dataset source schema");
    let problems = crate::schemas::violations(&schema, &value)?;
    if !problems.is_empty() {
        bail!("does not match the dataset source schema:\n  {}", problems.join("\n  "));
    }
    Ok(serde_json::from_value(value)?)
}

fn docker_image(task_dir: &Path) -> Result<Option<String>> {
    let path = task_dir.join("task.toml");
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(crate::task::TaskConfig::parse(&text).with_context(|| format!("parsing {}", path.display()))?.environment.docker_image)
}

impl Descriptor {
    pub fn info(&self) -> DatasetInfo {
        DatasetInfo {
            name: self.name.clone(),
            version: self.version.clone(),
            title: self.title.clone(),
            description: self.description.clone().unwrap_or_default(),
            license: self.license.clone(),
            upstream: self.upstream.clone(),
            canary: self.canary.clone(),
            source: self.source.as_ref().map(|s| Provenance { url: s.git_url.clone(), revision: Some(s.commit.clone()) }),
            created: None,
        }
    }

    /// The task directories `tasks` are the ones the descriptor describes: as many as `source.task_count`, each
    /// declaring the image `environment` says it does.
    pub fn check_tasks(&self, tasks: &[PathBuf]) -> Result<()> {
        if let Some(s) = &self.source
            && s.task_count != tasks.len()
        {
            bail!("found {} task directories, the descriptor's source.task_count is {}", tasks.len(), s.task_count);
        }
        let Some(env) = &self.environment else { return Ok(()) };
        let mut problems = vec![];
        for dir in tasks {
            let name = task_name(dir);
            let declared = env.declared_image.as_ref().map(|d| ("environment.declared_image", d.replace("{task}", &name)));
            let pinned = match (env.policy, env.images.get(&name)) {
                (ImagePolicy::PinnedByDigest, Some(image)) => Some(("environment.images", image.clone())),
                (ImagePolicy::PinnedByDigest, None) => {
                    problems.push(format!("{name}: not in environment.images"));
                    None
                }
                (ImagePolicy::AsDeclared, _) => None,
            };
            let image = docker_image(dir)?;
            for (field, expected) in declared.into_iter().chain(pinned) {
                if image.as_deref() != Some(expected.as_str()) {
                    problems.push(format!("{name}: docker_image is {}, {field} says {expected}", image.as_deref().unwrap_or("unset")));
                }
            }
        }
        let names: Vec<String> = tasks.iter().map(|d| task_name(d)).collect();
        problems
            .extend(env.images.keys().filter(|k| !names.contains(k)).map(|k| format!("environment.images names {k}, which is not a task")));
        if !problems.is_empty() {
            bail!("the tasks do not match the descriptor's environment:\n  {}", problems.join("\n  "));
        }
        Ok(())
    }
}
