//! Declarative model credentials and endpoint resolution.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub struct ProviderAccess {
    pub api_key_envs: &'static [&'static str],
    pub base_url: Option<&'static str>,
    pub base_url_envs: &'static [&'static str],
    pub extra_envs: &'static [&'static str],
}

const fn p(
    api_key_envs: &'static [&'static str],
    base_url: Option<&'static str>,
    base_url_envs: &'static [&'static str],
    extra_envs: &'static [&'static str],
) -> ProviderAccess {
    ProviderAccess { api_key_envs, base_url, base_url_envs, extra_envs }
}

const AWS: &[&str] = &["AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_REGION"];

pub fn provider(name: &str) -> Option<ProviderAccess> {
    Some(match name {
        "ai21" => p(&["AI21_API_KEY"], None, &[], &[]),
        "aleph_alpha" => p(&["ALEPH_ALPHA_API_KEY"], None, &[], &[]),
        "amazon-bedrock" | "sagemaker" => p(&["AWS_ACCESS_KEY_ID"], None, &[], AWS),
        "anthropic" => p(&["ANTHROPIC_API_KEY"], Some("https://api.anthropic.com/v1"), &["ANTHROPIC_BASE_URL"], &[]),
        "azure" => p(&["AZURE_API_KEY"], None, &["AZURE_BASE_URL", "AZURE_API_BASE"], &["AZURE_API_VERSION", "AZURE_RESOURCE_NAME"]),
        "anyscale" => p(&["ANYSCALE_API_KEY"], None, &[], &[]),
        "baseten" => p(&["BASETEN_API_KEY"], None, &[], &[]),
        "cerebras" => p(&["CEREBRAS_API_KEY"], None, &[], &[]),
        "cloudflare" => p(&["CLOUDFLARE_API_KEY"], None, &[], &[]),
        "codestral" => p(&["CODESTRAL_API_KEY"], None, &[], &[]),
        "cohere" => p(&["COHERE_API_KEY"], None, &[], &[]),
        "dashscope" => p(&["DASHSCOPE_API_KEY"], None, &[], &[]),
        "databricks" => p(&["DATABRICKS_TOKEN"], None, &["DATABRICKS_HOST"], &[]),
        "datarobot" => p(&["DATAROBOT_API_TOKEN"], None, &[], &[]),
        "deepseek" => p(&["DEEPSEEK_API_KEY"], Some("https://api.deepseek.com"), &["DEEPSEEK_BASE_URL"], &[]),
        "deepinfra" => p(&["DEEPINFRA_API_KEY"], None, &[], &[]),
        "doubao" => p(&["DOUBAO_API_KEY"], None, &["DOUBAO_BASE_URL"], &[]),
        "featherless_ai" => p(&["FEATHERLESS_AI_API_KEY"], None, &[], &[]),
        "fireworks_ai" => p(&["FIREWORKS_AI_API_KEY"], None, &[], &[]),
        "github-copilot" => p(&["GITHUB_TOKEN"], None, &[], &[]),
        "google" => p(
            &["GOOGLE_API_KEY", "GEMINI_API_KEY", "GOOGLE_GENERATIVE_AI_API_KEY"],
            Some("https://generativelanguage.googleapis.com"),
            &["GOOGLE_BASE_URL"],
            &["GOOGLE_APPLICATION_CREDENTIALS", "GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_LOCATION", "GOOGLE_GENAI_USE_VERTEXAI"],
        ),
        "groq" => p(&["GROQ_API_KEY"], Some("https://api.groq.com/openai/v1"), &["GROQ_BASE_URL"], &[]),
        "huggingface" => p(&["HF_TOKEN", "HUGGINGFACE_API_KEY"], None, &[], &[]),
        "infinity" => p(&["INFINITY_API_KEY"], None, &[], &[]),
        "kimi" => p(&["KIMI_API_KEY", "MOONSHOT_API_KEY"], Some("https://api.kimi.com/coding/v1"), &["KIMI_BASE_URL"], &[]),
        "llama" => p(&["LLAMA_API_KEY"], None, &[], &[]),
        "mistral" => p(&["MISTRAL_API_KEY"], Some("https://api.mistral.ai/v1"), &["MISTRAL_BASE_URL"], &[]),
        "minimax" => p(&["MINIMAX_API_KEY"], Some("https://api.minimax.io/anthropic"), &["MINIMAX_BASE_URL"], &[]),
        "moonshot" => p(&["MOONSHOT_API_KEY"], Some("https://api.moonshot.cn/v1"), &[], &[]),
        "nebius" => p(&["NEBIUS_API_KEY"], None, &[], &[]),
        "nlp_cloud" => p(&["NLP_CLOUD_API_KEY"], None, &[], &[]),
        "nvidia" => p(&["NVIDIA_API_KEY"], Some("https://integrate.api.nvidia.com/v1"), &["NVIDIA_BASE_URL"], &[]),
        "nvidia_nim" => p(&["NVIDIA_NIM_API_KEY"], None, &[], &[]),
        "novita" => p(&["NOVITA_API_KEY"], None, &[], &[]),
        "ollama" => p(&["OLLAMA_API_KEY"], None, &["OLLAMA_BASE_URL", "OLLAMA_API_BASE"], &[]),
        "openai" | "openai-responses" => {
            p(&["OPENAI_API_KEY"], Some("https://api.openai.com/v1"), &["OPENAI_BASE_URL", "OPENAI_API_BASE"], &[])
        }
        "opencode" => p(&["OPENCODE_API_KEY"], None, &[], &[]),
        "openrouter" => p(&["OPENROUTER_API_KEY"], Some("https://openrouter.ai/api/v1"), &["OPENROUTER_BASE_URL"], &[]),
        "ovhcloud" => p(&["OVHCLOUD_API_KEY"], Some("https://oai.endpoints.kepler.ai.cloud.ovh.net/v1"), &["OVHCLOUD_BASE_URL"], &[]),
        "palm" => p(&["PALM_API_KEY"], None, &[], &[]),
        "perplexity" => p(&["PERPLEXITYAI_API_KEY"], None, &[], &[]),
        "replicate" => p(&["REPLICATE_API_KEY"], None, &[], &[]),
        "tetrate" => p(&["TETRATE_API_KEY"], None, &["TETRATE_HOST"], &[]),
        "together" => p(&["TOGETHER_API_KEY", "TOGETHERAI_API_KEY"], Some("https://api.together.xyz/v1"), &[], &[]),
        "vercel_ai_gateway" => p(&["VERCEL_AI_GATEWAY_API_KEY"], None, &[], &[]),
        "vertex_ai" => p(&["VERTEXAI_PROJECT"], None, &[], &[]),
        "volcengine" => p(&["VOLCENGINE_API_KEY"], None, &[], &[]),
        "voyage" => p(&["VOYAGE_API_KEY"], None, &[], &[]),
        "writer" => p(&["WRITER_API_KEY"], None, &["WRITER_BASE_URL"], &[]),
        "xai" => p(&["XAI_API_KEY"], Some("https://api.x.ai/v1"), &["XAI_BASE_URL"], &[]),
        "zai" => p(&["ZAI_API_KEY"], None, &[], &[]),
        _ => return None,
    })
}

pub fn alias(provider: &str) -> &str {
    match provider {
        "ai21_chat" => "ai21",
        "bedrock" => "amazon-bedrock",
        "gemini" => "google",
        "llamaapi" => "llama",
        "litellm_proxy" => "openai",
        "openai_responses" => "openai-responses",
        "together_ai" => "together",
        p => p,
    }
}

/// Best-effort provider inference for bare model names.
fn infer_provider(model: &str) -> Option<&'static str> {
    let m = model.to_lowercase();
    let pre = |ps: &[&str]| ps.iter().any(|p| m.starts_with(p));
    Some(if pre(&["gpt-", "o1", "o3", "o4", "chatgpt", "codex", "text-embedding", "davinci"]) {
        "openai"
    } else if pre(&["claude"]) {
        "anthropic"
    } else if pre(&["gemini"]) {
        "google"
    } else if pre(&["grok"]) {
        "xai"
    } else if pre(&["deepseek"]) {
        "deepseek"
    } else if pre(&["mistral", "codestral", "devstral", "magistral"]) {
        "mistral"
    } else if pre(&["command"]) {
        "cohere"
    } else {
        return None;
    })
}

#[derive(Default, Clone, Copy)]
pub struct Spec {
    pub default_provider: Option<&'static str>,
    pub api_key_envs: &'static [&'static str],
    pub base_url_envs: &'static [&'static str],
    pub passthrough: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Resolved {
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub configured_base_url: Option<String>,
    pub env: BTreeMap<String, String>,
}

/// Looks up the first of `names` that is set, returning `(name, value)`.
pub type EnvLookup<'a> = dyn Fn(&[&str]) -> Option<(String, String)> + 'a;

/// `get(names)` returns the first (name, value) found in precedence order.
pub fn resolve(model: Option<&str>, spec: Spec, get: &EnvLookup) -> Resolved {
    let provider: Option<String> = spec
        .default_provider
        .map(String::from)
        .or_else(|| model.and_then(|m| m.split_once('/').map(|(p, _)| p.to_string())))
        .or_else(|| model.and_then(infer_provider).map(String::from))
        .map(|p| alias(&p).to_string());
    let pspec = provider.as_deref().and_then(self::provider);
    let upper_url = provider.as_ref().map(|p| format!("{}_BASE_URL", p.to_uppercase().replace('-', "_")));
    let provider_url_envs: Vec<&str> = match (&pspec, &upper_url) {
        (Some(s), _) => s.base_url_envs.to_vec(),
        (None, Some(u)) => vec![u.as_str()],
        _ => vec![],
    };
    let mut key_envs: Vec<&str> = spec.api_key_envs.to_vec();
    for k in pspec.as_ref().map(|s| s.api_key_envs).unwrap_or(&[]) {
        if !key_envs.contains(k) {
            key_envs.push(k);
        }
    }
    let key = if key_envs.is_empty() { None } else { get(&key_envs) };
    let mut url_envs: Vec<&str> = spec.base_url_envs.to_vec();
    for u in &provider_url_envs {
        if !url_envs.contains(u) {
            url_envs.push(u);
        }
    }
    let url = if url_envs.is_empty() { None } else { get(&url_envs) };
    let configured = url.as_ref().map(|(_, v)| v.clone()).filter(|v| !v.is_empty());

    let mut env = BTreeMap::new();
    if let Some((name, v)) = &key {
        if spec.api_key_envs.contains(&name.as_str()) {
            env.insert(spec.api_key_envs[0].to_string(), v.clone());
        }
        if spec.passthrough && pspec.as_ref().is_some_and(|s| s.api_key_envs.contains(&name.as_str())) {
            env.insert(name.clone(), v.clone());
        }
    }
    if let (Some(c), Some((name, _))) = (&configured, &url) {
        if spec.base_url_envs.contains(&name.as_str()) {
            env.insert(spec.base_url_envs[0].to_string(), c.clone());
        }
        if spec.passthrough && provider_url_envs.contains(&name.as_str()) {
            env.insert(name.clone(), c.clone());
        }
    }
    if spec.passthrough
        && let Some(s) = &pspec
    {
        for n in s.extra_envs {
            if let Some((_, v)) = get(&[n]).filter(|(_, v)| !v.is_empty()) {
                env.insert(n.to_string(), v);
            }
        }
    }
    let api_key = key.map(|(_, v)| v);
    Resolved {
        base_url: configured.clone().or_else(|| pspec.as_ref().filter(|_| api_key.is_some()).and_then(|s| s.base_url.map(String::from))),
        provider,
        api_key,
        configured_base_url: configured,
        env,
    }
}

impl Resolved {
    /// [`Backend::Custom`] when a base URL was configured, else [`Backend::Provider`] when the provider has a
    /// built-in endpoint; `None` when hbench knows none.
    pub fn backend(&self) -> Option<Backend> {
        if self.configured_base_url.is_some() {
            return Some(Backend::Custom);
        }
        self.provider.as_deref().and_then(provider).and_then(|p| p.base_url).map(|_| Backend::Provider)
    }
}

/// Which service answers a run's model requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// The provider's own API at its built-in endpoint.
    Provider,
    Bedrock,
    Vertex,
    /// Wherever a base URL override points.
    Custom,
}

/// When an env switch is on: `Set` to any non-empty value, a `Flag` of `1`/`true`/`yes`, or a `Bool` that
/// parses as true (`1`/`on`/`t`/`true`/`y`/`yes`).
#[derive(Clone, Copy)]
pub enum On {
    Set,
    Flag,
    Bool,
}

#[derive(Clone, Copy)]
pub struct Switch {
    pub env: &'static str,
    pub on: On,
}

impl Switch {
    pub const fn new(env: &'static str, on: On) -> Switch {
        Switch { env, on }
    }
    pub fn is_on(&self, get: &EnvLookup) -> bool {
        let Some((_, v)) = get(&[self.env]) else { return false };
        match self.on {
            On::Set => !v.is_empty(),
            On::Flag => matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes"),
            On::Bool => super::installed::common::opts::lax_bool(&Value::String(v)) == Some(true),
        }
    }
}

/// What, besides its provider and base URL envs, decides where an agent's model requests go.
#[derive(Clone, Copy)]
pub struct Routing {
    /// Credentials whose endpoint hbench can't know (a ChatGPT auth.json, Gemini OAuth).
    pub opaque_auth: &'static [Switch],
    /// Other credentials for the provider's own endpoint (an OAuth token).
    pub credential_envs: &'static [&'static str],
    /// Switches to another official backend; the first one on wins.
    pub backends: &'static [(Switch, Backend)],
    /// Key of the agent's own config whose map counts as env too (Claude Code settings' `env`).
    pub config_env: Option<&'static str>,
    /// Agent config keys that set a base URL.
    pub config_base_urls: &'static [&'static str],
    /// Agent config keys that pick a provider hbench can't know.
    pub config_opaque: &'static [&'static str],
}

impl Routing {
    pub const NONE: Routing =
        Routing { opaque_auth: &[], credential_envs: &[], backends: &[], config_env: None, config_base_urls: &[], config_opaque: &[] };

    pub fn backend_switch(&self, get: &EnvLookup) -> Option<Backend> {
        self.backends.iter().find(|(s, _)| s.is_on(get)).map(|(_, b)| *b)
    }
    pub fn opaque_auth_on(&self, get: &EnvLookup) -> bool {
        self.opaque_auth.iter().any(|s| s.is_on(get))
    }
}

/// The backend a non-passthrough agent's model requests go to: a backend switch, else [`Backend::Custom`] for
/// a base URL (env or `config`), else unknown for an opaque credential or `config` provider, else the
/// provider's built-in endpoint. `None` when hbench can't tell, as for passthrough agents.
pub fn backend(
    model: Option<&str>,
    spec: Spec,
    routing: &Routing,
    config: Option<&Map<String, Value>>,
    get: &EnvLookup,
) -> Option<Backend> {
    if spec.passthrough {
        return None;
    }
    let config_env = routing.config_env.and_then(|k| config?.get(k)?.as_object());
    let from_config = |names: &[&str]| names.iter().find_map(|n| Some((n.to_string(), config_env?.get(*n)?.as_str()?.to_string())));
    let get = |names: &[&str]| from_config(names).or_else(|| get(names));
    if let Some(b) = routing.backend_switch(&get) {
        return Some(b);
    }
    let in_config = |keys: &[&str]| config.is_some_and(|c| keys.iter().any(|k| c.contains_key(*k)));
    let resolved = resolve(model, spec, &get);
    if resolved.configured_base_url.is_some() || in_config(routing.config_base_urls) {
        return Some(Backend::Custom);
    }
    if in_config(routing.config_opaque) || routing.opaque_auth_on(&get) {
        return None;
    }
    resolved.backend()
}

/// Envs any one of which lets an agent reach its model, as the credential preflight names them: API keys,
/// opaque credentials, other credentials, base URLs (the agent's own, else its provider's), backend switches.
pub fn credential_envs(spec: Spec, routing: &Routing) -> Vec<&'static str> {
    let access = spec.default_provider.and_then(provider);
    let keys = spec.api_key_envs.iter().chain(access.as_ref().map_or(&[][..], |a| a.api_key_envs));
    let base_urls = if spec.base_url_envs.is_empty() { access.as_ref().map_or(&[][..], |a| a.base_url_envs) } else { spec.base_url_envs };
    let mut out: Vec<&'static str> = vec![];
    let all = keys
        .copied()
        .chain(routing.opaque_auth.iter().map(|s| s.env))
        .chain(routing.credential_envs.iter().copied())
        .chain(base_urls.iter().copied())
        .chain(routing.backends.iter().map(|(s, _)| s.env));
    for n in all {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// What lock.json and result.json record about where a trial's model was served: its [`Backend`] and the
/// labels the run declared. Never a URL, host or credential.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelEndpoint {
    /// `None`: hbench can't tell.
    pub backend: Option<Backend>,
    pub model_served_by: Option<String>,
    pub model_quantization: Option<String>,
}

/// Rewrite provider env aliases to each provider's canonical (first) name.
pub fn with_canonical_provider_envs(mut r: Resolved) -> Resolved {
    let Some(s) = r.provider.as_deref().and_then(provider) else { return r };
    for names in [s.api_key_envs, s.base_url_envs] {
        if names.is_empty() {
            continue;
        }
        let mut val = None;
        for n in names {
            if let Some(v) = r.env.remove(*n) {
                val = Some(v);
            }
        }
        if let Some(v) = val {
            r.env.insert(names[0].to_string(), v);
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn getter(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&[&str]) -> Option<(String, String)> {
        move |names: &[&str]| names.iter().find_map(|n| vars.iter().find(|(k, _)| k == n).map(|(k, v)| (k.to_string(), v.to_string())))
    }

    #[test]
    fn resolves_providers() {
        let claude = Spec {
            default_provider: Some("anthropic"),
            api_key_envs: &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
            base_url_envs: &["ANTHROPIC_BASE_URL"],
            passthrough: false,
        };
        let r = resolve(Some("anthropic/claude-x"), claude, &getter(&[("ANTHROPIC_AUTH_TOKEN", "tok")]));
        assert_eq!(r.provider.as_deref(), Some("anthropic"));
        assert_eq!(r.env["ANTHROPIC_API_KEY"], "tok"); // renamed to the spec's first env
        assert_eq!(r.base_url.as_deref(), Some("https://api.anthropic.com/v1"));

        let pass = Spec { passthrough: true, ..Default::default() };
        let r = resolve(Some("openai/gpt-5"), pass, &getter(&[("OPENAI_API_BASE", "http://x"), ("OPENAI_API_KEY", "k")]));
        assert_eq!((r.env["OPENAI_API_KEY"].as_str(), r.env["OPENAI_API_BASE"].as_str()), ("k", "http://x"));
        let c = with_canonical_provider_envs(r);
        assert_eq!(c.env["OPENAI_BASE_URL"], "http://x");
        assert!(!c.env.contains_key("OPENAI_API_BASE"));

        let r = resolve(Some("gpt-5"), pass, &getter(&[("OPENAI_API_KEY", "k")]));
        assert_eq!(r.provider.as_deref(), Some("openai"));
        let r = resolve(Some("myprov/m"), pass, &getter(&[("MYPROV_BASE_URL", "http://y")]));
        assert_eq!(r.env["MYPROV_BASE_URL"], "http://y");
    }

    #[test]
    fn provider_backend_unless_a_base_url_is_configured() {
        let backend =
            |model: &str, vars: &'static [(&'static str, &'static str)]| resolve(Some(model), Spec::default(), &getter(vars)).backend();
        assert_eq!(backend("openai/gpt-6.1-sol", &[("OPENAI_API_KEY", "k")]), Some(Backend::Provider));
        assert_eq!(backend("openai/gpt-6.1-sol", &[]), Some(Backend::Provider));
        assert_eq!(backend("openai/gpt-6.1-sol", &[("OPENAI_BASE_URL", "http://100.67.202.9:1234/v1")]), Some(Backend::Custom));
        assert_eq!(backend("openai/gpt-6.1-sol", &[("OPENAI_API_BASE", "http://h/v1")]), Some(Backend::Custom));
        assert_eq!(backend("openai/gpt-6.1-sol", &[("OPENAI_BASE_URL", "")]), Some(Backend::Provider));
        assert_eq!(backend("ollama/llama3", &[]), None);
        assert_eq!(backend("myprov/m", &[("MYPROV_BASE_URL", "http://y")]), Some(Backend::Custom));
        assert_eq!(backend("myprov/m", &[]), None);
    }

    const ROUTED: Routing = Routing {
        opaque_auth: &[Switch::new("AUTH_JSON", On::Set)],
        backends: &[(Switch::new("USE_BEDROCK", On::Flag), Backend::Bedrock), (Switch::new("USE_VERTEX", On::Bool), Backend::Vertex)],
        config_env: Some("env"),
        config_base_urls: &["base_url"],
        config_opaque: &["model_provider"],
        ..Routing::NONE
    };

    #[test]
    fn backend_follows_switches_config_and_opaque_auth() {
        let spec = Spec { default_provider: Some("anthropic"), base_url_envs: &["ANTHROPIC_BASE_URL"], ..Spec::default() };
        let backend = |vars: &'static [(&'static str, &'static str)], config: Value| {
            backend(Some("anthropic/claude-x"), spec, &ROUTED, config.as_object(), &getter(vars))
        };
        assert_eq!(backend(&[], Value::Null), Some(Backend::Provider));
        assert_eq!(backend(&[("USE_BEDROCK", "yes"), ("ANTHROPIC_BASE_URL", "http://p")], Value::Null), Some(Backend::Bedrock));
        assert_eq!(backend(&[("USE_BEDROCK", "on")], Value::Null), Some(Backend::Provider));
        assert_eq!(backend(&[("USE_VERTEX", "on")], Value::Null), Some(Backend::Vertex));
        assert_eq!(backend(&[], serde_json::json!({"env": {"USE_VERTEX": "1"}})), Some(Backend::Vertex));
        assert_eq!(backend(&[], serde_json::json!({"env": {"ANTHROPIC_BASE_URL": "http://gw"}})), Some(Backend::Custom));
        assert_eq!(backend(&[], serde_json::json!({"base_url": "http://gw"})), Some(Backend::Custom));
        assert_eq!(backend(&[("AUTH_JSON", "/a.json")], Value::Null), None);
        assert_eq!(backend(&[("AUTH_JSON", "/a.json"), ("ANTHROPIC_BASE_URL", "http://p")], Value::Null), Some(Backend::Custom));
        assert_eq!(backend(&[], serde_json::json!({"model_provider": "oss"})), None);
        let pass = Spec { passthrough: true, ..spec };
        assert_eq!(super::backend(Some("anthropic/claude-x"), pass, &ROUTED, None, &getter(&[])), None);
        assert_eq!(serde_json::to_value(Backend::Bedrock).unwrap(), "bedrock");
    }

    #[test]
    fn credential_envs_in_preflight_order() {
        let spec = Spec { default_provider: Some("anthropic"), api_key_envs: &["ANTHROPIC_AUTH_TOKEN"], ..Spec::default() };
        let routing = Routing { credential_envs: &["OAUTH"], ..ROUTED };
        assert_eq!(
            credential_envs(spec, &routing),
            ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY", "AUTH_JSON", "OAUTH", "ANTHROPIC_BASE_URL", "USE_BEDROCK", "USE_VERTEX"]
        );
    }
}
