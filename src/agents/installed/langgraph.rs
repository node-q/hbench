//! LangGraph: runs a user's LangGraph project (Python or LangGraph.js) via the bundled runners.

use super::common::{installed_dir, nvm_node_install_snippet, upload_asset, uv_install};
use super::pins;
use super::{ensure_system_dependencies, exec_agent, exec_root};
use crate::agents::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::quote;
use crate::fmt::single_quoted;
use anyhow::{Result, bail};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const PY_RUNNER: &str = include_str!("../../../assets/agents/langgraph/langgraph_runner.py");
const NODE_RUNNER: &str = include_str!("../../../assets/agents/langgraph/langgraphjs_runner.mjs");
const PROJECT_DIR: &str = "/installed-agent/langgraph-project";
const PY_RUNNER_PATH: &str = "/installed-agent/langgraph_runner.py";
const NODE_RUNNER_PATH: &str = "/installed-agent/langgraphjs_runner.mjs";
const VENV_DIR: &str = "/opt/hbench-langgraph-venv";
const NODE_TOOLS_DIR: &str = "/opt/hbench-langgraph-node-tools";
const INSTRUCTION_PATH: &str = "/installed-agent/instruction.txt";
const DEFAULT_NODE_LOADER: &str = "/opt/hbench-langgraph-node-tools/node_modules/tsx/dist/loader.mjs";
const NODE_SHELL_PREFIX: &str = "export NVM_DIR=\"$HOME/.nvm\"; . \"$NVM_DIR/nvm.sh\"; ";
const MAX_SUMMARY_BYTES: usize = 1_000_000;
const IGNORE_NAMES: [&str; 10] =
    [".env", ".git", ".hg", ".mypy_cache", ".pytest_cache", ".ruff_cache", ".tox", ".venv", "__pycache__", "node_modules"];
const NODE_SUFFIXES: [&str; 8] = [".js", ".mjs", ".cjs", ".ts", ".mts", ".cts", ".jsx", ".tsx"];
/// manager -> (lockfiles, locked install, unlocked install), matched in this order.
const NODE_PACKAGE_MANAGERS: [(&str, &[&str], &str, &str); 4] = [
    ("pnpm", &["pnpm-lock.yaml"], "corepack enable && pnpm install --frozen-lockfile", "corepack enable && pnpm install"),
    ("yarn", &["yarn.lock"], "corepack enable && yarn install --immutable", "corepack enable && yarn install"),
    ("bun", &["bun.lock", "bun.lockb"], "bun install --frozen-lockfile", "bun install"),
    ("npm", &["package-lock.json"], "npm ci", "npm install"),
];
const FORWARDED_ENV_VARS: [&str; 21] = [
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    "FIREWORKS_API_KEY",
    "OPENROUTER_API_KEY",
    "BASETEN_API_KEY",
    "GROQ_API_KEY",
    "XAI_API_KEY",
    "NVIDIA_API_KEY",
    "OLLAMA_API_KEY",
    "OLLAMA_HOST",
    "LANGSMITH_API_KEY",
    "LANGSMITH_TRACING",
    "LANGSMITH_TRACING_V2",
    "LANGSMITH_PROJECT",
    "LANGSMITH_WORKSPACE_ID",
    "LANGSMITH_PROFILE",
    "LANGSMITH_ENDPOINT",
    "LANGSMITH_DATASET",
    "HBENCH_LANGSMITH_PARENT",
    "HBENCH_LANGSMITH_BAGGAGE",
];

fn loader_package_re() -> Regex {
    Regex::new(r"^(?:@[a-z0-9][a-z0-9._-]*/)?[a-z0-9][a-z0-9._-]*$").unwrap()
}

fn suffix(p: &str) -> &str {
    let name = p.rsplit('/').next().unwrap_or(p);
    match name.rfind('.') {
        Some(i) if i > 0 => &name[i..],
        _ => "",
    }
}

/// Splits a graph spec `./file.py:attr.path` (identifier rules per runtime) into file and attribute path.
fn split_graph_path(spec: &str) -> Result<(String, String)> {
    let bad = || anyhow::anyhow!("Invalid graph path '{spec}'. Expected './path/to/file.py:attribute'");
    if spec.matches(':').count() != 1 {
        return Err(bad());
    }
    let (module, attr) = spec.split_once(':').unwrap();
    if module.is_empty() || attr.is_empty() {
        return Err(bad());
    }
    let (re, dialect) = if NODE_SUFFIXES.contains(&suffix(module)) {
        (r"^[A-Za-z_$][A-Za-z0-9_$]*$", "JavaScript")
    } else {
        (r"^[A-Za-z_][A-Za-z0-9_]*$", "Python")
    };
    let re = Regex::new(re).unwrap();
    if !attr.split('.').all(|p| re.is_match(p)) {
        bail!("Invalid graph attribute path '{attr}'. Expected dot-separated {dialect} identifiers");
    }
    Ok((module.into(), attr.into()))
}

fn graph_runtime(spec: &str) -> Result<&'static str> {
    let (module, _) = split_graph_path(spec)?;
    match suffix(&module) {
        ".py" => Ok("python"),
        s if NODE_SUFFIXES.contains(&s) => Ok("node"),
        s => bail!(
            "Unsupported LangGraph graph module suffix '{}' in '{spec}'. Supported suffixes: .cjs, .cts, .js, .jsx, .mjs, .mts, .py, .ts, .tsx",
            if s.is_empty() { "(none)" } else { s }
        ),
    }
}

fn select_graph(config: &Value, name: Option<&str>) -> Result<(String, String)> {
    let Some(graphs) = config["graphs"].as_object().filter(|g| !g.is_empty()) else {
        bail!("langgraph.json must define at least one graph")
    };
    let available = || graphs.keys().cloned().collect::<Vec<_>>().join(", ");
    let name = match name {
        Some(n) => n.to_string(),
        None if graphs.len() == 1 => graphs.keys().next().unwrap().clone(),
        None => bail!("langgraph.json defines multiple graphs; pass --graph. Available graphs: {}", available()),
    };
    let Some(spec) = graphs.get(&name) else { bail!("Unknown graph '{name}'. Available graphs: {}", available()) };
    let path = match spec {
        Value::String(s) => s.clone(),
        Value::Object(o) if o.get("path").is_some_and(Value::is_string) => o["path"].as_str().unwrap().to_string(),
        _ => bail!("Graph spec must be a string or object with a string 'path' field"),
    };
    Ok((name, path))
}

/// `posixpath.normpath` for absolute paths.
fn normpath(p: &str) -> String {
    let mut parts: Vec<&str> = vec![];
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

fn copy_project(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name();
        let n = name.to_string_lossy();
        if IGNORE_NAMES.contains(&n.as_ref()) || n.starts_with(".env.") {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            copy_project(&p, &dst.join(&name))?;
        } else {
            std::fs::copy(&p, dst.join(&name))?;
        }
    }
    Ok(())
}

pub struct LangGraph {
    args: AgentArgs,
    version: Option<String>,
    project_path: PathBuf,
    config: String,
    lg_config: Value,
    graph: String,
    graph_path: String,
    runtime: &'static str,
    node_version: u32,
    model_kwargs: Value,
    configurable: Value,
    dependency_overrides: Option<Vec<String>>,
    python_version: String,
    node_install_command: Option<String>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(LangGraph::new(args)?))
}

impl LangGraph {
    fn new(args: AgentArgs) -> Result<LangGraph> {
        let dict = |k: &str| -> Result<Value> {
            match args.kwargs.get(k) {
                None | Some(Value::Null) => Ok(json!({})),
                Some(v @ Value::Object(_)) => Ok(v.clone()),
                Some(_) => bail!("Invalid kwargs for agent 'langgraph':\n  {k}: Input should be a valid dictionary"),
            }
        };
        let dependency_overrides = match args.kwargs.get("dependency_overrides") {
            None | Some(Value::Null) => None,
            Some(Value::Array(a)) if a.iter().all(Value::is_string) => Some(a.iter().map(|v| v.as_str().unwrap().to_string()).collect()),
            Some(_) => bail!("Invalid kwargs for agent 'langgraph':\n  dependency_overrides: Input should be a valid list of strings"),
        };
        let raw = args.kw("project_path").map(PathBuf::from).unwrap_or(std::env::current_dir()?);
        let raw = match raw.strip_prefix("~") {
            Ok(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
            Err(_) => raw,
        };
        let project_path = std::fs::canonicalize(&raw)
            .ok()
            .filter(|p| p.is_dir())
            .ok_or_else(|| anyhow::anyhow!("LangGraph project_path does not exist: {}", raw.display()))?;
        let config = args.kw("config").unwrap_or_else(|| "langgraph.json".into());
        let config_abs = std::fs::canonicalize(project_path.join(&config))
            .map_err(|_| anyhow::anyhow!("LangGraph config file not found: {}", project_path.join(&config).display()))?;
        if !config_abs.starts_with(&project_path) {
            bail!("LangGraph config file must resolve within the LangGraph project: {config}");
        }
        if !config_abs.is_file() {
            bail!("LangGraph config file not found: {}", config_abs.display());
        }
        let lg_config: Value = serde_json::from_str(&std::fs::read_to_string(&config_abs)?)?;
        if !lg_config.is_object() {
            bail!("Expected {} to contain a JSON object", config_abs.display());
        }
        let (graph, graph_path) = select_graph(&lg_config, args.kw("graph").as_deref())?;
        let runtime = graph_runtime(&graph_path)?;
        let (module, _) = split_graph_path(&graph_path)?;
        match std::fs::canonicalize(project_path.join(&module)) {
            Ok(m) if !m.starts_with(&project_path) => bail!("Graph module must resolve within the LangGraph project directory: {module}"),
            Ok(m) if m.is_file() => {}
            _ => bail!("Graph module not found: {}", project_path.join(&module).display()),
        }
        let mut a = LangGraph {
            config: config_abs.strip_prefix(&project_path).unwrap().to_string_lossy().replace('\\', "/"),
            model_kwargs: dict("model_kwargs")?,
            configurable: dict("configurable")?,
            python_version: args.kw("python_version").unwrap_or_else(|| "3.12".into()),
            node_install_command: args.kw("node_install_command"),
            version: args.version_kw(),
            node_version: 22,
            dependency_overrides,
            project_path,
            lg_config,
            graph,
            graph_path,
            runtime,
            args,
        };
        if runtime == "node" {
            let v = a
                .args
                .kwargs
                .get("node_version")
                .filter(|v| !v.is_null())
                .or(a.lg_config.get("node_version").filter(|v| !v.is_null()))
                .cloned()
                .unwrap_or(json!(22));
            a.node_version = match &v {
                Value::Number(n) if matches!(n.as_i64(), Some(20 | 22 | 24)) => n.as_i64().unwrap() as u32,
                Value::String(s) if ["20", "22", "24"].contains(&s.as_str()) => s.parse().unwrap(),
                _ => bail!("node_version must be exactly one of 20, 22, 24"),
            };
            if let Some(reason) = a.plug_n_play_reason() {
                bail!(
                    "LangGraph Node graphs cannot run under Yarn Plug'n'Play ({reason}). The graph runs under `node`, which resolves dependencies from \
                     node_modules; PnP dependencies are reachable only through Yarn's own runtime. Set `nodeLinker: node-modules` in .yarnrc.yml and reinstall."
                );
            }
        }
        Ok(a)
    }

    fn normalized_model(&self) -> Option<String> {
        let m = self.args.model_name.clone()?;
        if m.contains(':') {
            return Some(m);
        }
        Some(m.split_once('/').map(|(p, n)| format!("{p}:{n}")).unwrap_or(m))
    }

    fn package_manager_field(&self) -> String {
        std::fs::read_to_string(self.project_path.join("package.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v["packageManager"].as_str().map(String::from))
            .unwrap_or_default()
    }

    fn yarn_major(&self) -> u64 {
        match self.package_manager_field().split_once('@') {
            Some(("yarn", v)) => v
                .split('.')
                .next()
                .filter(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_digit()))
                .and_then(|m| m.parse().ok())
                .unwrap_or(1),
            _ => 1,
        }
    }

    fn plug_n_play_reason(&self) -> Option<String> {
        if let Ok(rc) = std::fs::read_to_string(self.project_path.join(".yarnrc.yml")) {
            let re = Regex::new(r#"(?m)^\s*nodeLinker\s*:\s*["']?([\w-]+)"#).unwrap();
            if let Some(c) = re.captures(&rc) {
                return (&c[1] == "pnp").then(|| "`.yarnrc.yml` sets `nodeLinker: pnp`".into());
            }
        }
        for m in [".pnp.cjs", ".pnp.data.json"] {
            if self.project_path.join(m).exists() {
                return Some(format!("`{m}` is present"));
            }
        }
        let names_yarn = self.package_manager_field().starts_with("yarn@");
        ((names_yarn || self.project_path.join("yarn.lock").exists()) && self.yarn_major() >= 2)
            .then(|| "Yarn 2+ defaults to Plug'n'Play".into())
    }

    fn has_lockfile(&self, lockfiles: &[&str]) -> bool {
        lockfiles.iter().any(|l| self.project_path.join(l).exists())
    }

    fn manager_install(&self, manager: &str) -> String {
        let (_, locks, mut locked, unlocked) = *NODE_PACKAGE_MANAGERS.iter().find(|m| m.0 == manager).unwrap();
        if manager == "yarn" && self.yarn_major() < 2 {
            locked = "corepack enable && yarn install --frozen-lockfile";
        }
        let install = if self.has_lockfile(locks) { locked } else { unlocked };
        match manager {
            "bun" => format!("npm install -g {} && {install}", pins::BUN.npm_spec(None)),
            _ => install.to_string(),
        }
    }

    fn node_install_command(&self) -> String {
        if let Some(c) = &self.node_install_command {
            return c.clone();
        }
        if let Some((m, v)) = self.package_manager_field().split_once('@')
            && v.starts_with(|c: char| c.is_ascii_digit())
            && NODE_PACKAGE_MANAGERS.iter().any(|x| x.0 == m)
        {
            return self.manager_install(m);
        }
        for (m, locks, _, _) in NODE_PACKAGE_MANAGERS {
            if self.has_lockfile(locks) {
                return self.manager_install(m);
            }
        }
        "npm install".into()
    }

    fn version_command(&self) -> String {
        if self.runtime == "node" {
            let script = "let version; try { version = require('@langchain/langgraph/package.json').version } catch { version = `node ${process.version}` } console.log(version)";
            return format!("{NODE_SHELL_PREFIX}cd {PROJECT_DIR} && node -e {}", quote(script));
        }
        format!("{VENV_DIR}/bin/python -c \"import importlib.metadata; print(importlib.metadata.version('langgraph'))\"")
    }

    fn python_install_command(&self) -> String {
        let deps = serde_json::to_string(&self.dependency_overrides).unwrap();
        let pv = quote(&self.python_version);
        format!(
            "set -euo pipefail; {uv_install}; if [ -f \"$HOME/.local/bin/env\" ]; then . \"$HOME/.local/bin/env\"; fi; \
             uv python install {pv}; uv venv {VENV_DIR} --python {pv} --clear; . {VENV_DIR}/bin/activate; uv pip install {langgraph} {dotenv}; python - <<'PY'\n\
             import json, os, subprocess, sys\n\
             project_dir = {}\n\
             config_name = {}\n\
             dependency_overrides = json.loads({})\n\
             installer = ['uv', 'pip', 'install']\n\
             if 'UV_PRERELEASE' not in os.environ:\n    installer.append('--prerelease=if-necessary')\n\
             config_path = os.path.join(project_dir, config_name)\n\
             with open(config_path) as f:\n    config = json.load(f)\n\
             source = config.get('source')\n\
             if dependency_overrides is not None:\n    for dep in dependency_overrides:\n        subprocess.check_call([*installer, dep])\n\
             elif isinstance(source, dict) and source.get('kind') == 'uv':\n    root = os.path.join(project_dir, source.get('root', '.'))\n    subprocess.check_call([*installer, '-e', root])\n\
             else:\n    for dep in config.get('dependencies', []):\n        dep_path = os.path.join(project_dir, dep) if isinstance(dep, str) else None\n        \
             if dep_path and os.path.exists(dep_path):\n            if os.path.isfile(dep_path) and os.path.basename(dep_path).startswith('requirements'):\n                \
             subprocess.check_call([*installer, '-r', dep_path])\n            else:\n                subprocess.check_call([*installer, '-e', dep_path])\n        \
             elif isinstance(dep, str):\n            subprocess.check_call([*installer, dep])\n\
             PY",
            single_quoted(PROJECT_DIR),
            single_quoted(&self.config),
            single_quoted(&deps),
            uv_install = uv_install(None),
            langgraph = pins::LANGGRAPH.requirement(),
            dotenv = pins::PYTHON_DOTENV.requirement()
        )
    }

    fn configurable_json(&self) -> String {
        let mut c = self.configurable.clone();
        if !self.args.mcp_servers.is_empty() && c.get("mcp_servers").is_none() {
            c["mcp_servers"] = json!(
                self.args
                    .mcp_servers
                    .iter()
                    .map(|s| json!({"name": s.name, "transport": s.transport(), "url": s.url, "command": s.command, "args": s.args}))
                    .collect::<Vec<_>>()
            );
        }
        c.to_string()
    }

    fn runtime_env(&self, model: Option<&str>, model_kwargs_json: &str) -> BTreeMap<String, String> {
        let session = self.args.logs_dir.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut env = BTreeMap::from([
            ("HBENCH_SESSION_ID".to_string(), session),
            ("HBENCH_MODEL_KWARGS_JSON".to_string(), model_kwargs_json.to_string()),
        ]);
        if let Some(m) = model {
            env.insert("HBENCH_MODEL".into(), m.into());
        }
        let lookup = |k: &str| self.args.extra_env.get(k).cloned().or_else(|| std::env::var(k).ok());
        for k in FORWARDED_ENV_VARS {
            if let Some(v) = lookup(k) {
                env.entry(k.into()).or_insert(v);
            }
        }
        env
    }

    fn node_loader(&self) -> Result<String> {
        let env = self.args.extra_env.get("LANGGRAPH_NODE_LOADER").cloned().or_else(|| std::env::var("LANGGRAPH_NODE_LOADER").ok());
        let loader = match env {
            Some(v) => json!(v),
            None => self.lg_config.get("node_loader").cloned().unwrap_or(json!(DEFAULT_NODE_LOADER)),
        };
        let Some(loader) = loader.as_str().filter(|s| !s.is_empty()) else { bail!("node_loader must be a non-empty string") };
        if loader_package_re().is_match(loader) {
            return Ok(loader.into());
        }
        let escape = "node_loader path must resolve within the staged project or the LangGraph runner tools directory";
        if loader.starts_with('/') {
            let n = normpath(loader);
            if ![PROJECT_DIR, NODE_TOOLS_DIR].iter().any(|r| n == *r || n.starts_with(&format!("{r}/"))) {
                bail!("{escape}");
            }
            return Ok(n);
        }
        let resolved = std::fs::canonicalize(self.project_path.join(loader)).unwrap_or_else(|_| self.project_path.join(loader));
        if !resolved.starts_with(&self.project_path) {
            bail!("{escape}");
        }
        if !resolved.is_file() {
            bail!("node_loader must resolve to a file");
        }
        Ok(format!("{PROJECT_DIR}/{}", resolved.strip_prefix(&self.project_path).unwrap().to_string_lossy()))
    }

    fn runner_args(&self, model: Option<&str>, model_kwargs_json: &str, configurable_json: &str) -> Vec<String> {
        let mut a: Vec<String> = [
            "--project-dir",
            PROJECT_DIR,
            "--graph",
            &self.graph,
            "--graph-path",
            &self.graph_path,
            "--instruction-file",
            INSTRUCTION_PATH,
            "--result-path",
            "/logs/agent/result.json",
            "--output-path",
            "/logs/agent/langgraph.txt",
            "--summary-path",
            "/logs/agent/summary.json",
        ]
        .map(String::from)
        .to_vec();
        if self.runtime == "node" {
            a.extend(["--runner-tools-dir".into(), NODE_TOOLS_DIR.into()]);
        }
        if let Some(m) = model {
            a.extend(["--model".into(), m.into()]);
        }
        a.extend(["--model-kwargs-json".into(), model_kwargs_json.into(), "--configurable-json".into(), configurable_json.into()]);
        a
    }

    fn run_command(&self, model: Option<&str>, model_kwargs_json: &str, configurable_json: &str) -> Result<String> {
        let join = |v: &[String]| v.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ");
        let mut rest = vec![];
        let runner = if self.runtime == "node" {
            let loader = self.node_loader()?;
            rest.push(NODE_RUNNER_PATH.to_string());
            rest.extend(self.runner_args(model, model_kwargs_json, configurable_json));
            if loader_package_re().is_match(&loader) {
                let expr = format!("console.log(import.meta.resolve({}))", serde_json::to_string(&loader)?);
                format!(
                    "{NODE_SHELL_PREFIX}_lg_loader=$(cd {PROJECT_DIR} && node --input-type=module -e {}) && node --import \"$_lg_loader\" {}",
                    quote(&expr),
                    join(&rest)
                )
            } else {
                format!("{NODE_SHELL_PREFIX}{}", join(&[&["node".to_string(), "--import".into(), loader], &rest[..]].concat()))
            }
        } else {
            rest.extend([format!("{VENV_DIR}/bin/python"), PY_RUNNER_PATH.into()]);
            rest.extend(self.runner_args(model, model_kwargs_json, configurable_json));
            join(&rest)
        };
        Ok(format!("{runner} 2>&1 | stdbuf -oL tee /logs/agent/langgraph-run.log"))
    }
}

/// Apply the runner's `summary.json` (answer + token usage) to the context.
pub fn apply_summary(raw: &str, ctx: &mut AgentContext) {
    if raw.chars().count() > MAX_SUMMARY_BYTES {
        return;
    }
    let Ok(s) = serde_json::from_str::<Value>(raw) else { return };
    if !s.is_object() {
        return;
    }
    if let Some(a) = s["answer_written"].as_str() {
        let mut m = ctx.metadata.take().filter(Value::is_object).unwrap_or_else(|| json!({}));
        m["answer_written"] = json!(a);
        ctx.metadata = Some(m);
    }
    let u = &s["usage"];
    if let Some(n) = u["input_tokens"].as_i64() {
        ctx.n_input_tokens = Some(n);
    }
    if let Some(n) = u["output_tokens"].as_i64() {
        ctx.n_output_tokens = Some(n);
    }
    if let Some(n) = u["cache_read_tokens"].as_i64() {
        ctx.n_cache_tokens = Some(n);
    }
}

#[async_trait::async_trait]
impl Agent for LangGraph {
    fn name(&self) -> &str {
        "langgraph"
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        installed_dir(cx).await?;
        let none = BTreeMap::new();
        let node = self.runtime == "node";
        let (runner, runner_path, runtime_dir) =
            if node { (NODE_RUNNER, NODE_RUNNER_PATH, NODE_TOOLS_DIR) } else { (PY_RUNNER, PY_RUNNER_PATH, VENV_DIR) };
        let _ = std::fs::write(self.args.logs_dir.join(runner_path.rsplit('/').next().unwrap()), runner);
        let staged = self.args.logs_dir.join("langgraph_project");
        let _ = std::fs::remove_dir_all(&staged);
        copy_project(&self.project_path, &staged)?;
        ensure_system_dependencies(cx, &["curl"]).await?;
        let user = quote(cx.user.as_deref().unwrap_or("root"));
        exec_root(
            cx,
            &format!("rm -rf {PROJECT_DIR} {PY_RUNNER_PATH} {NODE_RUNNER_PATH} {runtime_dir} && mkdir -p {PROJECT_DIR} {runtime_dir} && chown -R {user}:{user} {runtime_dir}"),
            &none,
        )
        .await?;
        cx.env.upload_dir(&staged, PROJECT_DIR).await?;
        upload_asset(cx, runner, runner_path).await?;
        exec_root(cx, &format!("chown -R {user}:{user} {PROJECT_DIR}"), &none).await?;
        if node {
            let tools = [pins::TSX, pins::LANGSMITH].map(|p| p.npm_spec(None)).join(" ");
            exec_agent(
                cx,
                &format!(
                    "set -euo pipefail; if command -v ldd >/dev/null 2>&1 && {{ ldd --version 2>&1 || true; }} | grep -qi musl; then \
                     echo 'Node provisioning does not support musl/Alpine; use a glibc-based image.' >&2; exit 1; fi; \
                     {} && npm install --prefix {NODE_TOOLS_DIR} --no-save {tools}",
                    nvm_node_install_snippet(self.node_version)
                ),
                &none,
            )
            .await?;
            exec_agent(cx, &format!("cd {PROJECT_DIR} && {NODE_SHELL_PREFIX}{}", self.node_install_command()), &none).await?;
        } else {
            exec_agent(cx, &self.python_install_command(), &none).await?;
        }
        if self.version.is_none()
            && let Ok(r) = cx.exec(&self.version_command(), &none, None).await
            && r.code == 0
            && !r.stdout.is_empty()
        {
            self.version = Some(r.stdout.trim().to_string());
        }
        Ok(())
    }

    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let instruction = self.args.render_instruction(instruction)?;
        let _ = std::fs::write(cx.logs_dir.join("instruction.txt"), &instruction);
        upload_asset(cx, &instruction, INSTRUCTION_PATH).await?;
        let model = self.normalized_model();
        let mk = self.model_kwargs.to_string();
        let cmd = self.run_command(model.as_deref(), &mk, &self.configurable_json())?;
        exec_agent(cx, &cmd, &self.runtime_env(model.as_deref(), &mk)).await?;
        let mut meta = ctx.metadata.take().filter(Value::is_object).unwrap_or_else(|| json!({}));
        meta["langgraph_graph"] = json!(self.graph);
        meta["langgraph_config"] = json!(self.config);
        meta["langgraph_project_path"] = json!(self.project_path.display().to_string());
        ctx.metadata = Some(meta);
        let local = cx.logs_dir.join("summary.json");
        if cx.env.download_file(crate::task::MAIN, "/logs/agent/summary.json", &local).await.is_ok()
            && let Ok(raw) = std::fs::read_to_string(&local)
        {
            apply_summary(&raw, ctx);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::installed::common::test_args as args;

    fn project(files: &[(&str, &str)]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hbench-lg-{}", uuid::Uuid::new_v4()));
        for (p, c) in files {
            let f = d.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, c).unwrap();
        }
        d
    }

    fn lg(files: &[(&str, &str)], kwargs: Value, model: Option<&str>) -> Result<LangGraph> {
        let p = project(files);
        let mut kw = kwargs;
        kw["project_path"] = json!(p.display().to_string());
        LangGraph::new(args(kw, &[], model))
    }

    const PY: (&str, &str) = ("langgraph.json", r#"{"graphs": {"agent": "./agent.py:graph"}, "dependencies": ["."]}"#);

    #[test]
    fn graph_specs() {
        assert_eq!(graph_runtime("./agent.py:graph.child").unwrap(), "python");
        assert_eq!(graph_runtime("./agent.ts:graph$").unwrap(), "node");
        assert!(graph_runtime("./agent.py:graph$").is_err());
        assert!(graph_runtime("./agent.rb:graph").is_err());
        assert!(graph_runtime("./a.py:b:c").is_err());
        assert!(select_graph(&json!({"graphs": {"a": "x", "b": "y"}}), None).unwrap_err().to_string().contains("Available graphs: a, b"));
        assert!(select_graph(&json!({"graphs": {}}), None).is_err());
        assert_eq!(select_graph(&json!({"graphs": {"a": {"path": "./a.py:g"}}}), None).unwrap(), ("a".into(), "./a.py:g".into()));
    }

    #[test]
    fn python_project() {
        assert!(lg(&[("langgraph.json", r#"{"graphs": {"agent": "./missing.py:graph"}}"#)], json!({}), None).is_err());
        assert!(lg(&[PY, ("agent.py", "")], json!({"config": "../langgraph.json"}), None).is_err());
        let a = lg(
            &[PY, ("agent.py", ""), (".env", "SECRET"), ("node_modules/x.js", "")],
            json!({"model_kwargs": {"temperature": 0}, "python_version": 3.1}),
            Some("anthropic/claude-sonnet-4-5"),
        )
        .unwrap();
        assert_eq!((a.graph.as_str(), a.runtime, a.python_version.as_str()), ("agent", "python", "3.1"));
        assert_eq!(a.normalized_model().as_deref(), Some("anthropic:claude-sonnet-4-5"));
        let cmd = a.run_command(Some("anthropic:claude-sonnet-4-5"), "{}", "{}").unwrap();
        assert!(cmd.starts_with("/opt/hbench-langgraph-venv/bin/python /installed-agent/langgraph_runner.py --project-dir /installed-agent/langgraph-project --graph agent --graph-path ./agent.py:graph"));
        assert!(cmd.contains("--model anthropic:claude-sonnet-4-5 --model-kwargs-json '{}' --configurable-json '{}' 2>&1 | stdbuf -oL tee /logs/agent/langgraph-run.log"));
        assert!(a.python_install_command().contains("config_name = 'langgraph.json'\ndependency_overrides = json.loads('null')\n"));
        let staged = std::env::temp_dir().join(format!("hbench-lg-stage-{}", uuid::Uuid::new_v4()));
        copy_project(&a.project_path, &staged).unwrap();
        assert!(staged.join("agent.py").exists() && !staged.join(".env").exists() && !staged.join("node_modules").exists());
        assert_eq!(single_quoted("it's"), "\"it's\"");
    }

    #[test]
    fn node_project() {
        let ts = ("langgraph.json", r#"{"graphs": {"typescript": "./agent.ts:graph"}, "node_version": "20"}"#);
        let a = lg(&[ts, ("agent.ts", ""), ("package.json", r#"{"packageManager": "yarn@1.22.22"}"#), ("yarn.lock", "")], json!({}), None)
            .unwrap();
        assert_eq!((a.runtime, a.node_version), ("node", 20));
        assert_eq!(a.node_install_command(), "corepack enable && yarn install --frozen-lockfile");
        assert_eq!(a.node_loader().unwrap(), DEFAULT_NODE_LOADER);
        let cmd = a.run_command(None, "{}", "{}").unwrap();
        assert!(cmd.contains("node --import /opt/hbench-langgraph-node-tools/node_modules/tsx/dist/loader.mjs /installed-agent/langgraphjs_runner.mjs --project-dir"));
        assert!(cmd.contains("--runner-tools-dir /opt/hbench-langgraph-node-tools"));
        assert!(lg(&[ts, ("agent.ts", "")], json!({"node_version": 21}), None).is_err());
        assert!(
            lg(&[ts, ("agent.ts", ""), ("package.json", r#"{"packageManager": "yarn@4.1.0"}"#), ("yarn.lock", "")], json!({}), None)
                .is_err()
        );
        assert_eq!(
            lg(
                &[ts, ("agent.ts", ""), ("package.json", r#"{"packageManager": "npm@not-a-version"}"#), ("pnpm-lock.yaml", "")],
                json!({}),
                None
            )
            .unwrap()
            .node_install_command(),
            "corepack enable && pnpm install --frozen-lockfile"
        );
        assert_eq!(lg(&[ts, ("agent.ts", "")], json!({}), None).unwrap().node_install_command(), "npm install");
        let pkg = lg(
            &[("langgraph.json", r#"{"graphs": {"g": "./agent.ts:graph"}, "node_loader": "conditional-loader"}"#), ("agent.ts", "")],
            json!({}),
            None,
        )
        .unwrap();
        assert!(pkg.run_command(None, "{}", "{}").unwrap().contains("console.log(import.meta.resolve(\"conditional-loader\"))"));
        let esc = lg(
            &[("langgraph.json", r#"{"graphs": {"g": "./agent.ts:graph"}, "node_loader": "/etc/x.mjs"}"#), ("agent.ts", "")],
            json!({}),
            None,
        )
        .unwrap();
        assert!(esc.node_loader().is_err());
    }

    #[test]
    fn summary() {
        let mut ctx = AgentContext::default();
        apply_summary(r#"{"answer_written": "42", "usage": {"input_tokens": 10, "output_tokens": 3, "cache_read_tokens": 4}}"#, &mut ctx);
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens, ctx.n_cache_tokens), (Some(10), Some(3), Some(4)));
        assert_eq!(ctx.metadata.unwrap()["answer_written"], "42");
        let mut empty = AgentContext::default();
        apply_summary("[]", &mut empty);
        assert!(empty.is_empty());
    }

    #[test]
    fn install_heredoc_indentation() {
        let a = lg(&[PY, ("agent.py", "")], json!({"dependency_overrides": ["a==1"]}), None).unwrap();
        let c = a.python_install_command();
        assert!(c.contains("dependency_overrides = json.loads('[\"a==1\"]')\n"));
        assert!(c.contains(
            "\n        if dep_path and os.path.exists(dep_path):\n            if os.path.isfile(dep_path) and os.path.basename(dep_path).startswith('requirements'):\n                subprocess.check_call([*installer, '-r', dep_path])\n"
        ));
        assert!(c.ends_with("            subprocess.check_call([*installer, dep])\nPY"));
    }
}
