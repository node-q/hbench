//! Small filesystem helpers shared across modules.

use serde_json::Value;
use std::path::Path;

/// A JSON file's value; None when it is missing or not valid JSON.
pub fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}
