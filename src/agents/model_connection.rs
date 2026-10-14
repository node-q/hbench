//! Declarative model credentials and endpoint resolution (port of upstream's model_connection.py).

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
        "amazon-bedrock" => p(&["AWS_ACCESS_KEY_ID"], None, &[], AWS),
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
        "sagemaker" => p(&["AWS_ACCESS_KEY_ID"], None, &[], AWS),
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

/// Best-effort stand-in for litellm's provider inference on bare model names.
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
    fn resolves_like_hbench() {
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
}
