//! Terminal-Bench (v1) -> task migration (`tasks migrate`).
//! Output is deterministic and byte-stable: task.toml is written in hbench's fixed TOML layout, and
//! docker-compose files in block-style YAML (indent 2, width 80, ASCII-only, keys in source order).

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde_yaml_ng::{Mapping, Value};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use yaml::to_yaml;

mod yaml;

/// `--cpus/--memory-mb/--storage-mb/--gpus` environment overrides.
#[derive(Debug, Default, Clone, Copy)]
pub struct Overrides {
    pub cpus: Option<i64>,
    pub memory_mb: Option<i64>,
    pub storage_mb: Option<i64>,
    pub gpus: Option<i64>,
}

const REWARD_LOGGING_SUFFIX: &str = "
_EXIT_CODE=$?
if [ $_EXIT_CODE -eq 0 ]; then
    echo 1 > /logs/verifier/reward.txt
else
    echo 0 > /logs/verifier/reward.txt
fi
exit $_EXIT_CODE
";

const DOCKERFILE_FIELDS: &[&str] = &["environment", "working_dir", "expose", "entrypoint", "platform"];
const IGNORED_FIELDS: &[&str] = &["build", "image", "container_name", "command", "volumes", "environment"];
const TBENCH_DEFAULT_VOLUMES: &[&str] =
    &["${T_BENCH_TASK_LOGS_PATH}:${T_BENCH_CONTAINER_LOGS_PATH}", "${T_BENCH_TASK_AGENT_LOGS_PATH}:${T_BENCH_CONTAINER_AGENT_LOGS_PATH}"];
const TBENCH_DEFAULT_ENV: &str = "TEST_DIR=${T_BENCH_TEST_DIR}";
const MAIN_REPLACES: &[&str] = &["build", "image", "container_name", "command", "environment", "volumes", "deploy"];

/// Migrate a single Terminal-Bench task dir (has task.yaml) into `output/<name>`,
/// or every task dir under a parent dir. Returns the created task dirs.
/// Single-task errors propagate; in parent mode failures are printed and skipped.
pub fn migrate(input: &Path, output: &Path, overrides: Overrides) -> Result<Vec<PathBuf>> {
    let input = fs::canonicalize(input).with_context(|| format!("Input path does not exist: {}", input.display()))?;
    let output = fs::canonicalize(output).or_else(|_| std::path::absolute(output))?;
    if input.join("task.yaml").exists() {
        let name = input.file_name().ok_or_else(|| anyhow!("bad input path"))?;
        return Ok(vec![map_task(&input, &output.join(name), &overrides)?]);
    }
    if !input.is_dir() {
        bail!("Invalid source directory: {}", input.display());
    }
    fs::create_dir_all(&output)?;
    let mut mapped = Vec::new();
    // Sorted for determinism (not filesystem order).
    for task_dir in sorted_entries(&input)? {
        if !task_dir.is_dir() || !task_dir.join("task.yaml").exists() {
            continue;
        }
        let name = file_name(&task_dir);
        match map_task(&task_dir, &output.join(&name), &overrides) {
            Ok(p) => mapped.push(p),
            Err(e) => println!("Failed to map task {name}: {e:#}"),
        }
    }
    Ok(mapped)
}

fn map_task(src: &Path, target: &Path, ov: &Overrides) -> Result<PathBuf> {
    if target.exists() {
        bail!("Task directory already exists: {}", target.display());
    }
    let name = file_name(src);
    if src.join("solution.yaml").exists() {
        bail!("Task {name} uses solution.yaml which is not supported. Please convert it to solution.sh.");
    }
    let cfg = TbConfig::parse(&read_text(&src.join("task.yaml"))?)?;

    fs::create_dir_all(target)?;
    let task_dir = fs::canonicalize(target)?;
    let env_dir = task_dir.join("environment");
    let tests_dir = task_dir.join("tests");
    fs::create_dir_all(&env_dir)?;
    fs::create_dir_all(&tests_dir)?;

    let mut handled = process_docker_compose(src, &env_dir, &name)?;
    handled.extend(["task.yaml", "solution.sh", "run-tests.sh", "tests"].map(String::from));

    if src.join("solution.sh").exists() {
        let sol = task_dir.join("solution");
        fs::create_dir_all(&sol)?;
        fs::copy(src.join("solution.sh"), sol.join("solve.sh"))?;
        chmod_x(&sol.join("solve.sh"))?;
    }
    if src.join("tests").is_dir() {
        copy_tree(&src.join("tests"), &tests_dir)?;
    }
    let mut test_sh = read_text(&src.join("run-tests.sh"))?;
    if !test_sh.ends_with('\n') {
        test_sh.push('\n');
    }
    test_sh.push_str(REWARD_LOGGING_SUFFIX);
    let test_path = tests_dir.join("test.sh");
    fs::write(&test_path, test_sh.replace("$TEST_DIR", "/tests"))?;
    chmod_x(&test_path)?;

    copy_remaining_files(src, &env_dir, &handled)?;

    let mut instruction = cfg.instruction.clone();
    if !instruction.ends_with('\n') {
        instruction.push('\n');
    }
    fs::write(task_dir.join("instruction.md"), instruction)?;
    fs::write(task_dir.join("task.toml"), cfg.to_toml(ov)?)?;
    Ok(task_dir)
}

struct TbConfig {
    instruction: String,
    author_name: String,
    author_email: String,
    difficulty: String,
    category: String,
    tags: Vec<String>,
    estimated_duration_sec: Option<f64>,
    expert_time_estimate_min: Option<f64>,
    junior_time_estimate_min: Option<f64>,
    max_agent_timeout_sec: f64,
    max_test_timeout_sec: f64,
}

impl TbConfig {
    /// Parses and validates task.yaml (unknown keys ignored).
    fn parse(text: &str) -> Result<Self> {
        let v: Value = serde_yaml_ng::from_str(text).context("task.yaml")?;
        let m = v.as_mapping().ok_or_else(|| anyhow!("task.yaml: expected a mapping"))?;
        let s = |k: &str, d: Option<&str>| -> Result<String> {
            match (m.get(k), d) {
                (None, Some(d)) => Ok(d.to_string()),
                (Some(Value::String(s)), _) => Ok(s.clone()),
                (None, None) => bail!("task.yaml: {k}: required"),
                _ => bail!("task.yaml: {k}: must be a string"),
            }
        };
        let f = |k: &str| -> Result<Option<f64>> {
            match m.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Number(n)) => Ok(n.as_f64()),
                Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| anyhow!("task.yaml: {k}: must be a number")),
                _ => bail!("task.yaml: {k}: must be a number"),
            }
        };
        let req = |k: &str, d: f64| -> Result<f64> {
            if matches!(m.get(k), Some(Value::Null)) {
                bail!("task.yaml: {k}: must be a number");
            }
            Ok(f(k)?.unwrap_or(d))
        };
        let difficulty = s("difficulty", Some("unknown"))?;
        if !["easy", "medium", "hard", "unknown"].contains(&difficulty.as_str()) {
            bail!("task.yaml: difficulty: must be 'easy', 'medium', 'hard' or 'unknown'");
        }
        let tags = match m.get("tags") {
            None => vec![],
            Some(Value::Sequence(items)) => items
                .iter()
                .map(|t| t.as_str().map(String::from).ok_or_else(|| anyhow!("task.yaml: tags: must be a string")))
                .collect::<Result<_>>()?,
            _ => bail!("task.yaml: tags: must be a list"),
        };
        Ok(Self {
            instruction: s("instruction", None)?,
            author_name: s("author_name", Some("unknown"))?,
            author_email: s("author_email", Some("unknown"))?,
            difficulty,
            category: s("category", Some("software_engineering"))?,
            tags,
            estimated_duration_sec: f("estimated_duration_sec")?,
            expert_time_estimate_min: f("expert_time_estimate_min")?,
            junior_time_estimate_min: f("junior_time_estimate_min")?,
            max_agent_timeout_sec: req("max_agent_timeout_sec", 360.0)?,
            max_test_timeout_sec: req("max_test_timeout_sec", 60.0)?,
        })
    }

    /// The task.toml text for this config.
    fn to_toml(&self, ov: &Overrides) -> Result<String> {
        let mut o = String::from("schema_version = \"1.4\"\nartifacts = []\n\n[metadata]\n");
        o += &format!("author_name = {}\n", toml_str(&self.author_name)?);
        o += &format!("author_email = {}\n", toml_str(&self.author_email)?);
        o += &format!("difficulty = {}\n", toml_str(&self.difficulty)?);
        o += &format!("category = {}\n", toml_str(&self.category)?);
        let mut tags = String::from("[");
        for t in &self.tags {
            tags += &format!(" {},", toml_str(t)?);
        }
        o += &format!("tags = {tags}]\n");
        for (k, v) in [
            ("estimated_duration_sec", self.estimated_duration_sec),
            ("expert_time_estimate_min", self.expert_time_estimate_min),
            ("junior_time_estimate_min", self.junior_time_estimate_min),
        ] {
            if let Some(v) = v {
                o += &format!("{k} = {}\n", toml_float(v));
            }
        }
        o += &format!("\n[verifier]\ntimeout_sec = {}\ncollect = []\n\n[verifier.env]\n", toml_float(self.max_test_timeout_sec));
        o += &format!("\n[agent]\ntimeout_sec = {}\n", toml_float(self.max_agent_timeout_sec));
        o += "\n[environment]\nnetwork_mode = \"public\"\nbuild_timeout_sec = 600.0\nos = \"linux\"\n";
        for (k, v) in [("cpus", ov.cpus), ("memory_mb", ov.memory_mb), ("storage_mb", ov.storage_mb), ("gpus", ov.gpus)] {
            if let Some(v) = v {
                o += &format!("{k} = {v}\n");
            }
        }
        o += "mcp_servers = []\n\n[environment.env]\n\n[solution.env]\n";
        Ok(o)
    }
}

fn process_docker_compose(src: &Path, env_dir: &Path, task: &str) -> Result<HashSet<String>> {
    let compose_path = src.join("docker-compose.yaml");
    if !compose_path.exists() {
        bail!("docker-compose.yaml not found: {}", compose_path.display());
    }
    let mut handled: HashSet<String> = HashSet::from(["docker-compose.yaml".to_string()]);
    let mut data: Value = serde_yaml_ng::from_str(&read_text(&compose_path)?).context("docker-compose.yaml")?;
    data.apply_merge()?;
    let root = data.as_mapping().ok_or_else(|| anyhow!("docker-compose.yaml: expected a mapping"))?;
    let services = match root.get("services") {
        Some(Value::Mapping(m)) if !m.is_empty() => m,
        None | Some(Value::Null) | Some(Value::Mapping(_)) => return Ok(handled),
        Some(Value::String(s)) if s.is_empty() => return Ok(handled),
        Some(Value::Sequence(s)) if s.is_empty() => return Ok(handled),
        Some(_) => bail!("docker-compose.yaml: services must be a mapping"),
    };
    let (main_name, main) = main_service(services)?;
    let context = build_context(main);
    handled.insert(if context != "." { context.clone() } else { "Dockerfile".into() });

    if let Some(df) = copy_build_context(src, &context, env_dir)? {
        let content = read_text(&df)?;
        if ["COPY tests", "COPY ./tests", "ADD tests", "ADD ./tests"].iter().any(|p| content.contains(p))
            && src.join("tests").is_dir()
            && !env_dir.join("tests").exists()
        {
            copy_tree(&src.join("tests"), &env_dir.join("tests"))?;
        }
        if ["COPY solution.sh", "COPY ./solution.sh", "ADD solution.sh", "ADD ./solution.sh"].iter().any(|p| content.contains(p))
            && src.join("solution.sh").exists()
            && !env_dir.join("solution.sh").exists()
        {
            fs::copy(src.join("solution.sh"), env_dir.join("solution.sh"))?;
        }
        append_to_dockerfile(&df, main, task)?;
    }

    if !can_collapse_to_dockerfile(root, services)? {
        for (name, svc) in services {
            if *name == main_name {
                continue;
            }
            let svc = svc.as_mapping().ok_or_else(|| anyhow!("service {} must be a mapping", yaml_text(name)))?;
            let ctx = build_context(svc);
            if ctx != "." && src.join(&ctx).is_dir() {
                let dst = env_dir.join(&ctx);
                if !dst.exists() {
                    copy_tree(&src.join(&ctx), &dst)?;
                }
                handled.insert(ctx);
            }
        }
        write_compose(root, services, &main_name, main, &env_dir.join("docker-compose.yaml"), task)?;
    }
    Ok(handled)
}

fn main_service(services: &Mapping) -> Result<(Value, &Mapping)> {
    let name = if services.contains_key("client") {
        Value::from("client")
    } else {
        services.keys().next().cloned().ok_or_else(|| anyhow!("no services"))?
    };
    let svc = services[&name].as_mapping().ok_or_else(|| anyhow!("service {} must be a mapping", yaml_text(&name)))?;
    Ok((name, svc))
}

fn build_context(svc: &Mapping) -> String {
    match svc.get("build") {
        Some(Value::Mapping(b)) => b.get("context").map(yaml_text).unwrap_or_else(|| ".".into()),
        Some(Value::String(s)) => s.clone(),
        _ => ".".into(),
    }
}

fn can_collapse_to_dockerfile(root: &Mapping, services: &Mapping) -> Result<bool> {
    if services.len() != 1 {
        return Ok(false);
    }
    if root.keys().any(|k| !matches!(k.as_str(), Some("services" | "version"))) {
        return Ok(false);
    }
    let Some(svc) = services.values().next().and_then(Value::as_mapping) else {
        bail!("compose service must be a mapping");
    };
    for (k, v) in svc {
        let k = yaml_text(k);
        if !DOCKERFILE_FIELDS.contains(&k.as_str()) && !IGNORED_FIELDS.contains(&k.as_str()) {
            return Ok(false);
        }
        // Only the default volumes: every entry is in TBENCH_DEFAULT_VOLUMES.
        if k == "volumes" {
            let default = |x: &Value| x.as_str().is_some_and(|s| TBENCH_DEFAULT_VOLUMES.contains(&s));
            let ok = match v {
                Value::Sequence(items) => {
                    if items.iter().any(|i| matches!(i, Value::Mapping(_) | Value::Sequence(_))) {
                        bail!("volumes: entries must be strings");
                    }
                    items.iter().all(default)
                }
                Value::String(s) => s.is_empty(),
                Value::Mapping(m) => m.keys().all(default),
                _ => bail!("volumes: must be a list or a mapping"),
            };
            if !ok {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Copy the main service build context; returns the (last found) Dockerfile path.
fn copy_build_context(src: &Path, context: &str, env_dir: &Path) -> Result<Option<PathBuf>> {
    let mut dockerfile = None;
    if context != "." {
        let ctx = src.join(context);
        if ctx.is_dir() {
            for item in walk_files(&ctx)? {
                let target = env_dir.join(item.strip_prefix(&ctx)?);
                fs::create_dir_all(target.parent().unwrap())?;
                fs::copy(&item, &target)?;
                if target.file_name().is_some_and(|n| n == "Dockerfile") {
                    dockerfile = Some(target);
                }
            }
        }
    } else if src.join("Dockerfile").exists() {
        let target = env_dir.join("Dockerfile");
        fs::copy(src.join("Dockerfile"), &target)?;
        dockerfile = Some(target);
    }
    Ok(dockerfile)
}

fn dockerfile_additions(svc: &Mapping) -> Vec<String> {
    let mut add = vec!["ENV TEST_DIR=/tests".to_string()];
    match svc.get("environment") {
        Some(Value::Sequence(items)) => {
            for it in items {
                if it.as_str() != Some(TBENCH_DEFAULT_ENV) {
                    // "KEY=VALUE" -> "ENV KEY=VALUE"; bare "KEY" -> "ENV KEY": same text either way.
                    add.push(format!("ENV {}", yaml_text(it)));
                }
            }
        }
        Some(Value::Mapping(m)) => {
            for (k, v) in m {
                // `KEY:` with no value takes the variable from the shell running compose; a Dockerfile has no such shell.
                let value = if v.is_null() { String::new() } else { yaml_text(v) };
                let s = format!("{}={value}", yaml_text(k));
                if s != TBENCH_DEFAULT_ENV {
                    add.push(format!("ENV {s}"));
                }
            }
        }
        _ => {}
    }
    if let Some(wd) = svc.get("working_dir") {
        let wd = yaml_text(wd);
        if !wd.contains("${") {
            add.push(format!("WORKDIR {wd}"));
        }
    }
    match svc.get("expose") {
        Some(Value::Sequence(ports)) => add.extend(ports.iter().map(|p| format!("EXPOSE {}", yaml_text(p)))),
        Some(p) => add.push(format!("EXPOSE {}", yaml_text(p))),
        None => {}
    }
    match svc.get("entrypoint") {
        Some(ep @ Value::Sequence(_)) => add.push(format!("ENTRYPOINT {}", yaml_json(ep))),
        Some(ep) => add.push(format!("ENTRYPOINT {}", yaml_json(&Value::Sequence(vec![ep.clone()])))),
        None => {}
    }
    add
}

static PLATFORM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^(FROM\s+)--platform=\S+\s+(.+)$").unwrap());

fn append_to_dockerfile(path: &Path, svc: &Mapping, task: &str) -> Result<()> {
    let additions = dockerfile_additions(svc);
    let mut content = read_text(path)?;
    if let Some(p) = svc.get("platform") {
        let platform = yaml_text(p);
        let mut count = PLATFORM_RE.find_iter(&content).count();
        if count > 0 {
            content =
                PLATFORM_RE.replace_all(&content, |c: &regex::Captures| format!("{}--platform={platform} {}", &c[1], &c[2])).into_owned();
        } else {
            (content, count) = add_platform(&content, &platform);
        }
        if count > 0 {
            println!("[{task}] Set --platform={platform} on FROM");
        }
    }
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content += "\n# Mapped from docker-compose.yaml\n";
    content += &(additions.join("\n") + "\n");
    let non_default: Vec<String> = additions.iter().filter(|a| *a != "ENV TEST_DIR=/tests").map(|a| str_literal(a)).collect();
    if !non_default.is_empty() {
        println!("[{task}] Moved to Dockerfile: [{}]", non_default.join(", "));
    }
    fs::write(path, content)?;
    Ok(())
}

/// Prefixes `--platform=P` to every `FROM` line that lacks one (multiline `^FROM\s+(?!--platform)(.+)$`),
/// returning the new text and the number of lines changed.
/// The regex crate has no lookahead, so the backtracking is done by hand.
fn add_platform(content: &str, platform: &str) -> (String, usize) {
    let c: Vec<char> = content.chars().collect();
    let (mut out, mut count, mut i) = (String::new(), 0, 0);
    let mut line_start = true;
    while i < c.len() {
        if line_start && c[i..].starts_with(&['F', 'R', 'O', 'M']) {
            let ws_start = i + 4;
            let mut ws_end = ws_start;
            while ws_end < c.len() && c[ws_end].is_whitespace() {
                ws_end += 1;
            }
            // \s+ greedy, backtracking until the lookahead passes and `.+$` matches.
            let mut found = None;
            for g in (ws_start + 1..=ws_end).rev() {
                let rest = &c[g..];
                if rest.starts_with(&"--platform".chars().collect::<Vec<_>>()) {
                    continue;
                }
                let len = rest.iter().take_while(|&&ch| ch != '\n').count();
                if len > 0 {
                    found = Some((g, g + len));
                    break;
                }
            }
            if let Some((g, end)) = found {
                out += &format!("FROM --platform={platform} {}", c[g..end].iter().collect::<String>());
                count += 1;
                i = end;
                line_start = false;
                continue;
            }
        }
        line_start = c[i] == '\n';
        out.push(c[i]);
        i += 1;
    }
    (out, count)
}

fn write_compose(root: &Mapping, services: &Mapping, main_name: &Value, main: &Mapping, target: &Path, task: &str) -> Result<()> {
    println!("[{task}] Cannot collapse to Dockerfile, writing docker-compose.yaml");
    let mut converted = Mapping::new();
    converted.insert("services".into(), Value::Mapping(Mapping::new()));
    for (k, v) in root {
        if !matches!(k.as_str(), Some("services" | "version")) {
            converted.insert(k.clone(), v.clone());
        }
    }
    let mut new_main: Mapping = serde_yaml_ng::from_str(
        "build:\n  context: ${CONTEXT_DIR}\nimage: ${MAIN_IMAGE_NAME}\ncommand: [sh, -c, sleep infinity]\n\
         environment:\n- TEST_DIR=${TEST_DIR}\ndeploy:\n  resources:\n    limits:\n      cpus: ${CPUS}\n      memory: ${MEMORY}\n",
    )?;
    for (k, v) in main {
        if !k.as_str().is_some_and(|k| MAIN_REPLACES.contains(&k)) {
            new_main.insert(k.clone(), v.clone());
        }
    }
    let mut out_services = Mapping::new();
    out_services.insert("main".into(), Value::Mapping(new_main));
    for (name, svc) in services {
        if name != main_name {
            out_services.insert(name.clone(), svc.clone());
        }
    }
    converted["services"] = Value::Mapping(out_services);
    fs::create_dir_all(target.parent().unwrap())?;
    let content = to_yaml(&Value::Mapping(converted)).replace("${T_BENCH_TASK_DOCKER_NAME_PREFIX}", "hb");
    fs::write(target, content)?;
    Ok(())
}

fn copy_remaining_files(src: &Path, env_dir: &Path, handled: &HashSet<String>) -> Result<()> {
    for item in walk_files(src)? {
        let rel = item.strip_prefix(src)?;
        let first = rel.components().next().unwrap().as_os_str().to_string_lossy();
        if handled.contains(first.as_ref()) {
            continue;
        }
        let target = env_dir.join(rel);
        fs::create_dir_all(target.parent().unwrap())?;
        fs::copy(&item, &target)?;
    }
    Ok(())
}

/// A file as UTF-8 text with `\r\n` and `\r` line endings normalized to `\n`.
fn read_text(p: &Path) -> Result<String> {
    let s = fs::read_to_string(p).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => anyhow!("no such file: {}", p.display()),
        _ => anyhow!("reading {}: {e}", p.display()),
    })?;
    Ok(s.replace("\r\n", "\n").replace('\r', "\n"))
}

fn chmod_x(p: &Path) -> Result<()> {
    let mode = fs::metadata(p)?.permissions().mode();
    fs::set_permissions(p, fs::Permissions::from_mode(mode | 0o111))?;
    Ok(())
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn sorted_entries(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v = fs::read_dir(dir)?.map(|e| e.map(|e| e.path())).collect::<std::io::Result<Vec<_>>>()?;
    v.sort();
    Ok(v)
}

/// Files under `dir`, recursively, pre-order sorted; symlinked dirs not followed.
fn walk_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for p in sorted_entries(dir)? {
        if fs::symlink_metadata(&p)?.is_dir() {
            out.extend(walk_files(&p)?);
        } else if p.is_file() {
            out.push(p);
        }
    }
    Ok(out)
}

/// Recursive copy into `dst`, which may already exist (symlinks followed).
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for p in sorted_entries(src)? {
        let t = dst.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_tree(&p, &t)?;
        } else {
            fs::copy(&p, &t)?;
        }
    }
    Ok(())
}

/// Shortest round-trip float text: positional for exponents -4..16 (`0.001`, `1.0`), else `1e+20` style.
fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf" } else { "-inf" }.into();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let e = format!("{f:e}"); // shortest round-trip digits, e.g. "-1.5e20"
    let (mant, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let sign = if mant.starts_with('-') { "-" } else { "" };
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    let decpt = exp + 1;
    let n = digits.len() as i32;
    let body = if -4 < decpt && decpt <= 16 {
        if decpt <= 0 {
            format!("0.{}{digits}", "0".repeat(-decpt as usize))
        } else if decpt >= n {
            format!("{digits}{}.0", "0".repeat((decpt - n) as usize))
        } else {
            format!("{}.{}", &digits[..decpt as usize], &digits[decpt as usize..])
        }
    } else {
        let frac = if n > 1 { format!(".{}", &digits[1..]) } else { String::new() };
        format!("{}{frac}e{}{:02}", &digits[..1], if exp < 0 { '-' } else { '+' }, exp.abs())
    };
    format!("{sign}{body}")
}

fn toml_float(f: f64) -> String {
    float_repr(f).replace("e+0", "e+").replace("e-0", "e-")
}

/// Whether a non-ASCII char is printed as-is in a quoted literal (vs escaped).
// Approximates Unicode categories Cc/Cf/Zs/Zl/Zp/Co; exact tables if it ever matters.
fn is_printable(c: char) -> bool {
    !c.is_control()
        && !matches!(c, '\u{a0}' | '\u{ad}' | '\u{1680}' | '\u{2000}'..='\u{200f}' | '\u{2028}'..='\u{202f}'
            | '\u{205f}'..='\u{206f}' | '\u{3000}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{e000}'..='\u{f8ff}')
}

/// `s` as a quoted literal: single quotes unless it holds only single quotes; control and
/// non-printable characters as `\x`/`\u`/`\U` escapes.
fn str_literal(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut o = String::from(q);
    for c in s.chars() {
        match c {
            '\\' => o += "\\\\",
            '\t' => o += "\\t",
            '\n' => o += "\\n",
            '\r' => o += "\\r",
            c if c == q => {
                o.push('\\');
                o.push(c)
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => o += &format!("\\x{:02x}", c as u32),
            c if c.is_ascii() || is_printable(c) => o.push(c),
            c if (c as u32) <= 0xff => o += &format!("\\x{:02x}", c as u32),
            c if (c as u32) <= 0xffff => o += &format!("\\u{:04x}", c as u32),
            c => o += &format!("\\U{:08x}", c as u32),
        }
    }
    o.push(q);
    o
}

/// Display text of a YAML value: strings as-is, `null`/`true`/`false`, collections in literal form.
fn yaml_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) if n.is_f64() => float_repr(n.as_f64().unwrap()),
        Value::Number(n) => n.to_string(),
        Value::Sequence(s) => format!("[{}]", s.iter().map(yaml_literal).collect::<Vec<_>>().join(", ")),
        Value::Mapping(m) => {
            format!("{{{}}}", m.iter().map(|(k, v)| format!("{}: {}", yaml_literal(k), yaml_literal(v))).collect::<Vec<_>>().join(", "))
        }
        Value::Tagged(t) => yaml_text(&t.value),
    }
}

fn yaml_literal(v: &Value) -> String {
    match v {
        Value::String(s) => str_literal(s),
        _ => yaml_text(v),
    }
}

/// A YAML value as JSON with `, `/`: ` separators and non-ASCII escaped as `\uXXXX`.
fn yaml_json(v: &Value) -> String {
    match v {
        Value::String(s) => {
            let mut o = String::from('"');
            for c in s.chars() {
                match c {
                    '"' => o += "\\\"",
                    '\\' => o += "\\\\",
                    '\n' => o += "\\n",
                    '\r' => o += "\\r",
                    '\t' => o += "\\t",
                    '\u{8}' => o += "\\b",
                    '\u{c}' => o += "\\f",
                    ' '..='~' => o.push(c),
                    c => {
                        let mut buf = [0u16; 2];
                        for u in c.encode_utf16(&mut buf) {
                            o += &format!("\\u{:04x}", u);
                        }
                    }
                }
            }
            o.push('"');
            o
        }
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) if n.is_f64() => match n.as_f64().unwrap() {
            f if f.is_nan() => "NaN".into(),
            f if f.is_infinite() => if f > 0.0 { "Infinity" } else { "-Infinity" }.into(),
            f => float_repr(f),
        },
        Value::Number(n) => n.to_string(),
        Value::Sequence(s) => format!("[{}]", s.iter().map(yaml_json).collect::<Vec<_>>().join(", ")),
        Value::Mapping(m) => format!(
            "{{{}}}",
            m.iter().map(|(k, v)| format!("{}: {}", yaml_json(&Value::String(yaml_text(k))), yaml_json(v))).collect::<Vec<_>>().join(", ")
        ),
        Value::Tagged(t) => yaml_json(&t.value),
    }
}

/// A TOML basic string in hbench's fixed layout, including its `\x` -> `\u00` rewriting.
fn toml_str(s: &str) -> Result<String> {
    let r = str_literal(s);
    let single = r.starts_with('\'');
    let mut v = r[1..r.len() - 1].to_string();
    if single {
        v = v.replace("\\'", "'").replace('"', "\\\"");
    }
    let mut parts: Vec<String> = v.split("\\x").map(String::from).collect();
    while parts.len() > 1 {
        if parts[0].is_empty() {
            parts.remove(0);
        }
        parts[0] = parts[0].replace("\\\\", "\\");
        let p0: Vec<char> = parts[0].chars().collect();
        let (Some(&last), true) = (p0.last(), parts.len() > 1) else {
            bail!("toml: cannot encode string {s:?} (unencodable escape sequence)");
        };
        let mut joinx = last != '\\';
        let mut k = 1;
        while p0.len() > k && p0[p0.len() - k] == '\\' {
            joinx = !joinx;
            k += 1;
        }
        let joined = format!("{}{}{}", parts[0], if joinx { "x" } else { "u00" }, parts[1]);
        parts.splice(0..2, [joined]);
    }
    Ok(format!("\"{}\"", parts[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tasks: &[(&str, &[(&str, &str)])]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("hbench-migrate-{}", uuid::Uuid::new_v4()));
        for (task, files) in tasks {
            for (rel, content) in *files {
                let p = root.join("src").join(task).join(rel);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(&p, content).unwrap();
            }
        }
        root
    }

    fn read(p: PathBuf) -> String {
        fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    fn files(dir: &Path) -> Vec<String> {
        walk_files(dir).unwrap().iter().map(|p| p.strip_prefix(dir).unwrap().to_string_lossy().into_owned()).collect()
    }

    fn is_exec(p: PathBuf) -> bool {
        fs::metadata(p).unwrap().permissions().mode() & 0o111 != 0
    }

    const DEFAULT_COMPOSE: &str = "services:
  client:
    build:
      dockerfile: Dockerfile
    image: ${T_BENCH_TASK_DOCKER_CLIENT_IMAGE_NAME}
    container_name: ${T_BENCH_TASK_DOCKER_CLIENT_CONTAINER_NAME}
    command: [ \"sh\", \"-c\", \"sleep infinity\" ]
    environment:
      - TEST_DIR=${T_BENCH_TEST_DIR}
    volumes:
      - ${T_BENCH_TASK_LOGS_PATH}:${T_BENCH_CONTAINER_LOGS_PATH}
      - ${T_BENCH_TASK_AGENT_LOGS_PATH}:${T_BENCH_CONTAINER_AGENT_LOGS_PATH}
";

    const TASK_YAML: &str = "instruction: |-
  Create a file hello.txt containing \"Hello, world!\" in /app.
author_name: Jane Doe
author_email: jane@example.com
difficulty: medium
category: file-operations
tags:
  - file-operations
  - beginner
parser_name: pytest
max_agent_timeout_sec: 180
max_test_timeout_sec: 30.0
run_tests_in_same_shell: false
expert_time_estimate_min: 2
junior_time_estimate_min: 7.5
";

    const TOML_TAIL: &str = "mcp_servers = []\n\n[environment.env]\n\n[solution.env]\n";

    #[test]
    fn default_task_collapses_to_dockerfile() {
        let root = fixture(&[(
            "hello-world",
            &[
                ("task.yaml", TASK_YAML),
                ("docker-compose.yaml", DEFAULT_COMPOSE),
                ("Dockerfile", "FROM ghcr.io/laude-institute/t-bench/ubuntu-24-04:20250624\n\nWORKDIR /app\n"),
                ("run-tests.sh", "#!/bin/bash\nsource $TEST_DIR/setup-uv-pytest.sh\nbash $TEST_DIR/run-uv-pytest.sh\n"),
                ("solution.sh", "#!/bin/bash\necho 'Hello, world!' > hello.txt\n"),
                ("tests/test_outputs.py", "def test_hello():\n    pass\n"),
                ("data/input.csv", "a,b\n1,2\n"),
                ("README.md", "notes\n"),
            ],
        )]);
        let out = migrate(&root.join("src/hello-world"), &root.join("out"), Overrides::default()).unwrap();
        let t = fs::canonicalize(root.join("out/hello-world")).unwrap();
        assert_eq!(out, vec![t.clone()]);
        assert_eq!(
            files(&t),
            [
                "environment/Dockerfile",
                "environment/README.md",
                "environment/data/input.csv",
                "instruction.md",
                "solution/solve.sh",
                "task.toml",
                "tests/test.sh",
                "tests/test_outputs.py"
            ]
        );
        assert_eq!(
            read(t.join("task.toml")),
            "schema_version = \"1.4\"\nartifacts = []\n\n[metadata]\nauthor_name = \"Jane Doe\"\n\
             author_email = \"jane@example.com\"\ndifficulty = \"medium\"\ncategory = \"file-operations\"\n\
             tags = [ \"file-operations\", \"beginner\",]\nexpert_time_estimate_min = 2.0\n\
             junior_time_estimate_min = 7.5\n\n[verifier]\ntimeout_sec = 30.0\ncollect = []\n\n\
             [verifier.env]\n\n[agent]\ntimeout_sec = 180.0\n\n[environment]\nnetwork_mode = \"public\"\n\
             build_timeout_sec = 600.0\nos = \"linux\"\n"
                .to_string()
                + TOML_TAIL
        );
        assert_eq!(read(t.join("instruction.md")), "Create a file hello.txt containing \"Hello, world!\" in /app.\n");
        assert_eq!(
            read(t.join("tests/test.sh")),
            "#!/bin/bash\nsource /tests/setup-uv-pytest.sh\nbash /tests/run-uv-pytest.sh\n".to_string() + REWARD_LOGGING_SUFFIX
        );
        assert_eq!(
            read(t.join("environment/Dockerfile")),
            "FROM ghcr.io/laude-institute/t-bench/ubuntu-24-04:20250624\n\nWORKDIR /app\n\n\
             # Mapped from docker-compose.yaml\nENV TEST_DIR=/tests\n"
        );
        assert!(is_exec(t.join("tests/test.sh")) && is_exec(t.join("solution/solve.sh")));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn collapse_moves_compose_fields_into_dockerfile() {
        let root = fixture(&[(
            "collapse-extras",
            &[
                (
                    "task.yaml",
                    "instruction: \"Line one\\nLine two\"\ndifficulty: hard\ntags: []\n\
                     estimated_duration_sec: 1e20\nmax_test_timeout_sec: 0.0001\n",
                ),
                (
                    "docker-compose.yaml",
                    "version: '3.8'\nservices:\n  client:\n    build: .\n    platform: linux/amd64\n    environment:\n\
                     \x20     - TEST_DIR=${T_BENCH_TEST_DIR}\n      - FOO=bar=baz\n      - BARE\n    working_dir: /app\n\
                     \x20   expose: 8080\n    entrypoint: /bin/bash\n",
                ),
                (
                    "Dockerfile",
                    "FROM ubuntu:22.04 AS build\nRUN echo hi\nFROM --platform=linux/arm64x\nFROM   alpine\n\
                     COPY tests /tests\nCOPY solution.sh /sol.sh\n",
                ),
                ("run-tests.sh", "#!/bin/bash\r\nls ${TEST_DIR}\r\nls $TEST_DIR"),
                ("solution.sh", "#!/bin/bash\ntrue\n"),
                ("tests/test.sh", "old\n"),
                ("tests/sub/deep.txt", "deep\n"),
            ],
        )]);
        let t = migrate(&root.join("src/collapse-extras"), &root.join("out"), Overrides::default()).unwrap().remove(0);
        assert_eq!(
            files(&t),
            [
                "environment/Dockerfile",
                "environment/solution.sh",
                "environment/tests/sub/deep.txt",
                "environment/tests/test.sh",
                "instruction.md",
                "solution/solve.sh",
                "task.toml",
                "tests/sub/deep.txt",
                "tests/test.sh"
            ]
        );
        // `\s+` spans the newline, so the next line is taken as the image.
        assert_eq!(
            read(t.join("environment/Dockerfile")),
            "FROM ubuntu:22.04 AS build\nRUN echo hi\nFROM --platform=linux/amd64 FROM   alpine\nCOPY tests /tests\n\
             COPY solution.sh /sol.sh\n\n# Mapped from docker-compose.yaml\nENV TEST_DIR=/tests\nENV FOO=bar=baz\n\
             ENV BARE\nWORKDIR /app\nEXPOSE 8080\nENTRYPOINT [\"/bin/bash\"]\n"
        );
        assert_eq!(read(t.join("tests/test.sh")), "#!/bin/bash\nls ${TEST_DIR}\nls /tests\n".to_string() + REWARD_LOGGING_SUFFIX);
        assert_eq!(read(t.join("instruction.md")), "Line one\nLine two\n");
        let toml = read(t.join("task.toml"));
        assert!(toml.contains("difficulty = \"hard\"\ncategory = \"software_engineering\"\ntags = []\nestimated_duration_sec = 1e+20\n\n"));
        assert!(toml.contains("[verifier]\ntimeout_sec = 0.0001\n"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn platform_entrypoint_and_toml_string_quirks() {
        let root = fixture(&[(
            "p",
            &[
                ("task.yaml", "instruction: x\nauthor_name: \"O'Brien \\\\ \\\"q\\\" é\"\n"),
                (
                    "docker-compose.yaml",
                    "services:\n  client:\n    build:\n      context: .\n    platform: linux/amd64\n    environment:\n\
                     \x20     MY_VAR: hello\n      NUM: 5\n      FLAG: true\n      EMPTY:\n      FLT: 1.5\n\
                     \x20   expose: [80, \"443\"]\n    entrypoint: [\"python\", \"-c\", \"print('é')\"]\n\
                     \x20   working_dir: ${HOME}/app\n",
                ),
                ("Dockerfile", "FROM --platform=linux/arm64 ubuntu:22.04\nRUN apt-get update\n"),
                ("run-tests.sh", "#!/bin/bash\nexit 0\n"),
            ],
        )]);
        let t = migrate(&root.join("src/p"), &root.join("out"), Overrides::default()).unwrap().remove(0);
        assert_eq!(
            read(t.join("environment/Dockerfile")),
            "FROM --platform=linux/amd64 ubuntu:22.04\nRUN apt-get update\n\n# Mapped from docker-compose.yaml\n\
             ENV TEST_DIR=/tests\nENV MY_VAR=hello\nENV NUM=5\nENV FLAG=true\nENV EMPTY=\nENV FLT=1.5\n\
             EXPOSE 80\nEXPOSE 443\nENTRYPOINT [\"python\", \"-c\", \"print('\\u00e9')\"]\n"
        );
        assert!(read(t.join("task.toml")).contains("author_name = \"O'Brien \\\\ \\\"q\\\" é\"\n"));
        assert!(!t.join("environment/docker-compose.yaml").exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn multi_service_writes_expected_compose() {
        let compose = r#"version: "3"
services:
  db:
    image: postgres:15
    environment:
      POSTGRES_PASSWORD: "secret"
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U postgres && echo this is a really long health check command that goes past eighty columns"]
      interval: 5s
    ports:
      - "5432:5432"
    container_name: ${T_BENCH_TASK_DOCKER_NAME_PREFIX}__db
  client:
    build:
      context: .
    image: ${T_BENCH_TASK_DOCKER_CLIENT_IMAGE_NAME}
    command: [ "sh", "-c", "sleep infinity" ]
    environment:
      - TEST_DIR=${T_BENCH_TEST_DIR}
      - DB_HOST=db
    depends_on:
      db:
        condition: service_healthy
    labels:
      quoted: "yes"
      num: "123"
      empty: ""
      colon: "a: b"
      hash: "a #b"
      lead: " lead"
      multi: "line1\nline2"
      unicode: "café"
      date: "2024-01-01"
      quote: "it's"
      longsq: "'aaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb cccccccccccccccccccccccccccc dddddddddddddd eeeeeeeeeee"
      longdq: "\taaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb cccccccccccccccccccccccccccc dddddddddddddd eeeeeeeeeee"
    tmpfs: []
    sysctls: {}
    privileged: true
    mem_limit: null
  worker:
    build: ./worker
    command: python worker.py
networks:
  backend:
    driver: bridge
"#;
        let root = fixture(&[
            (
                "multi",
                &[
                    ("task.yaml", "instruction: m\n"),
                    ("docker-compose.yaml", compose),
                    ("Dockerfile", "FROM python:3.12\n"),
                    ("run-tests.sh", "#!/bin/bash\n"),
                    ("worker/Dockerfile", "FROM python:3.12-slim\n"),
                    ("worker/worker.py", "print('w')\n"),
                ],
            ),
            ("bad", &[("task.yaml", "instruction: z\n"), ("run-tests.sh", ""), ("solution.yaml", "- command: x\n")]),
            ("no-compose", &[("task.yaml", "instruction: z\n"), ("run-tests.sh", "")]),
            ("not-a-task", &[("README.md", "x\n")]),
        ]);
        // Parent-dir mode: failing tasks are reported and skipped.
        let out = migrate(&root.join("src"), &root.join("out"), Overrides::default()).unwrap();
        assert_eq!(out.len(), 1);
        let t = &out[0];
        assert_eq!(files(&t.join("environment")), ["Dockerfile", "docker-compose.yaml", "worker/Dockerfile", "worker/worker.py"]);
        assert_eq!(
            read(t.join("environment/Dockerfile")),
            "FROM python:3.12\n\n# Mapped from docker-compose.yaml\nENV TEST_DIR=/tests\nENV DB_HOST=db\n"
        );
        let expected = r#"services:
  main:
    build:
      context: ${CONTEXT_DIR}
    image: ${MAIN_IMAGE_NAME}
    command:
    - sh
    - -c
    - sleep infinity
    environment:
    - TEST_DIR=${TEST_DIR}
    deploy:
      resources:
        limits:
          cpus: ${CPUS}
          memory: ${MEMORY}
    depends_on:
      db:
        condition: service_healthy
    labels:
      quoted: 'yes'
      num: '123'
      empty: ''
      colon: 'a: b'
      hash: 'a #b'
      lead: ' lead'
      multi: 'line1

        line2'
      unicode: "caf\xE9"
      date: '2024-01-01'
      quote: it's
      longsq: '''aaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb cccccccccccccccccccccccccccc
        dddddddddddddd eeeeeeeeeee'
      longdq: "\taaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb cccccccccccccccccccccccccccc\
        \ dddddddddddddd eeeeeeeeeee"
    tmpfs: []
    sysctls: {}
    privileged: true
    mem_limit: null
  db:
    image: postgres:15
    environment:
      POSTGRES_PASSWORD: secret
    healthcheck:
      test:
      - CMD-SHELL
      - pg_isready -U postgres && echo this is a really long health check command
        that goes past eighty columns
      interval: 5s
    ports:
    - 5432:5432
    container_name: hb__db
  worker:
    build: ./worker
    command: python worker.py
networks:
  backend:
    driver: bridge
"#;
        assert_eq!(read(t.join("environment/docker-compose.yaml")), expected);
        // Single-task mode propagates errors; existing targets are refused.
        let e = migrate(&root.join("src/bad"), &root.join("out"), Overrides::default()).unwrap_err();
        assert!(e.to_string().contains("uses solution.yaml which is not supported. Please convert it to solution.sh."));
        let e = migrate(&root.join("src/multi"), &root.join("out"), Overrides::default()).unwrap_err();
        assert!(e.to_string().starts_with("Task directory already exists"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn environment_overrides_land_in_task_toml() {
        let root = fixture(&[(
            "m",
            &[
                ("task.yaml", "instruction: Do the thing\n"),
                ("docker-compose.yaml", "services:\n  client:\n    build:\n      context: .\n"),
                ("Dockerfile", "FROM python:3.13"),
                ("run-tests.sh", "#!/bin/bash\nexit 0\n"),
            ],
        )]);
        let ov = Overrides { cpus: Some(4), memory_mb: Some(8192), storage_mb: Some(20480), gpus: Some(1) };
        let t = migrate(&root.join("src/m"), &root.join("out"), ov).unwrap().remove(0);
        assert!(read(t.join("task.toml")).ends_with(
            "os = \"linux\"\ncpus = 4\nmemory_mb = 8192\nstorage_mb = 20480\ngpus = 1\nmcp_servers = []\n\n[environment.env]\n\n[solution.env]\n"
        ));
        assert_eq!(read(t.join("environment/Dockerfile")), "FROM python:3.13\n\n# Mapped from docker-compose.yaml\nENV TEST_DIR=/tests\n");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn literal_formatting_helpers() {
        for (f, s) in [
            (60.0, "60.0"),
            (1e20, "1e+20"),
            (1e-7, "1e-07"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (-2.5, "-2.5"),
        ] {
            assert_eq!(float_repr(f), s);
        }
        assert_eq!(toml_float(1e-7), "1e-7");
        assert_eq!(toml_str("Jo \\ \"x\"\n\u{1}é").unwrap(), "\"Jo \\ \\\"x\\\"\\nx01é\"");
        assert_eq!(str_literal("it's"), "\"it's\"");
    }
}
