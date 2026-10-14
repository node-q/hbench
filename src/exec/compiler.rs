//! Task compiler.

use super::*;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// `re.sub(r"[^A-Za-z0-9_.-]+", "-", v.strip().lower()).strip("-")`; may be empty.
pub(super) fn slug(v: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"[^A-Za-z0-9_.-]+").unwrap());
    re.replace_all(&v.trim().to_lowercase(), "-").trim_matches('-').to_string()
}

pub(super) fn slugify(v: &str) -> String {
    let s = slug(v);
    if s.is_empty() { "task".into() } else { s }
}

/// `shutil.copytree` (follows symlinks); `dst` must not exist.
pub(super) fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        bail!("[Errno 17] File exists: '{}'", dst.display());
    }
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let p = e?.path();
        let to = dst.join(p.file_name().unwrap_or_default());
        if p.is_dir() {
            copy_tree(&p, &to)?;
        } else {
            std::fs::copy(&p, &to).with_context(|| format!("copying {}", p.display()))?;
        }
    }
    Ok(())
}

pub(super) fn replace_dir(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }
    copy_tree(src, dst)
}

pub(super) fn write_file(p: &Path, content: &str, mode: u32) -> Result<()> {
    std::fs::write(p, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))?;
    }
    let _ = mode;
    Ok(())
}

/// Compile a CompileConfig into task dirs (instruction x environment x verifier cross product).
pub fn compile(cfg: &CompileConfig) -> Result<Vec<PathBuf>> {
    let out = cfg.output_dir.as_ref().ok_or_else(|| anyhow!("CompileConfig.output_dir is required."))?;
    let out = resolve(&out.0);
    std::fs::create_dir_all(&out)?;
    let instructions: Vec<Option<&CompileInstruction>> =
        if cfg.instructions.is_empty() { vec![None] } else { cfg.instructions.iter().map(Some).collect() };
    let environments: Vec<Option<&CompileEnvironment>> =
        if cfg.environments.is_empty() { vec![None] } else { cfg.environments.iter().map(Some).collect() };
    let verifiers: Vec<Option<&CompileVerifier>> =
        if cfg.verifiers.is_empty() { vec![None] } else { cfg.verifiers.iter().map(Some).collect() };
    let base = slugify(cfg.task_name_prefix.as_deref().or(cfg.dataset_name.as_deref()).unwrap_or("task"));
    let mut tasks = vec![];
    for i in &instructions {
        for e in &environments {
            for v in &verifiers {
                let dir = out.join(format!("{base}-{:04}", tasks.len() + 1));
                compile_task(cfg, &dir, *i, *e, *v)?;
                tasks.push(dir);
            }
        }
    }
    Ok(tasks)
}

fn compile_task(
    cfg: &CompileConfig,
    dir: &Path,
    instruction: Option<&CompileInstruction>,
    environment: Option<&CompileEnvironment>,
    verifier: Option<&CompileVerifier>,
) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    if let Some(t) = &cfg.task_template {
        let t = resolve(&t.0);
        if !t.is_dir() {
            bail!("task_template must be a directory: {}", t.display());
        }
        copy_tree(&t, dir)?;
    }
    std::fs::create_dir_all(dir)?;
    let env_dir = dir.join("environment");
    let tests_dir = dir.join("tests");

    if let Some(i) = instruction {
        if let Some(text) = &i.text {
            std::fs::write(dir.join("instruction.md"), if text.ends_with('\n') { text.clone() } else { format!("{text}\n") })?;
        } else {
            let p = resolve(&i.path.as_ref().ok_or_else(|| anyhow!("Instruction must define text or path."))?.0);
            if !p.is_file() {
                bail!("Instruction path must be a file: {}", p.display());
            }
            std::fs::copy(&p, dir.join("instruction.md"))?;
        }
    }

    match environment {
        None => std::fs::create_dir_all(&env_dir)?,
        Some(CompileEnvironment { path: Some(p), .. }) => {
            let src = resolve(&p.0);
            if !src.is_dir() {
                bail!("Environment path must be a directory: {}", src.display());
            }
            replace_dir(&src, &env_dir)?;
        }
        Some(e) if e.paths.is_empty() => std::fs::create_dir_all(&env_dir)?,
        Some(e) => {
            if env_dir.exists() {
                std::fs::remove_dir_all(&env_dir)?;
            }
            std::fs::create_dir_all(&env_dir)?;
            for src in expand_input_paths(&e.paths)? {
                let to = env_dir.join(src.file_name().unwrap_or_default());
                if src.is_dir() {
                    replace_dir(&src, &to)?;
                } else if src.is_file() {
                    std::fs::copy(&src, &to)?;
                } else {
                    bail!("Input path must be a file or directory: {}", src.display());
                }
            }
        }
    }

    if let Some(v) = verifier {
        if let Some(p) = &v.path {
            let src = resolve(&p.0);
            if !src.is_dir() {
                bail!("Verifier path must be a directory: {}", src.display());
            }
            replace_dir(&src, &tests_dir)?;
        } else {
            let av = v.auto_verifier.as_ref().ok_or_else(|| anyhow!("Verifier must define path or auto_verifier."))?;
            write_auto_verifier(cfg, &tests_dir, av)?;
        }
    }

    write_task_config(cfg, dir, environment)?;

    if !is_valid_dir(dir, true) {
        bail!("Compiled task is not a valid task directory: {}", dir.display());
    }
    if tests_dir.exists() && !is_valid_dir(dir, false) {
        bail!("Compiled task has a tests/ directory but no OS-compatible verifier script for its shared verifier environment.");
    }
    Ok(())
}

fn expand_input_paths(patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = vec![];
    for pat in patterns {
        let resolved = resolve(pat).to_string_lossy().into_owned();
        let mut paths: Vec<PathBuf> = py_glob(&resolved).iter().map(|p| PathBuf::from(py_path(p))).collect();
        paths.sort();
        if paths.is_empty() {
            bail!("Input path pattern did not match anything: {pat}");
        }
        out.extend(paths.into_iter().map(|p| p.canonicalize().unwrap_or(p)));
    }
    Ok(out)
}

fn write_auto_verifier(cfg: &CompileConfig, tests_dir: &Path, av: &AutoVerifier) -> Result<()> {
    if tests_dir.exists() {
        std::fs::remove_dir_all(tests_dir)?;
    }
    std::fs::create_dir_all(tests_dir)?;
    let mut required = av.required_artifacts.clone().unwrap_or_else(|| {
        cfg.artifacts
            .iter()
            .map(|a| a.as_str().or_else(|| a.get("source").and_then(Value::as_str)).unwrap_or_default().to_string())
            .collect()
    });
    if let Some(r) = &av.reward_artifact
        && !required.contains(r)
    {
        required.push(r.clone());
    }
    let script = if av.artifact_json_schemas.is_empty() {
        TPL_AUTO_VERIFY
    } else {
        let schemas = tests_dir.join("schemas");
        std::fs::create_dir_all(&schemas)?;
        let mut checks = vec![];
        for (i, (artifact, schema)) in av.artifact_json_schemas.iter().enumerate() {
            let src = resolve(&py_path(schema.as_str().unwrap_or_default()));
            if !src.is_file() {
                bail!("Artifact schema path must be a file: {}", src.display());
            }
            let name = format!("schema-{:04}.json", i + 1);
            std::fs::copy(&src, schemas.join(&name))?;
            checks.push(json!({"artifact_path": artifact, "schema_path": format!("/tests/schemas/{name}")}));
        }
        write_file(&tests_dir.join("validate_artifact_schemas.py"), TPL_SCHEMA_VALIDATOR, 0o644)?;
        std::fs::write(tests_dir.join("artifact-schema-checks.json"), ascii_json(&serde_json::to_string_pretty(&checks)?) + "\n")?;
        TPL_AUTO_VERIFY_SCHEMAS
    };
    std::fs::write(tests_dir.join("required-artifacts.txt"), required.iter().map(|a| format!("{a}\n")).collect::<String>())?;
    if let Some(r) = &av.reward_artifact {
        std::fs::write(tests_dir.join("reward-artifact.txt"), format!("{r}\n"))?;
        write_file(&tests_dir.join("promote_reward_artifact.py"), TPL_PROMOTE_REWARD, 0o644)?;
    }
    write_file(&tests_dir.join("test.sh"), script, 0o755)
}

/// Python `json.dumps` default `ensure_ascii=True`.
fn ascii_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

fn write_task_config(cfg: &CompileConfig, dir: &Path, environment: Option<&CompileEnvironment>) -> Result<()> {
    let path = dir.join("task.toml");
    let exists = path.exists();
    let raw: Map<String, Value> = if exists {
        parse_toml(&std::fs::read_to_string(&path)?).with_context(|| format!("parsing {}", path.display()))?
    } else {
        Map::new()
    };
    let mut tc = hbench_task_config(&raw);
    if !cfg.artifacts.is_empty() {
        tc.insert("artifacts".into(), cfg.artifacts.iter().map(artifact_value).collect());
    }
    let fresh_env =
        |image: &str, workdir: &str| Value::Object(env_cfg(json!({"docker_image": image, "workdir": workdir}).as_object().unwrap()));
    match environment {
        None if !exists => {
            tc.insert("environment".into(), fresh_env(DEFAULT_IMAGE, DEFAULT_WORKDIR));
        }
        None => {}
        Some(e) if e.path.is_none() => {
            tc.insert("environment".into(), fresh_env(e.image(), e.wd()));
        }
        Some(e) => {
            if let Some(Value::Object(env)) = tc.get_mut("environment") {
                if let Some(i) = &e.docker_image {
                    env.insert("docker_image".into(), i.clone().into());
                }
                if let Some(w) = &e.workdir {
                    env.insert("workdir".into(), w.clone().into());
                }
                // model_copy(update=...) keeps field order.
                *env = env_cfg(env);
            }
        }
    }
    std::fs::write(&path, task_config_toml(&tc)?)?;
    Ok(())
}

/// `Task.is_valid_dir`. ponytail: separate-verifier detection is "tests/ ships a Dockerfile or
/// compose file"; upstream also resolves [verifier.environment] definitions.
fn is_valid_dir(dir: &Path, disable_verification: bool) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join("task.toml")) else { return false };
    if !dir.join("environment").exists() {
        return false;
    }
    let Ok(raw) = parse_toml(&text) else { return false };
    let tc = Value::Object(hbench_task_config(&raw));
    let steps: Vec<String> = tc
        .get("steps")
        .and_then(Value::as_array)
        .map(|s| s.iter().filter_map(|x| x.get("name")?.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let has_instructions = if steps.is_empty() {
        dir.join("instruction.md").exists()
    } else {
        steps.iter().all(|s| dir.join("steps").join(s).join("instruction.md").exists())
    };
    if disable_verification || !has_instructions {
        return has_instructions;
    }
    let windows = tc.pointer("/environment/os").and_then(Value::as_str) == Some("windows");
    let script = if windows { "test.bat" } else { "test.sh" };
    let separate = tc.pointer("/verifier/environment_mode").and_then(Value::as_str) == Some("separate")
        || tc.pointer("/verifier/environment").is_some();
    let bundled =
        |tests: &Path| separate && ["Dockerfile", "docker-compose.yaml", "docker-compose.yml"].iter().any(|f| tests.join(f).exists());
    let tests = dir.join("tests");
    if steps.is_empty() {
        return bundled(&tests) || tests.join(script).is_file();
    }
    steps.iter().all(|s| {
        let st = dir.join("steps").join(s).join("tests");
        bundled(&st) || bundled(&tests) || st.join(script).is_file() || tests.join(script).is_file()
    })
}
