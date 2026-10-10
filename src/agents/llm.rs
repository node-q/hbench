//! Async LLM chat client: the part of upstream's LiteLLM wrapper (`llms/lite_llm.py`, `chat.py`,
//! `utils.py`) that host-side agents use.
//!
//! `anthropic/` models go to the Anthropic Messages API; everything else to an OpenAI-compatible
//! chat-completions endpoint (or the OpenAI Responses API when asked). Keys and base URLs come from
//! `model_connection`. Calls are retried like upstream's `@retry` on `LiteLLM.call`, context overflow
//! is a distinct error, and usage/cost are reported like LiteLLM's `ModelResponse.usage`.
//!
//! ponytail: HTTP goes through the host `curl` (no HTTP client crate in Cargo.toml), like
//! `registry.rs`. Secrets travel on curl's stdin config, never argv.

use super::model_connection::{self, Spec};
use crate::env::{kind_err, tail};
use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    /// Includes cached and cache-creation tokens (LiteLLM's `prompt_tokens`).
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    /// Cache reads (`prompt_tokens_details.cached_tokens`).
    pub cache_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Default)]
pub struct LlmResponse {
    pub content: String,
    pub reasoning_content: Option<String>,
    pub model_name: Option<String>,
    pub usage: Option<Usage>,
    pub response_id: Option<String>,
    pub prompt_token_ids: Option<Vec<i64>>,
    pub completion_token_ids: Option<Vec<i64>>,
    pub logprobs: Option<Vec<f64>>,
}

#[derive(Debug, Clone)]
pub enum LlmError {
    /// upstream's `ContextLengthExceededError`.
    ContextLength(String),
    /// upstream's `OutputLengthExceededError` (finish_reason == "length").
    OutputLength { msg: String, truncated: String },
    /// Anything else; `kind` is the LiteLLM exception class name upstream would report.
    Api { kind: &'static str, msg: String },
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::ContextLength(m) | LlmError::OutputLength { msg: m, .. } | LlmError::Api { msg: m, .. } => f.write_str(m),
        }
    }
}
impl std::error::Error for LlmError {}

impl LlmError {
    pub fn kind(&self) -> &'static str {
        match self {
            LlmError::ContextLength(_) => "ContextLengthExceededError",
            LlmError::OutputLength { .. } => "OutputLengthExceededError",
            LlmError::Api { kind, .. } => kind,
        }
    }
    /// upstream retries everything but these.
    fn retryable(&self) -> bool {
        !matches!(self, LlmError::ContextLength(_) | LlmError::OutputLength { .. } | LlmError::Api { kind: "AuthenticationError", .. })
    }
    /// For the trial result: a `KindError` carrying the exception type name.
    pub fn into_anyhow(self) -> anyhow::Error {
        kind_err(self.kind(), self.to_string())
    }
}

fn api_err(kind: &'static str, msg: impl Into<String>) -> LlmError {
    LlmError::Api { kind, msg: msg.into() }
}

/// LiteLLM model-map fields upstream reads (limits + per-token pricing).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelInfo {
    pub max_input_tokens: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub input_cost: f64,
    pub output_cost: f64,
    /// 0 = billed like input.
    pub cache_read_cost: f64,
    pub cache_write_cost: f64,
    /// Claude models where LiteLLM maps reasoning_effort to `thinking: {type: adaptive}`.
    pub adaptive_thinking: bool,
}

const fn mi(max_in: i64, max_out: i64, ic: f64, oc: f64, cr: f64, cw: f64, adaptive: bool) -> ModelInfo {
    ModelInfo { max_input_tokens: Some(max_in), max_output_tokens: Some(max_out), input_cost: ic, output_cost: oc, cache_read_cost: cr, cache_write_cost: cw, adaptive_thinking: adaptive }
}

/// LiteLLM 1.92 `model_prices_and_context_window.json`, only for models upstream's tests use.
/// ponytail: unknown models get no pricing (cost None) and the 1M fallback context limit, as in upstream.
fn builtin_info(name: &str) -> Option<ModelInfo> {
    Some(match name {
        "claude-sonnet-4-5" | "claude-sonnet-4-5-20250929" => mi(200_000, 64_000, 3e-6, 1.5e-5, 3e-7, 3.75e-6, false),
        "claude-sonnet-4-20250514" => mi(1_000_000, 64_000, 3e-6, 1.5e-5, 3e-7, 3.75e-6, false),
        "claude-sonnet-4-6" => mi(1_000_000, 64_000, 3e-6, 1.5e-5, 3e-7, 3.75e-6, true),
        "claude-sonnet-5" => mi(1_000_000, 128_000, 2e-6, 1e-5, 2e-7, 2.5e-6, true),
        "claude-opus-4-1" | "claude-opus-4-1-20250805" => mi(200_000, 32_000, 1.5e-5, 7.5e-5, 1.5e-6, 1.875e-5, false),
        "claude-opus-4-5" => mi(200_000, 64_000, 5e-6, 2.5e-5, 5e-7, 6.25e-6, false),
        "claude-opus-4-6" | "claude-opus-4-7" | "claude-opus-4-8" => mi(1_000_000, 128_000, 5e-6, 2.5e-5, 5e-7, 6.25e-6, true),
        "claude-haiku-4-5" | "claude-haiku-4-5-20251001" => mi(200_000, 64_000, 1e-6, 5e-6, 1e-7, 1.25e-6, false),
        "claude-fable-5" => mi(1_000_000, 128_000, 1e-5, 5e-5, 1e-6, 1.25e-5, true),
        "gpt-4o" => mi(128_000, 16_384, 2.5e-6, 1e-5, 1.25e-6, 0.0, false),
        "gpt-4o-mini" => mi(128_000, 16_384, 1.5e-7, 6e-7, 7.5e-8, 0.0, false),
        "gpt-4.1" => mi(1_047_576, 32_768, 2e-6, 8e-6, 5e-7, 0.0, false),
        "gpt-4" => mi(8_192, 4_096, 3e-5, 6e-5, 0.0, 0.0, false),
        "gpt-3.5-turbo" => mi(16_385, 4_096, 5e-7, 1.5e-6, 0.0, 0.0, false),
        "o3" => mi(200_000, 100_000, 2e-6, 8e-6, 5e-7, 0.0, false),
        "gpt-5" | "gpt-5.1" => mi(272_000, 128_000, 1.25e-6, 1e-5, 1.25e-7, 0.0, false),
        "gpt-5-mini" => mi(272_000, 128_000, 2.5e-7, 2e-6, 2.5e-8, 0.0, false),
        "gpt-5-nano" => mi(272_000, 128_000, 5e-8, 4e-7, 5e-9, 0.0, false),
        "gpt-5.2" | "gpt-5.3-codex" => mi(272_000, 128_000, 1.75e-6, 1.4e-5, 1.75e-7, 0.0, false),
        "gpt-5.4" => mi(1_050_000, 128_000, 2.5e-6, 1.5e-5, 2.5e-7, 0.0, false),
        "gpt-5.5" => mi(1_050_000, 128_000, 5e-6, 3e-5, 5e-7, 0.0, false),
        "xai/grok-4" => mi(256_000, 256_000, 3e-6, 1.5e-5, 0.0, 0.0, false),
        "deepseek/deepseek-chat" => mi(131_072, 8_192, 2.8e-7, 4.2e-7, 2.8e-8, 0.0, false),
        "deepseek/deepseek-v4-flash" => mi(1_000_000, 8_192, 1.4e-7, 2.8e-7, 2.8e-9, 0.0, false),
        "gemini/gemini-2.5-pro" => mi(1_048_576, 65_535, 1.25e-6, 1e-5, 1.25e-7, 0.0, false),
        "gemini/gemini-2.5-flash" => mi(1_048_576, 65_535, 3e-7, 2.5e-6, 3e-8, 0.0, false),
        _ => return None,
    })
}

/// Full name first, then with provider prefixes stripped (`openrouter/anthropic/x` -> `anthropic/x` -> `x`).
pub fn lookup_model_info(model: &str) -> Option<ModelInfo> {
    let mut n = model;
    loop {
        if let Some(i) = builtin_info(n) {
            return Some(i);
        }
        n = n.split_once('/')?.1;
    }
}

/// upstream's `model_info` kwarg (litellm.register_model): overrides the known entry.
fn merge_model_info(mut info: ModelInfo, v: &Value) -> ModelInfo {
    let i = |k: &str| v.get(k).and_then(|x| x.as_f64().or_else(|| x.as_str()?.parse().ok()));
    if let Some(x) = i("max_input_tokens").or_else(|| i("max_tokens")) {
        info.max_input_tokens = Some(x as i64);
    }
    if let Some(x) = i("max_output_tokens") {
        info.max_output_tokens = Some(x as i64);
    }
    for (k, f) in [
        ("input_cost_per_token", &mut info.input_cost),
        ("output_cost_per_token", &mut info.output_cost),
        ("cache_read_input_token_cost", &mut info.cache_read_cost),
        ("cache_creation_input_token_cost", &mut info.cache_write_cost),
    ] {
        if let Some(x) = i(k) {
            *f = x;
        }
    }
    info
}

/// Port of `validate_hosted_vllm_model_config`.
fn validate_hosted_vllm(model: &str, info: Option<&Value>) -> Result<()> {
    if !model.starts_with("hosted_vllm/") {
        bail!("hosted_vllm models must start with 'hosted_vllm/'. Got '{model}'.");
    }
    if model.matches('/').count() != 1 {
        bail!("hosted_vllm model names must contain exactly one '/'. Got '{model}'.");
    }
    let canonical = &model["hosted_vllm/".len()..];
    if canonical.is_empty() || canonical.len() > 64 || !canonical.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        bail!(
            "hosted_vllm canonical model names may only contain letters, numbers, '.', '-', '_' and must be fewer than 64 characters with no spaces. Got '{canonical}'."
        );
    }
    let Some(info) = info.filter(|v| v.as_object().is_some_and(|o| !o.is_empty())) else {
        bail!(
            "hosted_vllm models require `model_info` specifying token limits and costs. Please provide max_input_tokens, max_output_tokens, input_cost_per_token, and output_cost_per_token."
        )
    };
    for f in ["max_input_tokens", "max_output_tokens", "input_cost_per_token", "output_cost_per_token"] {
        match info.get(f) {
            None | Some(Value::Null) => bail!("hosted_vllm model_info missing '{f}'."),
            Some(v) if v.as_f64().or_else(|| v.as_str()?.parse::<f64>().ok()).is_none() => bail!("hosted_vllm model_info field '{f}' must be a number. Got '{v}'."),
            _ => {}
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Api {
    Anthropic,
    Chat,
    Responses,
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub api: Api,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Model name sent to the provider (provider prefix stripped, like LiteLLM).
    pub model: String,
    pub provider: String,
}

/// Base URLs LiteLLM knows that `model_connection` doesn't list.
fn default_base(provider: &str) -> Option<&'static str> {
    model_connection::provider(provider).and_then(|p| p.base_url).or(match provider {
        "ollama" | "ollama_chat" => Some("http://localhost:11434"),
        "fireworks_ai" => Some("https://api.fireworks.ai/inference/v1"),
        "cerebras" => Some("https://api.cerebras.ai/v1"),
        "deepinfra" => Some("https://api.deepinfra.com/v1/openai"),
        "perplexity" => Some("https://api.perplexity.ai"),
        "dashscope" => Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1"),
        "zai" => Some("https://api.z.ai/api/paas/v4"),
        _ => None,
    })
}

/// Where and how to send requests for `model` (credentials: agent env > host env via `get`).
pub fn resolve_endpoint(model: &str, api_base: Option<&str>, responses: bool, get: &dyn Fn(&str) -> Option<String>) -> Result<Endpoint> {
    let getter = |names: &[&str]| names.iter().find_map(|n| get(n).filter(|v| !v.is_empty()).map(|v| (n.to_string(), v)));
    let conn = model_connection::resolve(Some(model), Spec::default(), &getter);
    let provider = conn.provider.clone().unwrap_or_else(|| "openai".into());
    let sent = model.split_once('/').map(|(_, m)| m).unwrap_or(model).to_string();
    let upper = provider.to_uppercase().replace('-', "_");
    let key = conn.api_key.clone().or_else(|| get(&format!("{upper}_API_KEY")));
    let base = api_base
        .map(String::from)
        .or(conn.configured_base_url.clone())
        .or_else(|| get(&format!("{upper}_API_BASE")))
        .or_else(|| default_base(&provider).map(String::from))
        .map(|b| b.trim_end_matches('/').to_string());
    let mut headers: Vec<(String, String)> = vec![];
    let h = |k: &str, v: String| (k.to_string(), v);
    match provider.as_str() {
        "anthropic" => {
            if let Some(k) = key {
                headers.push(h("x-api-key", k));
            } else if let Some(t) = get("CLAUDE_CODE_OAUTH_TOKEN").or_else(|| get("ANTHROPIC_AUTH_TOKEN")).filter(|t| !t.is_empty()) {
                headers.push(h("authorization", format!("Bearer {}", t.trim())));
                headers.push(h("anthropic-beta", "oauth-2025-04-20".into()));
            }
            headers.push(h("anthropic-version", "2023-06-01".into()));
            let b = base.unwrap_or_else(|| "https://api.anthropic.com".into());
            let url = if b.ends_with("/v1/messages") {
                b
            } else if b.ends_with("/v1") {
                format!("{b}/messages")
            } else {
                format!("{b}/v1/messages")
            };
            return Ok(Endpoint { api: Api::Anthropic, url, headers, model: sent, provider });
        }
        "amazon-bedrock" | "vertex_ai" | "sagemaker" => {
            bail!("LLM provider '{provider}' (model {model}) needs AWS SigV4 / Google Cloud auth, which hbench's LLM client doesn't implement; use an OpenAI-compatible gateway via api_base instead")
        }
        _ => {}
    }
    let mut b = base.ok_or_else(|| anyhow!("No API base URL known for LLM provider '{provider}' (model {model}); set {upper}_BASE_URL or the api_base kwarg"))?;
    if provider == "azure" {
        let ver = get("AZURE_API_VERSION").unwrap_or_else(|| "2024-10-21".into());
        if let Some(k) = key {
            headers.push(h("api-key", k));
        }
        let url = format!("{b}/openai/deployments/{sent}/chat/completions?api-version={ver}");
        return Ok(Endpoint { api: Api::Chat, url, headers, model: sent, provider });
    }
    if provider == "google" && !b.contains("/openai") {
        b += "/v1beta/openai";
    }
    if provider.starts_with("ollama") && !b.ends_with("/v1") {
        b += "/v1";
    }
    if let Some(k) = key {
        headers.push(h("authorization", format!("Bearer {k}")));
    }
    let (api, url) = if responses { (Api::Responses, format!("{b}/responses")) } else { (Api::Chat, format!("{b}/chat/completions")) };
    Ok(Endpoint { api, url, headers, model: sent, provider })
}

/// upstream's LiteLLM constructor arguments.
#[derive(Debug, Clone, Default)]
pub struct LlmConfig {
    pub model_name: String,
    pub api_base: Option<String>,
    pub temperature: Option<f64>,
    pub reasoning_effort: Option<String>,
    pub max_thinking_tokens: Option<i64>,
    pub session_id: Option<String>,
    pub collect_rollout_details: bool,
    pub model_info: Option<Value>,
    pub use_responses_api: bool,
    /// Extra constructor kwargs (upstream `llm_kwargs`), sent with every request.
    pub llm_kwargs: Map<String, Value>,
}

#[async_trait::async_trait]
pub trait Llm: Send + Sync {
    /// `kw`: per-call kwargs (upstream `llm_call_kwargs`: extra_body, extra_headers, sampling params).
    async fn call(&self, prompt: &str, history: &[Value], kw: &Map<String, Value>, previous_response_id: Option<&str>) -> Result<LlmResponse, LlmError>;
    fn context_limit(&self) -> i64;
    fn output_limit(&self) -> Option<i64>;
    fn count_tokens(&self, messages: &[Value]) -> i64 {
        approx_tokens(messages)
    }
}

/// ponytail: ~4 chars/token (no tokenizer crate); LiteLLM's token_counter uses tiktoken.
pub fn approx_tokens(messages: &[Value]) -> i64 {
    3 + messages.iter().map(|m| 4 + (content_text(&m["content"]).chars().count() as i64 + 3) / 4).sum::<i64>()
}

/// Text of a string or content-part list.
pub fn content_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""),
        _ => String::new(),
    }
}

/// Port of `add_anthropic_caching`: ephemeral cache_control on the newest content blocks, max 4.
pub fn add_anthropic_caching(mut messages: Vec<Value>, model: &str) -> Vec<Value> {
    let l = model.to_lowercase();
    if !(l.contains("anthropic") || l.contains("claude")) {
        return messages;
    }
    let mut remaining = 4;
    for m in messages.iter_mut().rev() {
        if remaining == 0 {
            break;
        }
        let Some(c) = m.get_mut("content") else { continue };
        if let Some(s) = c.as_str().map(String::from) {
            *c = json!([{"type": "text", "text": s, "cache_control": {"type": "ephemeral"}}]);
            remaining -= 1;
        } else if let Some(parts) = c.as_array_mut() {
            for p in parts.iter_mut().rev() {
                if remaining == 0 {
                    break;
                }
                if p.get("type").is_some() {
                    p["cache_control"] = json!({"type": "ephemeral"});
                    remaining -= 1;
                }
            }
        }
    }
    messages
}

fn is_claude(model: &str) -> bool {
    let l = model.to_lowercase();
    l.contains("anthropic") || l.contains("claude")
}

/// upstream's `_is_context_length_error` phrases (plus LiteLLM's own mapping).
pub fn is_context_length_error(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "context length exceeded",
        "context_length_exceeded",
        "maximum context length",
        "`inputs` tokens + `max_new_tokens`",
        "model's context length",
        "prompt is too long",
        "input is too long for requested model",
    ]
    .iter()
    .any(|p| t.contains(p))
}

/// LiteLLM's exception class for an HTTP error status.
pub fn classify_http(status: u16, body: &str) -> LlmError {
    let msg = format!("HTTP {status}: {}", tail(body, 4000));
    if matches!(status, 400 | 413) && is_context_length_error(body) {
        return LlmError::ContextLength(msg);
    }
    let kind = match status {
        400 => "BadRequestError",
        401 => "AuthenticationError",
        403 => "PermissionDeniedError",
        404 => "NotFoundError",
        408 => "Timeout",
        422 => "UnprocessableEntityError",
        429 => "RateLimitError",
        503 => "ServiceUnavailableError",
        s if s >= 500 => "InternalServerError",
        _ => "APIError",
    };
    api_err(kind, msg)
}

/// One HTTP request, provider-specific.
#[derive(Debug, Clone)]
pub struct Request {
    pub headers: Vec<(String, String)>,
    pub body: Value,
    pub timeout: Duration,
}

/// LiteLLM's default request timeout.
const DEFAULT_TIMEOUT_SECS: f64 = 6000.0;
/// LiteLLM/transport kwargs that must not reach the request body.
const NON_BODY: [&str; 8] = ["api_base", "base_url", "drop_params", "num_retries", "custom_llm_provider", "logger_fn", "litellm_debug", "max_retries"];

fn obj<'a>(m: &'a mut Map<String, Value>, k: &str) -> &'a mut Map<String, Value> {
    if !m.get(k).is_some_and(Value::is_object) {
        m.insert(k.into(), json!({}));
    }
    m.get_mut(k).and_then(Value::as_object_mut).unwrap()
}

/// Pull transport-level settings out of LiteLLM-style params.
fn split_transport(p: &mut Map<String, Value>, ep: &Endpoint) -> (Vec<(String, String)>, Duration, Map<String, Value>) {
    let mut headers = ep.headers.clone();
    if let Some(k) = p.remove("api_key").and_then(|v| v.as_str().map(String::from)) {
        let (name, val) = match ep.api {
            Api::Anthropic => ("x-api-key", k),
            _ if ep.provider == "azure" => ("api-key", k),
            _ => ("authorization", format!("Bearer {k}")),
        };
        headers.retain(|(h, _)| h != name && !(name == "x-api-key" && h == "authorization"));
        headers.push((name.into(), val));
    }
    if let Some(Value::Object(eh)) = p.remove("extra_headers") {
        for (k, v) in eh {
            let v = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
            headers.retain(|(h, _)| !h.eq_ignore_ascii_case(&k));
            headers.push((k, v));
        }
    }
    let timeout = p.remove("timeout").and_then(|v| v.as_f64()).filter(|t| *t > 0.0).unwrap_or(DEFAULT_TIMEOUT_SECS);
    for k in NON_BODY {
        p.remove(k);
    }
    let extra = match p.remove("extra_body") {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    p.retain(|_, v| !v.is_null());
    (headers, Duration::from_secs_f64(timeout), extra)
}

/// OpenAI reasoning models only accept the default temperature; LiteLLM's drop_params drops it.
fn openai_rejects_temperature(ep: &Endpoint, t: &Value) -> bool {
    let m = ep.model.to_lowercase();
    ep.provider == "openai" && ["o1", "o3", "o4", "gpt-5"].iter().any(|p| m.starts_with(p)) && t.as_f64() != Some(1.0)
}

/// LiteLLM's `reasoning_effort` -> Anthropic `thinking` mapping.
fn anthropic_thinking(effort: &str, adaptive: bool) -> Option<Value> {
    let budget = match effort {
        "none" => return None,
        _ if adaptive => return Some(json!({"type": "adaptive"})),
        "minimal" | "low" => 1024,
        "medium" => 2048,
        "high" => 4096,
        "xhigh" => 8192,
        "max" => 16384,
        _ => return None,
    };
    Some(json!({"type": "enabled", "budget_tokens": budget}))
}

/// OpenAI-format messages -> Anthropic (system, messages).
fn to_anthropic_messages(messages: &[Value]) -> (Vec<Value>, Vec<Value>) {
    let parts = |c: &Value| -> Value {
        match c {
            Value::Array(ps) => Value::Array(
                ps.iter()
                    .map(|p| {
                        let url = p["image_url"]["url"].as_str().or(p["image_url"].as_str());
                        match (p["type"].as_str(), url) {
                            (Some("image_url"), Some(url)) => {
                                let (meta, data) = url.split_once(',').unwrap_or(("", url));
                                let media = meta.trim_start_matches("data:").split(';').next().filter(|m| !m.is_empty()).unwrap_or("image/png");
                                let mut b = json!({"type": "image", "source": {"type": "base64", "media_type": media, "data": data}});
                                if let Some(cc) = p.get("cache_control") {
                                    b["cache_control"] = cc.clone();
                                }
                                b
                            }
                            _ => p.clone(),
                        }
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    };
    let (mut system, mut out) = (vec![], vec![]);
    for m in messages {
        let c = parts(&m["content"]);
        if m["role"] == "system" {
            match c {
                Value::String(s) => system.push(json!({"type": "text", "text": s})),
                Value::Array(ps) => system.extend(ps),
                _ => {}
            }
        } else {
            out.push(json!({"role": m["role"], "content": c}));
        }
    }
    (system, out)
}

fn cost(info: Option<&ModelInfo>, uncached_in: i64, cache_read: i64, cache_write: i64, output: i64) -> f64 {
    let Some(i) = info else { return 0.0 };
    let or_in = |c: f64| if c > 0.0 { c } else { i.input_cost };
    uncached_in as f64 * i.input_cost + cache_read as f64 * or_in(i.cache_read_cost) + cache_write as f64 * or_in(i.cache_write_cost) + output as f64 * i.output_cost
}

fn n(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn ints(v: &Value) -> Option<Vec<i64>> {
    v.as_array().map(|a| a.iter().filter_map(Value::as_i64).collect())
}

/// Chat-completions response -> LlmResponse (LiteLLM's ModelResponse handling in `call`).
pub fn parse_chat(v: &Value, info: Option<&ModelInfo>, model: &str, rollout: bool) -> Result<LlmResponse, LlmError> {
    let choice = v["choices"].get(0).ok_or_else(|| api_err("APIError", format!("response has no choices: {}", tail(&v.to_string(), 2000))))?;
    let msg = &choice["message"];
    let content = content_text(&msg["content"]);
    let reasoning = msg["reasoning_content"].as_str().or(msg["reasoning"].as_str()).map(String::from);
    if choice["finish_reason"] == "length" {
        return Err(LlmError::OutputLength {
            msg: format!("Model {model} hit max_tokens limit. Response was truncated. Consider increasing max_tokens if possible."),
            truncated: content,
        });
    }
    let u = &v["usage"];
    let prompt = n(&u["prompt_tokens"]);
    let completion = n(&u["completion_tokens"]);
    let cached = u["prompt_tokens_details"]["cached_tokens"].as_i64().or(u["prompt_cache_hit_tokens"].as_i64()).unwrap_or(0);
    let cost_usd = u["cost"].as_f64().filter(|c| *c > 0.0).unwrap_or_else(|| cost(info, prompt - cached, cached, 0, completion));
    let mut r = LlmResponse {
        content,
        reasoning_content: reasoning,
        model_name: v["model"].as_str().map(String::from),
        usage: Some(Usage { prompt_tokens: prompt, completion_tokens: completion, cache_tokens: cached, cost_usd }),
        ..Default::default()
    };
    if rollout {
        r.prompt_token_ids = ints(&v["prompt_token_ids"]).or_else(|| ints(&choice["prompt_token_ids"]));
        r.completion_token_ids = ints(&choice["token_ids"]).filter(|t| !t.is_empty());
        r.logprobs = choice["logprobs"]["content"].as_array().map(|a| a.iter().filter_map(|t| t["logprob"].as_f64()).collect()).filter(|l: &Vec<f64>| !l.is_empty());
    }
    Ok(r)
}

/// Anthropic Messages response -> LlmResponse (usage as LiteLLM reports it).
pub fn parse_anthropic(v: &Value, info: Option<&ModelInfo>, model: &str) -> Result<LlmResponse, LlmError> {
    let blocks = v["content"].as_array().ok_or_else(|| api_err("APIError", format!("unexpected response: {}", tail(&v.to_string(), 2000))))?;
    let text: String = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
    let thinking: String = blocks.iter().filter(|b| b["type"] == "thinking").filter_map(|b| b["thinking"].as_str()).collect();
    if v["stop_reason"] == "max_tokens" {
        return Err(LlmError::OutputLength {
            msg: format!("Model {model} hit max_tokens limit. Response was truncated. Consider increasing max_tokens if possible."),
            truncated: text,
        });
    }
    let u = &v["usage"];
    let (input, read, write, out) = (n(&u["input_tokens"]), n(&u["cache_read_input_tokens"]), n(&u["cache_creation_input_tokens"]), n(&u["output_tokens"]));
    Ok(LlmResponse {
        content: text,
        reasoning_content: (!thinking.is_empty()).then_some(thinking),
        model_name: v["model"].as_str().map(String::from),
        usage: Some(Usage { prompt_tokens: input + read + write, completion_tokens: out, cache_tokens: read, cost_usd: cost(info, input, read, write, out) }),
        ..Default::default()
    })
}

/// Responses API response -> LlmResponse (`_call_responses`).
pub fn parse_responses(v: &Value, info: Option<&ModelInfo>, model: &str) -> Result<LlmResponse, LlmError> {
    let mut content = String::new();
    for item in v["output"].as_array().into_iter().flatten().filter(|i| i["type"] == "message") {
        for part in item["content"].as_array().into_iter().flatten().filter(|p| p["type"] == "output_text") {
            content += part["text"].as_str().unwrap_or("");
        }
    }
    if v["status"] == "incomplete" && v["incomplete_details"]["reason"] == "max_output_tokens" {
        return Err(LlmError::OutputLength { msg: format!("Model {model} hit max_tokens limit. Response was truncated."), truncated: content });
    }
    let usage = v.get("usage").filter(|u| !u.is_null()).map(|u| {
        let (i, o) = (n(&u["input_tokens"]), n(&u["output_tokens"]));
        Usage { prompt_tokens: i, completion_tokens: o, cache_tokens: 0, cost_usd: cost(info, i, 0, 0, o) }
    });
    Ok(LlmResponse { content, model_name: v["model"].as_str().map(String::from), usage, response_id: v["id"].as_str().map(String::from), ..Default::default() })
}

/// upstream's `LiteLLM` backend.
pub struct LiteLlm {
    cfg: LlmConfig,
    ep: Endpoint,
    info: Option<ModelInfo>,
}

impl LiteLlm {
    pub fn new(cfg: LlmConfig, get: &dyn Fn(&str) -> Option<String>) -> Result<LiteLlm> {
        if cfg.model_name.to_lowercase().contains("hosted_vllm") {
            validate_hosted_vllm(&cfg.model_name, cfg.model_info.as_ref())?;
        }
        let mut info = lookup_model_info(&cfg.model_name);
        if let Some(v) = cfg.model_info.as_ref().filter(|v| v.is_object()) {
            info = Some(merge_model_info(info.unwrap_or_default(), v));
        }
        let ep = resolve_endpoint(&cfg.model_name, cfg.api_base.as_deref(), cfg.use_responses_api, get)?;
        Ok(LiteLlm { cfg, ep, info })
    }

    /// LiteLLM `completion_kwargs` for one call (before provider translation).
    pub fn params(&self, messages: Vec<Value>, kw: &Map<String, Value>) -> Map<String, Value> {
        let mut p = self.cfg.llm_kwargs.clone();
        p.insert("messages".into(), Value::Array(messages));
        if let Some(e) = &self.cfg.reasoning_effort {
            p.insert("reasoning_effort".into(), json!(e));
        }
        if let Some(t) = self.cfg.temperature {
            p.insert("temperature".into(), json!(t));
        }
        if self.cfg.collect_rollout_details {
            p.insert("logprobs".into(), json!(true));
            obj(&mut p, "extra_body").insert("return_token_ids".into(), json!(true));
        }
        for (k, v) in kw {
            match (k.as_str(), v) {
                ("extra_body", Value::Object(o)) => obj(&mut p, "extra_body").extend(o.clone()),
                _ => {
                    p.insert(k.clone(), v.clone());
                }
            }
        }
        if let Some(b) = self.cfg.max_thinking_tokens.filter(|_| is_claude(&self.cfg.model_name)) {
            p.insert("thinking".into(), json!({"type": "enabled", "budget_tokens": b.max(1024)}));
        }
        if let Some(s) = &self.cfg.session_id {
            obj(&mut p, "extra_body").insert("session_id".into(), json!(s));
            obj(&mut p, "extra_headers").insert("X-Session-ID".into(), json!(s));
        }
        p
    }

    /// Translate params into the provider request (chat-completions or Anthropic Messages).
    pub fn request(&self, mut p: Map<String, Value>) -> Request {
        let (headers, timeout, extra) = split_transport(&mut p, &self.ep);
        let mut body = Map::new();
        body.insert("model".into(), json!(self.ep.model));
        if self.ep.api == Api::Anthropic {
            let msgs = p.remove("messages").unwrap_or(json!([]));
            let (system, messages) = to_anthropic_messages(msgs.as_array().map(Vec::as_slice).unwrap_or(&[]));
            body.insert("messages".into(), Value::Array(messages));
            if !system.is_empty() {
                body.insert("system".into(), Value::Array(system));
            }
            let max = p.remove("max_tokens").or_else(|| p.remove("max_completion_tokens"));
            body.insert("max_tokens".into(), max.unwrap_or_else(|| json!(self.info.and_then(|i| i.max_output_tokens).unwrap_or(4096))));
            let effort = p.remove("reasoning_effort");
            let thinking = p.remove("thinking").or_else(|| anthropic_thinking(effort.as_ref()?.as_str()?, self.info.is_some_and(|i| i.adaptive_thinking)));
            if let Some(t) = thinking {
                body.insert("thinking".into(), t);
            }
            if let Some(s) = p.remove("stop") {
                body.insert("stop_sequences".into(), if s.is_string() { json!([s]) } else { s });
            }
            // drop_params: only what the Messages API accepts.
            for k in ["temperature", "top_p", "top_k", "metadata", "tools", "tool_choice", "stream"] {
                if let Some(v) = p.remove(k) {
                    body.insert(k.into(), v);
                }
            }
        } else {
            p.remove("thinking");
            if p.get("temperature").is_some_and(|t| openai_rejects_temperature(&self.ep, t)) {
                p.remove("temperature");
            }
            body.extend(p);
        }
        body.extend(extra);
        Request { headers, body: Value::Object(body), timeout }
    }

    /// Responses API request (`_call_responses`).
    pub fn responses_request(&self, prompt: &str, history: &[Value], kw: &Map<String, Value>, previous_response_id: Option<&str>) -> Request {
        let mut p = self.cfg.llm_kwargs.clone();
        if let Some(e) = &self.cfg.reasoning_effort {
            p.insert("reasoning".into(), json!({"effort": e}));
        }
        if let Some(m) = self.output_limit() {
            p.insert("max_output_tokens".into(), json!(m));
        }
        if let Some(t) = self.cfg.temperature {
            p.insert("temperature".into(), json!(t));
        }
        p.extend(kw.clone());
        match previous_response_id {
            Some(id) => {
                p.insert("previous_response_id".into(), json!(id));
                p.insert("input".into(), json!(prompt));
            }
            None => {
                let mut input: Vec<Value> = history.iter().map(|m| json!({"role": m.get("role").cloned().unwrap_or(json!("user")), "content": m.get("content").cloned().unwrap_or(json!(""))})).collect();
                input.push(json!({"role": "user", "content": prompt}));
                p.insert("input".into(), Value::Array(input));
            }
        }
        let (headers, timeout, extra) = split_transport(&mut p, &self.ep);
        let mut body = Map::new();
        body.insert("model".into(), json!(self.ep.model));
        body.extend(p);
        body.extend(extra);
        Request { headers, body: Value::Object(body), timeout }
    }

    async fn call_once(&self, prompt: &str, history: &[Value], kw: &Map<String, Value>, prev: Option<&str>) -> Result<LlmResponse, LlmError> {
        let model = &self.cfg.model_name;
        if self.ep.api == Api::Responses {
            let v = send(&self.ep.url, &self.responses_request(prompt, history, kw, prev)).await?;
            return parse_responses(&v, self.info.as_ref(), model);
        }
        let mut messages = history.to_vec();
        messages.push(json!({"role": "user", "content": prompt}));
        let mut p = self.params(add_anthropic_caching(messages, model), kw);
        let v = match send(&self.ep.url, &self.request(p.clone())).await {
            // Providers that reject return_token_ids / session_id in extra_body: retry without them.
            Err(LlmError::Api { kind: "BadRequestError", msg }) if msg.contains("Unrecognized request argument") && p.contains_key("extra_body") => {
                let rejected: Vec<&str> = ["return_token_ids", "session_id"].into_iter().filter(|k| msg.contains(k)).collect();
                if rejected.is_empty() {
                    return Err(LlmError::Api { kind: "BadRequestError", msg });
                }
                let eb = obj(&mut p, "extra_body");
                for k in rejected {
                    eb.remove(k);
                }
                if eb.is_empty() {
                    p.remove("extra_body");
                }
                send(&self.ep.url, &self.request(p)).await?
            }
            r => r?,
        };
        match self.ep.api {
            Api::Anthropic => parse_anthropic(&v, self.info.as_ref(), model),
            _ => parse_chat(&v, self.info.as_ref(), model, self.cfg.collect_rollout_details),
        }
    }
}

#[async_trait::async_trait]
impl Llm for LiteLlm {
    /// upstream: `@retry(stop_after_attempt(3), wait_exponential(min=4, max=15))`, except context/output/auth errors.
    async fn call(&self, prompt: &str, history: &[Value], kw: &Map<String, Value>, previous_response_id: Option<&str>) -> Result<LlmResponse, LlmError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.call_once(prompt, history, kw, previous_response_id).await {
                Err(e) if e.retryable() && attempt < 3 => tokio::time::sleep(Duration::from_secs(4)).await,
                r => return r,
            }
        }
    }
    fn context_limit(&self) -> i64 {
        self.info.and_then(|i| i.max_input_tokens).unwrap_or(1_000_000)
    }
    fn output_limit(&self) -> Option<i64> {
        self.info.and_then(|i| i.max_output_tokens)
    }
}

/// curl config string quoting.
fn cq(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// POST JSON; non-2xx statuses become classified errors.
pub async fn send(url: &str, req: &Request) -> Result<Value, LlmError> {
    let path = std::env::temp_dir().join(format!("hbench-llm-{}.json", uuid::Uuid::new_v4().simple()));
    std::fs::write(&path, serde_json::to_vec(&req.body).unwrap_or_default()).map_err(|e| api_err("APIConnectionError", format!("writing request body: {e}")))?;
    let mut cfg = format!(
        "url = {}\nrequest = \"POST\"\nsilent\nshow-error\nmax-time = {}\nwrite-out = \"\\n%{{http_code}}\"\ndata-binary = {}\nheader = \"content-type: application/json\"\n",
        cq(url),
        req.timeout.as_secs_f64(),
        cq(&format!("@{}", path.display()))
    );
    for (k, v) in &req.headers {
        cfg += &format!("header = {}\n", cq(&format!("{k}: {v}")));
    }
    let out = async {
        let mut child = tokio::process::Command::new("curl")
            .args(["-K", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut si = child.stdin.take().unwrap();
        si.write_all(cfg.as_bytes()).await?;
        drop(si);
        child.wait_with_output().await
    }
    .await;
    let _ = std::fs::remove_file(&path);
    let out = out.map_err(|e| api_err("APIConnectionError", format!("running curl: {e}")))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let kind = if out.status.code() == Some(28) { "Timeout" } else { "APIConnectionError" };
        return Err(api_err(kind, format!("{url}: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let (body, status) = stdout.rsplit_once('\n').unwrap_or(("", &stdout));
    let status: u16 = status.trim().parse().unwrap_or(0);
    if !(200..300).contains(&status) {
        return Err(classify_http(status, body));
    }
    serde_json::from_str(body).map_err(|e| api_err("APIError", format!("invalid JSON from {url}: {e}: {}", tail(body, 1000))))
}

/// Port of upstream's `Chat`: message history plus cumulative usage.
#[derive(Debug, Default)]
pub struct Chat {
    pub messages: Vec<Value>,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_tokens: i64,
    pub total_cost: f64,
    interleaved_thinking: bool,
    last_response_id: Option<String>,
    prompt_token_ids: Vec<Vec<i64>>,
    completion_token_ids: Vec<Vec<i64>>,
    logprobs: Vec<Vec<f64>>,
}

impl Chat {
    pub fn new(interleaved_thinking: bool) -> Chat {
        Chat { interleaved_thinking, ..Default::default() }
    }

    pub async fn chat(&mut self, llm: &dyn Llm, prompt: &str, kw: &Map<String, Value>) -> Result<LlmResponse, LlmError> {
        let r = llm.call(prompt, &self.messages, kw, self.last_response_id.as_deref()).await?;
        if r.response_id.is_some() {
            self.last_response_id = r.response_id.clone();
        }
        if let Some(u) = &r.usage {
            self.total_input_tokens += u.prompt_tokens;
            self.total_output_tokens += u.completion_tokens;
            self.total_cache_tokens += u.cache_tokens;
            self.total_cost += u.cost_usd;
        }
        if let Some(p) = r.prompt_token_ids.clone().filter(|p| !p.is_empty()) {
            self.prompt_token_ids.push(p);
        }
        if let Some(c) = r.completion_token_ids.clone().filter(|c| !c.is_empty()) {
            self.completion_token_ids.push(c);
            if let Some(l) = r.logprobs.clone().filter(|l| !l.is_empty()) {
                self.logprobs.push(l);
            }
        }
        let mut assistant = json!({"role": "assistant", "content": r.content});
        if let Some(rc) = r.reasoning_content.as_ref().filter(|rc| self.interleaved_thinking && !rc.is_empty()) {
            assistant["reasoning_content"] = json!(rc);
        }
        self.messages.push(json!({"role": "user", "content": prompt}));
        self.messages.push(assistant);
        Ok(r)
    }

    /// Call after editing `messages` directly so the Responses API resends full history.
    pub fn reset_response_chain(&mut self) {
        self.last_response_id = None;
    }

    /// upstream's `Chat.rollout_details`: one entry for the linear history, or none.
    pub fn rollout_details(&self) -> Vec<Value> {
        if self.prompt_token_ids.is_empty() && self.completion_token_ids.is_empty() && self.logprobs.is_empty() {
            return vec![];
        }
        let mut d = Map::new();
        if !self.prompt_token_ids.is_empty() {
            d.insert("prompt_token_ids".into(), json!(self.prompt_token_ids));
        }
        if !self.completion_token_ids.is_empty() {
            d.insert("completion_token_ids".into(), json!(self.completion_token_ids));
        }
        if !self.logprobs.is_empty() {
            d.insert("logprobs".into(), json!(self.logprobs));
        }
        vec![Value::Object(d)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    fn llm(cfg: LlmConfig, vars: &'static [(&'static str, &'static str)]) -> LiteLlm {
        LiteLlm::new(cfg, &env(vars)).unwrap()
    }

    #[test]
    fn endpoints() {
        let e = resolve_endpoint("anthropic/claude-sonnet-4-5", None, false, &env(&[("ANTHROPIC_API_KEY", "sk")])).unwrap();
        assert_eq!((e.api, e.url.as_str(), e.model.as_str()), (Api::Anthropic, "https://api.anthropic.com/v1/messages", "claude-sonnet-4-5"));
        assert!(e.headers.contains(&("x-api-key".into(), "sk".into())));
        let e = resolve_endpoint("anthropic/claude-x", None, false, &env(&[("CLAUDE_CODE_OAUTH_TOKEN", "oat"), ("ANTHROPIC_BASE_URL", "http://proxy:8080")])).unwrap();
        assert_eq!(e.url, "http://proxy:8080/v1/messages");
        assert!(e.headers.contains(&("authorization".into(), "Bearer oat".into())) && e.headers.contains(&("anthropic-beta".into(), "oauth-2025-04-20".into())));
        let e = resolve_endpoint("openrouter/anthropic/claude-sonnet-4-5", None, false, &env(&[("OPENROUTER_API_KEY", "or")])).unwrap();
        assert_eq!((e.api, e.url.as_str(), e.model.as_str()), (Api::Chat, "https://openrouter.ai/api/v1/chat/completions", "anthropic/claude-sonnet-4-5"));
        let e = resolve_endpoint("gemini/gemini-2.5-pro", None, false, &env(&[("GEMINI_API_KEY", "g")])).unwrap();
        assert_eq!(e.url, "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions");
        let e = resolve_endpoint("ollama/llama3", None, false, &env(&[])).unwrap();
        assert_eq!(e.url, "http://localhost:11434/v1/chat/completions");
        assert!(e.headers.is_empty());
        let e = resolve_endpoint("gpt-4o", None, true, &env(&[("OPENAI_API_KEY", "k")])).unwrap();
        assert_eq!((e.api, e.url.as_str()), (Api::Responses, "https://api.openai.com/v1/responses"));
        let e = resolve_endpoint("hosted_vllm/m", Some("http://vllm:8000/v1/"), false, &env(&[])).unwrap();
        assert_eq!((e.url.as_str(), e.model.as_str()), ("http://vllm:8000/v1/chat/completions", "m"));
        assert!(resolve_endpoint("bedrock/x", None, false, &env(&[])).is_err());
        assert!(resolve_endpoint("myprov/x", None, false, &env(&[])).is_err());
        let e = resolve_endpoint("myprov/x", None, false, &env(&[("MYPROV_BASE_URL", "http://m"), ("MYPROV_API_KEY", "mk")])).unwrap();
        assert_eq!(e.url, "http://m/chat/completions");
        assert!(e.headers.contains(&("authorization".into(), "Bearer mk".into())));
    }

    #[test]
    fn anthropic_request_body() {
        let l = llm(
            LlmConfig { model_name: "anthropic/claude-opus-4-1".into(), temperature: Some(0.2), reasoning_effort: Some("high".into()), session_id: Some("s1".into()), ..Default::default() },
            &[("ANTHROPIC_API_KEY", "sk")],
        );
        let history = vec![json!({"role": "user", "content": "a"}), json!({"role": "assistant", "content": "b", "reasoning_content": "r"})];
        let mut msgs = history.clone();
        msgs.push(json!({"role": "user", "content": "c"}));
        let kw: Map<String, Value> = serde_json::from_value(json!({"extra_headers": {"X-A": "1"}, "max_tokens": 100})).unwrap();
        let req = l.request(l.params(add_anthropic_caching(msgs, "anthropic/claude-opus-4-1"), &kw));
        let b = &req.body;
        assert_eq!(b["model"], "claude-opus-4-1");
        assert_eq!(b["max_tokens"], 100);
        assert_eq!(b["temperature"], 0.2);
        assert_eq!(b["thinking"], json!({"type": "enabled", "budget_tokens": 4096}));
        assert_eq!(b["session_id"], "s1"); // extra_body merges into the body
        assert_eq!(b["messages"][2]["content"][0], json!({"type": "text", "text": "c", "cache_control": {"type": "ephemeral"}}));
        assert!(b["messages"][1].get("reasoning_content").is_none() && b.get("reasoning_effort").is_none());
        assert!(req.headers.contains(&("X-A".into(), "1".into())) && req.headers.contains(&("X-Session-ID".into(), "s1".into())));
        // Default max_tokens = model max output; adaptive models get adaptive thinking.
        let l = llm(LlmConfig { model_name: "anthropic/claude-opus-4-7".into(), reasoning_effort: Some("low".into()), ..Default::default() }, &[]);
        let b = l.request(l.params(vec![json!({"role": "system", "content": "sys"}), json!({"role": "user", "content": "x"})], &Map::new())).body;
        assert_eq!((b["max_tokens"].clone(), b["thinking"].clone()), (json!(128000), json!({"type": "adaptive"})));
        assert_eq!(b["system"], json!([{"type": "text", "text": "sys"}]));
        assert!(b.get("temperature").is_none());
        let l = llm(LlmConfig { model_name: "anthropic/claude-x".into(), max_thinking_tokens: Some(10), ..Default::default() }, &[]);
        assert_eq!(l.request(l.params(vec![], &Map::new())).body["thinking"]["budget_tokens"], 1024);
    }

    #[test]
    fn openai_request_body() {
        let l = llm(
            LlmConfig {
                model_name: "openai/gpt-4o".into(),
                temperature: Some(0.7),
                collect_rollout_details: true,
                llm_kwargs: serde_json::from_value(json!({"top_p": 0.9, "timeout": 30})).unwrap(),
                ..Default::default()
            },
            &[("OPENAI_API_KEY", "k")],
        );
        let kw: Map<String, Value> = serde_json::from_value(json!({"extra_body": {"foo": 1}, "temperature": 0.1})).unwrap();
        let req = l.request(l.params(vec![json!({"role": "user", "content": "hi"})], &kw));
        let b = &req.body;
        assert_eq!(b["model"], "gpt-4o");
        assert_eq!((b["temperature"].clone(), b["top_p"].clone(), b["logprobs"].clone()), (json!(0.1), json!(0.9), json!(true)));
        assert_eq!((b["return_token_ids"].clone(), b["foo"].clone()), (json!(true), json!(1)));
        assert_eq!(b["messages"][0]["content"], "hi"); // no caching for non-Claude models
        assert!(b.get("extra_body").is_none() && b.get("timeout").is_none());
        assert_eq!(req.timeout, Duration::from_secs(30));
        assert!(req.headers.contains(&("authorization".into(), "Bearer k".into())));
        let l = llm(LlmConfig { model_name: "openai/gpt-5".into(), temperature: Some(0.3), reasoning_effort: Some("high".into()), ..Default::default() }, &[]);
        let b = l.request(l.params(vec![], &Map::new())).body;
        assert!(b.get("temperature").is_none());
        assert_eq!(b["reasoning_effort"], "high");
        // Responses API, chained and unchained.
        let l = llm(LlmConfig { model_name: "openai/gpt-5".into(), use_responses_api: true, reasoning_effort: Some("low".into()), ..Default::default() }, &[]);
        let b = l.responses_request("p", &[json!({"role": "user", "content": "a"})], &Map::new(), None).body;
        assert_eq!((b["input"][1].clone(), b["reasoning"].clone(), b["max_output_tokens"].clone()), (json!({"role": "user", "content": "p"}), json!({"effort": "low"}), json!(128000)));
        let b = l.responses_request("p", &[], &Map::new(), Some("resp_1")).body;
        assert_eq!((b["input"].clone(), b["previous_response_id"].clone()), (json!("p"), json!("resp_1")));
    }

    #[test]
    fn caching_caps_at_four_blocks() {
        let mm = json!({"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image_url", "image_url": {"url": "data:image/webp;base64,AAAA"}}]});
        let out = add_anthropic_caching(vec![mm.clone(), mm.clone(), mm.clone()], "anthropic/claude-opus-4-1");
        let count: usize = out.iter().map(|m| m["content"].as_array().unwrap().iter().filter(|p| p.get("cache_control").is_some()).count()).sum();
        assert_eq!(count, 4);
        assert!(out[0]["content"][0].get("cache_control").is_none() && out[2]["content"][1].get("cache_control").is_some());
        let plain = vec![json!({"role": "user", "content": "hello"})];
        assert_eq!(add_anthropic_caching(plain.clone(), "openai/gpt-4o"), plain);
        let (_, a) = to_anthropic_messages(&out);
        assert_eq!(a[2]["content"][1]["source"], json!({"type": "base64", "media_type": "image/webp", "data": "AAAA"}));
    }

    #[test]
    fn usage_and_cost() {
        let info = lookup_model_info("anthropic/claude-sonnet-4-5");
        let v = json!({"model": "claude-sonnet-4-5-20250929", "stop_reason": "end_turn", "content": [{"type": "thinking", "thinking": "hmm"}, {"type": "text", "text": "hi"}],
            "usage": {"input_tokens": 100, "cache_creation_input_tokens": 1000, "cache_read_input_tokens": 2000, "output_tokens": 50}});
        let r = parse_anthropic(&v, info.as_ref(), "m").unwrap();
        assert_eq!((r.content.as_str(), r.reasoning_content.as_deref(), r.model_name.as_deref()), ("hi", Some("hmm"), Some("claude-sonnet-4-5-20250929")));
        let u = r.usage.unwrap();
        assert_eq!((u.prompt_tokens, u.cache_tokens, u.completion_tokens), (3100, 2000, 50));
        assert!((u.cost_usd - (100.0 * 3e-6 + 1000.0 * 3.75e-6 + 2000.0 * 3e-7 + 50.0 * 1.5e-5)).abs() < 1e-12);
        let trunc = json!({"stop_reason": "max_tokens", "content": [{"type": "text", "text": "partial"}], "usage": {}});
        assert!(matches!(parse_anthropic(&trunc, None, "m"), Err(LlmError::OutputLength { truncated, .. }) if truncated == "partial"));

        let v = json!({"model": "gpt-4o-2024-08-06", "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": "ok"},
            "logprobs": {"content": [{"token": "ok", "logprob": -0.5}]}, "token_ids": [7, 8]}], "prompt_token_ids": [1, 2, 3],
            "usage": {"prompt_tokens": 1000, "completion_tokens": 10, "prompt_tokens_details": {"cached_tokens": 400}}});
        let r = parse_chat(&v, lookup_model_info("openai/gpt-4o").as_ref(), "m", true).unwrap();
        let u = r.usage.clone().unwrap();
        assert_eq!((u.prompt_tokens, u.cache_tokens, u.completion_tokens), (1000, 400, 10));
        assert!((u.cost_usd - (600.0 * 2.5e-6 + 400.0 * 1.25e-6 + 10.0 * 1e-5)).abs() < 1e-12);
        assert_eq!((r.prompt_token_ids, r.completion_token_ids, r.logprobs), (Some(vec![1, 2, 3]), Some(vec![7, 8]), Some(vec![-0.5])));
        // OpenRouter reports cost itself; DeepSeek reports cache hits its own way; unknown models are unpriced.
        let v = json!({"choices": [{"finish_reason": "stop", "message": {"content": "x", "reasoning": "why"}}], "usage": {"prompt_tokens": 5, "completion_tokens": 1, "cost": 0.123, "prompt_cache_hit_tokens": 2}});
        let r = parse_chat(&v, None, "m", false).unwrap();
        assert_eq!((r.usage.clone().unwrap().cost_usd, r.usage.unwrap().cache_tokens, r.reasoning_content.as_deref()), (0.123, 2, Some("why")));
        let v = json!({"choices": [{"finish_reason": "length", "message": {"content": "cut"}}]});
        assert!(matches!(parse_chat(&v, None, "m", false), Err(LlmError::OutputLength { .. })));
        let v = json!({"id": "resp_9", "status": "completed", "output": [{"type": "reasoning"}, {"type": "message", "content": [{"type": "output_text", "text": "a"}, {"type": "output_text", "text": "b"}]}], "usage": {"input_tokens": 3, "output_tokens": 4}});
        let r = parse_responses(&v, None, "m").unwrap();
        assert_eq!((r.content.as_str(), r.response_id.as_deref(), r.usage.unwrap().prompt_tokens), ("ab", Some("resp_9"), 3));
    }

    #[test]
    fn error_classification() {
        assert!(matches!(classify_http(400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#), LlmError::ContextLength(_)));
        assert!(matches!(classify_http(400, r#"{"error":{"code":"context_length_exceeded"}}"#), LlmError::ContextLength(_)));
        assert!(matches!(classify_http(400, "This model's maximum context length is 8192 tokens"), LlmError::ContextLength(_)));
        assert_eq!(classify_http(429, "slow down").kind(), "RateLimitError");
        assert_eq!(classify_http(529, "overloaded").kind(), "InternalServerError");
        let auth = classify_http(401, "invalid x-api-key");
        assert_eq!(auth.kind(), "AuthenticationError");
        assert!(!auth.retryable() && classify_http(500, "").retryable() && !LlmError::ContextLength(String::new()).retryable());
        assert_eq!(crate::env::error_kind(&classify_http(429, "").into_anyhow()), "RateLimitError");
    }

    #[test]
    fn model_info_and_vllm() {
        assert_eq!(lookup_model_info("openrouter/anthropic/claude-haiku-4-5").unwrap().max_input_tokens, Some(200_000));
        assert!(lookup_model_info("openai/unknown").is_none());
        let l = llm(LlmConfig { model_name: "openai/unknown".into(), ..Default::default() }, &[]);
        assert_eq!((l.context_limit(), l.output_limit()), (1_000_000, None));
        let info = json!({"max_input_tokens": 32768, "max_output_tokens": "4096", "input_cost_per_token": 1e-7, "output_cost_per_token": 2e-7});
        let l = llm(LlmConfig { model_name: "hosted_vllm/llama".into(), api_base: Some("http://h/v1".into()), model_info: Some(info), ..Default::default() }, &[]);
        assert_eq!((l.context_limit(), l.output_limit()), (32768, Some(4096)));
        assert!(LiteLlm::new(LlmConfig { model_name: "hosted_vllm/llama".into(), api_base: Some("http://h".into()), ..Default::default() }, &env(&[])).is_err());
        assert!(LiteLlm::new(LlmConfig { model_name: "hosted_vllm/a/b".into(), ..Default::default() }, &env(&[])).is_err());
    }

    struct Fixed(&'static str);
    #[async_trait::async_trait]
    impl Llm for Fixed {
        async fn call(&self, _: &str, _: &[Value], _: &Map<String, Value>, _: Option<&str>) -> Result<LlmResponse, LlmError> {
            Ok(LlmResponse {
                content: self.0.into(),
                reasoning_content: Some("think".into()),
                usage: Some(Usage { prompt_tokens: 10, completion_tokens: 2, cache_tokens: 1, cost_usd: 0.5 }),
                completion_token_ids: Some(vec![1]),
                logprobs: Some(vec![-0.1]),
                ..Default::default()
            })
        }
        fn context_limit(&self) -> i64 {
            100
        }
        fn output_limit(&self) -> Option<i64> {
            None
        }
    }

    #[tokio::test]
    async fn chat_accumulates() {
        let mut c = Chat::new(true);
        c.chat(&Fixed("a"), "p1", &Map::new()).await.unwrap();
        c.chat(&Fixed("b"), "p2", &Map::new()).await.unwrap();
        assert_eq!((c.total_input_tokens, c.total_output_tokens, c.total_cache_tokens, c.total_cost), (20, 4, 2, 1.0));
        assert_eq!(c.messages.len(), 4);
        assert_eq!(c.messages[1], json!({"role": "assistant", "content": "a", "reasoning_content": "think"}));
        assert_eq!(c.rollout_details(), vec![json!({"completion_token_ids": [[1], [1]], "logprobs": [[-0.1], [-0.1]]})]);
        assert!(approx_tokens(&c.messages) > 0);
    }
}
