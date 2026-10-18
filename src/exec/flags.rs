//! Flags mode: building an `ExecConfig` from CLI flags.

use super::*;
use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// EnvironmentType values (accepted for config parity; hbench only runs docker).
const ENVIRONMENT_TYPES: &[&str] = &[
    "docker",
    "podman",
    "daytona",
    "e2b",
    "modal",
    "runloop",
    "langsmith",
    "ec2",
    "gke",
    "ack",
    "openshift",
    "novita",
    "apple-container",
    "singularity",
    "islo",
    "tensorlake",
    "cwsandbox",
    "use-computer",
    "cua-cloud",
    "blaxel",
    "opensandbox",
    "beam",
    "skypilot",
    "hf-sandbox",
    "hyperbrowser",
    "vercel",
    "runta",
    "prime",
    "kata",
    "mosaic",
    "smol",
    "sail",
];

pub(super) fn config_from_flags(a: &ExecArgs) -> Result<ExecConfig> {
    let map_instructions = compile_instructions(a.instruction.as_deref(), a.instruction_path.as_deref())?;
    validate_map_task_source(&map_instructions, a.task_template.as_deref())?;

    let map_workdir = a.workdir.clone().unwrap_or_else(workdir_default);
    let reward = a.reward_artifact.as_deref().map(|r| prompt_file_to_artifact_path(r, &map_workdir));
    if reward.is_some() && a.disable_verification {
        bail!("--reward-artifact cannot be used with --disable-verification.");
    }
    let artifacts = with_reward_artifact(compile_artifacts(&a.artifact, a.instruction.as_deref(), &map_workdir), reward.as_deref());
    let verify = !a.disable_verification;
    let verifiers = compile_verifiers(&artifacts, verify, reward.as_deref());
    let tasks_output_dir = match &a.tasks_dir {
        Some(d) => NormPath::new(d),
        None => NormPath::new(&temp_root().join(format!("hbench-exec-map-tasks-{}", uuid::Uuid::new_v4().simple())).to_string_lossy()),
    };
    let timestamp = chrono::Local::now().format("%Y-%m-%d__%H-%M-%S").to_string();
    let mut retry = Map::new();
    if let Some(r) = a.max_retries {
        retry.insert("max_retries".into(), r.into());
    }
    let agents = agent_configs(a.agent.as_deref(), &a.models, &a.agent_kwargs, &a.agent_env, a.agent_timeout_sec)?;
    let environment = environment_config(a.environment.as_deref())?;
    let map_job = ExecJobConfig {
        job_name: Some(a.job_name.clone().unwrap_or_else(|| format!("{timestamp}-map"))),
        jobs_dir: Some(NormPath::new(a.jobs_dir.as_deref().unwrap_or("jobs"))),
        n_attempts: a.n_attempts.unwrap_or(1),
        n_concurrent_trials: a.n_concurrent.unwrap_or(4),
        quiet: a.quiet,
        retry,
        environment,
        verifier: verifier_config(!verifiers.is_empty(), a.task_template.as_deref(), verify),
        metrics: vec![],
        agents,
    };
    map_job.validate()?;

    let compile = CompileConfig {
        task_name_prefix: a.job_name.clone(),
        output_dir: Some(tasks_output_dir.clone()),
        instructions: map_instructions,
        task_template: a.task_template.as_deref().map(NormPath::new),
        artifacts: artifacts.into_iter().map(Value::String).collect(),
        environments: compile_environments(&a.paths, a.scan(), a.limit, a.image.as_deref(), a.workdir.as_deref())?,
        verifiers,
        ..Default::default()
    };
    let reduce = reduce_config_from_flags(a, verify, &format!("{timestamp}-reduce"), &map_job, &tasks_output_dir)?;
    let cfg = ExecConfig { schema_version: v1(), map: ExecMapConfig { compile, job: map_job }, reduce };
    cfg.validate()?;
    Ok(cfg)
}

fn reduce_config_from_flags(
    a: &ExecArgs,
    verify: bool,
    default_job_name: &str,
    map_job: &ExecJobConfig,
    output_dir: &NormPath,
) -> Result<Option<ExecReduceConfig>> {
    if !reduce_flags_passed(a) {
        return Ok(None);
    }
    let workdir = a.reduce_workdir.clone().unwrap_or_else(workdir_default);
    let reward = a.reduce_reward_artifact.as_deref().map(|r| prompt_file_to_artifact_path(r, &workdir));
    if reward.is_some() && !verify {
        bail!("--reduce-reward-artifact cannot be used with --disable-verification.");
    }
    let artifacts =
        with_reward_artifact(compile_artifacts(&a.reduce_artifact, a.reduce_instruction.as_deref(), &workdir), reward.as_deref());
    let verifier = compile_verifiers(&artifacts, verify, reward.as_deref()).into_iter().next();
    let instruction = match (&a.reduce_instruction, &a.reduce_instruction_path) {
        (Some(_), Some(_)) => bail!("--reduce-instruction and --reduce-instruction-path are mutually exclusive."),
        (Some(t), None) => CompileInstruction::text(t),
        (None, Some(p)) => CompileInstruction { text: None, path: Some(NormPath::new(p)) },
        (None, None) => bail!("Reducer requires --reduce-instruction/--reduce-prompt or --reduce-instruction-path."),
    };
    instruction.validate()?;
    let job = ExecJobConfig {
        job_name: Some(a.reduce_job_name.clone().unwrap_or_else(|| default_job_name.to_string())),
        jobs_dir: map_job.jobs_dir.clone(),
        n_attempts: a.reduce_n_attempts.unwrap_or(1),
        n_concurrent_trials: map_job.n_concurrent_trials,
        quiet: map_job.quiet,
        retry: map_job.retry.clone(),
        environment: map_job.environment.clone(),
        verifier: verifier_config(verifier.is_some(), a.reduce_task_template.as_deref(), verify),
        metrics: map_job.metrics.clone(),
        agents: reduce_agent_configs(a, &map_job.agents)?,
    };
    job.validate()?;
    Ok(Some(ExecReduceConfig {
        task: ExecReduceTaskConfig {
            task_name: reduce_name(),
            output_dir: output_dir.clone(),
            task_template: a.reduce_task_template.as_deref().map(NormPath::new),
            instruction,
            artifacts: artifacts.into_iter().map(Value::String).collect(),
            environment: ExecReduceEnvironment {
                docker_image: a.reduce_image.clone().unwrap_or_else(image_default),
                workdir: a.reduce_workdir.clone().unwrap_or_else(workdir_default),
            },
            verifier,
        },
        job,
    }))
}

fn compile_instructions(instruction: Option<&str>, path: Option<&str>) -> Result<Vec<CompileInstruction>> {
    let v = match (instruction, path) {
        (Some(_), Some(_)) => bail!("--instruction and --instruction-path are mutually exclusive."),
        (Some(t), None) => vec![CompileInstruction::text(t)],
        (None, Some(p)) => vec![CompileInstruction { text: None, path: Some(NormPath::new(p)) }],
        (None, None) => vec![],
    };
    for i in &v {
        i.validate()?;
    }
    Ok(v)
}

fn validate_map_task_source(instructions: &[CompileInstruction], template: Option<&str>) -> Result<()> {
    if !instructions.is_empty() {
        return Ok(());
    }
    let Some(t) = template else {
        bail!("Pass --instruction/--prompt, --instruction-path, or --task-template.");
    };
    let dir = PathBuf::from(expanduser(t));
    if dir.is_dir() && !dir.join("instruction.md").is_file() {
        bail!("--task-template must contain instruction.md when no --instruction or --instruction-path is provided.");
    }
    Ok(())
}

fn compile_artifacts(artifact: &[String], instruction: Option<&str>, workdir: &str) -> Vec<String> {
    if !artifact.is_empty() {
        return artifact.to_vec();
    }
    instruction.map(|i| infer_artifacts_from_prompt(i, workdir)).unwrap_or_default()
}

fn with_reward_artifact(mut artifacts: Vec<String>, reward: Option<&str>) -> Vec<String> {
    if let Some(r) = reward
        && !artifacts.iter().any(|a| a == r)
    {
        artifacts.push(r.to_string());
    }
    artifacts
}

pub fn infer_artifacts_from_prompt(prompt: &str, workdir: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for m in file_mentions(prompt) {
        let a = prompt_file_to_artifact_path(&m, workdir);
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out
}

pub(super) fn prompt_file_to_artifact_path(filename: &str, workdir: &str) -> String {
    let f = filename.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'']);
    if f.starts_with('/') {
        return normpath(f);
    }
    let wd = if workdir.starts_with('/') { workdir.to_string() } else { format!("/{workdir}") };
    normpath(&posix_join(&wd, f))
}

fn compile_environments(
    paths: &[String],
    scan: Option<bool>,
    limit: Option<u64>,
    image: Option<&str>,
    workdir: Option<&str>,
) -> Result<Vec<CompileEnvironment>> {
    let env = |p: Vec<String>| CompileEnvironment {
        path: None,
        paths: p,
        docker_image: Some(image.unwrap_or(DEFAULT_IMAGE).into()),
        workdir: Some(workdir.unwrap_or(DEFAULT_WORKDIR).into()),
    };
    if !should_scan_paths(paths, scan) {
        if limit.is_some() {
            bail!("--limit only applies to scanned paths.");
        }
        return Ok(vec![env(paths.to_vec())]);
    }
    if paths.is_empty() {
        bail!("--scan requires at least one --path.");
    }
    Ok(scan_paths(paths, limit)?.into_iter().map(|p| env(vec![p])).collect())
}

fn should_scan_paths(paths: &[String], scan: Option<bool>) -> bool {
    if let Some(s) = scan {
        return s;
    }
    paths.len() == 1 && (has_magic(&paths[0]) || Path::new(&expanduser(&paths[0])).is_dir())
}

/// One entry per glob match / immediate subdirectory, deduped by resolved path.
pub fn scan_paths(values: &[String], limit: Option<u64>) -> Result<Vec<String>> {
    let mut out = vec![];
    let mut seen = std::collections::HashSet::new();
    for v in values {
        for p in scan_one_path(v)? {
            let resolved = std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(&p));
            if seen.insert(resolved) {
                out.push(p);
            }
        }
    }
    if out.is_empty() {
        bail!("--scan did not discover any paths.");
    }
    if let Some(l) = limit {
        out.truncate(l as usize);
    }
    Ok(out)
}

fn scan_one_path(value: &str) -> Result<Vec<String>> {
    if has_magic(value) {
        let mut matches: Vec<PathBuf> =
            glob_paths(&posix_normalize(&expanduser(value))).iter().map(|m| PathBuf::from(posix_normalize(m))).collect();
        matches.sort();
        if matches.is_empty() {
            bail!("Path pattern did not match anything: {value}");
        }
        return Ok(matches.into_iter().map(|p| p.to_string_lossy().into_owned()).collect());
    }
    let base = posix_normalize(&expanduser(value));
    let path = Path::new(&base);
    if path.is_dir() {
        let mut kids: Vec<PathBuf> = std::fs::read_dir(path)?
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| if base == "." { PathBuf::from(e.file_name()) } else { path.join(e.file_name()) })
            .collect();
        kids.sort();
        if kids.is_empty() {
            bail!("Path directory has no subdirectories to scan: {value}");
        }
        return Ok(kids.into_iter().map(|p| p.to_string_lossy().into_owned()).collect());
    }
    if path.exists() {
        return Ok(vec![base]);
    }
    bail!("Path does not exist: {value}")
}

fn compile_verifiers(artifacts: &[String], verify: bool, reward: Option<&str>) -> Vec<CompileVerifier> {
    if verify && (!artifacts.is_empty() || reward.is_some()) {
        return vec![CompileVerifier {
            path: None,
            auto_verifier: Some(AutoVerifier {
                required_artifacts: Some(artifacts.to_vec()),
                reward_artifact: reward.map(String::from),
                artifact_json_schemas: Map::new(),
            }),
        }];
    }
    vec![]
}

/// `key=value` kwargs: JSON values, Python literals True/False/None, else strings.
fn parse_kwargs(items: &[String]) -> Result<Map<String, Value>> {
    let mut m = Map::new();
    for kw in items {
        let (k, v) = kw.split_once('=').ok_or_else(|| anyhow!("Invalid kwarg format: {kw}. Expected key=value"))?;
        let v = v.trim();
        let val = serde_json::from_str(v).unwrap_or_else(|_| match v {
            "True" => Value::Bool(true),
            "False" => Value::Bool(false),
            "None" => Value::Null,
            _ => Value::String(v.to_string()),
        });
        m.insert(k.trim().to_string(), val);
    }
    Ok(m)
}

fn parse_env_vars(items: &[String]) -> Result<Map<String, Value>> {
    let mut m = Map::new();
    for it in items {
        let (k, v) = it.split_once('=').ok_or_else(|| anyhow!("Invalid env var format: {it}. Expected KEY=VALUE"))?;
        m.insert(k.trim().to_string(), Value::String(v.trim().to_string()));
    }
    Ok(m)
}

fn agent_configs(
    agent: Option<&str>,
    models: &[String],
    kwargs: &[String],
    env: &[String],
    timeout: Option<f64>,
) -> Result<Vec<Map<String, Value>>> {
    if agent.is_none() && (!models.is_empty() || !kwargs.is_empty() || !env.is_empty()) {
        bail!("--agent is required when passing agent-specific flags.");
    }
    let kwargs = parse_kwargs(kwargs)?;
    let env = parse_env_vars(env)?;
    let build = |model: Option<&String>| {
        let mut m = Map::new();
        if let Some(a) = agent {
            m.insert("name".into(), a.into());
        }
        if let Some(model) = model {
            m.insert("model_name".into(), model.clone().into());
        }
        if let Some(t) = timeout {
            m.insert("override_timeout_sec".into(), t.into());
        }
        if agent.is_some() {
            m.insert("kwargs".into(), Value::Object(kwargs.clone()));
            m.insert("env".into(), Value::Object(env.clone()));
        }
        m
    };
    if agent.is_some() && !models.is_empty() {
        return Ok(models.iter().map(|m| build(Some(m))).collect());
    }
    Ok(vec![build(None)])
}

fn reduce_agent_configs(a: &ExecArgs, map_agents: &[Map<String, Value>]) -> Result<Vec<Map<String, Value>>> {
    let inherited: Vec<Map<String, Value>> = if map_agents.is_empty() { vec![Map::new()] } else { map_agents.to_vec() };
    if !reduce_agent_flags_passed(a) {
        return Ok(inherited);
    }
    let kwargs = (!a.reduce_agent_kwargs.is_empty()).then(|| parse_kwargs(&a.reduce_agent_kwargs)).transpose()?;
    let env = (!a.reduce_agent_env.is_empty()).then(|| parse_env_vars(&a.reduce_agent_env)).transpose()?;
    let mut models: Vec<Value> = if a.reduce_models.is_empty() {
        map_agents.iter().map(|m| m.get("model_name").cloned().unwrap_or(Value::Null)).collect()
    } else {
        a.reduce_models.iter().map(|m| Value::String(m.clone())).collect()
    };
    if models.is_empty() {
        models.push(Value::Null);
    }
    Ok(models
        .into_iter()
        .enumerate()
        .map(|(i, model)| {
            let mut c = inherited[i % inherited.len()].clone();
            c.insert("model_name".into(), model);
            if let Some(n) = &a.reduce_agent {
                c.insert("name".into(), n.clone().into());
            }
            if let Some(k) = &kwargs {
                c.insert("kwargs".into(), Value::Object(k.clone()));
            }
            if let Some(e) = &env {
                c.insert("env".into(), Value::Object(e.clone()));
            }
            c
        })
        .collect())
}

fn environment_config(spec: Option<&str>) -> Result<Map<String, Value>> {
    let mut m = Map::new();
    let Some(s) = spec else { return Ok(m) };
    if s.contains(':') {
        m.insert("import_path".into(), s.into());
    } else if ENVIRONMENT_TYPES.contains(&s) {
        m.insert("type".into(), s.into());
    } else {
        bail!(
            "Invalid value: Unknown environment '{s}'. Valid types: {}. Pass a custom environment as an import path (module.path:ClassName).",
            ENVIRONMENT_TYPES.join(", ")
        );
    }
    Ok(m)
}

fn verifier_config(has_compile_verifier: bool, template: Option<&str>, verify: bool) -> Map<String, Value> {
    let template_has_tests = template.is_some_and(|t| Path::new(&expanduser(t)).join("tests").is_dir());
    let mut m = Map::new();
    if !verify || (!has_compile_verifier && !template_has_tests) {
        m.insert("disable".into(), true.into());
    }
    m
}
