//! Eve: runs a local Eve app through
//! Eve's HTTP session API via the uploaded `eve_runner.mjs`.

use super::common::{installed_dir, nvm_install_script, parse_bool_env_value, skills_dir};
use super::pins;
use super::{ensure_system_dependencies, exec_agent, exec_error, exec_root};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use crate::fmt::single_quoted;
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const RUNNER: &str = include_str!("../../../assets/agents/eve/eve_runner.mjs");
const PROJECT_DIR: &str = "/installed-agent/eve-project";
const RUNNER_PATH: &str = "/installed-agent/eve_runner.mjs";
const INSTRUCTION_PATH: &str = "/installed-agent/eve-instruction.txt";
const NODE_PREFIX: &str = "export NVM_DIR=\"$HOME/.nvm\"; [ -s \"$NVM_DIR/nvm.sh\" ] && . \"$NVM_DIR/nvm.sh\"; ";
const IGNORE_NAMES: &[&str] = &[
    ".DS_Store",
    ".cache",
    ".eve",
    ".git",
    ".hg",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".vercel",
    ".vite",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "out",
    "tmp",
    "temp",
    "__pycache__",
];
const PROVIDER_ENV_KEYS: &[&str] = &[
    "AI_GATEWAY_API_KEY",
    "ANTHROPIC_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_DEFAULT_REGION",
    "AWS_REGION",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AZURE_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_ENDPOINT",
    "AZURE_RESOURCE_NAME",
    "DEEPSEEK_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_LOCATION",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_GENERATIVE_AI_API_KEY",
    "GROQ_API_KEY",
    "MISTRAL_API_KEY",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENROUTER_API_KEY",
    "VERCEL_OIDC_TOKEN",
    "XAI_API_KEY",
];
const GATEWAY_KEYS: &[&str] = &["AI_GATEWAY_API_KEY", "VERCEL_OIDC_TOKEN"];
const AUTO_ENV_PACKAGE_KEYS: &[(&str, &[&str])] = &[
    ("@ai-sdk/anthropic", &["ANTHROPIC_API_KEY"]),
    ("@ai-sdk/azure", &["AZURE_API_KEY", "AZURE_OPENAI_API_KEY", "AZURE_OPENAI_ENDPOINT", "AZURE_RESOURCE_NAME"]),
    ("@ai-sdk/deepseek", &["DEEPSEEK_API_KEY"]),
    ("@ai-sdk/gateway", GATEWAY_KEYS),
    ("@ai-sdk/google", &["GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_GENERATIVE_AI_API_KEY"]),
    ("@ai-sdk/google-vertex", &["GOOGLE_APPLICATION_CREDENTIALS", "GOOGLE_CLOUD_LOCATION", "GOOGLE_CLOUD_PROJECT"]),
    ("@ai-sdk/groq", &["GROQ_API_KEY"]),
    ("@ai-sdk/mistral", &["MISTRAL_API_KEY"]),
    ("@ai-sdk/openai", &["OPENAI_API_KEY", "OPENAI_BASE_URL"]),
    ("@ai-sdk/openrouter", &["OPENROUTER_API_KEY"]),
    ("@ai-sdk/xai", &["XAI_API_KEY"]),
];
const ENV_EXAMPLE_NAMES: &[&str] = &[".env.example", ".env.sample"];
const SOURCE_SUFFIXES: &[&str] = &["cjs", "cts", "js", "jsx", "mjs", "mts", "ts", "tsx"];
const MAX_INFERENCE_FILE_BYTES: u64 = 1_000_000;

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}
fn env_assignment_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?m)^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=")
}
fn process_env_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r#"process\.env(?:\.([A-Za-z_][A-Za-z0-9_]*)|\[['"]([A-Za-z_][A-Za-z0-9_]*)['"]\])"#)
}
fn gateway_model_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r#"['"](?:anthropic|bedrock|deepseek|google|groq|mistral|openai|openrouter|xai)/[^'"]+['"]"#)
}
fn env_name_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^[A-Za-z_][A-Za-z0-9_]*$")
}

pub struct Eve {
    args: AgentArgs,
    version: Option<String>,
    path: PathBuf,
    install_command: Option<String>,
    auto_env: bool,
    port: i64,
    startup_timeout_ms: i64,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(Eve::new(args)?))
}

/// Integer kwarg: ints, integral floats, numeric strings.
fn int_opt(args: &AgentArgs, k: &str, default: i64) -> Result<i64> {
    let bad = || anyhow!("Invalid kwargs for agent 'eve':\n  {k}: must be an integer");
    match args.kwargs.get(k) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)).ok_or_else(bad),
        Some(Value::String(s)) => s.trim().parse().map_err(|_| bad()),
        Some(_) => Err(bad()),
    }
}

fn is_ignored(name: &str) -> bool {
    IGNORE_NAMES.contains(&name) || (name.starts_with(".env") && !ENV_EXAMPLE_NAMES.contains(&name))
}

/// Recursive copy with Eve's ignore rule applied at every level.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().to_string();
        if is_ignored(&name) {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            copy_tree(&p, &dst.join(&name))?;
        } else {
            std::fs::copy(&p, dst.join(&name))?;
        }
    }
    Ok(())
}

fn sanitize_eve_filename(v: &str) -> String {
    static R1: OnceLock<Regex> = OnceLock::new();
    static R2: OnceLock<Regex> = OnceLock::new();
    let n = re(&R1, r"[^a-z0-9-]+").replace_all(&v.to_lowercase(), "-").to_string();
    let n = re(&R2, r"-{2,}").replace_all(&n, "-").trim_matches('-').to_string();
    if n.is_empty() { "mcp".into() } else { n }
}

fn connection_filename(used: &mut BTreeSet<String>, server: &str) -> String {
    let mut base = format!("hbench-{}", sanitize_eve_filename(server));
    base.truncate(64);
    let base = base.trim_end_matches('-').to_string();
    let mut name = base.clone();
    let mut suffix = 2;
    while used.contains(&name) {
        let s = format!("-{suffix}");
        name = format!("{}{s}", base[..base.len().min(64 - s.len())].trim_end_matches('-'));
        suffix += 1;
    }
    used.insert(name.clone());
    format!("{name}.ts")
}

fn extract_provider_env_names(text: &str) -> BTreeSet<String> {
    let mut keys: BTreeSet<String> = env_assignment_re().captures_iter(text).map(|c| c[1].to_string()).collect();
    for c in process_env_re().captures_iter(text) {
        keys.extend(c.get(1).or(c.get(2)).map(|m| m.as_str().to_string()));
    }
    keys.retain(|k| PROVIDER_ENV_KEYS.contains(&k.as_str()));
    keys
}

/// Texts of `.env.example`-style and JS/TS source files outside ignored dirs.
fn auto_env_texts(dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            // Files under any ignored dir are skipped, so don't descend.
            if !IGNORE_NAMES.contains(&name.as_str()) {
                auto_env_texts(&p, out);
            }
            continue;
        }
        if !p.is_file() {
            continue;
        }
        let ext = p.extension().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
        if !ENV_EXAMPLE_NAMES.contains(&name.as_str()) && !SOURCE_SUFFIXES.contains(&ext.as_str()) {
            continue;
        }
        if p.metadata().is_ok_and(|m| m.len() > MAX_INFERENCE_FILE_BYTES) {
            continue;
        }
        if let Ok(b) = std::fs::read(&p) {
            out.push(String::from_utf8_lossy(&b).to_string());
        }
    }
}

impl Eve {
    pub fn new(args: AgentArgs) -> Result<Eve> {
        let raw = args.kw("path").map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let raw = match raw.to_str().and_then(|s| s.strip_prefix("~/")) {
            Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
            None => raw,
        };
        let path = std::fs::canonicalize(&raw).unwrap_or(raw);
        let auto_env = parse_bool_env_value(&args.kw("auto_env").unwrap_or_else(|| "true".into()), "auto_env")?;
        let a = Eve {
            port: int_opt(&args, "port", 3024)?,
            startup_timeout_ms: int_opt(&args, "startup_timeout_ms", 60_000)?,
            install_command: args.kw("install_command"),
            version: args.version_kw(),
            auto_env,
            path,
            args,
        };
        a.validate_path()?;
        Ok(a)
    }

    fn validate_path(&self) -> Result<()> {
        let p = &self.path;
        if !p.is_dir() {
            bail!("Eve path does not exist: {}", p.display());
        }
        if !p.join("package.json").is_file() {
            bail!("Eve path must contain package.json: {}", p.display());
        }
        let flat = p.join("agent.ts").is_file()
            && (p.join("instructions.md").is_file() || p.join("instructions.ts").is_file() || p.join("instructions").is_dir());
        if !p.join("agent").is_dir() && !flat {
            bail!("Eve path must contain an agent/ directory or flat agent.ts/instructions.md files: {}", p.display());
        }
        Ok(())
    }

    fn remote_agent_dir(&self) -> String {
        if self.path.join("agent").is_dir() { format!("{PROJECT_DIR}/agent") } else { PROJECT_DIR.into() }
    }

    /// Copy the project (minus heavy dirs and .env files) into `target` and add MCP connections.
    fn stage_project(&self, target: &Path) -> Result<()> {
        if target.exists() {
            std::fs::remove_dir_all(target)?;
        }
        copy_tree(&self.path, target)?;
        self.inject_mcp_connections(target)
    }

    fn inject_mcp_connections(&self, project: &Path) -> Result<()> {
        if self.args.mcp_servers.is_empty() {
            return Ok(());
        }
        let agent_dir = if project.join("agent").is_dir() { project.join("agent") } else { project.to_path_buf() };
        let dir = agent_dir.join("connections");
        let mut used: BTreeSet<String> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().is_file())
                    .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        for s in &self.args.mcp_servers {
            let Some(url) = s.url.as_deref().filter(|u| !u.is_empty() && matches!(s.transport(), "sse" | "streamable-http")) else {
                continue;
            };
            std::fs::create_dir_all(&dir)?;
            let file = connection_filename(&mut used, &s.name);
            let description = format!("hbench-provided MCP server {} via {} transport.", single_quoted(&s.name), s.transport());
            std::fs::write(
                dir.join(file),
                format!(
                    "import {{ defineMcpClientConnection }} from \"eve/connections\";\n\nexport default defineMcpClientConnection({{\n  url: {},\n  description: {},\n}});\n",
                    serde_json::to_string(url)?,
                    serde_json::to_string(&description)?
                ),
            )?;
        }
        Ok(())
    }

    fn package_json(&self) -> Result<Map<String, Value>> {
        let raw = std::fs::read_to_string(self.path.join("package.json"))?;
        match serde_json::from_str::<Value>(&raw).map_err(|e| anyhow!("Invalid package.json in Eve project: {e}"))? {
            Value::Object(o) => Ok(o),
            _ => bail!("Eve project package.json must be a JSON object"),
        }
    }

    fn inferred_auto_env_keys(&self) -> Result<BTreeSet<String>> {
        let pkg = self.package_json()?;
        let deps: BTreeSet<&str> = ["dependencies", "devDependencies", "optionalDependencies", "peerDependencies"]
            .iter()
            .filter_map(|k| pkg.get(*k)?.as_object())
            .flat_map(|o| o.keys().map(String::as_str))
            .collect();
        let mut keys = BTreeSet::new();
        let add = |ks: &[&str], keys: &mut BTreeSet<String>| keys.extend(ks.iter().map(|k| k.to_string()));
        for (p, ks) in AUTO_ENV_PACKAGE_KEYS {
            if deps.contains(p) {
                add(ks, &mut keys);
            }
        }
        let mut texts = vec![];
        auto_env_texts(&self.path, &mut texts);
        for t in &texts {
            keys.extend(extract_provider_env_names(t));
            for (p, ks) in AUTO_ENV_PACKAGE_KEYS {
                if t.contains(p) {
                    add(ks, &mut keys);
                }
            }
            if gateway_model_re().is_match(t) || t.contains("gateway(") {
                add(GATEWAY_KEYS, &mut keys);
            }
        }
        Ok(keys)
    }

    fn infer_install_command(&self) -> Result<String> {
        if let Some(c) = &self.install_command {
            return Ok(c.clone());
        }
        let has = |f: &str| self.path.join(f).exists();
        let bun = format!("npm install -g {} && bun install --frozen-lockfile", pins::BUN.npm_spec(None));
        let (pnpm, yarn) = ("corepack enable && pnpm install --frozen-lockfile", "corepack enable && yarn install --frozen-lockfile");
        if let Some(pm) = self.package_json()?.get("packageManager").and_then(Value::as_str) {
            if pm.starts_with("pnpm@") {
                return Ok(pnpm.into());
            }
            if pm.starts_with("yarn@") {
                return Ok(yarn.into());
            }
            if pm.starts_with("bun@") {
                return Ok(bun);
            }
            if pm.starts_with("npm@") {
                return Ok(if has("package-lock.json") { "npm ci" } else { "npm install" }.into());
            }
        }
        Ok(if has("pnpm-lock.yaml") {
            pnpm.into()
        } else if has("yarn.lock") {
            yarn.into()
        } else if has("bun.lock") || has("bun.lockb") {
            bun
        } else if has("package-lock.json") {
            "npm ci".into()
        } else {
            "npm install".into()
        })
    }

    /// Inferred provider keys forwarded from the host env, plus the model hint.
    fn runtime_env_with(&self, host: &dyn Fn(&str) -> Option<String>) -> Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::new();
        if self.auto_env {
            for k in self.inferred_auto_env_keys()? {
                if let Some(v) = host(&k).filter(|_| env_name_re().is_match(&k)) {
                    env.insert(k, v);
                }
            }
        }
        if let Some(m) = &self.args.model_name {
            env.insert("HBENCH_MODEL".into(), m.clone());
        }
        Ok(env)
    }

    fn runtime_env(&self) -> Result<BTreeMap<String, String>> {
        self.runtime_env_with(&|k| std::env::var(k).ok())
    }

    fn runner_env_with(&self, host: &dyn Fn(&str) -> Option<String>) -> Result<BTreeMap<String, String>> {
        let mut env = self.runtime_env_with(host)?;
        env.extend(self.args.extra_env.clone());
        for (k, v) in [
            ("EVE_AGENT_LOGS_DIR", "/logs/agent".to_string()),
            ("EVE_HOST", "127.0.0.1".into()),
            ("EVE_INSTRUCTION_PATH", INSTRUCTION_PATH.into()),
            ("EVE_PORT", self.port.to_string()),
            ("EVE_PROJECT_DIR", PROJECT_DIR.into()),
            ("EVE_STARTUP_TIMEOUT_MS", self.startup_timeout_ms.to_string()),
        ] {
            env.insert(k.into(), v);
        }
        Ok(env)
    }

    fn register_skills_command(&self) -> Option<String> {
        let src = quote(&skills_dir(&self.args)?);
        let dst = quote(&format!("{}/skills", self.remote_agent_dir()));
        Some(format!(
            "if [ -d {src} ]; then mkdir -p {dst} && dest_dir={dst}; for skill_path in {src}/*; do [ -e \"$skill_path\" ] || continue; \
             skill_name=\"$(basename \"$skill_path\")\"; cp -r \"$skill_path\" \"$dest_dir/hbench_$skill_name\"; done; fi"
        ))
    }
}

/// Step metrics extracted from an event's `usage` (or the data itself).
#[derive(Clone, Copy, Default)]
struct Metrics {
    prompt: Option<i64>,
    completion: Option<i64>,
    cached: Option<i64>,
    cost: Option<f64>,
}

fn usage_metrics(data: &Map<String, Value>) -> Option<Metrics> {
    let src = data.get("usage").and_then(Value::as_object).unwrap_or(data);
    // Bools count as integers (0/1) here.
    let int = |ks: &[&str]| {
        ks.iter().find_map(|k| match src.get(*k)? {
            Value::Bool(b) => Some(*b as i64),
            v => v.as_i64(),
        })
    };
    let float = |ks: &[&str]| {
        ks.iter().find_map(|k| match src.get(*k)? {
            Value::Bool(b) => Some(*b as i64 as f64),
            v => v.as_f64(),
        })
    };
    let m = Metrics {
        prompt: int(&["inputTokens", "input_tokens", "promptTokens", "prompt_tokens", "totalPromptTokens"]),
        completion: int(&["outputTokens", "output_tokens", "completionTokens", "completion_tokens", "totalCompletionTokens"]),
        cached: int(&["cachedTokens", "cached_tokens", "cachedInputTokens", "cached_input_tokens"]),
        cost: float(&["costUsd", "cost_usd", "totalCostUsd", "total_cost_usd", "totalCost"]),
    };
    let nz = |v: Option<i64>| v.is_some_and(|x| x != 0);
    (nz(m.prompt) || nz(m.completion) || nz(m.cached) || m.cost.is_some_and(|c| c != 0.0)).then_some(m)
}

fn message_from_data(data: &Map<String, Value>) -> String {
    for k in ["message", "text", "content"] {
        if let Some(Value::String(s)) = data.get(k) {
            return s.clone();
        }
    }
    if let Some(Value::Array(parts)) = data.get("message") {
        return parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn action_id(action: &Map<String, Value>, fallback: String) -> String {
    ["id", "callId", "callID", "toolCallId", "tool_call_id", "actionId"]
        .iter()
        .find_map(|k| action.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
        .map(String::from)
        .unwrap_or(fallback)
}

fn extract_actions(data: &Map<String, Value>) -> Vec<Map<String, Value>> {
    for k in ["actions", "toolCalls", "tool_calls", "requests"] {
        if let Some(Value::Array(a)) = data.get(k) {
            return a.iter().filter_map(|i| i.as_object().cloned()).collect();
        }
    }
    if data.is_empty() { vec![] } else { vec![data.clone()] }
}

/// Trajectory final_metrics totals: replays how events become steps (only the
/// parts that affect metrics).
/// Returns None when the trajectory would have no steps.
fn event_totals(text: &str) -> Option<(i64, i64, i64, f64)> {
    let empty = Map::new();
    // (is_agent, metrics)
    let mut steps: Vec<(bool, Option<Metrics>)> = vec![];
    let mut pending: HashMap<String, usize> = HashMap::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Ok(Value::Object(ev)) = serde_json::from_str::<Value>(line) else { continue };
        let data = ev.get("data").and_then(Value::as_object).unwrap_or(&empty);
        match ev.get("type").and_then(Value::as_str).unwrap_or("") {
            "message.received" => {
                if !message_from_data(data).is_empty() {
                    steps.push((false, None));
                }
            }
            "actions.requested" => {
                let actions = extract_actions(data);
                if !actions.is_empty() {
                    let idx = steps.len();
                    for (i, a) in actions.iter().enumerate() {
                        pending.insert(action_id(a, format!("action-{}-{}", idx + 1, i + 1)), idx);
                    }
                    steps.push((true, None));
                }
            }
            "action.result" => {
                let payload = ["result", "output", "value"].iter().find_map(|k| data.get(*k));
                let nested = payload.and_then(Value::as_object).unwrap_or(if payload.is_none() { data } else { &empty });
                let call_id = action_id(data, action_id(nested, format!("action-result-{}", steps.len() + 1)));
                if !pending.remove(&call_id).is_some_and(|i| i < steps.len()) {
                    steps.push((true, None));
                }
            }
            "message.completed" => {
                if !message_from_data(data).is_empty() {
                    steps.push((true, usage_metrics(data)));
                }
            }
            "result.completed" => steps.push((true, None)),
            "step.completed" => {
                if let (Some(m), Some(last)) = (usage_metrics(data), steps.last_mut())
                    && last.0
                    && last.1.is_none()
                {
                    last.1 = Some(m);
                }
            }
            "step.failed" | "turn.failed" | "session.failed" => steps.push((false, None)),
            _ => {}
        }
    }
    if steps.is_empty() {
        return None;
    }
    let ms = steps.iter().filter_map(|s| s.1);
    Some(ms.fold((0, 0, 0, 0.0), |t, m| {
        (t.0 + m.prompt.unwrap_or(0), t.1 + m.completion.unwrap_or(0), t.2 + m.cached.unwrap_or(0), t.3 + m.cost.unwrap_or(0.0))
    }))
}

/// Post-run context: runner summary first, then event totals override when non-zero.
fn populate(logs_dir: &Path, ctx: &mut AgentContext) {
    if let Ok(raw) = std::fs::read_to_string(logs_dir.join("eve-result.json")) {
        let result = serde_json::from_str::<Value>(&raw).unwrap_or(json!({}));
        if let Some(r) = result.as_object() {
            if let Some(u) = r.get("usage").and_then(Value::as_object) {
                let int = |k: &str| u.get(k).and_then(Value::as_i64);
                if let Some(v) = int("inputTokens") {
                    ctx.n_input_tokens = Some(v);
                }
                if let Some(v) = int("outputTokens") {
                    ctx.n_output_tokens = Some(v);
                }
                if let Some(v) = int("cachedTokens") {
                    ctx.n_cache_tokens = Some(v);
                }
                if let Some(v) = u.get("costUsd").and_then(Value::as_f64) {
                    ctx.cost_usd = Some(v);
                }
            }
            let mut meta = ctx.metadata.take().and_then(|m| m.as_object().cloned()).unwrap_or_default();
            meta.insert("eve_status".into(), r.get("status").cloned().unwrap_or(Value::Null));
            meta.insert("eve_session_id".into(), r.get("sessionId").cloned().unwrap_or(Value::Null));
            meta.insert("answer_written".into(), r.get("message").cloned().unwrap_or(Value::Null));
            ctx.metadata = Some(Value::Object(meta));
        }
    }
    let Some((p, c, k, cost)) = std::fs::read_to_string(logs_dir.join("eve-events.ndjson")).ok().and_then(|t| event_totals(&t)) else {
        return;
    };
    if p != 0 {
        ctx.n_input_tokens = Some(p);
    }
    if c != 0 {
        ctx.n_output_tokens = Some(c);
    }
    if k != 0 {
        ctx.n_cache_tokens = Some(k);
    }
    if cost != 0.0 {
        ctx.cost_usd = Some(cost);
    }
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hbench-eve-{}-{name}", uuid::Uuid::new_v4().simple()))
}

#[async_trait::async_trait]
impl Agent for Eve {
    fn name(&self) -> &str {
        "eve"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        // Staged in a temp dir, not logs_dir/eve_project, so the project isn't synced back into /logs/agent.
        let staged = temp_path("project");
        self.stage_project(&staged)?;
        let runner = temp_path("eve_runner.mjs");
        std::fs::write(&runner, RUNNER)?;
        let r = self.install(cx, &staged, &runner).await;
        let _ = std::fs::remove_dir_all(&staged);
        let _ = std::fs::remove_file(&runner);
        r?;
        if self.version.is_none() {
            let r = cx.exec(&format!("{NODE_PREFIX}cd {PROJECT_DIR} && node_modules/.bin/eve --version"), &BTreeMap::new(), None).await?;
            if r.code == 0 && !r.stdout.is_empty() {
                self.version = Some(r.stdout.trim().to_string());
            }
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let local = temp_path("instruction.txt");
        std::fs::write(&local, &instruction)?;
        let up = cx.env.upload_file(&local, INSTRUCTION_PATH).await;
        let _ = std::fs::remove_file(&local);
        up?;
        let command = format!("{NODE_PREFIX}node {}", quote(RUNNER_PATH));
        let env = self.runner_env_with(&|k| std::env::var(k).ok())?;
        let r = cx.exec(&format!("set -o pipefail; cd {PROJECT_DIR} || exit 1; {command}"), &env, None).await?;
        // Artifacts land in /logs/agent; the trial downloads them and calls collect_usage.
        if r.code != 0 {
            return Err(exec_error(&command, &r, None));
        }
        Ok(())
    }

    fn collect_usage(&self, logs_dir: &Path, ctx: &mut AgentContext) {
        populate(logs_dir, ctx);
    }
}

impl Eve {
    async fn install(&self, cx: &AgentCtx<'_>, staged: &Path, runner: &Path) -> Result<()> {
        ensure_system_dependencies(cx, &["curl", "ca_certificates", "bash"]).await?;
        exec_agent(
            cx,
            &format!(
                "set -euo pipefail; {NODE_PREFIX}if command -v node >/dev/null 2>&1 && node -e 'process.exit(Number(process.versions.node.split(\".\")[0]) >= 24 ? 0 : 1)'; then \
                 node --version; else {nvm_install}; {NODE_PREFIX}nvm install 24; nvm alias default 24; \
                 node --version; fi; npm --version; corepack enable || true", nvm_install = nvm_install_script()),
            &BTreeMap::from([("NVM_NODEJS_ORG_MIRROR".to_string(), "https://nodejs.org/dist".to_string())]),
        )
        .await?;
        exec_root(cx, &format!("rm -rf {PROJECT_DIR} && mkdir -p /installed-agent /logs/agent"), &BTreeMap::new()).await?;
        cx.env.upload_dir(staged, PROJECT_DIR).await?;
        cx.env.upload_file(runner, RUNNER_PATH).await?;
        let u = quote(cx.user.as_deref().unwrap_or("root"));
        exec_root(cx, &format!("chmod +x {RUNNER_PATH} && chown -R {u}:{u} {PROJECT_DIR} {RUNNER_PATH}"), &BTreeMap::new()).await?;

        let in_project = |c: &str| format!("cd {PROJECT_DIR} || exit 1; {c}");
        let no_env = BTreeMap::new();
        let install = self.infer_install_command()?;
        if !install.is_empty() {
            exec_agent(cx, &in_project(&format!("{NODE_PREFIX}{install}")), &no_env).await?;
        }
        if let Some(c) = self.register_skills_command() {
            exec_agent(cx, &in_project(&c), &no_env).await?;
        }
        let rt = self.runtime_env()?;
        // `eve info` failing is non-fatal.
        let _ = cx
            .exec(
                &format!(
                    "set -o pipefail; {}",
                    in_project(&format!(
                        "{NODE_PREFIX}node_modules/.bin/eve info --json > /logs/agent/eve-info.json 2> /logs/agent/eve-info.stderr"
                    ))
                ),
                &rt,
                None,
            )
            .await?;
        exec_agent(cx, &in_project(&format!("{NODE_PREFIX}node_modules/.bin/eve build 2>&1 | tee /logs/agent/eve-build.log")), &rt).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common;

    fn project(pkg: Value, flat: bool) -> PathBuf {
        let p = std::env::temp_dir().join(format!("hbench-eve-proj-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("package.json"), pkg.to_string()).unwrap();
        if flat {
            std::fs::write(p.join("agent.ts"), "import { defineAgent } from \"eve\";\n").unwrap();
            std::fs::write(p.join("instructions.md"), "flat\n").unwrap();
        } else {
            std::fs::create_dir_all(p.join("agent")).unwrap();
            std::fs::write(
                p.join("agent/agent.ts"),
                "import { defineAgent } from \"eve\";\nexport default defineAgent({ model: \"anthropic/claude-sonnet-4.6\" });\n",
            )
            .unwrap();
            std::fs::write(p.join("agent/instructions.md"), "x\n").unwrap();
        }
        p
    }

    fn agent(kwargs: Value, env: &[(&str, &str)], mcp: Vec<crate::task::McpServer>) -> Result<Eve> {
        Eve::new(AgentArgs {
            mcp_servers: mcp,
            skills: vec!["s".into()],
            ..common::test_args(kwargs, env, Some("anthropic/claude-sonnet-4.6"))
        })
    }

    fn host(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn options_and_validation() {
        let p = project(json!({"dependencies": {"eve": "^0.11.3"}}), false);
        let ps = p.display().to_string();
        assert!(agent(json!({"path": ps, "port": "x"}), &[], vec![]).is_err());
        assert!(agent(json!({"path": ps, "auto_env": "maybe"}), &[], vec![]).is_err());
        let empty = std::env::temp_dir().join(format!("hbench-eve-empty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&empty).unwrap();
        assert!(agent(json!({"path": empty.display().to_string()}), &[], vec![]).err().unwrap().to_string().contains("package.json"));
        std::fs::write(empty.join("package.json"), "{}").unwrap();
        assert!(agent(json!({"path": empty.display().to_string()}), &[], vec![]).err().unwrap().to_string().contains("agent"));

        let a =
            agent(json!({"path": ps, "port": 4321, "startup_timeout_ms": 12345}), &[("AI_GATEWAY_API_KEY", "agent-key")], vec![]).unwrap();
        let env = a.runner_env_with(&host(&[("AI_GATEWAY_API_KEY", "host-key")])).unwrap();
        assert_eq!(env["AI_GATEWAY_API_KEY"], "agent-key");
        assert_eq!(env["HBENCH_MODEL"], "anthropic/claude-sonnet-4.6");
        assert_eq!((env["EVE_PORT"].as_str(), env["EVE_STARTUP_TIMEOUT_MS"].as_str()), ("4321", "12345"));
        assert_eq!(env["EVE_INSTRUCTION_PATH"], INSTRUCTION_PATH);
        // String model literal implies the AI gateway keys.
        let env = a.runner_env_with(&host(&[("AI_GATEWAY_API_KEY", "g"), ("VERCEL_OIDC_TOKEN", "o")])).unwrap();
        assert_eq!(env["VERCEL_OIDC_TOKEN"], "o");
        let skills = a.register_skills_command().unwrap();
        assert!(
            skills.contains("/hbench/skills")
                && skills.contains("/installed-agent/eve-project/agent/skills")
                && skills.contains("hbench_$skill_name")
        );

        let flat = project(json!({}), true);
        let f = agent(json!({"path": flat.display().to_string()}), &[], vec![]).unwrap();
        let skills = f.register_skills_command().unwrap();
        assert!(skills.contains("/installed-agent/eve-project/skills") && !skills.contains("eve-project/agent/skills"));
    }

    #[test]
    fn auto_env_inference() {
        let p = project(json!({"dependencies": {"eve": "1", "@ai-sdk/openai": "latest"}}), false);
        std::fs::write(
            p.join("agent/agent.ts"),
            "import { openai } from \"@ai-sdk/openai\";\nconst fallback = process.env.ANTHROPIC_API_KEY;\nexport default defineAgent({ model: openai(\"gpt-4o-mini\") });\n",
        )
        .unwrap();
        let vars = host(&[("OPENAI_API_KEY", "o"), ("OPENAI_BASE_URL", "u"), ("ANTHROPIC_API_KEY", "a"), ("GEMINI_API_KEY", "g")]);
        let a = agent(json!({"path": p.display().to_string(), "auto_env": "true"}), &[], vec![]).unwrap();
        let env = a.runner_env_with(&vars).unwrap();
        assert_eq!((env["OPENAI_API_KEY"].as_str(), env["OPENAI_BASE_URL"].as_str(), env["ANTHROPIC_API_KEY"].as_str()), ("o", "u", "a"));
        assert!(!env.contains_key("GEMINI_API_KEY"));
        let off = agent(json!({"path": p.display().to_string(), "auto_env": false}), &[], vec![]).unwrap();
        assert!(!off.runner_env_with(&vars).unwrap().contains_key("OPENAI_API_KEY"));
    }

    #[test]
    fn install_command_inference() {
        for (pkg, lock, want) in [
            (json!({"packageManager": "pnpm@9"}), None, "corepack enable && pnpm install --frozen-lockfile"),
            (json!({"packageManager": "yarn@4"}), None, "corepack enable && yarn install --frozen-lockfile"),
            (json!({"packageManager": "bun@1"}), None, "npm install -g bun@1.4.3 && bun install --frozen-lockfile"),
            (json!({"packageManager": "npm@10"}), Some("package-lock.json"), "npm ci"),
            (json!({}), Some("pnpm-lock.yaml"), "corepack enable && pnpm install --frozen-lockfile"),
            (json!({}), Some("yarn.lock"), "corepack enable && yarn install --frozen-lockfile"),
            (json!({}), Some("bun.lockb"), "npm install -g bun@1.4.3 && bun install --frozen-lockfile"),
            (json!({}), Some("package-lock.json"), "npm ci"),
            (json!({}), None, "npm install"),
        ] {
            let p = project(pkg, false);
            if let Some(l) = lock {
                std::fs::write(p.join(l), "{}").unwrap();
            }
            assert_eq!(agent(json!({"path": p.display().to_string()}), &[], vec![]).unwrap().infer_install_command().unwrap(), want);
        }
        let p = project(json!({}), false);
        let a = agent(json!({"path": p.display().to_string(), "install_command": "pnpm install --offline"}), &[], vec![]).unwrap();
        assert_eq!(a.infer_install_command().unwrap(), "pnpm install --offline");
    }

    #[test]
    fn staging_ignores_and_injects_mcp() {
        let p = project(json!({}), false);
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(p.join("src/keep.ts"), "x").unwrap();
        for f in [".env", ".env.local", ".env.example"] {
            std::fs::write(p.join(f), "S=\n").unwrap();
        }
        for d in [".git", "node_modules", "dist"] {
            std::fs::create_dir_all(p.join(d)).unwrap();
            std::fs::write(p.join(d).join("i.txt"), "x").unwrap();
        }
        let srv = |name: &str, t: &str, url: Option<&str>| crate::task::McpServer {
            name: name.into(),
            transport: t.into(),
            url: url.map(String::from),
            command: None,
            args: vec![],
        };
        let a = agent(
            json!({"path": p.display().to_string()}),
            &[],
            vec![
                srv("linear", "sse", Some("http://linear-mcp:8000/sse")),
                srv("api.server", "streamable-http", Some("http://api-mcp:8000/mcp")),
                srv("local", "stdio", None),
            ],
        )
        .unwrap();
        let staged = temp_path("staged-test");
        a.stage_project(&staged).unwrap();
        assert!(staged.join("agent/agent.ts").is_file() && staged.join("src/keep.ts").is_file() && staged.join(".env.example").is_file());
        for gone in [".env", ".env.local", ".git", "node_modules", "dist"] {
            assert!(!staged.join(gone).exists(), "{gone}");
        }
        let linear = std::fs::read_to_string(staged.join("agent/connections/hbench-linear.ts")).unwrap();
        assert!(linear.contains("defineMcpClientConnection") && linear.contains("\"http://linear-mcp:8000/sse\""));
        assert!(linear.contains("\"hbench-provided MCP server 'linear' via sse transport.\""));
        assert!(
            std::fs::read_to_string(staged.join("agent/connections/hbench-api-server.ts")).unwrap().contains("\"http://api-mcp:8000/mcp\"")
        );
        assert!(!staged.join("agent/connections/hbench-local.ts").exists());
        let mut used = BTreeSet::from(["hbench-x".to_string()]);
        assert_eq!(connection_filename(&mut used, "X"), "hbench-x-2.ts");
        assert_eq!(connection_filename(&mut used, "!!!"), "hbench-mcp.ts");
    }

    #[test]
    fn usage_from_result_and_events() {
        let dir = temp_path("logs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("eve-result.json"),
            r#"{"status":"session.waiting","sessionId":"s1","message":null,"usage":{"inputTokens":7,"outputTokens":0}}"#,
        )
        .unwrap();
        let mut c = AgentContext::default();
        populate(&dir, &mut c);
        assert_eq!((c.n_input_tokens, c.n_output_tokens), (Some(7), Some(0)));
        let m = c.metadata.unwrap();
        assert_eq!(
            (m["eve_status"].as_str(), m["eve_session_id"].as_str(), m["answer_written"].is_null()),
            (Some("session.waiting"), Some("s1"), true)
        );

        // Representative event stream: step.completed usage attaches to the final message.
        let events = [
            json!({"type": "message.received", "timestamp": "2026-06-17T00:00:00Z", "data": {"sessionId": "s1", "message": "What is the weather?"}}),
            json!({"type": "actions.requested", "data": {"actions": [{"id": "call-1", "name": "get_weather", "arguments": {"city": "SF"}}]}}),
            json!({"type": "action.result", "data": {"toolCallId": "call-1", "result": {"temperatureF": 72}}}),
            json!({"type": "reasoning.completed", "data": {"message": "I checked the tool."}}),
            json!({"type": "message.completed", "data": {"message": "It is 72F."}}),
            json!({"type": "step.completed", "data": {"usage": {"inputTokens": 10, "outputTokens": 4, "cachedTokens": 2, "costUsd": 0.01}}}),
            // No agent step without metrics remains; this usage is dropped.
            json!({"type": "step.completed", "data": {"usage": {"inputTokens": 99}}}),
        ];
        let text = events.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n");
        assert_eq!(event_totals(&text), Some((10, 4, 2, 0.01)));
        std::fs::write(dir.join("eve-events.ndjson"), &text).unwrap();
        std::fs::write(dir.join("eve-result.json"), r#"{"status":"session.completed","sessionId":"s1","message":"Done","usage":{"inputTokens":5,"outputTokens":3,"cachedTokens":1,"costUsd":0.02}}"#).unwrap();
        let mut c = AgentContext::default();
        populate(&dir, &mut c);
        assert_eq!((c.n_input_tokens, c.n_output_tokens, c.n_cache_tokens, c.cost_usd), (Some(10), Some(4), Some(2), Some(0.01)));

        // Eve's current tool order: result id nested in payload; usage on a tool step via step.completed.
        let text = [
            json!({"type": "actions.requested", "data": {"actions": [{"callId": "call-1", "toolName": "w", "input": {}}]}}),
            json!({"type": "action.result", "data": {"result": {"callId": "call-1", "output": {"p": 1}}}}),
            json!({"type": "step.completed", "data": {"inputTokens": 3, "outputTokens": 1}}),
            json!({"type": "session.failed", "data": {"message": "boom"}}),
        ]
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert_eq!(event_totals(&text), Some((3, 1, 0, 0.0)));
        assert_eq!(event_totals("not json\n"), None);
    }

    #[test]
    fn runner_payload() {
        assert!(RUNNER.contains(r#"["start", "--host", cfg.host, "--port", String(cfg.port)]"#));
        assert!(RUNNER.contains("session.unsettled") && !RUNNER.contains("step.failed"));
    }
}
