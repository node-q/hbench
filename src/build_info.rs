//! The build of hbench that ran a job: its version, its results epoch and the commit it was built from.

/// Releases whose results compare. A release that changes how a task runs or is graded starts a new
/// epoch, so its results are never mixed with an earlier epoch's; 0.2.0 began this one.
pub const RESULTS_EPOCH: &str = "0.2";

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit this binary was built from, when the build knew it (build.rs).
pub fn commit() -> Option<&'static str> {
    option_env!("HBENCH_COMMIT").filter(|c| !c.is_empty())
}

/// `{"version", "results_epoch", "commit"}`, as lock.json and manifest.json record the build.
pub fn json() -> serde_json::Value {
    let mut v = serde_json::json!({"version": VERSION, "results_epoch": RESULTS_EPOCH});
    if let Some(c) = commit() {
        v["commit"] = c.into();
    }
    v
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_names_the_version_and_epoch() {
        let v = super::json();
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["results_epoch"], super::RESULTS_EPOCH);
    }
}
