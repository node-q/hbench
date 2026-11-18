//! The launcher's dataset catalog: the default registry's datasets, each filed under a domain.

use super::*;
use crate::oci::format::Dataset;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// The catalog's domains, in tile order, and the dataset names (globs) each one claims.
const DOMAINS: &str = include_str!("../../assets/catalog/domains.json");
/// Where a dataset no domain claims goes.
const OTHER: &str = "other";
/// The registry is listed again after this long; `?refresh=1` lists it now.
const FRESH_FOR: Duration = Duration::from_secs(600);
const LIST_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Deserialize)]
struct Domain {
    id: String,
    title: String,
    description: String,
    #[serde(default)]
    datasets: Vec<String>,
}

pub(super) struct Listed {
    registry: String,
    at: Instant,
    body: Value,
}

/// Orders versions oldest first: by their numbers, a pre-release before its release.
fn version_key(v: &str) -> (Vec<u64>, bool, String) {
    let (core, pre) = v.split_once('-').unwrap_or((v, ""));
    (core.split('.').map(|p| p.parse().unwrap_or(u64::MAX)).collect(), pre.is_empty(), pre.to_string())
}

/// A registry description without the sentences about who packaged the tasks: the catalog credits the dataset's
/// creator, and describes what the tasks are.
fn blurb(description: &str) -> String {
    let packaging = |sentence: &str| ["node-q", "hbench"].iter().any(|by| sentence.to_ascii_lowercase().contains(by));
    description.split_inclusive(". ").filter(|s| !packaging(s)).collect::<String>().trim_end().to_string()
}

/// One entry per dataset name, described by its newest version, under the first domain that claims the name.
fn catalog(registry: &str, datasets: &[Dataset]) -> Value {
    let domains: Vec<Domain> = serde_json::from_str(DOMAINS).expect("vendored catalog domains");
    let mut by_name: BTreeMap<&str, Vec<&Dataset>> = BTreeMap::new();
    for d in datasets {
        by_name.entry(d.config.name.as_str()).or_default().push(d);
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut entries = vec![];
    for (name, mut versions) in by_name {
        versions.sort_by_key(|d| version_key(&d.config.version));
        let newest = versions[versions.len() - 1];
        let domain = domains.iter().find(|d| d.datasets.iter().any(|g| crate::glob_match(g, name))).map_or(OTHER, |d| d.id.as_str());
        *counts.entry(domain).or_default() += 1;
        let upstream = newest.config.upstream.as_ref();
        entries.push(json!({
            "name": name,
            "version": newest.config.version,
            "versions": versions.iter().map(|d| d.config.version.as_str()).collect::<Vec<_>>(),
            "description": blurb(&newest.config.description),
            "n_tasks": newest.layers.len(),
            "license": newest.config.license,
            "domain": domain,
            "upstream_url": upstream.map(|u| u.url.as_str()),
            "creator": upstream.and_then(|u| u.creator.as_deref()),
        }));
    }
    let count = |id: &str| counts.get(id).copied().unwrap_or(0);
    let mut tiles: Vec<Value> =
        domains.iter().map(|d| json!({"id": d.id, "title": d.title, "description": d.description, "n_datasets": count(&d.id)})).collect();
    if count(OTHER) > 0 {
        tiles.push(
            json!({"id": OTHER, "title": "Other", "description": "Datasets not filed under a domain yet.", "n_datasets": count(OTHER)}),
        );
    }
    json!({"registry": registry, "domains": tiles, "datasets": entries})
}

impl App {
    /// `GET /api/run/catalog`: the default registry's datasets by domain, listed at most once per [`FRESH_FOR`].
    /// The lock is held while listing, so requests that arrive together share one listing.
    pub(super) fn catalog(&self, refresh: bool) -> Res {
        let registry = crate::registry::default_registry();
        let mut listed = self.catalog_cache.lock().unwrap();
        if let Some(l) = listed.as_ref().filter(|l| !refresh && l.registry == registry && l.at.elapsed() < FRESH_FOR) {
            return ok_json(l.body.clone());
        }
        // Workers are plain threads, outside any runtime, so each listing brings its own.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| err(500, e.to_string()))?;
        let datasets = match runtime.block_on(async { tokio::time::timeout(LIST_TIMEOUT, crate::oci::list(&registry)).await }) {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => return Err(err(502, format!("{e:#}"))),
            Err(_) => return Err(err(504, format!("Listing {registry} took more than {}s.", LIST_TIMEOUT.as_secs()))),
        };
        let body = catalog(&registry, &datasets);
        *listed = Some(Listed { registry, at: Instant::now(), body: body.clone() });
        ok_json(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci::format::{ConfigBlob, Layer, Upstream};

    fn dataset(name: &str, version: &str, tasks: usize) -> Dataset {
        let layer = |i: usize| Layer { name: format!("t{i}"), digest: String::new(), size: 0, package_digest: String::new() };
        Dataset {
            config: ConfigBlob {
                name: name.into(),
                version: version.into(),
                description: format!("{name} {version}"),
                license: "MIT".into(),
                upstream: Some(Upstream {
                    url: "https://example.com".into(),
                    revision: None,
                    creator: Some("Them".into()),
                    citation: None,
                }),
                canary: None,
                tasks: vec![],
            },
            layers: (0..tasks).map(layer).collect(),
            listing: None,
            cover: None,
        }
    }

    #[test]
    fn catalog_files_each_dataset_under_a_domain_at_its_newest_version() {
        let listed = [
            dataset("terminal-bench", "2.0.1", 89),
            dataset("terminal-bench", "2.0", 80),
            dataset("terminal-bench", "2.1.0-rc.1", 90),
            dataset("terminal-bench", "2.1.0", 91),
            dataset("swebench-lite", "1.0", 270),
            dataset("hello", "1.0", 1),
        ];
        let c = catalog("oci://r/ns", &listed);
        assert_eq!(c["registry"], "oci://r/ns");
        let names: Vec<&str> = c["datasets"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["hello", "swebench-lite", "terminal-bench"]);
        let tb = &c["datasets"][2];
        assert_eq!(tb["versions"], json!(["2.0", "2.0.1", "2.1.0-rc.1", "2.1.0"]));
        assert_eq!((tb["version"].as_str(), tb["n_tasks"].as_u64(), tb["domain"].as_str()), (Some("2.1.0"), Some(91), Some("coding")));
        assert_eq!((tb["creator"].as_str(), tb["upstream_url"].as_str()), (Some("Them"), Some("https://example.com")));
        assert_eq!(c["datasets"][0]["domain"], OTHER);
        assert_eq!(tb["description"], "terminal-bench 2.1.0");
        let packaged =
            "164 problems from v0.1.10. Each asks for one function. The task directories are generated by node-q from the v0.1.10 release.";
        assert_eq!(blurb(packaged), "164 problems from v0.1.10. Each asks for one function.");
        assert_eq!(blurb("Packed for hbench. Hard tasks."), "Hard tasks.");
        let tiles: Vec<(&str, u64)> =
            c["domains"].as_array().unwrap().iter().map(|d| (d["id"].as_str().unwrap(), d["n_datasets"].as_u64().unwrap())).collect();
        // Tiles follow the vendored order, with Other last; only the domains that claim a dataset count any.
        assert_eq!((tiles[0], tiles[tiles.len() - 1]), (("coding", 2), ("other", 1)));
        assert!(tiles[1..tiles.len() - 1].iter().all(|t| t.1 == 0) && tiles.iter().any(|t| t.0 == "underwriting"));
        // With every dataset claimed there is no Other tile.
        let c = catalog("oci://r/ns", &listed[..5]);
        assert!(c["domains"].as_array().unwrap().iter().all(|d| d["id"] != OTHER));
    }
}
