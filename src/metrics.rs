//! Job metrics: mean/sum/min/max/uv-script and unbiased pass@k.

use crate::config::{MetricConfig, MetricKind};
use crate::trial::{Rewards, TrialResult};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};

pub type MetricOut = BTreeMap<String, serde_json::Value>;

/// Aggregate reward objects: missing rewards count as 0; one key => {name: x}, else per key.
fn aggregate(rewards: &[Option<&Rewards>], name: &str, f: fn(&[f64]) -> f64) -> MetricOut {
    let keys: BTreeSet<&String> = rewards.iter().flatten().flat_map(|r| r.keys()).collect();
    let val = |r: &Option<&Rewards>, k: Option<&str>| -> f64 {
        let Some(r) = r else { return 0.0 };
        match k {
            Some(k) => r.get(k).and_then(|n| n.as_f64()).unwrap_or(0.0),
            None => r.values().next().and_then(|n| n.as_f64()).unwrap_or(0.0),
        }
    };
    let num = |x: f64| serde_json::Number::from_f64(x).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null);
    if keys.len() <= 1 {
        let v: Vec<f64> = rewards.iter().map(|r| val(r, None)).collect();
        return [(name.to_string(), num(f(&v)))].into();
    }
    keys.into_iter().map(|k| (k.clone(), num(f(&rewards.iter().map(|r| val(r, Some(k))).collect::<Vec<_>>())))).collect()
}

pub fn compute(m: &MetricConfig, rewards: &[Option<&Rewards>]) -> Result<MetricOut> {
    let name = m.kind.as_str();
    Ok(match m.kind {
        MetricKind::Mean => aggregate(rewards, name, |v| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 }),
        MetricKind::Sum => aggregate(rewards, name, |v| v.iter().sum()),
        MetricKind::Min => aggregate(rewards, name, |v| v.iter().cloned().fold(f64::INFINITY, f64::min)),
        MetricKind::Max => aggregate(rewards, name, |v| v.iter().cloned().fold(f64::NEG_INFINITY, f64::max)),
        MetricKind::UvScript => uv_script(m, rewards)?,
    })
}

/// `uv run <script> -i rewards.jsonl -o metric.json` (one JSON reward object or null per line).
fn uv_script(m: &MetricConfig, rewards: &[Option<&Rewards>]) -> Result<MetricOut> {
    let script = m.kwargs.get("script_path").and_then(|v| v.as_str()).context("uv-script metric needs kwargs.script_path")?;
    if !std::path::Path::new(script).exists() {
        bail!("Script file not found: {script}");
    }
    let dir = std::env::temp_dir().join(format!("hbench-metric-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir)?;
    let input = dir.join("rewards.jsonl");
    let output = dir.join("metric.json");
    let lines: Vec<String> = rewards.iter().map(|r| r.map_or("null".to_string(), |r| serde_json::to_string(r).unwrap())).collect();
    std::fs::write(&input, lines.join("\n") + "\n")?;
    let out = std::process::Command::new("uv")
        .arg("run")
        .arg(script)
        .arg("-i")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .output()
        .context("running uv")?;
    if !out.status.success() {
        bail!("Failed to compute custom metric: {}", String::from_utf8_lossy(&out.stderr));
    }
    let v = serde_json::from_str(&std::fs::read_to_string(&output)?)?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(v)
}

/// Unbiased pass@k over binary single-key rewards; empty if rewards aren't binary.
pub fn pass_at_k(results: &[&TrialResult]) -> BTreeMap<usize, f64> {
    let mut per_task: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    for r in results {
        let v = match r.reward() {
            None => 0,
            Some(rw) if rw.len() != 1 => return BTreeMap::new(),
            Some(rw) => match rw.values().next().and_then(|n| n.as_f64()) {
                Some(0.0) => 0,
                Some(1.0) => 1,
                _ => return BTreeMap::new(),
            },
        };
        per_task.entry(&r.task_name).or_default().push(v);
    }
    let Some(min_n) = per_task.values().map(Vec::len).min() else { return BTreeMap::new() };
    let mut ks = BTreeSet::new();
    let mut k = 2;
    while k <= min_n {
        ks.insert(k);
        k *= 2;
    }
    let mut k = 5;
    while k <= min_n {
        ks.insert(k);
        k += 5;
    }
    ks.into_iter()
        .map(|k| {
            let total: f64 = per_task
                .values()
                .map(|s| {
                    let (n, c) = (s.len(), s.iter().sum::<u32>() as usize);
                    if n - c < k {
                        return 1.0;
                    }
                    1.0 - (0..k).map(|i| (n - c - i) as f64 / (n - i) as f64).product::<f64>()
                })
                .sum();
            (k, total / per_task.len() as f64)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rw(s: &str) -> Rewards {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn metrics_match_reference() {
        let (a, b) = (rw(r#"{"reward":1}"#), rw(r#"{"reward":0.5}"#));
        let rs = [Some(&a), Some(&b), None];
        let m = |kind: MetricKind| compute(&MetricConfig { kind, kwargs: Default::default() }, &rs).unwrap();
        assert_eq!(m(MetricKind::Mean)["mean"], 0.5);
        assert_eq!(m(MetricKind::Sum)["sum"], 1.5);
        assert_eq!(m(MetricKind::Min)["min"], 0.0);
        assert_eq!(m(MetricKind::Max)["max"], 1.0);
        let (x, y) = (rw(r#"{"x":1,"y":0.5}"#), rw(r#"{"x":0}"#));
        let multi = compute(&MetricConfig::default(), &[Some(&x), Some(&y), None]).unwrap();
        assert_eq!((multi["x"].as_f64().unwrap(), multi["y"].as_f64().unwrap()), (1.0 / 3.0, 0.5 / 3.0));
        assert!(serde_json::from_value::<MetricConfig>(serde_json::json!({"type": "median"})).is_err());
        let uv: MetricConfig = serde_json::from_value(serde_json::json!({"type": "uv-script"})).unwrap();
        assert_eq!((uv.kind, serde_json::to_value(&uv).unwrap()["type"].as_str()), (MetricKind::UvScript, Some("uv-script")));
    }
}
