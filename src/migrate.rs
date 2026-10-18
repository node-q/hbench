//! Terminal-Bench (v1) -> task migration (`tasks migrate`).
//! Byte-for-byte output parity: the TOML writer mimics
//! `TaskConfig.model_dump_toml()` (python `toml` lib quirks included), and the
//! docker-compose writer mimics PyYAML's `safe_dump(sort_keys=False)`.

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde_yaml_ng::{Mapping, Value};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

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
    // Sorted for determinism; Python iterates in iterdir() (fs) order.
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
    /// Mirrors the pydantic model (extra keys ignored). Error texts differ from pydantic's.
    fn parse(text: &str) -> Result<Self> {
        let v: Value = serde_yaml_ng::from_str(text).context("task.yaml")?;
        let m = v.as_mapping().ok_or_else(|| anyhow!("task.yaml: expected a mapping"))?;
        let s = |k: &str, d: Option<&str>| -> Result<String> {
            match (m.get(k), d) {
                (None, Some(d)) => Ok(d.to_string()),
                (Some(Value::String(s)), _) => Ok(s.clone()),
                (None, None) => bail!("task.yaml: {k}: Field required"),
                _ => bail!("task.yaml: {k}: Input should be a valid string"),
            }
        };
        let f = |k: &str| -> Result<Option<f64>> {
            match m.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Number(n)) => Ok(n.as_f64()),
                Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| anyhow!("task.yaml: {k}: Input should be a valid number")),
                _ => bail!("task.yaml: {k}: Input should be a valid number"),
            }
        };
        let req = |k: &str, d: f64| -> Result<f64> {
            if matches!(m.get(k), Some(Value::Null)) {
                bail!("task.yaml: {k}: Input should be a valid number");
            }
            Ok(f(k)?.unwrap_or(d))
        };
        let difficulty = s("difficulty", Some("unknown"))?;
        if !["easy", "medium", "hard", "unknown"].contains(&difficulty.as_str()) {
            bail!("task.yaml: difficulty: Input should be 'easy', 'medium', 'hard' or 'unknown'");
        }
        let tags = match m.get("tags") {
            None => vec![],
            Some(Value::Sequence(items)) => items
                .iter()
                .map(|t| t.as_str().map(String::from).ok_or_else(|| anyhow!("task.yaml: tags: Input should be a valid string")))
                .collect::<Result<_>>()?,
            _ => bail!("task.yaml: tags: Input should be a valid list"),
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

    /// The exact task.toml (`model_dump_toml()` output) for this config.
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
            let svc = svc.as_mapping().ok_or_else(|| anyhow!("service {} must be a mapping", py_str(name)))?;
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
    let svc = services[&name].as_mapping().ok_or_else(|| anyhow!("service {} must be a mapping", py_str(&name)))?;
    Ok((name, svc))
}

fn build_context(svc: &Mapping) -> String {
    match svc.get("build") {
        Some(Value::Mapping(b)) => b.get("context").map(py_str).unwrap_or_else(|| ".".into()),
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
        bail!("'NoneType' object has no attribute 'items'");
    };
    for (k, v) in svc {
        let k = py_str(k);
        if !DOCKERFILE_FIELDS.contains(&k.as_str()) && !IGNORED_FIELDS.contains(&k.as_str()) {
            return Ok(false);
        }
        // Python: set(volumes).issubset(TBENCH_DEFAULT_VOLUMES)
        if k == "volumes" {
            let default = |x: &Value| x.as_str().is_some_and(|s| TBENCH_DEFAULT_VOLUMES.contains(&s));
            let ok = match v {
                Value::Sequence(items) => {
                    if items.iter().any(|i| matches!(i, Value::Mapping(_) | Value::Sequence(_))) {
                        bail!("unhashable type: 'dict'");
                    }
                    items.iter().all(default)
                }
                Value::String(s) => s.is_empty(),
                Value::Mapping(m) => m.keys().all(default),
                _ => bail!("volumes: object is not iterable"),
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
                    add.push(format!("ENV {}", py_str(it)));
                }
            }
        }
        Some(Value::Mapping(m)) => {
            for (k, v) in m {
                let s = format!("{}={}", py_str(k), py_str(v));
                if s != TBENCH_DEFAULT_ENV {
                    add.push(format!("ENV {s}"));
                }
            }
        }
        _ => {}
    }
    if let Some(wd) = svc.get("working_dir") {
        let wd = py_str(wd);
        if !wd.contains("${") {
            add.push(format!("WORKDIR {wd}"));
        }
    }
    match svc.get("expose") {
        Some(Value::Sequence(ports)) => add.extend(ports.iter().map(|p| format!("EXPOSE {}", py_str(p)))),
        Some(p) => add.push(format!("EXPOSE {}", py_str(p))),
        None => {}
    }
    match svc.get("entrypoint") {
        Some(ep @ Value::Sequence(_)) => add.push(format!("ENTRYPOINT {}", py_json(ep))),
        Some(ep) => add.push(format!("ENTRYPOINT {}", py_json(&Value::Sequence(vec![ep.clone()])))),
        None => {}
    }
    add
}

static PLATFORM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^(FROM\s+)--platform=\S+\s+(.+)$").unwrap());

fn append_to_dockerfile(path: &Path, svc: &Mapping, task: &str) -> Result<()> {
    let additions = dockerfile_additions(svc);
    let mut content = read_text(path)?;
    if let Some(p) = svc.get("platform") {
        let platform = py_str(p);
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
    let non_default: Vec<String> = additions.iter().filter(|a| *a != "ENV TEST_DIR=/tests").map(|a| py_repr_str(a)).collect();
    if !non_default.is_empty() {
        println!("[{task}] Moved to Dockerfile: [{}]", non_default.join(", "));
    }
    fs::write(path, content)?;
    Ok(())
}

/// Python: re.subn(r"^FROM\s+(?!--platform)(.+)$", "FROM --platform=P \1", flags=M).
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
    let content = pyyaml_dump(&Value::Mapping(converted)).replace("${T_BENCH_TASK_DOCKER_NAME_PREFIX}", "hb");
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

/// Python `read_text()`: UTF-8 with universal newlines.
fn read_text(p: &Path) -> Result<String> {
    let s = fs::read_to_string(p).with_context(|| format!("[Errno 2] No such file or directory: '{}'", p.display()))?;
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

/// shutil.copytree(src, dst, dirs_exist_ok=True) (symlinks followed).
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

/// Python `repr(float)`.
fn py_float_repr(f: f64) -> String {
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
    py_float_repr(f).replace("e+0", "e+").replace("e-0", "e-")
}

/// Python `str.isprintable()` for non-ASCII.
// Approximates Unicode categories Cc/Cf/Zs/Zl/Zp/Co; exact tables if it ever matters.
fn py_printable(c: char) -> bool {
    !c.is_control()
        && !matches!(c, '\u{a0}' | '\u{ad}' | '\u{1680}' | '\u{2000}'..='\u{200f}' | '\u{2028}'..='\u{202f}'
            | '\u{205f}'..='\u{206f}' | '\u{3000}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{e000}'..='\u{f8ff}')
}

/// Python `repr(str)`.
fn py_repr_str(s: &str) -> String {
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
            c if c.is_ascii() || py_printable(c) => o.push(c),
            c if (c as u32) <= 0xff => o += &format!("\\x{:02x}", c as u32),
            c if (c as u32) <= 0xffff => o += &format!("\\u{:04x}", c as u32),
            c => o += &format!("\\U{:08x}", c as u32),
        }
    }
    o.push(q);
    o
}

/// Python `str(x)` of a YAML-loaded value.
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) if n.is_f64() => py_float_repr(n.as_f64().unwrap()),
        Value::Number(n) => n.to_string(),
        Value::Sequence(s) => format!("[{}]", s.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Mapping(m) => {
            format!("{{{}}}", m.iter().map(|(k, v)| format!("{}: {}", py_repr(k), py_repr(v))).collect::<Vec<_>>().join(", "))
        }
        Value::Tagged(t) => py_str(&t.value),
    }
}

fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => py_repr_str(s),
        _ => py_str(v),
    }
}

/// Python `json.dumps(x)` (ensure_ascii, default separators).
fn py_json(v: &Value) -> String {
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
            f => py_float_repr(f),
        },
        Value::Number(n) => n.to_string(),
        Value::Sequence(s) => format!("[{}]", s.iter().map(py_json).collect::<Vec<_>>().join(", ")),
        Value::Mapping(m) => format!(
            "{{{}}}",
            m.iter().map(|(k, v)| format!("{}: {}", py_json(&Value::String(py_str(k))), py_json(v))).collect::<Vec<_>>().join(", ")
        ),
        Value::Tagged(t) => py_json(&t.value),
    }
}

/// python `toml` 0.10.2 `_dump_str`, quirks and all.
fn toml_str(s: &str) -> Result<String> {
    let r = py_repr_str(s);
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
            bail!("toml: cannot encode string {s:?} (IndexError in python toml)");
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

/// `yaml.safe_dump(v, sort_keys=False)`: block style, indent 2, width 80, ASCII only.
/// Port of PyYAML's Emitter for the node shapes safe_load produces (no anchors/tags).
fn pyyaml_dump(v: &Value) -> String {
    let mut e = Emitter { out: String::new(), col: 0, ws: true, indention: true, indent: None, indents: vec![] };
    e.node(v, false, false);
    e.write_indent();
    e.out
}

static NON_STR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(?:yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF",
        r"|[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?",
        r"|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN)",
        r"|[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+",
        r"|<<|~|null|Null|NULL|=|",
        r"|[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]",
        r"|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?(?:[Tt]|[ \t]+)[0-9][0-9]?:[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?",
        r")$"
    ))
    .unwrap()
});

fn is_break(c: char) -> bool {
    matches!(c, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

fn is_ws0(c: char) -> bool {
    matches!(c, '\0' | ' ' | '\t' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

struct Analysis {
    empty: bool,
    multiline: bool,
    allow_block_plain: bool,
    allow_single_quoted: bool,
}

fn analyze_scalar(s: &[char]) -> Analysis {
    if s.is_empty() {
        return Analysis { empty: true, multiline: false, allow_block_plain: true, allow_single_quoted: true };
    }
    let (mut block_ind, mut line_breaks, mut special) = (false, false, false);
    let (mut lead_space, mut lead_break, mut trail_space, mut trail_break, mut break_space, mut space_break) =
        (false, false, false, false, false, false);
    if s.starts_with(&['-', '-', '-']) || s.starts_with(&['.', '.', '.']) {
        block_ind = true;
    }
    let mut preceded_ws = true;
    let mut followed_ws = s.len() == 1 || is_ws0(s[1]);
    let (mut prev_space, mut prev_break) = (false, false);
    for (i, &ch) in s.iter().enumerate() {
        // Block indicators (flow-plain is never used: no flow context with scalars).
        let ind = if i == 0 {
            "#,[]{}&*!|>'\"%@`".contains(ch) || ((ch == '?' || ch == ':' || ch == '-') && followed_ws)
        } else {
            (ch == ':' && followed_ws) || (ch == '#' && preceded_ws)
        };
        block_ind |= ind;
        if is_break(ch) {
            line_breaks = true;
        }
        if !(ch == '\n' || (' '..='~').contains(&ch)) {
            special = true; // allow_unicode=False: every non-ASCII char is special
        }
        if ch == ' ' {
            lead_space |= i == 0;
            trail_space |= i == s.len() - 1;
            break_space |= prev_break;
            prev_space = true;
            prev_break = false;
        } else if is_break(ch) {
            lead_break |= i == 0;
            trail_break |= i == s.len() - 1;
            space_break |= prev_space;
            prev_space = false;
            prev_break = true;
        } else {
            prev_space = false;
            prev_break = false;
        }
        preceded_ws = is_ws0(ch);
        followed_ws = i + 2 >= s.len() || is_ws0(s[i + 2]);
    }
    let abp =
        !(lead_space || lead_break || trail_space || trail_break || break_space || space_break || special || line_breaks || block_ind);
    let asq = !(break_space || space_break || special);
    Analysis { empty: false, multiline: line_breaks, allow_block_plain: abp, allow_single_quoted: asq }
}

/// (text, implicit-plain-ok) as PyYAML's representer + resolver see a scalar.
fn scalar_text(v: &Value) -> (String, bool) {
    match v {
        Value::Null => ("null".into(), true),
        Value::Bool(b) => (b.to_string(), true),
        Value::Number(n) if n.is_f64() => {
            let f = n.as_f64().unwrap();
            let s = if f.is_nan() {
                ".nan".into()
            } else if f.is_infinite() {
                if f > 0.0 { ".inf" } else { "-.inf" }.into()
            } else {
                let r = py_float_repr(f).to_lowercase();
                if !r.contains('.') && r.contains('e') { r.replacen('e', ".0e", 1) } else { r }
            };
            (s, true)
        }
        Value::Number(n) => (n.to_string(), true),
        Value::String(s) => (s.clone(), !NON_STR_RE.is_match(s)),
        _ => (py_str(v), true),
    }
}

struct Emitter {
    out: String,
    col: usize,
    ws: bool,
    indention: bool,
    indent: Option<usize>,
    indents: Vec<Option<usize>>,
}

impl Emitter {
    fn node(&mut self, v: &Value, mapping_ctx: bool, simple_key: bool) {
        match v {
            Value::Tagged(t) => self.node(&t.value, mapping_ctx, simple_key),
            Value::Sequence(s) if s.is_empty() => self.flow_empty("[", "]"),
            Value::Mapping(m) if m.is_empty() => self.flow_empty("{", "}"),
            Value::Sequence(s) => {
                let indentless = mapping_ctx && !self.indention;
                self.increase_indent(false, indentless);
                for item in s {
                    self.write_indent();
                    self.write_indicator("-", true, false, true);
                    self.node(item, false, false);
                }
                self.indent = self.indents.pop().unwrap();
            }
            Value::Mapping(m) => {
                self.increase_indent(false, false);
                for (k, val) in m {
                    self.write_indent();
                    if check_simple_key(k) {
                        self.node(k, true, true);
                        self.write_indicator(":", false, false, false);
                    } else {
                        self.write_indicator("?", true, false, true);
                        self.node(k, true, false);
                        self.write_indent();
                        self.write_indicator(":", true, false, true);
                    }
                    self.node(val, true, false);
                }
                self.indent = self.indents.pop().unwrap();
            }
            _ => {
                let (text, implicit) = scalar_text(v);
                let chars: Vec<char> = text.chars().collect();
                self.increase_indent(true, false);
                let a = analyze_scalar(&chars);
                let split = !simple_key;
                if implicit && !(simple_key && (a.empty || a.multiline)) && a.allow_block_plain {
                    self.write_plain(&chars, split);
                } else if a.allow_single_quoted && !(simple_key && a.multiline) {
                    self.write_single_quoted(&chars, split);
                } else {
                    self.write_double_quoted(&chars, split);
                }
                self.indent = self.indents.pop().unwrap();
            }
        }
    }

    // Only empty collections go flow style (all safe_dump(default_flow_style=False) emits).
    fn flow_empty(&mut self, open: &str, close: &str) {
        self.write_indicator(open, true, true, false);
        self.write_indicator(close, false, false, false);
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.indents.push(self.indent);
        match self.indent {
            None => self.indent = Some(if flow { 2 } else { 0 }),
            Some(i) if !indentless => self.indent = Some(i + 2),
            _ => {}
        }
    }

    fn write(&mut self, s: &str) {
        self.col += s.chars().count();
        self.out += s;
    }

    fn write_indicator(&mut self, ind: &str, need_ws: bool, whitespace: bool, indention: bool) {
        let data = if self.ws || !need_ws { ind.to_string() } else { format!(" {ind}") };
        self.ws = whitespace;
        self.indention = self.indention && indention;
        self.write(&data);
    }

    fn write_indent(&mut self) {
        let indent = self.indent.unwrap_or(0);
        if !self.indention || self.col > indent || (self.col == indent && !self.ws) {
            self.write_line_break(None);
        }
        if self.col < indent {
            self.ws = true;
            self.out += &" ".repeat(indent - self.col);
            self.col = indent;
        }
    }

    fn write_line_break(&mut self, br: Option<char>) {
        self.ws = true;
        self.indention = true;
        self.col = 0;
        self.out.push(br.unwrap_or('\n'));
    }

    // No line-break branch; PyYAML never emits multiline plain scalars.
    fn write_plain(&mut self, t: &[char], split: bool) {
        if t.is_empty() {
            return;
        }
        if !self.ws {
            self.write(" ");
        }
        self.ws = false;
        self.indention = false;
        let (mut spaces, mut start, mut end) = (false, 0, 0);
        while end <= t.len() {
            let ch = t.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.col > 80 && split {
                        self.write_indent();
                        self.ws = false;
                        self.indention = false;
                    } else {
                        self.write(&t[start..end].iter().collect::<String>());
                    }
                    start = end;
                }
            } else if ch.is_none_or(|c| c == ' ' || is_break(c)) {
                self.write(&t[start..end].iter().collect::<String>());
                start = end;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
            }
            end += 1;
        }
    }

    fn write_single_quoted(&mut self, t: &[char], split: bool) {
        self.write_indicator("'", true, false, false);
        let (mut spaces, mut breaks, mut start, mut end) = (false, false, 0, 0);
        while end <= t.len() {
            let ch = t.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.col > 80 && split && start != 0 && end != t.len() {
                        self.write_indent();
                    } else {
                        self.write(&t[start..end].iter().collect::<String>());
                    }
                    start = end;
                }
            } else if breaks {
                if ch.is_none_or(|c| !is_break(c)) {
                    if t[start] == '\n' {
                        self.write_line_break(None);
                    }
                    for &br in &t[start..end] {
                        self.write_line_break(if br == '\n' { None } else { Some(br) });
                    }
                    self.write_indent();
                    start = end;
                }
            } else if ch.is_none_or(|c| c == ' ' || c == '\'' || is_break(c)) && start < end {
                self.write(&t[start..end].iter().collect::<String>());
                start = end;
            }
            if ch == Some('\'') {
                self.write("''");
                start = end + 1;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = is_break(c);
            }
            end += 1;
        }
        self.write_indicator("'", false, false, false);
    }

    fn write_double_quoted(&mut self, t: &[char], split: bool) {
        self.write_indicator("\"", true, false, false);
        let n = t.len() as isize;
        let (mut start, mut end): (isize, isize) = (0, 0);
        let slice = |a: isize, b: isize| -> String { if a < b { t[a as usize..b as usize].iter().collect() } else { String::new() } };
        while end <= n {
            let ch = t.get(end as usize).copied();
            if ch.is_none_or(|c| matches!(c, '"' | '\\' | '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{feff}') || !(' '..='~').contains(&c)) {
                if start < end {
                    self.write(&slice(start, end));
                    start = end;
                }
                if let Some(c) = ch {
                    let data = match c {
                        '\0' => "\\0".into(),
                        '\u{7}' => "\\a".into(),
                        '\u{8}' => "\\b".into(),
                        '\t' => "\\t".into(),
                        '\n' => "\\n".into(),
                        '\u{b}' => "\\v".into(),
                        '\u{c}' => "\\f".into(),
                        '\r' => "\\r".into(),
                        '\u{1b}' => "\\e".into(),
                        '"' => "\\\"".into(),
                        '\\' => "\\\\".into(),
                        '\u{85}' => "\\N".into(),
                        '\u{a0}' => "\\_".into(),
                        '\u{2028}' => "\\L".into(),
                        '\u{2029}' => "\\P".into(),
                        c if (c as u32) <= 0xff => format!("\\x{:02X}", c as u32),
                        c if (c as u32) <= 0xffff => format!("\\u{:04X}", c as u32),
                        c => format!("\\U{:08X}", c as u32),
                    };
                    self.write(&data);
                    start = end + 1;
                }
            }
            if 0 < end && end < n - 1 && (ch == Some(' ') || start >= end) && self.col as isize + (end - start) > 80 && split {
                let data = slice(start, end) + "\\";
                if start < end {
                    start = end;
                }
                self.write(&data);
                self.write_indent();
                self.ws = false;
                self.indention = false;
                if t[start as usize] == ' ' {
                    self.write("\\");
                }
            }
            end += 1;
        }
        self.write_indicator("\"", false, false, false);
    }
}

fn check_simple_key(k: &Value) -> bool {
    match k {
        Value::Sequence(s) => s.is_empty(),
        Value::Mapping(m) => m.is_empty(),
        _ => {
            let chars: Vec<char> = scalar_text(k).0.chars().collect();
            let a = analyze_scalar(&chars);
            chars.len() < 128 && !a.empty && !a.multiline
        }
    }
}

// Expected outputs below come from the reference Python mapper run on identical fixtures.
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
        // `\s+` spans the newline, exactly as Python's re does.
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
             ENV TEST_DIR=/tests\nENV MY_VAR=hello\nENV NUM=5\nENV FLAG=True\nENV EMPTY=None\nENV FLT=1.5\n\
             EXPOSE 80\nEXPOSE 443\nENTRYPOINT [\"python\", \"-c\", \"print('\\u00e9')\"]\n"
        );
        assert!(read(t.join("task.toml")).contains("author_name = \"O'Brien \\\\ \\\"q\\\" é\"\n"));
        assert!(!t.join("environment/docker-compose.yaml").exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn multi_service_writes_pyyaml_identical_compose() {
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
    fn python_formatting_helpers() {
        for (f, s) in [
            (60.0, "60.0"),
            (1e20, "1e+20"),
            (1e-7, "1e-07"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (-2.5, "-2.5"),
        ] {
            assert_eq!(py_float_repr(f), s);
        }
        assert_eq!(toml_float(1e-7), "1e-7");
        assert_eq!(toml_str("Jo \\ \"x\"\n\u{1}é").unwrap(), "\"Jo \\ \\\"x\\\"\\nx01é\"");
        assert_eq!(py_repr_str("it's"), "\"it's\"");
    }
}
