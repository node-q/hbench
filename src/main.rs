mod agents;
mod analyze;
mod authoring;
mod artifacts;
mod bridge;
mod atif;
mod config;
mod diff;
mod env;
mod job;
mod metrics;
mod migrate;
mod regrade;
mod registry;
mod schemas;
mod sweeps;
mod task;
mod tools;
mod traces;
mod trial;
mod viewer;

use crate::config::{AgentConfig, DatasetConfig, JobConfig, SourceTrial, TaskRef, TrialConfig};
use crate::task::{Artifact, Task};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// hbench: run agent benchmarks on upstream-format tasks in Docker.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a job (alias of `jobs start`).
    Run(Box<RunArgs>),
    /// Jobs: start, resume, summary, regrade.
    Jobs {
        #[command(subcommand)]
        cmd: JobsCmd,
    },
    /// Single trials.
    Trials {
        #[command(subcommand)]
        cmd: TrialsCmd,
    },
    /// Initialize a new task or dataset.
    Init(Box<InitArgs>),
    /// Add tasks, datasets, or files to a dataset.toml manifest.
    Add(AddArgs),
    /// Remove tasks from a dataset.toml manifest.
    Remove(RemoveArgs),
    /// Update task digests in a dataset manifest.
    Sync {
        /// Path to dataset.toml or directory containing one.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Also update registry tasks to their latest digest (needs upstream Hub; unsupported).
        #[arg(short = 'u', long)]
        upgrade: bool,
        /// Max concurrent registry lookups.
        #[arg(short = 'c', long, default_value_t = 50)]
        concurrency: usize,
    },
    /// Registry datasets and dataset manifests.
    #[command(alias = "dataset")]
    Datasets {
        #[command(subcommand)]
        cmd: DatasetsCmd,
    },
    /// Task authoring helpers.
    #[command(alias = "task")]
    Tasks {
        #[command(subcommand)]
        cmd: TasksCmd,
    },
    /// Built-in agents: list, or print an agent's options schema.
    Agents {
        #[command(subcommand)]
        cmd: Option<AgentsCmd>,
    },
    /// Cache management.
    Cache {
        #[command(subcommand)]
        cmd: CacheCmd,
    },
    /// Successive sweeps that drop tasks once they succeed.
    Sweeps {
        #[command(subcommand)]
        cmd: SweepsCmd,
    },
    /// Web results viewer for a jobs dir.
    View {
        #[arg(short = 'p', long = "path", default_value = "jobs")]
        jobs_dir: PathBuf,
        #[arg(long, default_value_t = 8080)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
    },
    /// Trace export.
    Traces {
        #[command(subcommand)]
        cmd: TracesCmd,
    },
    /// Analyze trial trajectories against a rubric (trial dir or job dir).
    Analyze(Box<analyze::AnalyzeArgs>),
    /// Check task quality against a rubric (task dir or dir of tasks).
    Check(Box<analyze::CheckArgs>),
}

#[derive(Args)]
struct InitArgs {
    /// Task or dataset name (org/name format).
    name: Option<String>,
    /// Initialize a task.
    #[arg(short = 't', long)]
    task: bool,
    /// Initialize a dataset.
    #[arg(short = 'd', long)]
    dataset: bool,
    #[arg(short = 'o', long, default_value = ".")]
    output_dir: PathBuf,
    /// Organization name (if name lacks org/).
    #[arg(long)]
    org: Option<String>,
    #[arg(long, default_value = "")]
    description: String,
    /// Author in 'Name <email>' or 'Name' format. Repeatable.
    #[arg(long)]
    author: Vec<String>,
    /// Create metric.py template (dataset only).
    #[arg(long)]
    with_metric: bool,
    #[arg(long)]
    no_pytest: bool,
    #[arg(long)]
    no_solution: bool,
    #[arg(long)]
    include_canary_strings: bool,
    #[arg(long)]
    include_standard_metadata: bool,
    /// Skip [task] package section in task.toml.
    #[arg(long)]
    no_package: bool,
    /// Scaffold a multi-step task with N steps (0 = single-step).
    #[arg(long, default_value_t = 0)]
    steps: usize,
}

#[derive(Args)]
struct AddArgs {
    /// Local task dirs, dataset dirs/dataset.toml, or metric.py.
    #[arg(required = true)]
    packages: Vec<String>,
    /// Path to dataset.toml or directory containing one.
    #[arg(short = 't', long, default_value = ".")]
    to: PathBuf,
    /// When a directory has no task.toml/dataset.toml, scan subdirs for tasks.
    #[arg(long)]
    scan: bool,
}

#[derive(Args)]
struct RemoveArgs {
    /// Package to remove (path or org/name).
    package: String,
    /// Target dataset.toml or directory containing one.
    #[arg(long = "from", default_value = ".")]
    from: PathBuf,
    /// Treat package as a directory and scan subdirs for tasks to remove.
    #[arg(long)]
    scan: bool,
}

#[derive(Subcommand)]
enum SweepsCmd {
    Run {
        #[arg(short = 'c', long = "config")]
        config: PathBuf,
        #[arg(long, default_value_t = 3)]
        max_sweeps: usize,
        #[arg(long, default_value_t = 2)]
        trials_per_task: usize,
        /// Generic hint passed to agents as kwargs.hint.
        #[arg(long)]
        hint: Option<String>,
        /// JSON object {task_name: hint}; runs one job per task.
        #[arg(long)]
        hints_file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum TracesCmd {
    /// Export ATIF trajectories as conversation rows (JSONL; upstream's HF trace schema).
    Export {
        /// Trial dir, or a root containing trials recursively.
        #[arg(short = 'p', long = "path")]
        path: PathBuf,
        /// Output .jsonl (default: stdout).
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,
        #[arg(long)]
        no_recursive: bool,
        /// all | last
        #[arg(long, default_value = "all")]
        episodes: String,
        #[arg(long, value_parser = ["success", "failure", "all"])]
        filter: Option<String>,
        /// Also emit a ShareGPT `conversations_sharegpt` column.
        #[arg(long)]
        sharegpt: bool,
        #[arg(long)]
        instruction_metadata: bool,
        #[arg(long)]
        verifier_metadata: bool,
    },
}

#[derive(Subcommand)]
enum AgentsCmd {
    List {
        #[arg(long)]
        json: bool,
    },
    /// The user-facing kwargs an agent accepts (upstream's options schema).
    Schema {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Remove ~/.cache/hbench and hbench-built docker images.
    Clean {
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(long)]
        dry: bool,
        #[arg(long)]
        no_docker: bool,
        #[arg(long)]
        no_cache_dir: bool,
    },
}

#[derive(Subcommand)]
enum JobsCmd {
    /// Start a job.
    Start(Box<RunArgs>),
    /// Resume a job: finished trials are kept, the rest (and filtered error types) rerun.
    Resume {
        #[arg(short = 'p', long = "job-path")]
        path: PathBuf,
        /// Remove trials with these error types before resuming (repeatable).
        #[arg(short = 'f', long = "filter-error-type", default_values_t = vec!["CancelledError".to_string()])]
        filter_error_types: Vec<String>,
    },
    /// Print a job's stats.
    Summary {
        #[arg(short = 'p', long = "job-path")]
        path: PathBuf,
    },
    /// Re-run (separate) verification against a finished job's recorded outputs.
    Regrade {
        /// Source job dir whose trials are regraded.
        #[arg(long)]
        source: PathBuf,
        /// Task dir or dir of tasks providing the (new) verifiers; matched by task name.
        #[arg(short = 'p', long = "path")]
        path: PathBuf,
        #[arg(long, default_value = "jobs")]
        jobs_dir: PathBuf,
        #[arg(long)]
        job_name: Option<String>,
        #[arg(short = 'n', long = "n-concurrent", default_value_t = 4)]
        n_concurrent: usize,
    },
}

#[derive(Subcommand)]
enum TrialsCmd {
    /// Print the trial config JSON schema.
    Schema,
    /// Resume a finished trial's agent session in the local agent CLI (claude-code, muse-code).
    Handoff { trial_dir: PathBuf },
    /// Run one trial: `-p task` or `-c trial.yaml`.
    Start {
        #[arg(short = 'c', long = "config")]
        config: Option<PathBuf>,
        #[arg(short = 'p', long = "path")]
        path: Option<PathBuf>,
        #[arg(short = 'a', long = "agent", default_value = "oracle")]
        agent: String,
        #[arg(short = 'm', long = "model")]
        model: Option<String>,
        #[arg(long = "ak", value_parser = parse_kv)]
        agent_kwargs: Vec<(String, String)>,
        #[arg(long = "ae", value_parser = parse_kv)]
        agent_env: Vec<(String, String)>,
        #[arg(long, default_value = "trials")]
        trials_dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum DatasetsCmd {
    /// List datasets in the registry.
    List(RegistryArgs),
    /// Download a registry dataset's tasks into the cache (or --output-dir).
    Download {
        dataset: String,
        #[command(flatten)]
        registry: RegistryArgs,
        #[arg(short = 'o', long)]
        output_dir: Option<PathBuf>,
    },
    /// Initialize a new dataset directory.
    Init {
        name: Option<String>,
        #[arg(short = 'o', long, default_value = ".")]
        output_dir: PathBuf,
        #[arg(long)]
        org: Option<String>,
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long)]
        with_metric: bool,
        #[arg(long)]
        author: Vec<String>,
    },
    /// Add tasks or datasets to a dataset.toml.
    Add(AddArgs),
    /// Remove tasks from a dataset.toml.
    Remove(RemoveArgs),
}

#[derive(Subcommand)]
enum TasksCmd {
    /// Initialize a new task directory.
    Init {
        name: Option<String>,
        #[arg(short = 'p', long = "tasks-dir", default_value = ".")]
        tasks_dir: PathBuf,
        #[arg(long)]
        org: Option<String>,
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long)]
        no_pytest: bool,
        #[arg(long)]
        no_solution: bool,
        #[arg(long)]
        include_canary_strings: bool,
        /// Path to a template TOML file for task metadata and defaults.
        #[arg(long)]
        metadata_template: Option<PathBuf>,
        #[arg(long, hide = true)]
        include_standard_metadata: bool,
        #[arg(long)]
        no_package: bool,
        #[arg(long)]
        author: Vec<String>,
        #[arg(long, default_value_t = 0)]
        steps: usize,
    },
    /// Print the task.toml JSON schema.
    Schema,
    /// Start a task's environment and attach a shell (solution + tests uploaded by default).
    StartEnv {
        #[arg(short = 'p', long = "path")]
        path: PathBuf,
        /// Don't upload solution/ and tests/.
        #[arg(long)]
        no_all: bool,
        #[arg(long)]
        non_interactive: bool,
        #[arg(long)]
        agent: Option<String>,
        #[arg(short = 'm', long = "model")]
        model: Option<String>,
        #[arg(long = "ak", value_parser = parse_kv)]
        agent_kwargs: Vec<(String, String)>,
    },
    /// Migrate Terminal-Bench (v1) tasks to upstream format.
    Migrate {
        #[arg(short = 'i', long = "input")]
        input: PathBuf,
        #[arg(short = 'o', long = "output")]
        output: PathBuf,
        #[arg(long)]
        cpus: Option<i64>,
        #[arg(long)]
        memory_mb: Option<i64>,
        #[arg(long)]
        storage_mb: Option<i64>,
        #[arg(long)]
        gpus: Option<i64>,
    },
    /// Validate task dirs (config, files, test scripts).
    Check {
        #[arg(short = 'p', long = "path")]
        path: PathBuf,
    },
}

#[derive(Args)]
struct RegistryArgs {
    #[arg(long)]
    registry_url: Option<String>,
    /// Local registry.json instead of --registry-url.
    #[arg(long)]
    registry_path: Option<PathBuf>,
}

#[derive(Args)]
struct RunArgs {
    /// upstream job config (YAML or JSON). CLI flags override it.
    #[arg(short = 'c', long = "config")]
    config: Option<PathBuf>,
    /// Local task dir, or a dir of task dirs.
    #[arg(short = 'p', long = "path")]
    path: Option<PathBuf>,
    /// Registry dataset, name[@version].
    #[arg(short = 'd', long = "dataset")]
    dataset: Option<String>,
    #[command(flatten)]
    registry: RegistryArgs,
    #[arg(short = 'a', long = "agent")]
    agent: Option<String>,
    /// Model(s); one agent config per model.
    #[arg(short = 'm', long = "model")]
    models: Vec<String>,
    /// Agent kwarg KEY=VALUE (JSON values allowed), repeatable.
    #[arg(long = "ak", alias = "agent-kwarg", value_parser = parse_kv)]
    agent_kwargs: Vec<(String, String)>,
    /// Agent env var KEY=VALUE, repeatable.
    #[arg(long = "ae", alias = "agent-env", value_parser = parse_kv)]
    agent_env: Vec<(String, String)>,
    /// Environment (container) env var KEY=VALUE, repeatable.
    #[arg(long = "ee", alias = "environment-env", value_parser = parse_kv)]
    env_env: Vec<(String, String)>,
    /// Verifier env var KEY=VALUE, repeatable.
    #[arg(long = "ve", alias = "verifier-env", value_parser = parse_kv)]
    verifier_env: Vec<(String, String)>,
    #[arg(short = 'k', long = "n-attempts")]
    n_attempts: Option<usize>,
    #[arg(short = 'n', long = "n-concurrent")]
    n_concurrent: Option<usize>,
    /// Per-agent cap on concurrent agent runs.
    #[arg(long)]
    n_concurrent_agent: Option<usize>,
    #[arg(short = 'i', long = "include-task-name")]
    include: Vec<String>,
    #[arg(short = 'x', long = "exclude-task-name")]
    exclude: Vec<String>,
    #[arg(short = 'l', long = "n-tasks")]
    n_tasks: Option<usize>,
    #[arg(long)]
    jobs_dir: Option<PathBuf>,
    #[arg(long)]
    job_name: Option<String>,
    #[arg(long)]
    timeout_multiplier: Option<f64>,
    #[arg(long)]
    agent_timeout_multiplier: Option<f64>,
    #[arg(long)]
    verifier_timeout_multiplier: Option<f64>,
    #[arg(long)]
    agent_setup_timeout_multiplier: Option<f64>,
    #[arg(long)]
    environment_build_timeout_multiplier: Option<f64>,
    #[arg(long)]
    max_retries: Option<u32>,
    #[arg(long = "retry-include")]
    retry_include: Vec<String>,
    #[arg(long = "retry-exclude")]
    retry_exclude: Vec<String>,
    #[arg(long)]
    force_build: bool,
    /// Keep images/volumes after trials.
    #[arg(long)]
    no_delete: bool,
    /// Stop (don't remove) containers after trials.
    #[arg(long)]
    keep_containers: bool,
    #[arg(long)]
    disable_verification: bool,
    #[arg(long)]
    install_only: bool,
    #[arg(long)]
    override_cpus: Option<u32>,
    #[arg(long)]
    override_memory_mb: Option<u64>,
    #[arg(long)]
    override_gpus: Option<u32>,
    /// Extra host allowed during network-restricted tasks (repeatable).
    #[arg(long)]
    extra_allowed_host: Vec<String>,
    /// Artifact path to collect (repeatable).
    #[arg(long)]
    artifact: Vec<String>,
    #[arg(long)]
    extra_instruction: Vec<String>,
    #[arg(long)]
    extra_instruction_path: Vec<PathBuf>,
    /// Skill dir uploaded to /hbench/skills (repeatable).
    #[arg(long)]
    skill: Vec<String>,
    #[arg(long)]
    quiet: bool,
    /// Compare against earlier job(s): reuse / regrade / rerun (repeatable).
    #[arg(long)]
    diff: Vec<PathBuf>,
    /// Don't ask for confirmation (with --diff).
    #[arg(short = 'y', long)]
    yes: bool,
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or_else(|| format!("expected KEY=VALUE, got {s:?}"))
}

/// fnmatch-style glob: `*`, `?`, `[seq]`, `[!seq]` (case-sensitive like Python on POSIX).
pub fn glob_match(pat: &str, s: &str) -> bool {
    let mut re = String::from("^");
    let mut chars = pat.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '[' => {
                let mut class = String::new();
                let mut closed = false;
                if chars.peek() == Some(&'!') {
                    chars.next();
                    class.push('^');
                }
                for c2 in chars.by_ref() {
                    if c2 == ']' && !class.is_empty() && class != "^" {
                        closed = true;
                        break;
                    }
                    if c2 == '\\' || c2 == '[' || c2 == ']' {
                        class.push('\\');
                    }
                    class.push(c2);
                }
                if closed {
                    re.push('[');
                    re.push_str(&class);
                    re.push(']');
                } else {
                    re.push_str(&regex::escape(&format!("[{}", class.replace('^', "!"))));
                }
            }
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    regex::Regex::new(&re).is_ok_and(|r| r.is_match(s))
}

/// KEY=VALUE where VALUE may be JSON (numbers, bools, objects); else a string.
fn kwarg_value(v: &str) -> Value {
    serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_string()))
}

fn build_job_config(a: &RunArgs) -> Result<JobConfig> {
    let mut cfg = match &a.config {
        Some(p) => JobConfig::load(p)?,
        None => JobConfig::default(),
    };
    if let Some(p) = &a.path {
        let p = p.canonicalize().with_context(|| format!("path {}", p.display()))?;
        if Task::is_task_dir(&p) {
            cfg.tasks.push(TaskRef::local(p, None));
        } else {
            cfg.datasets.push(DatasetConfig { path: Some(p), ..Default::default() });
        }
    }
    if let Some(d) = &a.dataset {
        let (name, version) = match d.split_once('@') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (d.clone(), None),
        };
        cfg.datasets.push(DatasetConfig {
            name: Some(name),
            version,
            registry_url: a.registry.registry_url.clone(),
            registry_path: a.registry.registry_path.clone(),
            ..Default::default()
        });
    }
    // CLI task filters apply to every dataset (upstream semantics).
    for d in cfg.datasets.iter_mut() {
        if !a.include.is_empty() {
            d.task_names = Some(a.include.clone());
        }
        if !a.exclude.is_empty() {
            d.exclude_task_names = Some(a.exclude.clone());
        }
        if a.n_tasks.is_some() {
            d.n_tasks = a.n_tasks;
        }
    }
    if a.agent.is_some() || !a.models.is_empty() || !a.agent_kwargs.is_empty() || !a.agent_env.is_empty() || a.n_concurrent_agent.is_some() || !a.skill.is_empty() {
        let base = cfg.agents.first().cloned().unwrap_or_default();
        let base = AgentConfig { name: a.agent.clone().or(base.name), ..base };
        let models: Vec<Option<String>> = if a.models.is_empty() { vec![base.model_name.clone()] } else { a.models.iter().cloned().map(Some).collect() };
        cfg.agents = models
            .into_iter()
            .map(|m| {
                let mut c = AgentConfig { model_name: m, ..base.clone() };
                for (k, v) in &a.agent_kwargs {
                    c.kwargs.insert(k.clone(), kwarg_value(v));
                }
                c.env.extend(a.agent_env.iter().cloned());
                if a.n_concurrent_agent.is_some() {
                    c.n_concurrent = a.n_concurrent_agent;
                }
                c.skills.extend(a.skill.iter().cloned());
                c
            })
            .collect();
    }
    macro_rules! set {
        ($field:expr, $v:expr) => {
            if let Some(v) = $v {
                $field = v;
            }
        };
    }
    set!(cfg.n_attempts, a.n_attempts);
    set!(cfg.n_concurrent_trials, a.n_concurrent);
    set!(cfg.jobs_dir, a.jobs_dir.clone());
    set!(cfg.job_name, a.job_name.clone());
    set!(cfg.timeout_multiplier, a.timeout_multiplier);
    set!(cfg.retry.max_retries, a.max_retries);
    cfg.agent_timeout_multiplier = a.agent_timeout_multiplier.or(cfg.agent_timeout_multiplier);
    cfg.verifier_timeout_multiplier = a.verifier_timeout_multiplier.or(cfg.verifier_timeout_multiplier);
    cfg.agent_setup_timeout_multiplier = a.agent_setup_timeout_multiplier.or(cfg.agent_setup_timeout_multiplier);
    cfg.environment_build_timeout_multiplier = a.environment_build_timeout_multiplier.or(cfg.environment_build_timeout_multiplier);
    if !a.retry_include.is_empty() {
        cfg.retry.include_exceptions = Some(a.retry_include.iter().cloned().collect());
    }
    if !a.retry_exclude.is_empty() {
        cfg.retry.exclude_exceptions = Some(a.retry_exclude.iter().cloned().collect());
    }
    cfg.environment.force_build |= a.force_build;
    if a.no_delete {
        cfg.environment.delete = false;
    }
    if a.keep_containers {
        cfg.environment.kwargs.insert("keep_containers".into(), Value::Bool(true));
    }
    cfg.environment.override_cpus = a.override_cpus.or(cfg.environment.override_cpus);
    cfg.environment.override_memory_mb = a.override_memory_mb.or(cfg.environment.override_memory_mb);
    cfg.environment.override_gpus = a.override_gpus.or(cfg.environment.override_gpus);
    cfg.environment.extra_allowed_hosts.extend(a.extra_allowed_host.iter().cloned());
    cfg.environment.env.extend(a.env_env.iter().cloned());
    cfg.verifier.env.extend(a.verifier_env.iter().cloned());
    cfg.verifier.disable |= a.disable_verification;
    cfg.install_only |= a.install_only;
    cfg.quiet |= a.quiet;
    cfg.artifacts.extend(a.artifact.iter().cloned().map(Artifact::Path));
    cfg.extra_instructions.extend(a.extra_instruction.iter().cloned());
    cfg.extra_instruction_paths.extend(a.extra_instruction_path.iter().cloned());
    if cfg.tasks.is_empty() && cfg.datasets.is_empty() {
        bail!("nothing to run: pass -p <task or dataset dir>, -d <dataset>, or -c <job config>");
    }
    cfg.validate()?;
    Ok(cfg)
}

fn preflight_agents(cfg: &JobConfig) -> Result<()> {
    let known = agents::agent_names();
    for a in &cfg.agents {
        if !known.contains(&a.name()) {
            bail!("Unknown agent {:?}. Known agents: {}", a.name(), known.join(", "));
        }
    }
    Ok(())
}

async fn run_with_ctrl_c(cfg: JobConfig, trials: Option<Vec<TrialConfig>>, filter: &[String]) -> Result<()> {
    env::preflight().await?;
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\ncancelling: tearing down containers (Ctrl-C again to force quit)...");
            let _ = tx.send(true);
            if tokio::signal::ctrl_c().await.is_ok() {
                std::process::exit(130);
            }
        }
    });
    let job_dir = cfg.job_dir();
    let jr = job::run_job(cfg, trials, filter, rx).await?;
    job::print_summary(&jr, &job_dir);
    Ok(())
}

/// `hbench init`: task or dataset, with upstream's cross-type flag checks.
fn run_init(a: InitArgs) -> Result<()> {
    let (mut task, mut dataset) = (a.task, a.dataset);
    if task && dataset {
        bail!("Cannot specify both --task and --dataset.");
    }
    if !task && !dataset {
        print!("Initialize a [t]ask or [d]ataset?: ");
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut choice = String::new();
        std::io::stdin().read_line(&mut choice)?;
        match choice.trim().to_lowercase().as_str() {
            "t" | "task" => task = true,
            "d" | "dataset" => dataset = true,
            _ => bail!("Invalid choice. Please enter 't' for task or 'd' for dataset."),
        }
    }
    if dataset {
        let task_flags = [
            ("--no-pytest", a.no_pytest),
            ("--no-solution", a.no_solution),
            ("--include-canary-strings", a.include_canary_strings),
            ("--include-standard-metadata", a.include_standard_metadata),
            ("--no-package", a.no_package),
            ("--steps", a.steps > 0),
        ];
        if let Some((flag, _)) = task_flags.iter().find(|(_, set)| *set) {
            bail!("{flag} is a task-only option and cannot be used with --dataset.");
        }
    }
    if task && a.with_metric {
        bail!("--with-metric is a dataset-only option and cannot be used with --task.");
    }
    let name = authoring::resolve_name(a.name, a.org)?;
    let authors = authoring::parse_authors(&a.author);
    if task {
        let opts = authoring::TaskOpts {
            no_pytest: a.no_pytest,
            no_solution: a.no_solution,
            include_canary_strings: a.include_canary_strings,
            include_standard_metadata: a.include_standard_metadata,
            no_package: a.no_package,
            description: a.description,
            authors,
            metadata_template: None,
            steps: a.steps,
        };
        authoring::init_task(&name, &a.output_dir, &opts).map(|_| ())
    } else {
        authoring::init_dataset(&name, &a.output_dir, &a.description, a.with_metric, &authors)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Run(a) | Cmd::Jobs { cmd: JobsCmd::Start(a) } => {
            let mut cfg = build_job_config(&a)?;
            preflight_agents(&cfg)?;
            if cfg.job_dir().join("config.json").exists() {
                bail!("job {} already exists; use `hbench jobs resume -p {}`", cfg.job_name, cfg.job_dir().display());
            }
            if a.diff.is_empty() {
                return run_with_ctrl_c(cfg, None, &[]).await;
            }
            if cfg.install_only {
                bail!("--diff cannot use --install-only.");
            }
            let sources = diff::load_sources(&diff::source_dirs(&a.diff)?)?;
            let tasks = job::resolve_tasks(&cfg)?;
            let targets = job::build_trial_configs(&cfg, &tasks, uuid::Uuid::new_v4());
            let diffs = diff::compare(&targets, &sources)?;
            diff::print_preview(&diffs, &sources);
            let trials = diff::trial_configs(targets, &diffs, &sources);
            if trials.is_empty() {
                println!("Nothing to run: every target trial can be reused.");
                return Ok(());
            }
            if !a.yes {
                eprint!("Proceed with {} trial(s)? [y/N] ", trials.len());
                let mut ans = String::new();
                std::io::stdin().read_line(&mut ans)?;
                if !matches!(ans.trim(), "y" | "Y" | "yes") {
                    bail!("aborted");
                }
            }
            cfg.source_jobs = a.diff.iter().map(|p| config::SourceJob { action: "diff".into(), path: p.clone() }).collect();
            run_with_ctrl_c(cfg, Some(trials), &[]).await
        }
        Cmd::Jobs { cmd: JobsCmd::Resume { path, filter_error_types } } => {
            let cfg = job::load_config(&path)?;
            preflight_agents(&cfg)?;
            run_with_ctrl_c(cfg, None, &filter_error_types).await
        }
        Cmd::Jobs { cmd: JobsCmd::Summary { path } } => {
            let s = std::fs::read_to_string(path.join("result.json")).context("reading result.json")?;
            job::print_summary(&serde_json::from_str(&s)?, &path);
            Ok(())
        }
        Cmd::Jobs { cmd: JobsCmd::Regrade { source, path, jobs_dir, job_name, n_concurrent } } => {
            let path = path.canonicalize()?;
            let task_dirs: Vec<PathBuf> = if Task::is_task_dir(&path) {
                vec![path.clone()]
            } else {
                let mut v: Vec<PathBuf> = std::fs::read_dir(&path)?.flatten().map(|e| e.path()).filter(|p| Task::is_task_dir(p)).collect();
                v.sort();
                v
            };
            let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
            for d in task_dirs {
                let t = Task::load(&d)?;
                if let Some(why) = regrade::check_task_regradable(&t) {
                    bail!(why);
                }
                by_name.insert(t.name(), d);
            }
            let source = source.canonicalize()?;
            let mut cfg = JobConfig { jobs_dir, n_concurrent_trials: n_concurrent, ..Default::default() };
            if let Some(n) = job_name {
                cfg.job_name = n;
            }
            let mut trials = vec![];
            for e in std::fs::read_dir(&source)?.flatten() {
                let Some(r) = std::fs::read_to_string(e.path().join("result.json")).ok().and_then(|s| serde_json::from_str::<trial::TrialResult>(&s).ok()) else { continue };
                let Some(dir) = by_name.get(&r.task_name) else { continue };
                let tref = TaskRef { path: Some(dir.clone()), source: r.source.clone(), ..Default::default() };
                trials.push(TrialConfig {
                    task: tref.clone(),
                    trial_name: job::trial_name(&tref),
                    trials_dir: cfg.job_dir(),
                    agent: r.config.agent.clone(),
                    source_trial: Some(SourceTrial { action: "regrade".into(), kind: "local".into(), trial_id: Some(r.id.to_string()), path: Some(e.path()) }),
                    ..serde_json::from_value(serde_json::json!({"task": {"path": dir}, "trial_name": "x", "trials_dir": "x"}))?
                });
            }
            if trials.is_empty() {
                bail!("no trials in {} match the provided tasks by name", source.display());
            }
            cfg.tasks = by_name.values().map(|p| TaskRef::local(p.clone(), None)).collect();
            run_with_ctrl_c(cfg, Some(trials), &[]).await
        }
        Cmd::Trials { cmd: TrialsCmd::Start { config, path, agent, model, agent_kwargs, agent_env, trials_dir } } => {
            let mut tc: TrialConfig = match config {
                Some(c) => {
                    let raw = std::fs::read_to_string(&c)?;
                    serde_yaml_ng::from_str(&raw).with_context(|| format!("parsing {}", c.display()))?
                }
                None => {
                    let p = path.context("pass -p <task dir> or -c <trial.yaml>")?.canonicalize()?;
                    let tref = TaskRef::local(p, None);
                    let mut a = AgentConfig { name: Some(agent), model_name: model, ..Default::default() };
                    a.kwargs = agent_kwargs.iter().map(|(k, v)| (k.clone(), kwarg_value(v))).collect();
                    a.env = agent_env.into_iter().collect();
                    serde_json::from_value(serde_json::json!({"task": tref, "trial_name": job::trial_name(&tref), "trials_dir": trials_dir, "agent": a}))?
                }
            };
            if tc.trial_name.is_empty() {
                tc.trial_name = job::trial_name(&tc.task);
            }
            tc.task.validate()?;
            registry::download(std::slice::from_ref(&tc.task))?;
            env::preflight().await?;
            let task = std::sync::Arc::new(Task::load(&registry::local_path(&tc.task))?);
            let (_tx, rx) = tokio::sync::watch::channel(false);
            let r = trial::Trial::new(tc, task, None)?.run(rx).await;
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "trial_name": r.trial_name, "rewards": r.reward(), "exception": r.exception_info.as_ref().map(|e| &e.exception_type), "trial_uri": r.trial_uri
            }))?);
            Ok(())
        }
        Cmd::Datasets { cmd: DatasetsCmd::List(r) } => {
            for d in registry::load(r.registry_url.as_deref(), r.registry_path.as_deref())? {
                println!("{}@{}  ({} tasks)  {}", d.name, d.version, d.tasks.len(), d.description);
            }
            Ok(())
        }
        Cmd::Datasets { cmd: DatasetsCmd::Download { dataset, registry: r, output_dir } } => {
            let (name, version) = match dataset.split_once('@') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (dataset.clone(), None),
            };
            let d = DatasetConfig { name: Some(name), version, registry_url: r.registry_url, registry_path: r.registry_path, download_dir: output_dir, ..Default::default() };
            let tasks = registry::dataset_tasks(&d, false)?;
            registry::download(&tasks)?;
            for t in &tasks {
                println!("{}", registry::local_path(t).display());
            }
            Ok(())
        }
        Cmd::Tasks {
            cmd: TasksCmd::Init { name, tasks_dir, org, description, no_pytest, no_solution, include_canary_strings, metadata_template, include_standard_metadata, no_package, author, steps },
        } => {
            if include_standard_metadata {
                bail!("--include-standard-metadata has been removed. Use --metadata-template <path-to-template.toml> instead.");
            }
            if let Some(t) = &metadata_template
                && !t.exists()
            {
                bail!("Template file not found: {}", t.display());
            }
            let name = authoring::resolve_name(name, org)?;
            let opts = authoring::TaskOpts { no_pytest, no_solution, include_canary_strings, no_package, description, authors: authoring::parse_authors(&author), metadata_template, steps, ..Default::default() };
            authoring::init_task(&name, &tasks_dir, &opts).map(|_| ())
        }
        Cmd::Init(a) => run_init(*a),
        Cmd::Add(a) | Cmd::Datasets { cmd: DatasetsCmd::Add(a) } => authoring::add(&a.packages, &a.to, a.scan),
        Cmd::Remove(a) | Cmd::Datasets { cmd: DatasetsCmd::Remove(a) } => authoring::remove(&a.package, &a.from, a.scan),
        Cmd::Sync { path, upgrade, concurrency: _ } => authoring::sync(&path, upgrade),
        Cmd::Datasets { cmd: DatasetsCmd::Init { name, output_dir, org, description, with_metric, author } } => {
            let name = authoring::resolve_name(name, org)?;
            authoring::init_dataset(&name, &output_dir, &description, with_metric, &authoring::parse_authors(&author))
        }
        Cmd::Tasks { cmd: TasksCmd::Check { path } } => {
            let dirs: Vec<PathBuf> = if Task::is_task_dir(&path) {
                vec![path]
            } else {
                let mut v: Vec<PathBuf> = std::fs::read_dir(&path)?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
                v.sort();
                v
            };
            let mut bad = 0;
            for d in dirs {
                match Task::load(&d).and_then(|t| t.validate_tests().map(|_| t)) {
                    Ok(t) => println!("ok    {}", t.name()),
                    Err(e) => {
                        bad += 1;
                        println!("FAIL  {}: {e:#}", d.display());
                    }
                }
            }
            if bad > 0 {
                bail!("{bad} invalid task(s)");
            }
            Ok(())
        }
        Cmd::Sweeps { cmd: SweepsCmd::Run { config, max_sweeps, trials_per_task, hint, hints_file } } => {
            let cfg = JobConfig::load(&config)?;
            preflight_agents(&cfg)?;
            env::preflight().await?;
            let hints: BTreeMap<String, String> = match hints_file {
                Some(p) => serde_json::from_str(&std::fs::read_to_string(&p)?).context("--hints-file must be a JSON object {task_name: hint}")?,
                None => BTreeMap::new(),
            };
            for d in sweeps::run(cfg, max_sweeps, trials_per_task, hint, hints).await? {
                println!("{}", d.display());
            }
            Ok(())
        }
        Cmd::Tasks { cmd: TasksCmd::Migrate { input, output, cpus, memory_mb, storage_mb, gpus } } => {
            println!("⚠️  WARNING: This migration tool is not foolproof. Please review migrated tasks carefully and make manual adjustments as needed.\n");
            if !input.exists() {
                println!("❌ Input path does not exist: {}", input.display());
                return Ok(());
            }
            let single = input.join("task.yaml").exists();
            if single {
                println!("📦 Migrating single task from: {}", input.display());
            } else {
                println!("📦 Migrating tasks from: {}", input.display());
                println!("📤 Output directory: {}", output.display());
            }
            match migrate::migrate(&input, &output, migrate::Overrides { cpus, memory_mb, storage_mb, gpus }) {
                Ok(dirs) if single => println!("✓ Successfully migrated task to: {}", dirs[0].display()),
                Ok(dirs) if dirs.is_empty() => println!("⚠️  No tasks found to migrate"),
                Ok(dirs) => println!("✓ Successfully migrated {} task(s)", dirs.len()),
                Err(e) if single => println!("❌ Failed to migrate task: {e:#}"),
                Err(e) => return Err(e),
            }
            Ok(())
        }
        Cmd::Cache { cmd: CacheCmd::Clean { force, dry, no_docker, no_cache_dir } } => tools::cache_clean(force, dry, no_docker, no_cache_dir).await,
        Cmd::Tasks { cmd: TasksCmd::StartEnv { path, no_all, non_interactive, agent, model, agent_kwargs } } => {
            env::preflight().await?;
            let a = agent.map(|name| AgentConfig {
                name: Some(name),
                model_name: model,
                kwargs: agent_kwargs.iter().map(|(k, v)| (k.clone(), kwarg_value(v))).collect(),
                ..Default::default()
            });
            tools::start_env(&path, !no_all, !non_interactive, a).await
        }
        Cmd::Trials { cmd: TrialsCmd::Handoff { trial_dir } } => {
            let cmd = tools::handoff_command(&trial_dir.canonicalize()?, &std::env::current_dir()?)?;
            println!("Resuming session from trial {} with: {}", trial_dir.display(), cmd.join(" "));
            use std::os::unix::process::CommandExt;
            Err(std::process::Command::new(&cmd[0]).args(&cmd[1..]).exec().into())
        }
        Cmd::Agents { cmd: None | Some(AgentsCmd::List { json: false }) } => {
            for n in agents::agent_names() {
                println!("{n}");
            }
            Ok(())
        }
        Cmd::Agents { cmd: Some(AgentsCmd::List { json: true }) } => {
            println!("{}", serde_json::to_string_pretty(&agents::agent_names())?);
            Ok(())
        }
        Cmd::Agents { cmd: Some(AgentsCmd::Schema { name, json }) } => {
            let Some(s) = schemas::agent(&name) else { bail!("Agent '{name}' does not declare an options_model") };
            if json {
                println!("{s}");
            } else {
                print!("{}", schemas::options_table(&name, &serde_json::from_str(s)?));
            }
            Ok(())
        }
        Cmd::Tasks { cmd: TasksCmd::Schema } => {
            println!("{}", schemas::TASK);
            Ok(())
        }
        Cmd::Trials { cmd: TrialsCmd::Schema } => {
            println!("{}", schemas::TRIAL);
            Ok(())
        }
        Cmd::Analyze(a) => analyze::analyze_command(*a).await,
        Cmd::Check(a) => analyze::check_command(*a).await,
        Cmd::View { jobs_dir, port, host } => viewer::serve(&jobs_dir, &host, port),
        Cmd::Traces { cmd: TracesCmd::Export { path, output, no_recursive, episodes, filter, sharegpt, instruction_metadata, verifier_metadata } } => {
            let o = traces::ExportOpts {
                recursive: !no_recursive,
                episodes,
                success_filter: filter.filter(|f| f != "all"),
                sharegpt,
                include_instruction: instruction_metadata,
                include_verifier_output: verifier_metadata,
            };
            traces::run(&path, output.as_deref(), &o)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn globs() {
        let g = glob_match;
        assert!(g("*", "anything") && g("hello-*", "hello-world") && g("h?llo", "hello") && g("*-c-*", "polyglot-c-py"));
        assert!(!g("hello", "hello-world") && !g("*x", "abc") && !g("a?", "a"));
        assert!(g("[ab]*", "beta") && !g("[!ab]*", "beta") && g("x.y", "x.y") && !g("x.y", "xzy"));
    }

    #[test]
    fn cli_builds_job_config() {
        let dir = std::env::temp_dir().join(format!("hbench-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cli = Cli::parse_from([
            "hbench", "run", "-p", dir.to_str().unwrap(), "-a", "claude-code", "-m", "anthropic/a", "-m", "anthropic/b", "--ak", "max_turns=5", "--ae", "X=1", "-k", "3", "-i", "t*",
            "--no-delete", "--artifact", "/app/x",
        ]);
        let Cmd::Run(a) = cli.cmd else { panic!() };
        let c = build_job_config(&a).unwrap();
        assert_eq!(c.agents.len(), 2);
        assert_eq!(c.agents[1].model_name.as_deref(), Some("anthropic/b"));
        assert_eq!(c.agents[0].kwargs["max_turns"], 5);
        assert_eq!(c.agents[0].env["X"], "1");
        assert_eq!((c.n_attempts, c.environment.delete), (3, false));
        assert_eq!(c.datasets[0].task_names.as_ref().unwrap(), &["t*"]);
        assert_eq!(c.artifacts, vec![Artifact::Path("/app/x".into())]);
    }
}
