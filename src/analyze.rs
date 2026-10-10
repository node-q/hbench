//! `hbench analyze` / `hbench check` (upstream's `hbench analyze` / `hbench check`).
//!
//! Each trial (analyze) or task (check) becomes an ephemeral wrapper task built from
//! a template: a prebuilt-image task that uploads a copy of the trial/task to the
//! workdir, asks an agent to evaluate it against a rubric and write a JSON result,
//! and verifies that result against the rubric (reward 1.0 = valid result). All
//! wrappers run as one ordinary hbench job; the results are then aggregated into a
//! report that is printed and saved in the job dir.

use crate::config::{AgentConfig, EnvConfig, JobConfig, TaskRef};
use crate::task::Task;
use crate::trial::TrialResult;
use anyhow::{Context, Result, bail};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const ANALYZE_PROMPT: &str = include_str!("../assets/analyze/prompts/analyze.txt");
const ANALYZE_OUTPUT_PROMPT: &str = include_str!("../assets/analyze/prompts/analyze-output.txt");
const ANALYZE_RUBRIC: &str = include_str!("../assets/analyze/prompts/analyze-rubric.toml");
const CHECK_PROMPT: &str = include_str!("../assets/analyze/prompts/check.txt");
const CHECK_OUTPUT_PROMPT: &str = include_str!("../assets/analyze/prompts/check-output.txt");
const CHECK_RUBRIC: &str = include_str!("../assets/analyze/default-rubric.toml");

/// A wrapper-task template (task.toml + verifier) and its output contract.
struct Template {
    task_toml: &'static str,
    test_sh: &'static str,
    validate_py: &'static str,
    output_prompt: &'static str,
    result_filename: &'static str,
    /// "Analyze" / "Check", used in error messages.
    label: &'static str,
    model_hint: &'static str,
}

const ANALYZE: Template = Template {
    task_toml: include_str!("../assets/analyze/analyze-task-template/task.toml"),
    test_sh: include_str!("../assets/analyze/analyze-task-template/tests/test.sh"),
    validate_py: include_str!("../assets/analyze/analyze-task-template/tests/validate.py"),
    output_prompt: ANALYZE_OUTPUT_PROMPT,
    result_filename: "analysis.json",
    label: "Analyze",
    model_hint: "-m claude-sonnet-4-6",
};

const CHECK: Template = Template {
    task_toml: include_str!("../assets/analyze/check-task-template/task.toml"),
    test_sh: include_str!("../assets/analyze/check-task-template/tests/test.sh"),
    validate_py: include_str!("../assets/analyze/check-task-template/tests/validate.py"),
    output_prompt: CHECK_OUTPUT_PROMPT,
    result_filename: "check-result.json",
    label: "Check",
    model_hint: "-m sonnet or -m opus",
};

// ---------------------------------------------------------------- rubric / schema

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RubricCriterion {
    pub name: String,
    pub description: String,
    pub guidance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rubric {
    pub criteria: Vec<RubricCriterion>,
}

/// Load a rubric from a TOML, YAML, or JSON file (by suffix); `default` TOML when no path.
pub fn load_rubric(path: Option<&Path>, default: &str) -> Result<Rubric> {
    let Some(path) = path else { return Ok(toml::from_str(default)?) };
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let suffix = path.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default();
    let r = match suffix.as_str() {
        ".toml" => toml::from_str(&text)?,
        ".yaml" | ".yml" => serde_yaml_ng::from_str(&text)?,
        ".json" => serde_json::from_str(&text)?,
        _ => bail!("Unsupported rubric format '{suffix}'. Use .toml, .yaml, .yml, or .json."),
    };
    Ok(r)
}

pub fn criteria_guidance(r: &Rubric) -> String {
    r.criteria.iter().map(|c| format!("- {}: {}", c.name, c.guidance)).collect::<Vec<_>>().join("\n")
}

/// Pydantic-style object schema (omits `required` when empty, like `model_json_schema`).
fn object_schema(title: &str, props: Vec<(String, Value)>) -> Map<String, Value> {
    let required: Vec<Value> = props.iter().map(|(k, _)| Value::String(k.clone())).collect();
    let mut o = Map::new();
    o.insert("properties".into(), Value::Object(props.into_iter().collect()));
    if !required.is_empty() {
        o.insert("required".into(), Value::Array(required));
    }
    o.insert("title".into(), title.into());
    o.insert("type".into(), "object".into());
    o
}

fn criteria_props(r: &Rubric) -> Vec<(String, Value)> {
    r.criteria.iter().map(|c| (c.name.clone(), json!({"$ref": "#/$defs/QualityCheckModel"}))).collect()
}

/// `$defs` first, then the root object, matching pydantic's key order.
fn with_defs(mut defs: Vec<(&str, Value)>, root: Map<String, Value>, uses_check_model: bool) -> Value {
    if uses_check_model {
        defs.push(("CheckOutcome", json!({"enum": ["pass", "fail", "not_applicable"], "title": "CheckOutcome", "type": "string"})));
        defs.push((
            "QualityCheckModel",
            json!({
                "properties": {"explanation": {"title": "Explanation", "type": "string"}, "outcome": {"$ref": "#/$defs/CheckOutcome"}},
                "required": ["explanation", "outcome"],
                "title": "QualityCheckModel",
                "type": "object"
            }),
        ));
    }
    let mut o = Map::new();
    if !defs.is_empty() {
        defs.sort_by(|a, b| a.0.cmp(b.0));
        o.insert("$defs".into(), Value::Object(defs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()));
    }
    o.extend(root);
    Value::Object(o)
}

/// JSON schema of upstream's `QualityCheckResponse` (check output).
pub fn check_schema(r: &Rubric) -> Value {
    with_defs(vec![], object_schema("QualityCheckResponse", criteria_props(r)), !r.criteria.is_empty())
}

/// JSON schema of upstream's `AnalyzeResultResponse` (summary + checks).
pub fn analyze_schema(r: &Rubric) -> Value {
    let checks = object_schema("AnalyzeResultChecks", criteria_props(r));
    let root = object_schema(
        "AnalyzeResultResponse",
        vec![("summary".into(), json!({"title": "Summary", "type": "string"})), ("checks".into(), json!({"$ref": "#/$defs/AnalyzeResultChecks"}))],
    );
    with_defs(vec![("AnalyzeResultChecks", Value::Object(checks))], root, !r.criteria.is_empty())
}

// ---------------------------------------------------------------- results

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    Fail,
    NotApplicable,
}

/// upstream's QualityCheckModel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Check {
    pub explanation: String,
    pub outcome: Outcome,
}

/// Criterion name -> check, in rubric order.
pub type Checks = Vec<(String, Check)>;

fn ser_checks<S: serde::Serializer>(c: &Checks, s: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut m = s.serialize_map(Some(c.len()))?;
    for (k, v) in c {
        m.serialize_entry(k, v)?;
    }
    m.end()
}

/// One trial's analysis in a report (upstream's AnalyzeReportResult).
#[derive(Debug, Clone, Default, Serialize)]
pub struct AnalyzeReportResult {
    pub trial_name: Option<String>,
    pub summary: Option<String>,
    #[serde(serialize_with = "ser_checks")]
    pub checks: Checks,
    pub cost_usd: Option<f64>,
    pub error: Option<String>,
}

/// One task's check in a report (upstream's QualityCheckResult).
#[derive(Debug, Clone, Default, Serialize)]
pub struct QualityCheckResult {
    #[serde(serialize_with = "ser_checks")]
    pub checks: Checks,
    pub cost_usd: Option<f64>,
    pub task_name: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize)]
struct Report<'a, T> {
    results: &'a [T],
}

/// Validate `checks` against the rubric (all criteria present, valid outcomes), in rubric order.
fn parse_checks(v: &Value, r: &Rubric) -> Result<Checks, String> {
    let obj = v.as_object().ok_or("checks: input should be an object")?;
    r.criteria
        .iter()
        .map(|c| {
            let raw = obj.get(&c.name).ok_or_else(|| format!("{}: field required", c.name))?;
            let check: Check = serde_json::from_value(raw.clone()).map_err(|e| format!("{}: {e}", c.name))?;
            Ok((c.name.clone(), check))
        })
        .collect()
}

/// Read the agent's validated result JSON from a finished wrapper trial (+ its cost).
fn extract_result(r: &TrialResult, trial_dir: &Path, t: &Template) -> Result<(Value, Option<f64>), String> {
    let label = t.label;
    if let Some(e) = &r.exception_info {
        return Err(format!("{label} trial failed with {}: {}\nTrial artifacts: {}", e.exception_type, e.exception_message, trial_dir.display()));
    }
    let result_path = trial_dir.join("artifacts").join(t.result_filename);
    let reward_one = r.reward().and_then(|rw| rw.get("reward")).and_then(|n| n.as_f64()) == Some(1.0);
    if !reward_one || !result_path.exists() {
        let stdout = trial_dir.join("verifier/test-stdout.txt");
        let reasons = std::fs::read_to_string(&stdout).map(|s| s.trim().to_string()).unwrap_or_else(|_| "no verifier output".into());
        return Err(format!(
            "{label} agent did not produce a valid result.\nVerifier output:\n{reasons}\nTrial artifacts: {}\nTry again or use a more capable model ({}).",
            trial_dir.display(),
            t.model_hint
        ));
    }
    let text = std::fs::read_to_string(&result_path).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("invalid JSON in {}: {e}", result_path.display()))?;
    Ok((v, r.usage_totals().3))
}

fn extract_analyze_result(r: &TrialResult, trial_dir: &Path, rubric: &Rubric) -> Result<AnalyzeReportResult, String> {
    let (v, cost_usd) = extract_result(r, trial_dir, &ANALYZE)?;
    let summary = v.get("summary").and_then(Value::as_str).ok_or("summary: input should be a valid string")?;
    let checks = parse_checks(v.get("checks").unwrap_or(&Value::Null), rubric)?;
    Ok(AnalyzeReportResult { summary: Some(summary.into()), checks, cost_usd, ..Default::default() })
}

fn extract_check_result(r: &TrialResult, trial_dir: &Path, rubric: &Rubric) -> Result<QualityCheckResult, String> {
    let (v, cost_usd) = extract_result(r, trial_dir, &CHECK)?;
    Ok(QualityCheckResult { checks: parse_checks(&v, rubric)?, cost_usd, ..Default::default() })
}

/// Write analysis.json (upstream's AnalyzeResult) into the analyzed trial dir.
fn write_analysis_json(trial_dir: &Path, r: &AnalyzeReportResult) -> Result<()> {
    #[derive(Serialize)]
    struct AnalyzeResult<'a> {
        trial_name: String,
        summary: &'a str,
        #[serde(serialize_with = "ser_checks")]
        checks: &'a Checks,
        estimated_cost_usd: Option<f64>,
    }
    let out = AnalyzeResult {
        trial_name: r.trial_name.clone().unwrap_or_else(|| dir_name(trial_dir)),
        summary: r.summary.as_deref().unwrap_or(""),
        checks: &r.checks,
        estimated_cost_usd: r.cost_usd,
    };
    std::fs::write(trial_dir.join("analysis.json"), serde_json::to_string_pretty(&out)?)?;
    Ok(())
}

// ---------------------------------------------------------------- dir resolution

fn dir_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn is_trial_dir(p: &Path) -> bool {
    p.join("trial.log").exists()
}

pub fn is_job_dir(p: &Path) -> bool {
    p.join("job.log").exists()
}

fn read_result_json(trial_dir: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(trial_dir.join("result.json")).ok()?).ok()
}

/// reward == 1.0 and no exception. Unreadable result.json counts as failing.
fn is_passing(trial_dir: &Path) -> bool {
    read_result_json(trial_dir)
        .is_some_and(|r| r["verifier_result"]["rewards"]["reward"].as_f64() == Some(1.0) && r["exception_info"].is_null())
}

/// Trial dirs of a job dir (sorted); `filter_passing`: Some(true) passing only, Some(false) failing only.
pub fn get_trial_dirs(job_dir: &Path, filter_passing: Option<bool>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(job_dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir() && is_trial_dir(p)).collect();
    dirs.sort();
    match filter_passing {
        None => dirs,
        Some(want) => dirs.into_iter().filter(|d| is_passing(d) == want).collect(),
    }
}

pub fn resolve_trial_dirs(path: &Path, filter_passing: Option<bool>, n_trials: Option<usize>) -> Result<Vec<PathBuf>> {
    if is_trial_dir(path) {
        return Ok(vec![path.to_path_buf()]);
    }
    if !is_job_dir(path) {
        bail!(
            "'{}' is not a trial directory (no trial.log) or job directory (no job.log). For task quality checks, use 'hbench check <task-dir>' instead.",
            path.display()
        );
    }
    let mut dirs = get_trial_dirs(path, filter_passing);
    if let Some(n) = n_trials {
        dirs.truncate(n);
    }
    if dirs.is_empty() {
        let q = match filter_passing {
            Some(true) => "passing ",
            Some(false) => "failing ",
            None => "",
        };
        bail!("No {q}trial directories found in '{}'", path.display());
    }
    Ok(dirs)
}

/// Task dir of a trial, from its result.json config (local path or git cache path).
fn extract_task_dir(trial_dir: &Path) -> Option<PathBuf> {
    let r = read_result_json(trial_dir)?;
    let t: TaskRef = serde_json::from_value(r.get("config")?.get("task")?.clone()).ok()?;
    t.path.as_ref()?;
    Some(crate::registry::local_path(&t))
}

/// upstream's `Task.is_valid_dir`: task.toml + environment/, loadable config, a test script.
// ponytail: hbench's Task::load also wants instruction.md and an environment definition.
pub fn is_valid_task_dir(p: &Path) -> bool {
    p.join("task.toml").is_file() && p.join("environment").is_dir() && Task::load(p).and_then(|t| t.validate_tests()).is_ok()
}

pub fn resolve_task_dirs(path: &Path, include: &[String], exclude: &[String], n_tasks: Option<usize>) -> Result<Vec<PathBuf>> {
    if is_valid_task_dir(path) {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.is_dir() {
        bail!(
            "'{}' is not a valid task directory (missing instruction.md, task.toml, or tests/) or a directory of tasks. For trial analysis, use 'hbench analyze <trial-dir>' instead.",
            path.display()
        );
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(path)?.flatten().map(|e| e.path()).filter(|d| d.is_dir() && is_valid_task_dir(d)).collect();
    dirs.sort();
    if !include.is_empty() {
        dirs.retain(|d| include.iter().any(|p| crate::glob_match(p, &dir_name(d))));
    }
    if !exclude.is_empty() {
        dirs.retain(|d| !exclude.iter().any(|p| crate::glob_match(p, &dir_name(d))));
    }
    if let Some(n) = n_tasks {
        dirs.truncate(n);
    }
    if dirs.is_empty() {
        bail!("No valid task directories found in '{}'", path.display());
    }
    Ok(dirs)
}

// ---------------------------------------------------------------- wrapper tasks

/// shutil.copytree(src, dst, ignore=ignore_patterns(*ignore)): names ignored at every level.
fn copy_tree(src: &Path, dst: &Path, ignore: &[&str]) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name();
        if ignore.iter().any(|i| name == *i) {
            continue;
        }
        let (s, d) = (e.path(), dst.join(&name));
        if s.is_dir() {
            copy_tree(&s, &d, ignore)?;
        } else {
            std::fs::copy(&s, &d).with_context(|| format!("copying {}", s.display()))?;
        }
    }
    Ok(())
}

/// Sorted relative paths of all files under `dir` (Python's sorted(rglob) order).
pub fn build_file_tree(dir: &Path) -> String {
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
    let mut files = vec![];
    walk(dir, &mut files);
    files.sort();
    let lines: Vec<String> = files.iter().map(|p| p.strip_prefix(dir).unwrap().to_string_lossy().into_owned()).collect();
    if lines.is_empty() { "No files found".into() } else { lines.join("\n") }
}

/// Python `str.format_map(defaultdict(str, vars))`: `{name}` substituted (missing -> ""), `{{`/`}}` escapes.
pub fn py_format(t: &str, vars: &[(&str, &str)]) -> Result<String> {
    let mut out = String::new();
    let mut it = t.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '{' if it.peek() == Some(&'{') => {
                it.next();
                out.push('{');
            }
            '}' if it.peek() == Some(&'}') => {
                it.next();
                out.push('}');
            }
            '{' => {
                let mut field = String::new();
                loop {
                    match it.next() {
                        Some('}') => break,
                        Some(ch) => field.push(ch),
                        None => bail!("expected '}}' before end of string"),
                    }
                }
                let name = field.split(['.', '[', ':', '!']).next().unwrap_or("");
                out.push_str(vars.iter().find(|(k, _)| *k == name).map_or("", |(_, v)| v));
            }
            '}' => bail!("Single '}}' encountered in format string"),
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Write the template skeleton (task.toml, tests/) into a fresh `dest`; returns the workdir.
fn write_skeleton(dest: &Path, t: &Template, rubric: &Rubric) -> Result<String> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::create_dir_all(dest.join("environment"))?;
    std::fs::create_dir_all(dest.join("tests"))?;
    std::fs::write(dest.join("task.toml"), t.task_toml)?;
    std::fs::write(dest.join("tests/test.sh"), t.test_sh)?;
    std::fs::write(dest.join("tests/validate.py"), t.validate_py)?;
    let names: Vec<&str> = rubric.criteria.iter().map(|c| c.name.as_str()).collect();
    std::fs::write(dest.join("tests/criteria.json"), serde_json::to_string_pretty(&names)?)?;
    let cfg: toml::Table = toml::from_str(t.task_toml)?;
    let wd = cfg.get("environment").and_then(|e| e.get("workdir")).and_then(|w| w.as_str()).filter(|w| !w.is_empty()).unwrap_or("/");
    Ok(wd.to_string())
}

fn posix_join(base: &str, name: &str) -> String {
    format!("{}/{name}", base.trim_end_matches('/'))
}

fn write_instruction(dest: &Path, rendered: &str, t: &Template, schema: &Value) -> Result<()> {
    let output = py_format(t.output_prompt, &[("result_filename", t.result_filename), ("output_schema", &serde_json::to_string_pretty(schema)?)])?;
    std::fs::write(dest.join("instruction.md"), format!("{}\n\n{}\n", rendered.trim_end(), output.trim()))?;
    Ok(())
}

/// Wrapper task analyzing `trial_dir` (and its task, when available locally).
pub fn assemble_analyze_task(trial_dir: &Path, task_dir: Option<&Path>, rubric: &Rubric, template: &str, schema: &Value, dest: &Path) -> Result<PathBuf> {
    let workdir = write_skeleton(dest, &ANALYZE, rubric)?;
    // exclude a prior run's analysis so the agent can't read its own output
    copy_tree(trial_dir, &dest.join("environment/trial"), &[".git", "analysis.json", "analysis.md"])?;
    if let Some(td) = task_dir {
        copy_tree(td, &dest.join("environment/task"), &[".git"])?;
    }
    let trial_path = posix_join(&workdir, "trial");
    let task_section = match task_dir {
        Some(_) => {
            let p = posix_join(&workdir, "task");
            format!(
                "The task being attempted is at {p}. Read its files first to understand the requirements:\n\
                 - {p}/instruction.md — what the agent was asked to do\n\
                 - {p}/task.toml — task configuration\n\
                 - {p}/tests/ — tests the agent's work was verified against\n\
                 - {p}/solution/ — reference solution (if present)"
            )
        }
        None => "The task definition is not available. Use the trajectory and test output to infer what the task required.".into(),
    };
    let rendered = py_format(template, &[("trial_path", &trial_path), ("task_section", &task_section), ("criteria_guidance", &criteria_guidance(rubric))])?;
    write_instruction(dest, &rendered, &ANALYZE, schema)?;
    Ok(dest.to_path_buf())
}

/// Wrapper task reviewing `task_dir`.
pub fn assemble_check_task(task_dir: &Path, rubric: &Rubric, template: &str, schema: &Value, dest: &Path) -> Result<PathBuf> {
    let workdir = write_skeleton(dest, &CHECK, rubric)?;
    let review = dest.join("environment/task");
    copy_tree(task_dir, &review, &[".git"])?;
    let rendered = py_format(
        template,
        &[("file_tree", &build_file_tree(&review)), ("criteria_guidance", &criteria_guidance(rubric)), ("task_path", &posix_join(&workdir, "task"))],
    )?;
    write_instruction(dest, &rendered, &CHECK, schema)?;
    Ok(dest.to_path_buf())
}

// ---------------------------------------------------------------- CLI

#[derive(Args, Debug)]
pub struct CommonArgs {
    /// Rubric file (TOML/YAML/JSON). Uses the built-in default if not specified.
    #[arg(short = 'r', long)]
    rubric: Option<PathBuf>,
    /// Prompt file for the evaluator agent. Uses the built-in default if not specified.
    #[arg(short = 'p', long)]
    prompt: Option<PathBuf>,
    /// Agent to use.
    #[arg(short = 'a', long, default_value = "claude-code")]
    agent: String,
    /// Agent kwarg key=value (repeatable).
    #[arg(long = "ak", alias = "agent-kwarg", value_parser = crate::parse_kv)]
    agent_kwargs: Vec<(String, String)>,
    /// Env var KEY=VALUE for the agent (repeatable).
    #[arg(long = "ae", alias = "agent-env", value_parser = crate::parse_kv)]
    agent_env: Vec<(String, String)>,
    /// Environment type to run in.
    #[arg(short = 'e', long = "env", default_value = "docker")]
    env: String,
    /// Environment kwarg key=value (repeatable).
    #[arg(long = "ek", alias = "environment-kwarg", value_parser = crate::parse_kv)]
    environment_kwargs: Vec<(String, String)>,
    /// Max concurrent evaluations.
    #[arg(short = 'n', long = "n-concurrent", default_value_t = 4)]
    n_concurrent: usize,
    /// Attempts per trial/task.
    #[arg(short = 'k', long = "n-attempts", default_value_t = 1)]
    n_attempts: usize,
    /// Job name (default: timestamp).
    #[arg(long)]
    job_name: Option<String>,
    /// Directory to store job results (default: jobs).
    #[arg(short = 'o', long)]
    jobs_dir: Option<PathBuf>,
    /// Base JobConfig (YAML/JSON) for advanced settings.
    #[arg(short = 'c', long)]
    config: Option<PathBuf>,
    /// Suppress trial progress.
    #[arg(short = 'q', long)]
    quiet: bool,
}

#[derive(Args, Debug)]
pub struct AnalyzeArgs {
    /// Path to a trial directory or a job directory of trials.
    path: PathBuf,
    /// Model to use.
    #[arg(short = 'm', long, default_value = "claude-haiku-4-5")]
    model: String,
    /// Only analyze passing trials (reward=1.0).
    #[arg(long)]
    passing: bool,
    /// Only analyze failing trials (reward<1.0 or exception).
    #[arg(long)]
    failing: bool,
    /// Max trials to analyze.
    #[arg(short = 'l', long = "n-trials")]
    n_trials: Option<usize>,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Args, Debug)]
pub struct CheckArgs {
    /// Path to a task directory or a directory of task directories.
    path: PathBuf,
    /// Model to use.
    #[arg(short = 'm', long, default_value = "claude-sonnet-4-6")]
    model: String,
    /// Only check tasks matching glob (repeatable).
    #[arg(short = 'i', long = "include-task-name")]
    include: Vec<String>,
    /// Skip tasks matching glob (repeatable).
    #[arg(short = 'x', long = "exclude-task-name")]
    exclude: Vec<String>,
    /// Max tasks to check.
    #[arg(short = 'l', long = "n-tasks")]
    n_tasks: Option<usize>,
    #[command(flatten)]
    common: CommonArgs,
}

/// Job config for the wrapper tasks: the base config (-c) with the CLI's settings on top.
fn wrapper_job_config(wrappers: &[PathBuf], o: &CommonArgs, model: &str) -> Result<JobConfig> {
    let mut cfg = match &o.config {
        Some(p) => JobConfig::load(p)?,
        None => JobConfig::default(),
    };
    if let Some(d) = &o.jobs_dir {
        cfg.jobs_dir = d.clone();
    }
    if let Some(n) = &o.job_name {
        cfg.job_name = n.clone();
    }
    cfg.n_concurrent_trials = o.n_concurrent;
    cfg.n_attempts = o.n_attempts;
    cfg.quiet = o.quiet;
    cfg.agents = vec![AgentConfig {
        name: Some(o.agent.clone()),
        model_name: Some(model.into()),
        kwargs: o.agent_kwargs.iter().map(|(k, v)| (k.clone(), crate::kwarg_value(v))).collect(),
        env: o.agent_env.iter().cloned().collect(),
        ..Default::default()
    }];
    cfg.environment = EnvConfig { kind: Some(o.env.clone()), kwargs: o.environment_kwargs.iter().map(|(k, v)| (k.clone(), crate::kwarg_value(v))).collect(), ..Default::default() };
    cfg.tasks = wrappers.iter().map(|w| TaskRef::local(w.clone(), None)).collect();
    cfg.datasets = vec![];
    cfg.validate()?;
    Ok(cfg)
}

/// Run all wrappers as one job; returns its trial results (sorted by dir) and job dir.
async fn run_wrapper_job(wrappers: &[PathBuf], o: &CommonArgs, model: &str) -> Result<(Vec<TrialResult>, PathBuf)> {
    let cfg = wrapper_job_config(wrappers, o, model)?;
    crate::preflight_agents(&cfg)?;
    let job_dir = cfg.job_dir();
    if job_dir.join("config.json").exists() {
        bail!("job {} already exists", job_dir.display());
    }
    crate::run_with_ctrl_c(cfg, None, &[]).await?;
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&job_dir)?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    let results = dirs.iter().filter_map(|d| serde_json::from_str(&std::fs::read_to_string(d.join("result.json")).ok()?).ok()).collect();
    Ok((results, job_dir))
}

fn temp_root(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()))
}

/// Canonical wrapper path -> source dir (trial dir or task dir).
fn key_of(r: &TrialResult) -> Option<PathBuf> {
    r.config.task.path.as_ref().map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
}

async fn run_analyze(a: &AnalyzeArgs, filter_passing: Option<bool>) -> Result<(Vec<AnalyzeReportResult>, PathBuf)> {
    if !a.path.exists() {
        bail!("Path '{}' does not exist", a.path.display());
    }
    let trial_dirs = resolve_trial_dirs(&a.path, filter_passing, a.n_trials)?;
    let rubric = load_rubric(a.common.rubric.as_deref(), ANALYZE_RUBRIC)?;
    let schema = analyze_schema(&rubric);
    let template = match &a.common.prompt {
        Some(p) => std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
        None => ANALYZE_PROMPT.into(),
    };
    let tmp = temp_root("hbench-analyze");
    let out = async {
        let mut source: HashMap<PathBuf, PathBuf> = HashMap::new();
        let mut wrappers = vec![];
        for td in &trial_dirs {
            let task_dir = extract_task_dir(td).filter(|p| p.exists());
            let dest = tmp.join(format!("analyze-{}", dir_name(&td.canonicalize()?)));
            let w = assemble_analyze_task(td, task_dir.as_deref(), &rubric, &template, &schema, &dest)?.canonicalize()?;
            source.insert(w.clone(), td.clone());
            wrappers.push(w);
        }
        let (trials, job_dir) = run_wrapper_job(&wrappers, &a.common, &a.model).await?;
        let mut results = vec![];
        for r in &trials {
            let src = key_of(r).and_then(|k| source.get(&k));
            let name = src.map(|s| dir_name(s)).unwrap_or_else(|| r.trial_name.clone());
            let res = match extract_analyze_result(r, &job_dir.join(&r.trial_name), &rubric) {
                Ok(mut x) => {
                    x.trial_name = Some(name);
                    if let Some(s) = src {
                        write_analysis_json(s, &x)?;
                    }
                    x
                }
                Err(e) => AnalyzeReportResult { trial_name: Some(name), error: Some(e), ..Default::default() },
            };
            results.push(res);
        }
        results.sort_by(|x, y| x.trial_name.cmp(&y.trial_name));
        std::fs::write(job_dir.join("analysis.json"), serde_json::to_string_pretty(&Report { results: &results })?)?;
        Ok((results, job_dir))
    }
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
    out
}

async fn run_checks(a: &CheckArgs) -> Result<(Vec<QualityCheckResult>, PathBuf)> {
    if !a.path.exists() {
        bail!("Path '{}' does not exist", a.path.display());
    }
    let task_dirs = resolve_task_dirs(&a.path, &a.include, &a.exclude, a.n_tasks)?;
    let rubric = load_rubric(a.common.rubric.as_deref(), CHECK_RUBRIC)?;
    let schema = check_schema(&rubric);
    let template = match &a.common.prompt {
        Some(p) => std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
        None => CHECK_PROMPT.into(),
    };
    let tmp = temp_root("hbench-check");
    let out = async {
        let mut names: HashMap<PathBuf, String> = HashMap::new();
        let mut wrappers = vec![];
        for td in &task_dirs {
            let dest = tmp.join(format!("check-{}", dir_name(&td.canonicalize()?)));
            let w = assemble_check_task(td, &rubric, &template, &schema, &dest)?.canonicalize()?;
            names.insert(w.clone(), dir_name(td));
            wrappers.push(w);
        }
        let (trials, job_dir) = run_wrapper_job(&wrappers, &a.common, &a.model).await?;
        let mut results: Vec<QualityCheckResult> = trials
            .iter()
            .map(|r| {
                let name = key_of(r).and_then(|k| names.get(&k).cloned()).unwrap_or_else(|| r.trial_name.clone());
                match extract_check_result(r, &job_dir.join(&r.trial_name), &rubric) {
                    Ok(x) => QualityCheckResult { task_name: Some(name), ..x },
                    Err(e) => QualityCheckResult { task_name: Some(name), error: Some(e), ..Default::default() },
                }
            })
            .collect();
        results.sort_by(|x, y| x.task_name.cmp(&y.task_name));
        std::fs::write(job_dir.join("check_report.json"), serde_json::to_string_pretty(&Report { results: &results })?)?;
        Ok((results, job_dir))
    }
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
    out
}

// ---------------------------------------------------------------- report rendering

fn outcome_str(o: Outcome) -> &'static str {
    match o {
        Outcome::Pass => "pass",
        Outcome::Fail => "fail",
        Outcome::NotApplicable => "not_applicable",
    }
}

/// Python's `str.title()` after replacing underscores with spaces.
fn title_case(name: &str) -> String {
    let mut out = String::new();
    let mut prev_alpha = false;
    for c in name.replace('_', " ").chars() {
        out.extend(if prev_alpha { c.to_lowercase().collect::<Vec<_>>() } else { c.to_uppercase().collect() });
        prev_alpha = c.is_alphabetic();
    }
    out
}

fn render_table(title: &str, headers: &[&str], rows: &[Vec<String>]) -> String {
    let rows: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(|c| c.replace('\n', " ")).collect()).collect();
    let widths: Vec<usize> = (0..headers.len()).map(|i| rows.iter().map(|r| r[i].chars().count()).chain([headers[i].chars().count()]).max().unwrap_or(0)).collect();
    let line = |cells: Vec<&str>| cells.iter().zip(&widths).map(|(c, w)| format!("{c:<w$}")).collect::<Vec<_>>().join(" | ").trim_end().to_string();
    let mut out = vec![title.to_string(), line(headers.to_vec()), widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-")];
    out.extend(rows.iter().map(|r| line(r.iter().map(String::as_str).collect())));
    out.join("\n")
}

fn checks_table(title: &str, checks: &Checks) -> String {
    let rows: Vec<Vec<String>> = checks.iter().map(|(n, c)| vec![title_case(n), outcome_str(c.outcome).into(), c.explanation.clone()]).collect();
    render_table(title, &["Check", "Outcome", "Explanation"], &rows)
}

/// One row per trial/task: (name, checks, cost, error) -> summary table + error lines + total cost.
fn summary_table(title: &str, first_col: &str, rows: &[(Option<&str>, &Checks, Option<f64>, Option<&str>)]) -> String {
    let table_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|(name, checks, cost, err)| {
            let name = name.unwrap_or("?").to_string();
            if err.is_some() {
                return vec![name, "-".into(), "-".into(), "-".into(), "-".into()];
            }
            let n = |o: Outcome| checks.iter().filter(|(_, c)| c.outcome == o).count().to_string();
            vec![name, n(Outcome::Pass), n(Outcome::Fail), n(Outcome::NotApplicable), cost.map_or("-".into(), |c| format!("{c:.4}"))]
        })
        .collect();
    let mut out = vec![render_table(title, &[first_col, "Pass", "Fail", "N/A", "Cost ($)"], &table_rows)];
    for (name, _, _, err) in rows {
        if let Some(e) = err {
            out.push(format!("❌ {}: {}", name.unwrap_or("None"), e.lines().next().unwrap_or("")));
        }
    }
    let costs: Vec<f64> = rows.iter().filter_map(|r| r.2).collect();
    if !costs.is_empty() {
        out.push(format!("Total agent cost: ${:.4}", costs.iter().sum::<f64>()));
    }
    out.join("\n")
}

fn fail(e: anyhow::Error) -> ! {
    println!("❌ {e:#}");
    std::process::exit(1)
}

pub async fn analyze_command(a: AnalyzeArgs) -> Result<()> {
    if a.passing && a.failing {
        fail(anyhow::anyhow!("Cannot use both --passing and --failing"));
    }
    let filter = if a.passing { Some(true) } else if a.failing { Some(false) } else { None };
    println!("\n🔍 Analyzing trial(s)...");
    let (results, job_dir) = run_analyze(&a, filter).await.unwrap_or_else(|e| fail(e));
    if let [r] = results.as_slice() {
        let name = r.trial_name.as_deref().unwrap_or("None");
        if let Some(e) = &r.error {
            println!("❌ {name}: {e}");
        } else {
            if let Some(s) = r.summary.as_deref().filter(|s| !s.is_empty()) {
                println!("\nSummary: {s}\n");
            }
            println!("{}", checks_table(&format!("Trial: {name}"), &r.checks));
            if let Some(c) = r.cost_usd {
                println!("Agent cost: ${c:.4}");
            }
        }
    } else {
        let rows: Vec<_> = results.iter().map(|r| (r.trial_name.as_deref(), &r.checks, r.cost_usd, r.error.as_deref())).collect();
        println!("{}", summary_table("Trial Analyses", "Trial", &rows));
    }
    let parent = job_dir.parent().unwrap_or(Path::new("."));
    println!("\nReport: {}", job_dir.join("analysis.json").display());
    println!("Inspect with `hbench view -p {}` — two ways to read the analysis:", parent.display());
    println!("  1. Open the analyzed job → a trial's files (analysis.json).");
    println!("  2. Open this job → a trial's Artifacts (analysis.json).");
    if results.iter().any(|r| r.error.is_some()) {
        std::process::exit(1);
    }
    Ok(())
}

pub async fn check_command(a: CheckArgs) -> Result<()> {
    println!("\n🔎 Checking task quality...");
    let (results, job_dir) = run_checks(&a).await.unwrap_or_else(|e| fail(e));
    if let [r] = results.as_slice() {
        let name = r.task_name.as_deref().unwrap_or("None");
        if let Some(e) = &r.error {
            println!("❌ {name}: {e}");
        } else {
            println!("{}", checks_table(&format!("Task Quality Checks: {name}"), &r.checks));
            if let Some(c) = r.cost_usd {
                println!("Agent cost: ${c:.4}");
            }
        }
    } else {
        let rows: Vec<_> = results.iter().map(|r| (r.task_name.as_deref(), &r.checks, r.cost_usd, r.error.as_deref())).collect();
        println!("{}", summary_table("Task Quality Checks", "Task", &rows));
    }
    println!("\nReport: {}", job_dir.join("check_report.json").display());
    println!("Inspect results by running `hbench view -p {}`", job_dir.parent().unwrap_or(Path::new(".")).display());
    println!("In the viewer, open a trial's Artifacts to read its check-result.json, and use ← / → to switch between tasks.");
    if results.iter().any(|r| r.error.is_some()) {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn tmp() -> PathBuf {
        let d = temp_root("hbench-analyze-test");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    fn make_task(root: &Path, name: &str) -> PathBuf {
        let d = root.join(name);
        write(&d.join("instruction.md"), "Do the thing.");
        write(&d.join("task.toml"), "");
        write(&d.join("environment/Dockerfile"), "FROM ubuntu:22.04\n");
        write(&d.join("tests/test.sh"), "#!/bin/bash\nexit 0");
        d
    }

    fn make_trial(root: &Path, name: &str, result: Value) -> PathBuf {
        let d = root.join(name);
        write(&d.join("trial.log"), "");
        write(&d.join("result.json"), &result.to_string());
        d
    }

    fn make_job(root: &Path) -> PathBuf {
        let j = root.join("job");
        write(&j.join("job.log"), "");
        for n in ["trial__aaa", "trial__bbb", "trial__ccc"] {
            make_trial(&j, n, json!({"task_name": "test"}));
        }
        j
    }

    fn default_rubric() -> Rubric {
        load_rubric(None, ANALYZE_RUBRIC).unwrap()
    }

    #[test]
    fn rubrics_load_from_all_formats() {
        let names = |r: &Rubric| r.criteria.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&default_rubric()), ["reward_hacking", "task_specification"]);
        assert_eq!(load_rubric(None, CHECK_RUBRIC).unwrap().criteria.len(), 11);
        let d = tmp();
        write(&d.join("r.toml"), "[[criteria]]\nname = \"custom_check\"\ndescription = \"A custom check\"\nguidance = \"Check custom things.\"\n");
        write(&d.join("r.YML"), "criteria:\n  - {name: y, description: d, guidance: g}\n");
        write(&d.join("r.json"), r#"{"criteria": [{"name": "j", "description": "d", "guidance": "g"}]}"#);
        write(&d.join("r.txt"), "");
        assert_eq!(names(&load_rubric(Some(&d.join("r.toml")), "").unwrap()), ["custom_check"]);
        assert_eq!(names(&load_rubric(Some(&d.join("r.YML")), "").unwrap()), ["y"]);
        assert_eq!(names(&load_rubric(Some(&d.join("r.json")), "").unwrap()), ["j"]);
        assert!(load_rubric(Some(&d.join("r.txt")), "").unwrap_err().to_string().contains("Unsupported rubric format '.txt'"));
        let g = criteria_guidance(&load_rubric(Some(&d.join("r.toml")), "").unwrap());
        assert_eq!(g, "- custom_check: Check custom things.");
    }

    #[test]
    fn schemas_match_pydantic() {
        // Generated with pydantic 2.14 model_json_schema() on upstream's models.
        let defs = r##""CheckOutcome": {"enum": ["pass", "fail", "not_applicable"], "title": "CheckOutcome", "type": "string"},
            "QualityCheckModel": {"properties": {"explanation": {"title": "Explanation", "type": "string"}, "outcome": {"$ref": "#/$defs/CheckOutcome"}},
                "required": ["explanation", "outcome"], "title": "QualityCheckModel", "type": "object"}"##;
        let props = r##""properties": {"reward_hacking": {"$ref": "#/$defs/QualityCheckModel"}, "task_specification": {"$ref": "#/$defs/QualityCheckModel"}},
            "required": ["reward_hacking", "task_specification"]"##;
        let check: Value = serde_json::from_str(&format!(r##"{{"$defs": {{{defs}}}, {props}, "title": "QualityCheckResponse", "type": "object"}}"##)).unwrap();
        let analyze: Value = serde_json::from_str(&format!(
            r##"{{"$defs": {{"AnalyzeResultChecks": {{{props}, "title": "AnalyzeResultChecks", "type": "object"}}, {defs}}},
               "properties": {{"summary": {{"title": "Summary", "type": "string"}}, "checks": {{"$ref": "#/$defs/AnalyzeResultChecks"}}}},
               "required": ["summary", "checks"], "title": "AnalyzeResultResponse", "type": "object"}}"##
        ))
        .unwrap();
        let r = default_rubric();
        // to_string compares key order too (preserve_order).
        assert_eq!(check_schema(&r).to_string(), check.to_string());
        assert_eq!(analyze_schema(&r).to_string(), analyze.to_string());
        let empty = Rubric { criteria: vec![] };
        assert_eq!(check_schema(&empty), json!({"properties": {}, "title": "QualityCheckResponse", "type": "object"}));
    }

    #[test]
    fn python_format() {
        let v = [("a", "1"), ("b", "{x}")];
        assert_eq!(py_format("{a}-{b}-{missing}-{{lit}}-{a!r:>3}", &v).unwrap(), "1-{x}--{lit}-1");
        assert!(py_format("oops }", &v).is_err() && py_format("{a", &v).is_err());
    }

    #[test]
    fn file_tree_sorted_like_python() {
        let d = tmp();
        assert_eq!(build_file_tree(&d), "No files found");
        for f in ["a.txt", "a/b", "tests/test.sh", "B"] {
            write(&d.join(f), "");
        }
        assert_eq!(build_file_tree(&d), "B\na/b\na.txt\ntests/test.sh");
    }

    #[test]
    fn trial_and_job_dir_resolution() {
        let d = tmp();
        let j = make_job(&d);
        assert!(is_job_dir(&j) && !is_trial_dir(&j) && !is_job_dir(&d.join("nope")));
        std::fs::create_dir_all(j.join("logs")).unwrap();
        let names = |v: Vec<PathBuf>| v.iter().map(|p| dir_name(p)).collect::<Vec<_>>();
        assert_eq!(names(resolve_trial_dirs(&j, None, None).unwrap()), ["trial__aaa", "trial__bbb", "trial__ccc"]);
        assert_eq!(resolve_trial_dirs(&j, None, Some(2)).unwrap().len(), 2);
        let t = j.join("trial__aaa");
        assert_eq!(resolve_trial_dirs(&t, Some(true), None).unwrap(), vec![t.clone()]);
        assert!(resolve_trial_dirs(&j, Some(true), None).unwrap_err().to_string().contains("No passing trial directories"));
        assert!(resolve_trial_dirs(&d, None, None).unwrap_err().to_string().contains("not a trial directory"));
        // passing = reward 1.0 and no exception; unparseable result counts as failing
        make_trial(&j, "trial__pass", json!({"verifier_result": {"rewards": {"reward": 1}}, "exception_info": null}));
        make_trial(&j, "trial__exc", json!({"verifier_result": {"rewards": {"reward": 1.0}}, "exception_info": {"exception_type": "X"}}));
        make_trial(&j, "trial__half", json!({"verifier_result": {"rewards": {"reward": 0.5}}}));
        write(&j.join("trial__aaa/result.json"), "{not json");
        assert_eq!(names(get_trial_dirs(&j, Some(true))), ["trial__pass"]);
        assert_eq!(names(get_trial_dirs(&j, Some(false))), ["trial__aaa", "trial__bbb", "trial__ccc", "trial__exc", "trial__half"]);
        // task dir comes from result.json config.task.path
        make_trial(&j, "trial__cfg", json!({"config": {"task": {"path": "/some/task"}}}));
        assert_eq!(extract_task_dir(&j.join("trial__cfg")), Some(PathBuf::from("/some/task")));
        assert_eq!(extract_task_dir(&j.join("trial__bbb")), None);
    }

    #[test]
    fn task_dir_resolution() {
        let d = tmp();
        let root = d.join("tasks");
        for n in ["alpha", "beta", "gamma"] {
            make_task(&root, n);
        }
        std::fs::create_dir_all(root.join("not-a-task")).unwrap();
        let names = |v: Vec<PathBuf>| v.iter().map(|p| dir_name(p)).collect::<Vec<_>>();
        assert_eq!(resolve_task_dirs(&root.join("alpha"), &[], &[], None).unwrap(), vec![root.join("alpha")]);
        assert_eq!(names(resolve_task_dirs(&root, &[], &[], None).unwrap()), ["alpha", "beta", "gamma"]);
        assert_eq!(names(resolve_task_dirs(&root, &["a*".into(), "b*".into()], &[], None).unwrap()), ["alpha", "beta"]);
        assert_eq!(names(resolve_task_dirs(&root, &[], &["beta".into()], None).unwrap()), ["alpha", "gamma"]);
        assert_eq!(resolve_task_dirs(&root, &[], &[], Some(2)).unwrap().len(), 2);
        write(&d.join("afile.txt"), "x");
        assert!(resolve_task_dirs(&d.join("afile.txt"), &[], &[], None).unwrap_err().to_string().contains("not a valid task directory"));
        std::fs::create_dir_all(d.join("empty")).unwrap();
        assert!(resolve_task_dirs(&d.join("empty"), &[], &[], None).unwrap_err().to_string().contains("No valid task directories"));
    }

    #[test]
    fn analyze_wrapper_task() {
        let d = tmp();
        let trial = make_trial(&d, "trial__xyz", json!({"task_name": "test"}));
        write(&trial.join(".git/HEAD"), "ref");
        write(&trial.join("analysis.json"), "{}");
        write(&trial.join("analysis.md"), "old");
        write(&trial.join("agent/analysis.json"), "{}");
        let task = make_task(&d, "task");
        let r = default_rubric();
        let tpl = "Analyze {trial_path}.\n\n{task_section}\n\n{criteria_guidance}";
        let w = assemble_analyze_task(&trial, Some(&task), &r, tpl, &analyze_schema(&r), &d.join("work/analyze-task")).unwrap();
        for f in ["task.toml", "instruction.md", "environment/trial/trial.log", "environment/trial/result.json", "environment/task/instruction.md", "tests/test.sh", "tests/validate.py"] {
            assert!(w.join(f).is_file(), "{f}");
        }
        for f in ["environment/Dockerfile", "environment/trial/.git", "environment/trial/analysis.json", "environment/trial/analysis.md", "environment/trial/agent/analysis.json"] {
            assert!(!w.join(f).exists(), "{f}");
        }
        assert!(std::fs::read_to_string(w.join("task.toml")).unwrap().contains("docker_image = \"python:3.13-slim\""));
        assert_eq!(std::fs::read_to_string(w.join("tests/criteria.json")).unwrap(), "[\n  \"reward_hacking\",\n  \"task_specification\"\n]");
        let ins = std::fs::read_to_string(w.join("instruction.md")).unwrap();
        assert!(ins.starts_with("Analyze /app/trial.\n\nThe task being attempted is at /app/task."));
        assert!(ins.contains("- /app/task/solution/ — reference solution (if present)\n\n- reward_hacking: Read the agent's"));
        assert!(ins.contains("write your result to a file named analysis.json") && ins.contains("<output_schema>\n{\n  \"$defs\"") && ins.ends_with("is your only deliverable.\n"));
        assert!(!ins.contains("{trial_path}"));
        let t = Task::load(&w).unwrap();
        assert!(t.validate_tests().is_ok() && is_valid_task_dir(&w));
        assert_eq!(t.config.environment.workdir.as_deref(), Some("/app"));

        // without a task; default prompt; custom rubric
        write(&d.join("custom.toml"), "[[criteria]]\nname = \"custom_check\"\ndescription = \"A custom check\"\nguidance = \"Check custom things.\"\n");
        let r = load_rubric(Some(&d.join("custom.toml")), "").unwrap();
        let w = assemble_analyze_task(&trial, None, &r, ANALYZE_PROMPT, &analyze_schema(&r), &d.join("work/analyze-task")).unwrap();
        assert!(!w.join("environment/task").exists());
        let ins = std::fs::read_to_string(w.join("instruction.md")).unwrap();
        assert!(ins.contains("The task definition is not available.") && ins.contains("Guidance:\n- custom_check: Check custom things.\n\n# Output"));
        assert_eq!(std::fs::read_to_string(w.join("tests/criteria.json")).unwrap(), "[\n  \"custom_check\"\n]");
    }

    #[test]
    fn check_wrapper_task() {
        let d = tmp();
        let task = make_task(&d, "task");
        write(&task.join(".git/HEAD"), "ref");
        let r = load_rubric(None, CHECK_RUBRIC).unwrap();
        let w = assemble_check_task(&task, &r, CHECK_PROMPT, &check_schema(&r), &d.join("work/check-task")).unwrap();
        for f in ["task.toml", "instruction.md", "environment/task/instruction.md", "environment/task/tests/test.sh", "tests/test.sh", "tests/validate.py", "tests/criteria.json"] {
            assert!(w.join(f).is_file(), "{f}");
        }
        assert!(!w.join("environment/task/.git").exists() && !w.join("environment/Dockerfile").exists());
        let ins = std::fs::read_to_string(w.join("instruction.md")).unwrap();
        assert!(ins.contains("<file_tree>\nenvironment/Dockerfile\ninstruction.md\ntask.toml\ntests/test.sh\n</file_tree>"));
        assert!(ins.contains("The task to review is at /app/task.") && ins.contains("- typos: Look closely") && ins.contains("check-result.json"));
        let criteria: Vec<String> = serde_json::from_str(&std::fs::read_to_string(w.join("tests/criteria.json")).unwrap()).unwrap();
        assert_eq!(criteria.len(), 11);
        assert!(is_valid_task_dir(&w));
    }

    fn trial_result(reward: Option<f64>, exception: bool, cost: Option<f64>) -> TrialResult {
        serde_json::from_value(json!({
            "id": uuid::Uuid::nil(), "task_name": "t", "trial_name": "t__1", "trial_uri": "", "task_id": {}, "task_checksum": "",
            "config": {"task": {"path": "/w"}, "trial_name": "t__1", "trials_dir": "j"},
            "agent_info": {"name": "claude-code", "version": "1"},
            "agent_result": {"cost_usd": cost},
            "verifier_result": reward.map(|r| json!({"rewards": {"reward": r}})),
            "exception_info": exception.then(|| json!({"exception_type": "RuntimeError", "exception_message": "boom", "exception_traceback": "", "occurred_at": "2026-01-01T00:00:00Z"})),
        }))
        .unwrap()
    }

    #[test]
    fn extract_results() {
        let d = tmp();
        let r = default_rubric();
        let checks = json!({"task_specification": {"outcome": "fail", "explanation": "vague"}, "reward_hacking": {"outcome": "pass", "explanation": "OK"}});
        write(&d.join("artifacts/analysis.json"), &json!({"summary": "The agent solved it.", "checks": checks}).to_string());
        let x = extract_analyze_result(&trial_result(Some(1.0), false, Some(0.42)), &d, &r).unwrap();
        assert_eq!((x.summary.as_deref(), x.cost_usd), (Some("The agent solved it."), Some(0.42)));
        // rubric order, not file order
        assert_eq!(x.checks.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["reward_hacking", "task_specification"]);
        assert_eq!(x.checks[1].1, Check { explanation: "vague".into(), outcome: Outcome::Fail });

        let e = extract_analyze_result(&trial_result(Some(1.0), true, None), &d, &r).unwrap_err();
        assert!(e.starts_with("Analyze trial failed with RuntimeError: boom"));
        write(&d.join("verifier/test-stdout.txt"), "summary must be a non-empty string\n");
        let e = extract_analyze_result(&trial_result(Some(0.0), false, None), &d, &r).unwrap_err();
        assert!(e.contains("did not produce a valid result.\nVerifier output:\nsummary must be a non-empty string\nTrial artifacts:"));
        let e = extract_check_result(&trial_result(Some(1.0), false, None), &d, &r).unwrap_err();
        assert!(e.contains("Check agent did not produce a valid result") && e.contains("-m sonnet or -m opus"));
        write(&d.join("artifacts/check-result.json"), &checks.to_string());
        let x = extract_check_result(&trial_result(Some(1.0), false, None), &d, &r).unwrap();
        assert_eq!((x.checks.len(), x.cost_usd), (2, None));
        write(&d.join("artifacts/check-result.json"), r#"{"reward_hacking": {"outcome": "maybe", "explanation": "x"}}"#);
        assert!(extract_check_result(&trial_result(Some(1.0), false, None), &d, &r).is_err());
    }

    #[test]
    fn report_json_and_rendering() {
        let checks: Checks = vec![("reward_hacking".into(), Check { explanation: "clean".into(), outcome: Outcome::Pass })];
        let ok = AnalyzeReportResult { trial_name: Some("trial__write".into()), summary: Some("Solved.".into()), checks: checks.clone(), cost_usd: Some(0.05), error: None };
        let d = tmp();
        write_analysis_json(&d, &ok).unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("analysis.json")).unwrap(),
            "{\n  \"trial_name\": \"trial__write\",\n  \"summary\": \"Solved.\",\n  \"checks\": {\n    \"reward_hacking\": {\n      \"explanation\": \"clean\",\n      \"outcome\": \"pass\"\n    }\n  },\n  \"estimated_cost_usd\": 0.05\n}"
        );
        let err = AnalyzeReportResult { trial_name: Some("b".into()), error: Some("boom\nmore".into()), ..Default::default() };
        let report = serde_json::to_value(Report { results: &[ok.clone(), err.clone()] }).unwrap();
        assert_eq!(report["results"][1], json!({"trial_name": "b", "summary": null, "checks": {}, "cost_usd": null, "error": "boom\nmore"}));
        let q = serde_json::to_string(&QualityCheckResult { task_name: Some("t".into()), ..Default::default() }).unwrap();
        assert_eq!(q, r#"{"checks":{},"cost_usd":null,"task_name":"t","error":null}"#);

        assert_eq!(title_case("test_deps_in_image"), "Test Deps In Image");
        let rows = [(ok.trial_name.as_deref(), &ok.checks, ok.cost_usd, None), (Some("b"), &err.checks, None, err.error.as_deref())];
        let s = summary_table("Trial Analyses", "Trial", &rows);
        assert!(s.contains("trial__write | 1    | 0    | 0   | 0.0500") && s.contains("b            | -    | -    | -   | -"));
        assert!(s.contains("❌ b: boom\n") && s.ends_with("Total agent cost: $0.0500"));
        assert!(checks_table("Trial: x", &checks).contains("Reward Hacking | pass    | clean"));
    }

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        cmd: Sub,
    }
    #[derive(clap::Subcommand)]
    enum Sub {
        Analyze(AnalyzeArgs),
        Check(CheckArgs),
    }

    #[test]
    fn cli_flags_and_job_config() {
        let Sub::Analyze(a) = Cli::parse_from(["x", "analyze", "jobs/j", "--failing", "-l", "3", "--ak", "max_turns=5", "--ae", "X=1", "-n", "2", "-o", "out", "--job-name", "an", "-q"]).cmd else { panic!() };
        assert_eq!((a.model.as_str(), a.failing, a.n_trials, a.common.agent.as_str()), ("claude-haiku-4-5", true, Some(3), "claude-code"));
        let cfg = wrapper_job_config(&[PathBuf::from("/w/a")], &a.common, &a.model).unwrap();
        assert_eq!(cfg.job_dir(), PathBuf::from("out/an"));
        assert_eq!((cfg.n_concurrent_trials, cfg.n_attempts, cfg.quiet), (2, 1, true));
        assert_eq!(cfg.agents[0].model_name.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!((cfg.agents[0].kwargs["max_turns"].clone(), cfg.agents[0].env["X"].as_str()), (json!(5), "1"));
        assert_eq!((cfg.environment.kind.as_deref(), cfg.tasks.len(), cfg.datasets.len()), (Some("docker"), 1, 0));
        let Sub::Check(c) = Cli::parse_from(["x", "check", "tasks", "-i", "a*", "-x", "b", "-l", "1", "-e", "daytona"]).cmd else { panic!() };
        assert_eq!((c.model.as_str(), c.include.as_slice(), c.exclude.as_slice(), c.n_tasks), ("claude-sonnet-4-6", &["a*".to_string()][..], &["b".to_string()][..], Some(1)));
        assert!(wrapper_job_config(&[], &c.common, &c.model).is_err());
    }
}
