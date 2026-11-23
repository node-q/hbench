//! The launcher's dataset catalog: the default registry's datasets, each filed under a domain. The list comes from
//! the registry's dataset index alone, so it answers in one round of requests; each dataset's details (task count,
//! description, licence, creator) come with it when the index or the blob cache has them, else from
//! `/api/run/catalog/details`, which the launcher asks for the datasets it shows.

use super::*;
use crate::oci::Indexed;
use crate::oci::format::{Dataset, IndexEntry};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// The catalog's domains, in tile order, the dataset names (globs) each one claims, and the listing domains it
/// takes when no name claims a dataset.
const DOMAINS: &str = include_str!("../../assets/catalog/domains.json");
/// Where a dataset no domain claims goes.
const OTHER: &str = "other";
/// The index is read again after this long; `?refresh=1` reads it now.
const FRESH_FOR: Duration = Duration::from_secs(600);
/// Reading the index is one token, one manifest and one blob.
const INDEX_TIMEOUT: Duration = Duration::from_secs(30);
/// Opening the datasets of one details request, [`crate::oci::PARALLEL_OPENS`] at a time.
const DETAILS_TIMEOUT: Duration = Duration::from_secs(40);
/// The most datasets one details request opens.
const MAX_DETAILS: usize = 24;

#[derive(Deserialize)]
struct Domain {
    id: String,
    title: String,
    description: String,
    #[serde(default)]
    datasets: Vec<String>,
    #[serde(default)]
    listing_domains: Vec<String>,
}

fn domains() -> Vec<Domain> {
    serde_json::from_str(DOMAINS).expect("vendored catalog domains")
}

/// The index as last read.
pub(super) struct Listed {
    registry: String,
    at: Instant,
    entries: Vec<IndexEntry>,
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

/// The domain a dataset is filed under: the first that claims its name, else the first that takes the domain its
/// listing gives (the index's `domain`), else [`OTHER`].
fn domain_of<'a>(domains: &'a [Domain], name: &str, listing_domain: Option<&str>) -> &'a str {
    let by_name = domains.iter().find(|d| d.datasets.iter().any(|g| crate::glob_match(g, name)));
    let by_listing =
        || listing_domain.and_then(|l| domains.iter().find(|d| d.listing_domains.iter().any(|x| x.eq_ignore_ascii_case(l.trim()))));
    by_name.or_else(by_listing).map_or(OTHER, |d| d.id.as_str())
}

/// What a dataset card shows besides its name and versions.
fn details(d: &Dataset) -> Value {
    let upstream = d.config.upstream.as_ref();
    json!({
        "description": blurb(&d.config.description),
        "n_tasks": d.layers.len(),
        "license": d.config.license,
        "upstream_url": upstream.map(|u| u.url.as_str()),
        "creator": upstream.and_then(|u| u.creator.as_deref()),
    })
}

/// [`details`] from the index entry alone, when it carries a task count and a description.
fn details_in_index(e: &IndexEntry) -> Option<Value> {
    let (n_tasks, description) = (e.n_tasks?, e.description.as_deref()?);
    Some(json!({
        "description": blurb(description), "n_tasks": n_tasks, "license": e.license.clone().unwrap_or_default(),
        "upstream_url": null, "creator": null,
    }))
}

/// Each dataset name's newest version.
fn newest(entries: &[IndexEntry]) -> BTreeMap<&str, Vec<&IndexEntry>> {
    let mut by_name: BTreeMap<&str, Vec<&IndexEntry>> = BTreeMap::new();
    for e in entries {
        by_name.entry(e.name.as_str()).or_default().push(e);
    }
    for versions in by_name.values_mut() {
        versions.sort_by_key(|e| version_key(&e.version));
    }
    by_name
}

/// One entry per dataset name, at its newest version, under its domain; `known` gives the details at hand
/// (null for those still to load).
fn catalog(registry: &str, entries: &[IndexEntry], known: impl Fn(&IndexEntry) -> Option<Value>) -> Value {
    let domains = domains();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut datasets = vec![];
    for (name, versions) in newest(entries) {
        let latest = versions[versions.len() - 1];
        let domain = domain_of(&domains, name, latest.domain.as_deref());
        *counts.entry(domain).or_default() += 1;
        datasets.push(json!({
            "name": name,
            "version": latest.version,
            "versions": versions.iter().map(|e| e.version.as_str()).collect::<Vec<_>>(),
            "domain": domain,
            "details": known(latest),
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
    json!({"registry": registry, "domains": tiles, "datasets": datasets})
}

/// Workers are plain threads, outside any runtime, so each request that reaches the registry brings its own.
fn block_on<T>(limit: Duration, what: &str, f: impl std::future::Future<Output = T>) -> std::result::Result<T, Err> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| err(500, e.to_string()))?;
    runtime
        .block_on(async { tokio::time::timeout(limit, f).await })
        .map_err(|_| err(504, format!("{what} took more than {}s.", limit.as_secs())))
}

impl App {
    /// The default registry's index entries, read at most once per [`FRESH_FOR`]. The lock is held while reading,
    /// so requests that arrive together share one read.
    fn index_entries(&self, refresh: bool) -> std::result::Result<(String, Vec<IndexEntry>), Err> {
        let registry = crate::registry::default_registry();
        let mut listed = self.catalog_cache.lock().unwrap();
        if let Some(l) = listed.as_ref().filter(|l| !refresh && l.registry == registry && l.at.elapsed() < FRESH_FOR) {
            return Ok((registry, l.entries.clone()));
        }
        let read = block_on(INDEX_TIMEOUT, &format!("Reading {registry}'s dataset index"), Indexed::read(&registry))?;
        let entries = match read {
            Ok(Some(index)) => index.entries,
            Ok(None) => return Err(err(502, crate::oci::no_index(&registry))),
            Err(e) => return Err(err(502, format!("{e:#}"))),
        };
        *listed = Some(Listed { registry: registry.clone(), at: Instant::now(), entries: entries.clone() });
        Ok((registry, entries))
    }

    /// The details of `e` this session already has, from the index, memory or the blob cache; no request.
    fn known_details(&self, index: &Indexed, e: &IndexEntry) -> Option<Value> {
        if let Some(d) = details_in_index(e) {
            return Some(d);
        }
        let mut cache = self.catalog_details.lock().unwrap();
        if let Some(d) = cache.get(&e.digest) {
            return Some(d.clone());
        }
        let d = details(&index.cached(e)?);
        cache.insert(e.digest.clone(), d.clone());
        Some(d)
    }

    /// `GET /api/run/catalog`: the default registry's datasets by domain, from its index; `details` is null for a
    /// dataset whose details neither the index nor this machine has yet.
    pub(super) fn catalog(&self, refresh: bool) -> Res {
        let (registry, entries) = self.index_entries(refresh)?;
        let index = Indexed::with_entries(&registry, vec![]).map_err(|e| err(502, format!("{e:#}")))?;
        ok_json(catalog(&registry, &entries, |e| self.known_details(&index, e)))
    }

    /// `GET /api/run/catalog/details?name=…`: the details of each named dataset's newest version, opening those
    /// not at hand [`crate::oci::PARALLEL_OPENS`] at a time; `errors` says why a dataset's details didn't load.
    pub(super) fn catalog_details(&self, names: &[String]) -> Res {
        if names.len() > MAX_DETAILS {
            return Err(err(422, format!("at most {MAX_DETAILS} names per request")));
        }
        let (registry, entries) = self.index_entries(false)?;
        let latest = newest(&entries);
        let index = Indexed::with_entries(&registry, vec![]).map_err(|e| err(502, format!("{e:#}")))?;
        let (mut found, mut errors) = (Map::new(), Map::new());
        let mut wanted: Vec<&IndexEntry> = vec![];
        for name in names {
            match latest.get(name.as_str()).and_then(|v| v.last()) {
                None => {
                    errors.insert(name.clone(), json!(format!("{registry}'s index doesn't list {name}.")));
                }
                Some(e) => match self.known_details(&index, e) {
                    Some(d) => {
                        found.insert(name.clone(), d);
                    }
                    None => wanted.push(e),
                },
            }
        }
        if !wanted.is_empty() {
            match block_on(DETAILS_TIMEOUT, &format!("Opening {} dataset(s)", wanted.len()), index.open_all(wanted.iter().copied())) {
                Ok(opened) => {
                    for (e, d) in wanted.iter().zip(opened) {
                        match d {
                            Ok(d) => {
                                let d = details(&d);
                                self.catalog_details.lock().unwrap().insert(e.digest.clone(), d.clone());
                                found.insert(e.name.clone(), d);
                            }
                            Err(x) => {
                                errors.insert(e.name.clone(), json!(format!("{x:#}")));
                            }
                        }
                    }
                }
                Err(x) => {
                    for e in wanted {
                        errors.insert(e.name.clone(), json!(x.1.clone()));
                    }
                }
            }
        }
        ok_json(json!({"details": found, "errors": errors}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci::format::{ConfigBlob, Layer, Upstream};

    fn entry(name: &str, version: &str) -> IndexEntry {
        IndexEntry { name: name.into(), version: version.into(), digest: format!("sha256:{name}-{version}"), ..Default::default() }
    }

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
        }
    }

    #[test]
    fn catalog_files_each_dataset_under_a_domain_at_its_newest_version() {
        let listed = [
            entry("terminal-bench", "2.0.1"),
            entry("terminal-bench", "2.0"),
            entry("terminal-bench", "2.1.0-rc.1"),
            entry("terminal-bench", "2.1.0"),
            entry("swebench-lite", "1.0"),
            entry("hello", "1.0"),
        ];
        let known = |e: &IndexEntry| (e.name == "terminal-bench").then(|| details(&dataset(&e.name, &e.version, 91)));
        let c = catalog("oci://r/ns", &listed, known);
        assert_eq!(c["registry"], "oci://r/ns");
        let names: Vec<&str> = c["datasets"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["hello", "swebench-lite", "terminal-bench"]);
        let tb = &c["datasets"][2];
        assert_eq!(tb["versions"], json!(["2.0", "2.0.1", "2.1.0-rc.1", "2.1.0"]));
        assert_eq!((tb["version"].as_str(), tb["domain"].as_str()), (Some("2.1.0"), Some("coding")));
        let d = &tb["details"];
        assert_eq!((d["n_tasks"].as_u64(), d["description"].as_str()), (Some(91), Some("terminal-bench 2.1.0")));
        assert_eq!((d["creator"].as_str(), d["upstream_url"].as_str()), (Some("Them"), Some("https://example.com")));
        // Details not at hand are null: the launcher loads them when it shows the card.
        assert!(c["datasets"][1]["details"].is_null());
        assert_eq!(c["datasets"][0]["domain"], OTHER);
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
        let c = catalog("oci://r/ns", &listed[..5], |_| None);
        assert!(c["domains"].as_array().unwrap().iter().all(|d| d["id"] != OTHER));
    }

    #[test]
    fn an_index_that_carries_the_details_needs_no_dataset_opened() {
        let mut e = entry("fmla-leave", "1.0");
        assert!(details_in_index(&e).is_none());
        (e.n_tasks, e.description, e.license) = (Some(40), Some("Leave cases. Packaged by node-q.".into()), Some("CC-BY-4.0".into()));
        let d = details_in_index(&e).unwrap();
        assert_eq!(
            (d["n_tasks"].as_u64(), d["description"].as_str(), d["license"].as_str()),
            (Some(40), Some("Leave cases."), Some("CC-BY-4.0"))
        );
    }

    #[test]
    fn every_published_dataset_has_a_domain() {
        let domains = domains();
        // The registry's index on 2026-11-23, and the domain each version's listing gives.
        let published = [
            ("aca-fl", "Insurance", "underwriting"),
            ("aca-oh", "Insurance", "underwriting"),
            ("aca-tx", "Insurance", "underwriting"),
            ("air-passenger-rights", "Transport", "customer-support"),
            ("bigcodebench", "Coding", "coding"),
            ("cp-oh", "Insurance", "underwriting"),
            ("cruxeval-input", "Coding", "coding"),
            ("eo-oh", "Insurance", "underwriting"),
            ("escrow-accounts", "Finance", "finance"),
            ("exercism-polyglot", "Coding", "coding"),
            ("farm-oh", "Insurance", "underwriting"),
            ("fdic-banking", "Finance", "finance"),
            ("federal-grants", "Finance", "finance"),
            ("flight-duty", "Transport", "legal"),
            ("flsa-overtime", "Employment law", "legal"),
            ("fmla-leave", "Employment law", "legal"),
            ("food-labeling", "Health compliance", "healthcare"),
            ("gl-oh", "Insurance", "underwriting"),
            ("hipaa-breach", "Health compliance", "healthcare"),
            ("hours-of-service", "Transport", "legal"),
            ("humaneval-plus", "Coding", "coding"),
            ("humanevalfix", "Coding", "coding"),
            ("icd10cm-coding", "Health compliance", "healthcare"),
            ("irs-inflation-adjustments", "Finance", "finance"),
            ("mbpp-plus", "Coding", "coding"),
            ("medicare-oh", "Insurance", "underwriting"),
            ("medmal-oh", "Insurance", "underwriting"),
            ("medsupp-oh", "Insurance", "actuarial"),
            ("multi-swe-bench", "Software engineering", "coding"),
            ("nfip-fl", "Insurance", "claims"),
            ("nfip-la", "Insurance", "claims"),
            ("nfip-oh", "Insurance", "claims"),
            ("nfip-tx", "Insurance", "claims"),
            ("nist-800-53", "Security compliance", "security"),
            ("noaa-storm-events", "Natural hazards", "data-analysis"),
            ("osha-recordkeeping", "Employment law", "legal"),
            ("personal-auto-oh", "Insurance", "underwriting"),
            ("pet-oh", "Insurance", "underwriting"),
            ("ratemaking-oh", "Insurance", "actuarial"),
            ("surety-oh", "Insurance", "underwriting"),
            ("swebench-lite", "Software engineering", "coding"),
            ("swebench-multilingual", "Software engineering", "coding"),
            ("swebench-verified", "Software engineering", "coding"),
            ("terminal-bench", "Terminal work", "coding"),
            ("title-oh", "Insurance", "underwriting"),
            ("treasury-auctions", "Finance", "finance"),
            ("underwriting-starter", "Insurance", "underwriting"),
            ("us-life-tables", "Insurance", "actuarial"),
            ("usgs-earthquakes", "Natural hazards", "data-analysis"),
        ];
        for (name, listing, want) in published {
            assert_eq!(domain_of(&domains, name, None), want, "{name}");
            assert_eq!(domain_of(&domains, name, Some(listing)), want, "{name}");
        }
        // A name no glob claims goes where its listing's domain points, else to Other.
        let unclaimed = [
            "Insurance",
            "Employment law",
            "Health compliance",
            "Transport",
            "Natural hazards",
            "Security compliance",
            "Software engineering",
        ];
        assert!(unclaimed.iter().all(|l| domain_of(&domains, "new-set", Some(l)) != OTHER));
        assert_eq!(domain_of(&domains, "new-set", Some("employment LAW ")), "legal");
        assert_eq!(domain_of(&domains, "new-set", Some("Astrology")), OTHER);
        assert_eq!(domain_of(&domains, "new-set", None), OTHER);
    }
}
