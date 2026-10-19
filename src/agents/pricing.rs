//! Token pricing from the bundled model price table (assets/litellm, pinned to 1.93.0): per-token
//! input/output/cache rates, with tiered rates above a prompt-size threshold; text tokens only.

use serde_json::{Map, Value};
use std::sync::OnceLock;

fn table() -> &'static Map<String, Value> {
    static T: OnceLock<Map<String, Value>> = OnceLock::new();
    T.get_or_init(|| serde_json::from_str(include_str!("../../assets/litellm/model_prices_and_context_window.json")).unwrap_or_default())
}

/// The price-table entry for `key` (non-empty entries only).
pub fn model_cost(key: &str) -> Option<&'static Map<String, Value>> {
    table().get(key).and_then(Value::as_object).filter(|m| !m.is_empty() && key != "sample_spec")
}

/// First of `model`, `model` without its `provider/` prefix, that has a pricing entry.
pub fn pricing_key(model: &str) -> Option<String> {
    [model, model.split_once('/').map_or(model, |(_, n)| n)].into_iter().find(|k| model_cost(k).is_some()).map(String::from)
}

fn unit(m: &Map<String, Value>, key: &str) -> Option<f64> {
    match m.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn threshold(key: &str) -> Option<f64> {
    let t = key.split("_above_").nth(1)?.split("_tokens").next()?;
    let n: f64 = t.replace('k', "").parse().ok()?;
    Some(if t.contains('k') { n * 1000.0 } else { n })
}

/// Per-token rates for one request.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Rates {
    /// (prompt_cost, completion_cost); `prompt` includes cached and cache-write tokens.
    pub fn cost(&self, prompt: i64, completion: i64, cache_creation: i64, cache_read: i64) -> (f64, f64) {
        let text = (prompt - cache_read - cache_creation).max(0);
        let prompt_cost = text as f64 * self.input + cache_read as f64 * self.cache_read + cache_creation as f64 * self.cache_write;
        (prompt_cost, completion as f64 * self.output)
    }
}

/// The rates of pricing entry `m` for a request of `prompt` tokens (tiers applied).
pub fn rates(m: &Map<String, Value>, prompt: i64) -> Rates {
    let mut r = Rates {
        input: unit(m, "input_cost_per_token").unwrap_or(0.0),
        output: unit(m, "output_cost_per_token").unwrap_or(0.0),
        cache_read: unit(m, "cache_read_input_token_cost").unwrap_or(0.0),
        cache_write: unit(m, "cache_creation_input_token_cost").unwrap_or(0.0),
    };
    if r.output == 0.0
        && let Some(img) = unit(m, "output_cost_per_image_token")
    {
        r.output = img;
    }
    // Tiered pricing: the highest threshold below prompt_tokens applies to every token type.
    let tier_suffixes = ["_flex", "_priority"];
    let mut keys: Vec<&String> =
        m.keys().filter(|k| k.starts_with("input_cost_per_token_above_") && !tier_suffixes.iter().any(|s| k.ends_with(s))).collect();
    keys.sort_by(|a, b| threshold(b).unwrap_or(0.0).total_cmp(&threshold(a).unwrap_or(0.0)));
    for k in keys {
        let (Some(t), Some(v)) = (threshold(k), unit(m, k)) else { continue };
        if prompt as f64 > t {
            let ts = k.split("_above_").nth(1).unwrap().split("_tokens").next().unwrap();
            r.input = v;
            r.output = unit(m, &format!("output_cost_per_token_above_{ts}_tokens")).unwrap_or(r.output);
            r.cache_write = unit(m, &format!("cache_creation_input_token_cost_above_{ts}_tokens")).unwrap_or(r.cache_write);
            r.cache_read = unit(m, &format!("cache_read_input_token_cost_above_{ts}_tokens")).unwrap_or(r.cache_read);
            break;
        }
    }
    r
}

/// (prompt_cost, completion_cost) of one request to `model`. `prompt` includes the cache-write
/// (`cache_creation`) and cache-read (`cache_read`) tokens, which are priced at their own rates.
pub fn cost_per_token(model: &str, prompt: i64, completion: i64, cache_creation: i64, cache_read: i64) -> Option<(f64, f64)> {
    Some(rates(model_cost(model)?, prompt).cost(prompt, completion, cache_creation, cache_read))
}

/// Total cost for a (possibly provider-prefixed) model, or None if the price table has no entry.
pub fn total_cost(model: &str, prompt: i64, completion: i64, cache_creation: i64, cache_read: i64) -> Option<f64> {
    let key = pricing_key(model)?;
    cost_per_token(&key, prompt, completion, cache_creation, cache_read).map(|(a, b)| a + b)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn price_table_costs() {
        // claude-opus-4-1: in 1.5e-5, out 7.5e-5, cache read 1.5e-6, cache write 1.875e-5.
        let (p, c) = cost_per_token("claude-opus-4-1", 1_000_000, 100_000, 100_000, 400_000).unwrap();
        assert!((p - (500_000.0 * 1.5e-5 + 400_000.0 * 1.5e-6 + 100_000.0 * 1.875e-5)).abs() < 1e-9);
        assert!((c - 100_000.0 * 7.5e-5).abs() < 1e-9);
        // gemini-2.5-pro tiers above 200k prompt tokens.
        let (p, c) = cost_per_token("gemini-2.5-pro", 300_000, 1_000, 0, 0).unwrap();
        assert!((p - 300_000.0 * 2.5e-6).abs() < 1e-9 && (c - 1_000.0 * 1.5e-5).abs() < 1e-9);
        let (p, _) = cost_per_token("gemini-2.5-pro", 100_000, 0, 0, 0).unwrap();
        assert!((p - 100_000.0 * 1.25e-6).abs() < 1e-9);
        assert_eq!(pricing_key("openai/gpt-5").as_deref(), Some("gpt-5"));
        assert!(pricing_key("nope/not-a-model").is_none());
        assert!(total_cost("anthropic/claude-sonnet-4-5", 10, 10, 0, 0).is_some());
    }
}
