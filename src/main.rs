mod agents;
mod analyze;
mod artifacts;
mod atif;
mod authoring;
mod bridge;
mod build_info;
mod config;
mod diff;
mod env;
mod exec;
mod fmt;
mod fsutil;
mod integrity;
mod job;
mod manifest;
mod metrics;
mod migrate;
mod oci;
mod redact;
mod registry;
mod regrade;
mod schemas;
mod sweeps;
mod task;
mod tools;
mod traces;
mod trial;
mod update;
mod viewer;
mod wizard;

use crate::config::{AgentConfig, DatasetConfig, JobConfig, SourceTrial, TaskRef, TrialConfig};
use crate::task::{Artifact, Task};
use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

/// hbench: run agent benchmarks on tasks in Docker.
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
        /// Folder containing job directories or task definitions (auto-detected).
        folder: Option<PathBuf>,
        /// Same as FOLDER. A port or port range here (e.g. `-p 9000`) is taken as `--port`.
        #[arg(short = 'p', long = "path")]
        path: Option<String>,
        /// Port or port range (e.g., 8080 or 8080-8089).
        #[arg(long, default_value = "8080-8089")]
        port: String,
        /// Host to bind the server to. Only on a loopback address (127.0.0.1, ::1, localhost) does the
        /// viewer start runs and delete jobs; bound to any other address it is read-only, since anyone who
        /// can reach it could otherwise run hbench with any config.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Force task definitions mode.
        #[arg(long, conflicts_with = "jobs")]
        tasks: bool,
        /// Force jobs mode.
        #[arg(long)]
        jobs: bool,
        /// Ignored: the viewer is prebuilt and embedded.
        #[arg(long, hide = true)]
        build: bool,
        #[arg(long, hide = true)]
        no_build: bool,
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
    /// Compile paths into tasks and run a job.
    ///
    /// Experimental: flags and behavior may change.
    ///
    /// Verification: use --print-config to inspect auto-inferred artifacts, --artifact to override them, or --disable-verification to skip existence-only checks.
    Exec(Box<exec::ExecArgs>),
    /// Print a shell completion script, e.g. `hbench completions zsh > ~/.zfunc/_hbench`.
    Completions {
        /// Target shell.
        shell: clap_complete::Shell,
    },
    /// Install the latest release (runs `curl -fsSL https://nodeq.tech/install.sh | sh`).
    Update,
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
    /// Directory to create the task or dataset in.
    #[arg(short = 'o', long, default_value = ".")]
    output_dir: PathBuf,
    /// Organization name (if name lacks org/).
    #[arg(long)]
    org: Option<String>,
    /// Description written to the manifest.
    #[arg(long, default_value = "")]
    description: String,
    /// Author in 'Name <email>' or 'Name' format. Repeatable.
    #[arg(long)]
    author: Vec<String>,
    /// Create metric.py template (dataset only).
    #[arg(long)]
    with_metric: bool,
    /// Don't scaffold a pytest test (task only).
    #[arg(long)]
    no_pytest: bool,
    /// Don't scaffold solution/ (task only).
    #[arg(long)]
    no_solution: bool,
    /// Add canary strings to generated files (task only).
    #[arg(long)]
    include_canary_strings: bool,
    /// Add standard metadata fields to task.toml (task only).
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
    /// Run sweeps from a job config.
    Run {
        /// Job config (YAML or JSON).
        #[arg(short = 'c', long = "config")]
        config: PathBuf,
        /// Maximum number of sweeps.
        #[arg(long, default_value_t = 3)]
        max_sweeps: usize,
        /// Attempts per task in each sweep.
        #[arg(long, default_value_t = 2)]
        trials_per_task: usize,
        /// Hint appended to every task instruction.
        #[arg(long)]
        hint: Option<String>,
        /// JSON object {task_name: hint}; runs one job per task and appends the task's hint to its instruction.
        #[arg(long)]
        hints_file: Option<PathBuf>,
        /// Skip the agent credential check before starting.
        #[arg(long)]
        skip_preflight: bool,
    },
}

#[derive(Subcommand)]
enum TracesCmd {
    /// Export ATIF trajectories as conversation rows (JSONL, HF trace schema).
    Export {
        /// Trial dir, or a root containing trials recursively.
        #[arg(short = 'p', long = "path")]
        path: PathBuf,
        /// Output .jsonl (default: stdout).
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,
        /// Only look at the given dir, not its subdirectories.
        #[arg(long)]
        no_recursive: bool,
        /// Episodes per trial to export.
        #[arg(long, value_enum, default_value_t = traces::Episodes::All)]
        episodes: traces::Episodes,
        /// Keep only successful or failed trials.
        #[arg(long, value_enum, default_value_t = traces::TrialFilter::All)]
        filter: traces::TrialFilter,
        /// Also emit a ShareGPT `conversations_sharegpt` column.
        #[arg(long)]
        sharegpt: bool,
        /// Include the task instruction in each row.
        #[arg(long)]
        instruction_metadata: bool,
        /// Include verifier output in each row.
        #[arg(long)]
        verifier_metadata: bool,
    },
}

#[derive(Subcommand)]
enum AgentsCmd {
    /// List agent names.
    List {
        /// Print as a JSON array.
        #[arg(long)]
        json: bool,
    },
    /// The user-facing kwargs an agent accepts.
    Schema {
        /// Agent name.
        name: String,
        /// Print the raw JSON schema.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Remove ~/.cache/hbench and hbench-built docker images.
    Clean {
        /// Don't ask for confirmation.
        #[arg(short = 'f', long)]
        force: bool,
        /// Show what would be removed.
        #[arg(long)]
        dry: bool,
        /// Keep docker images.
        #[arg(long)]
        no_docker: bool,
        /// Keep the cache dir.
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
        /// Job dir to resume.
        #[arg(short = 'p', long = "job-path")]
        path: PathBuf,
        /// Remove trials with these error types before resuming (repeatable).
        #[arg(short = 'f', long = "filter-error-type", default_values_t = vec!["CancelledError".to_string()])]
        filter_error_types: Vec<String>,
        /// Skip the agent credential check before starting.
        #[arg(long)]
        skip_preflight: bool,
        /// Exit 1 if any trial errored.
        #[arg(long)]
        fail_on_error: bool,
    },
    /// Print a job's stats.
    Summary {
        /// Job dir.
        #[arg(required_unless_present = "job_path", conflicts_with = "job_path")]
        path: Option<PathBuf>,
        /// Job dir (same as the positional argument).
        #[arg(short = 'p', long = "job-path")]
        job_path: Option<PathBuf>,
        /// Print result.json (stats included) as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check a job's files against its manifest.json: exit 1 if any was changed, removed or added.
    Verify {
        /// Job dir.
        #[arg(required_unless_present = "job_path", conflicts_with = "job_path")]
        path: Option<PathBuf>,
        /// Job dir (same as the positional argument).
        #[arg(short = 'p', long = "job-path")]
        job_path: Option<PathBuf>,
    },
    /// Re-run (separate) verification against a finished job's recorded outputs.
    Regrade {
        /// Source job dir whose trials are regraded.
        #[arg(long)]
        source: PathBuf,
        /// Task dir or dir of tasks providing the (new) verifiers; matched by task name.
        #[arg(short = 'p', long = "path")]
        path: PathBuf,
        /// Where the regrade job dir is created.
        #[arg(long, default_value = "jobs")]
        jobs_dir: PathBuf,
        /// Regrade job name (default: timestamp).
        #[arg(long)]
        job_name: Option<String>,
        /// Concurrent trials.
        #[arg(short = 'n', long = "n-concurrent", default_value_t = 4)]
        n_concurrent: usize,
    },
}

#[derive(Subcommand)]
enum TrialsCmd {
    /// Print the trial config JSON schema.
    Schema,
    /// Resume a finished trial's agent session in the local agent CLI (claude-code, muse-code).
    Handoff {
        /// Trial dir of a finished trial.
        trial_dir: PathBuf,
    },
    /// Run one trial: `-p task` or `-c trial.yaml`.
    Start {
        /// Trial config (YAML or JSON).
        #[arg(short = 'c', long = "config")]
        config: Option<PathBuf>,
        /// Task dir.
        #[arg(short = 'p', long = "path")]
        path: Option<PathBuf>,
        /// Agent name (see `hbench agents list`).
        #[arg(short = 'a', long = "agent", default_value = "oracle")]
        agent: String,
        /// Model, usually provider/name.
        #[arg(short = 'm', long = "model")]
        model: Option<String>,
        /// Agent kwarg KEY=VALUE (JSON values allowed), repeatable.
        #[arg(long = "ak", value_parser = parse_kv)]
        agent_kwargs: Vec<(String, String)>,
        /// Agent env var KEY=VALUE, repeatable.
        #[arg(long = "ae", value_parser = parse_kv)]
        agent_env: Vec<(String, String)>,
        /// Where the trial dir is created.
        #[arg(long, default_value = "trials")]
        trials_dir: PathBuf,
        /// Skip the agent credential check before starting.
        #[arg(long)]
        skip_preflight: bool,
    },
}

#[derive(Subcommand)]
enum DatasetsCmd {
    /// List datasets in a registry (--registry-url, --registry-path, --registry oci-layout:<dir>, or --registry oci://... with a dataset index).
    List(RegistryArgs),
    /// Download a registry dataset's tasks into the cache (or --output-dir); needs --registry-url, --registry-path or --registry.
    Download {
        /// Dataset, name[@version].
        dataset: String,
        #[command(flatten)]
        registry: RegistryArgs,
        #[command(flatten)]
        lock: LockArgs,
        /// Download into this dir instead of the cache.
        #[arg(short = 'o', long)]
        output_dir: Option<PathBuf>,
    },
    /// Pack every task directory (one with a task.toml) in TASKS_DIR into an OCI image layout, `<dir>/<name>`,
    /// tagged with the version; prints the manifest digest.
    Pack {
        /// Directory whose subdirectories are the tasks.
        tasks_dir: PathBuf,
        /// Source descriptor (YAML): the dataset's name, version, licence and the rest of its config.
        #[arg(long)]
        descriptor: PathBuf,
        /// Directory the layout is written under.
        #[arg(long, value_name = "DIR")]
        oci_layout: PathBuf,
    },
    /// Pack TASKS_DIR as `pack` does and push it to a registry, with the docker config's credentials; prints the
    /// manifest digest.
    Push {
        /// Directory whose subdirectories are the tasks.
        tasks_dir: PathBuf,
        /// Source descriptor (YAML): the dataset's name, version, licence and the rest of its config.
        #[arg(long)]
        descriptor: PathBuf,
        /// oci://<registry>/<repository>:<version>, the repository ending in the dataset name.
        #[arg(long, value_name = "TARGET")]
        to: String,
        /// Move the tag when it already names another manifest.
        #[arg(long)]
        allow_overwrite: bool,
    },
    /// Pack a registry.json dataset's tasks into an OCI image layout or push them to a registry; prints
    /// `name@version` and the manifest digest.
    Convert {
        /// A registry.json, or its http(s) URL.
        registry: String,
        /// Dataset, name@version.
        #[arg(short = 'd', long)]
        dataset: String,
        /// Write the dataset to an OCI image layout, `<dir>/<name>`.
        #[arg(long, value_name = "DIR", required_unless_present = "to", conflicts_with = "to")]
        oci_layout: Option<PathBuf>,
        /// Push the dataset to oci://<registry>/<repository>:<version>.
        #[arg(long, value_name = "TARGET")]
        to: Option<String>,
        /// Source descriptor (YAML); without one the licence is NOASSERTION.
        #[arg(long)]
        descriptor: Option<PathBuf>,
        /// Unpack the result and compare it, file by file, with the git checkouts.
        #[arg(long)]
        check_equivalence: bool,
        /// With --to, move the tag when it already names another manifest.
        #[arg(long)]
        allow_overwrite: bool,
    },
    /// Copy a dataset from a registry into an OCI image layout, `<dir>/<name>`, for offline use; prints the
    /// manifest digest.
    Export {
        /// Dataset, name@version.
        #[arg(short = 'd', long)]
        dataset: String,
        /// oci://<registry>/<namespace>, or oci-layout:<dir>.
        #[arg(long)]
        registry: String,
        /// Directory the layout is written under.
        #[arg(long, value_name = "DIR")]
        oci_layout: PathBuf,
    },
    /// Initialize a new dataset directory.
    Init {
        /// Dataset name (org/name format).
        name: Option<String>,
        /// Directory to create the dataset in.
        #[arg(short = 'o', long, default_value = ".")]
        output_dir: PathBuf,
        /// Organization name (if name lacks org/).
        #[arg(long)]
        org: Option<String>,
        /// Description written to dataset.toml.
        #[arg(long, default_value = "")]
        description: String,
        /// Create metric.py template.
        #[arg(long)]
        with_metric: bool,
        /// Author in 'Name <email>' or 'Name' format. Repeatable.
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
        /// Task name (org/name format).
        name: Option<String>,
        /// Directory to create the task in.
        #[arg(short = 'p', long = "tasks-dir", default_value = ".")]
        tasks_dir: PathBuf,
        /// Organization name (if name lacks org/).
        #[arg(long)]
        org: Option<String>,
        /// Description written to task.toml.
        #[arg(long, default_value = "")]
        description: String,
        /// Don't scaffold a pytest test.
        #[arg(long)]
        no_pytest: bool,
        /// Don't scaffold solution/.
        #[arg(long)]
        no_solution: bool,
        /// Add canary strings to generated files.
        #[arg(long)]
        include_canary_strings: bool,
        /// Path to a template TOML file for task metadata and defaults.
        #[arg(long)]
        metadata_template: Option<PathBuf>,
        /// Add standard metadata fields to task.toml.
        #[arg(long, conflicts_with = "metadata_template")]
        include_standard_metadata: bool,
        /// Skip [task] package section in task.toml.
        #[arg(long)]
        no_package: bool,
        /// Author in 'Name <email>' or 'Name' format. Repeatable.
        #[arg(long)]
        author: Vec<String>,
        /// Scaffold a multi-step task with N steps (0 = single-step).
        #[arg(long, default_value_t = 0)]
        steps: usize,
    },
    /// Print the task.toml JSON schema.
    Schema,
    /// Start a task's environment and attach a shell (solution + tests uploaded by default).
    StartEnv {
        /// Task dir.
        #[arg(short = 'p', long = "path")]
        path: PathBuf,
        /// Don't upload solution/ and tests/.
        #[arg(long)]
        no_all: bool,
        /// Start the environment without attaching a shell.
        #[arg(long)]
        non_interactive: bool,
        /// Also install this agent in the environment.
        #[arg(long)]
        agent: Option<String>,
        /// Model for --agent.
        #[arg(short = 'm', long = "model")]
        model: Option<String>,
        /// Agent kwarg KEY=VALUE (JSON values allowed), repeatable.
        #[arg(long = "ak", value_parser = parse_kv)]
        agent_kwargs: Vec<(String, String)>,
    },
    /// Migrate Terminal-Bench (v1) tasks to the current task format.
    Migrate {
        /// Task dir or dir of tasks to migrate.
        #[arg(short = 'i', long = "input")]
        input: PathBuf,
        /// Output dir.
        #[arg(short = 'o', long = "output")]
        output: PathBuf,
        /// Override CPUs in migrated task.toml.
        #[arg(long)]
        cpus: Option<i64>,
        /// Override memory (MB).
        #[arg(long)]
        memory_mb: Option<i64>,
        /// Override storage (MB).
        #[arg(long)]
        storage_mb: Option<i64>,
        /// Override GPUs.
        #[arg(long)]
        gpus: Option<i64>,
    },
    /// Print each task's checksum and lock digest, as trial results and lock.json record them.
    Digest {
        /// A task directory, or a directory of tasks.
        #[arg(short = 'p', long = "path")]
        path: PathBuf,
    },
    /// Validate task dirs (config, files, test scripts).
    Check {
        /// A task directory, or a directory of tasks.
        #[arg(short = 'p', long = "path")]
        path: PathBuf,
    },
}

#[derive(Args)]
struct RegistryArgs {
    /// Dataset registry URL (a registry.json), instead of the default OCI registry.
    #[arg(long)]
    registry_url: Option<String>,
    /// Local dataset registry file (a registry.json), used instead of --registry-url.
    #[arg(long)]
    registry_path: Option<PathBuf>,
    /// OCI dataset registry, oci://<registry>/<namespace>, or OCI image layouts on disk, oci-layout:<dir>.
    /// Default: $HBENCH_REGISTRY, else oci://ghcr.io/node-q/hbench-datasets.
    #[arg(long, conflicts_with_all = ["registry_url", "registry_path"])]
    registry: Option<String>,
}

#[derive(Args)]
struct LockArgs {
    /// Refuse an OCI dataset that is not in ./hbench.lock, or whose tag now resolves to another digest.
    #[arg(long, conflicts_with = "lock_write")]
    locked: bool,
    /// Record each OCI dataset's manifest digest in ./hbench.lock.
    #[arg(long)]
    lock_write: bool,
}

impl LockArgs {
    fn mode(&self) -> oci::lockfile::LockMode {
        match (self.locked, self.lock_write) {
            (true, _) => oci::lockfile::LockMode::Locked,
            (false, true) => oci::lockfile::LockMode::Write,
            (false, false) => oci::lockfile::LockMode::Read,
        }
    }
}

#[derive(Args)]
struct RunArgs {
    /// Job config (YAML or JSON). CLI flags override it.
    #[arg(short = 'c', long = "config", help_heading = "Selection")]
    config: Option<PathBuf>,
    /// Local task dir, or a dir of task dirs.
    #[arg(short = 'p', long = "path", help_heading = "Selection")]
    path: Option<PathBuf>,
    /// Registry dataset, name[@version]. Needs --registry-url, --registry-path or --registry.
    #[arg(short = 'd', long = "dataset", help_heading = "Selection")]
    dataset: Option<String>,
    #[command(flatten, next_help_heading = "Selection")]
    registry: RegistryArgs,
    #[command(flatten, next_help_heading = "Selection")]
    lock: LockArgs,
    /// Only tasks matching this glob (repeatable).
    #[arg(short = 'i', long = "include-task-name", help_heading = "Selection")]
    include: Vec<String>,
    /// Skip tasks matching this glob (repeatable).
    #[arg(short = 'x', long = "exclude-task-name", help_heading = "Selection")]
    exclude: Vec<String>,
    /// Run at most N tasks.
    #[arg(short = 'l', long = "n-tasks", help_heading = "Selection")]
    n_tasks: Option<usize>,
    /// Attempts per task.
    #[arg(short = 'k', long = "n-attempts", help_heading = "Selection")]
    n_attempts: Option<usize>,
    /// Compare against earlier job(s): reuse / regrade / rerun (repeatable).
    #[arg(long, help_heading = "Selection")]
    diff: Vec<PathBuf>,
    /// Don't ask for confirmation (with --diff).
    #[arg(short = 'y', long, help_heading = "Selection")]
    yes: bool,

    /// Agent name (see `hbench agents list`).
    #[arg(short = 'a', long = "agent", help_heading = "Agent")]
    agent: Option<String>,
    /// Model(s); one agent config per model.
    #[arg(short = 'm', long = "model", help_heading = "Agent")]
    models: Vec<String>,
    /// Agent kwarg KEY=VALUE (JSON values allowed), repeatable.
    #[arg(long = "ak", alias = "agent-kwarg", value_parser = parse_kv, help_heading = "Agent")]
    agent_kwargs: Vec<(String, String)>,
    /// Agent env var KEY=VALUE, repeatable.
    #[arg(long = "ae", alias = "agent-env", value_parser = parse_kv, help_heading = "Agent")]
    agent_env: Vec<(String, String)>,
    /// Per-agent cap on concurrent agent runs.
    #[arg(long, help_heading = "Agent")]
    n_concurrent_agent: Option<usize>,
    /// Skill dir uploaded to /hbench/skills (repeatable).
    #[arg(long, help_heading = "Agent")]
    skill: Vec<String>,
    /// Text appended to every task instruction (repeatable).
    #[arg(long, help_heading = "Agent")]
    extra_instruction: Vec<String>,
    /// File whose contents are appended to every task instruction (repeatable).
    #[arg(long, help_heading = "Agent")]
    extra_instruction_path: Vec<PathBuf>,
    /// Only install the agent; don't run it.
    #[arg(long, help_heading = "Agent")]
    install_only: bool,
    /// Don't check that agent credentials are set before starting.
    #[arg(long, help_heading = "Agent")]
    skip_preflight: bool,

    /// Environment (container) env var KEY=VALUE, repeatable.
    #[arg(long = "ee", alias = "environment-env", value_parser = parse_kv, help_heading = "Environment")]
    env_env: Vec<(String, String)>,
    /// Verifier env var KEY=VALUE, repeatable.
    #[arg(long = "ve", alias = "verifier-env", value_parser = parse_kv, help_heading = "Environment")]
    verifier_env: Vec<(String, String)>,
    /// Concurrent trials.
    #[arg(short = 'n', long = "n-concurrent", help_heading = "Environment")]
    n_concurrent: Option<usize>,
    /// Rebuild task images even if cached.
    #[arg(long, help_heading = "Environment")]
    force_build: bool,
    /// Keep images/volumes after trials.
    #[arg(long, help_heading = "Environment")]
    no_delete: bool,
    /// Stop (don't remove) containers after trials.
    #[arg(long, help_heading = "Environment")]
    keep_containers: bool,
    /// Skip the verifier (no rewards).
    #[arg(long, help_heading = "Environment")]
    disable_verification: bool,
    /// Override each task's CPU count.
    #[arg(long, help_heading = "Environment")]
    override_cpus: Option<u32>,
    /// Override each task's memory (MB).
    #[arg(long, help_heading = "Environment")]
    override_memory_mb: Option<u64>,
    /// Override each task's GPU count.
    #[arg(long, help_heading = "Environment")]
    override_gpus: Option<u32>,
    /// Extra host allowed during network-restricted tasks (repeatable).
    #[arg(long, help_heading = "Environment")]
    extra_allowed_host: Vec<String>,

    /// Scale every timeout.
    #[arg(long, help_heading = "Timeouts")]
    timeout_multiplier: Option<f64>,
    /// Scale the agent run timeout.
    #[arg(long, help_heading = "Timeouts")]
    agent_timeout_multiplier: Option<f64>,
    /// Scale the verifier timeout.
    #[arg(long, help_heading = "Timeouts")]
    verifier_timeout_multiplier: Option<f64>,
    /// Scale the agent setup (install) timeout.
    #[arg(long, help_heading = "Timeouts")]
    agent_setup_timeout_multiplier: Option<f64>,
    /// Scale the image build timeout.
    #[arg(long, help_heading = "Timeouts")]
    environment_build_timeout_multiplier: Option<f64>,
    /// Retries per failed trial.
    #[arg(long, help_heading = "Timeouts")]
    max_retries: Option<u32>,
    /// Retry only these exception types (repeatable).
    #[arg(long = "retry-include", help_heading = "Timeouts")]
    retry_include: Vec<String>,
    /// Never retry these exception types (repeatable).
    #[arg(long = "retry-exclude", help_heading = "Timeouts")]
    retry_exclude: Vec<String>,

    /// Where job dirs are created (default: jobs).
    #[arg(long, help_heading = "Output")]
    jobs_dir: Option<PathBuf>,
    /// Job dir name (default: timestamp).
    #[arg(long, help_heading = "Output")]
    job_name: Option<String>,
    /// Artifact path to collect (repeatable).
    #[arg(long, help_heading = "Output")]
    artifact: Vec<String>,
    /// No per-trial progress lines.
    #[arg(long, help_heading = "Output")]
    quiet: bool,
    /// Exit 1 if any trial errored (default: only when every trial errored).
    #[arg(long, help_heading = "Output")]
    fail_on_error: bool,
}

/// One task directory, or every task directory directly under `path`, sorted.
fn task_dirs(path: &std::path::Path) -> Result<Vec<PathBuf>> {
    if Task::is_task_dir(path) {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(path)?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    Ok(dirs)
}

fn push_summary(p: &oci::publish::Pushed) -> String {
    let c = &p.copied;
    if p.unchanged {
        format!("{} names this manifest already, so nothing was pushed", c.reference)
    } else {
        format!("pushed to {}: {} blob(s), {} uploaded ({} bytes)", c.reference, c.blobs, c.copied, c.copied_bytes)
    }
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or_else(|| format!("expected KEY=VALUE, got {s:?}"))
}

/// fnmatch-style glob: `*`, `?`, `[seq]`, `[!seq]` (case-sensitive).
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
            registry: a.registry.registry.clone(),
            ..Default::default()
        });
    }
    // CLI task filters apply to every dataset and to the job's single tasks.
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
    if !cfg.tasks.is_empty() && (!a.include.is_empty() || !a.exclude.is_empty() || a.n_tasks.is_some()) {
        let f = DatasetConfig {
            task_names: Some(a.include.clone()),
            exclude_task_names: Some(a.exclude.clone()),
            n_tasks: a.n_tasks,
            ..Default::default()
        };
        cfg.tasks = registry::filter_names(std::mem::take(&mut cfg.tasks), TaskRef::display_name, &f)?;
    }
    apply_agent_flags(&mut cfg, a);
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

/// `-a`/`-m` replace the config's agents with one per model, based on its first agent; `--ak`, `--ae`,
/// `--skill` and `--n-concurrent-agent` apply to every agent.
fn apply_agent_flags(cfg: &mut JobConfig, a: &RunArgs) {
    let has_overrides = !a.agent_kwargs.is_empty() || !a.agent_env.is_empty() || a.n_concurrent_agent.is_some() || !a.skill.is_empty();
    if a.agent.is_some() || !a.models.is_empty() || (has_overrides && cfg.agents.is_empty()) {
        let base = cfg.agents.first().cloned().unwrap_or_default();
        let base = AgentConfig { name: a.agent.clone().or(base.name), ..base };
        let models: Vec<Option<String>> =
            if a.models.is_empty() { vec![base.model_name.clone()] } else { a.models.iter().cloned().map(Some).collect() };
        cfg.agents = models.into_iter().map(|m| AgentConfig { model_name: m, ..base.clone() }).collect();
    }
    for c in cfg.agents.iter_mut() {
        for (k, v) in &a.agent_kwargs {
            c.kwargs.insert(k.clone(), kwarg_value(v));
        }
        c.env.extend(a.agent_env.iter().cloned());
        if a.n_concurrent_agent.is_some() {
            c.n_concurrent = a.n_concurrent_agent;
        }
        c.skills.extend(a.skill.iter().cloned());
    }
}

/// Unknown agent names and agent kwargs outside the agent's options schema. `acp:<id>[@version]`
/// names any agent in the ACP registry, so only the spec's shape is checked here.
fn preflight_agents(cfg: &JobConfig) -> Result<()> {
    let known = agents::agent_names();
    for a in &cfg.agents {
        if agents::installed::acp::is_acp_registry_shorthand(a.name()) {
            agents::installed::acp::parse_registry_spec(a.name()).with_context(|| format!("agent {:?}", a.name()))?;
        } else if !known.contains(&a.name()) {
            bail!("Unknown agent {:?}. Known agents: {}", a.name(), known.join(", "));
        }
        agents::check_kwargs(a.name(), &a.kwargs)?;
    }
    Ok(())
}

/// Credential env vars an agent accepts, any one of which is enough (API keys, OAuth tokens,
/// Bedrock/Vertex switches, custom endpoints). None = not decidable up front, so not checked.
fn auth_envs(agent: &str) -> Option<&'static [&'static str]> {
    // Only agents with a fixed provider; passthrough agents depend on the model/endpoint.
    Some(match agent {
        "claude-code" => &[
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "AWS_BEARER_TOKEN_BEDROCK",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
        ],
        "codex" => &["OPENAI_API_KEY", "CODEX_AUTH_JSON_PATH", "CODEX_FORCE_AUTH_JSON", "OPENAI_BASE_URL", "OPENAI_API_BASE"],
        "gemini-cli" => &[
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GOOGLE_GENERATIVE_AI_API_KEY",
            "GEMINI_OAUTH_CREDS_PATH",
            "GEMINI_FORCE_OAUTH",
            "GOOGLE_GENAI_USE_VERTEXAI",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_GEMINI_BASE_URL",
        ],
        _ => return None,
    })
}

/// Fail fast when an agent has none of its credential env vars (host env or agent `env`).
fn preflight_auth(cfg: &JobConfig) -> Result<()> {
    if cfg.install_only {
        return Ok(());
    }
    for a in &cfg.agents {
        let Some(names) = auth_envs(a.name()) else { continue };
        let env = crate::task::resolve_env_vars(&a.env).unwrap_or_default();
        let set = |n: &&str| env.get(*n).or(std::env::var(n).ok().as_ref()).is_some_and(|v| !v.is_empty());
        if !names.iter().any(set) {
            bail!("agent {} needs credentials: set {} (or one of {}), or pass --skip-preflight", a.name(), names[0], names[1..].join(", "));
        }
    }
    Ok(())
}

/// Marker error: the run was cancelled by a signal (exit 130).
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Subdirectory names (trial names) of these job dirs.
fn trial_names(job_dirs: &[PathBuf]) -> Vec<String> {
    job_dirs
        .iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Cancel on SIGINT/SIGTERM/SIGHUP: the first signal flips the returned flag (trials tear down);
/// the second force-quits with 130 after a quick `docker compose down` of the projects of
/// `trials()` (trial names; compose projects are named after them).
pub fn cancel_on_signal(trials: impl Fn() -> Vec<String> + Send + 'static) -> tokio::sync::watch::Receiver<bool> {
    use tokio::signal::unix::{SignalKind, signal};
    let (sig_tx, mut sigs) = tokio::sync::mpsc::unbounded_channel();
    for kind in [SignalKind::interrupt(), SignalKind::terminate(), SignalKind::hangup()] {
        let Ok(mut s) = signal(kind) else { continue };
        let sig_tx = sig_tx.clone();
        tokio::spawn(async move { while s.recv().await.is_some() && sig_tx.send(()).is_ok() {} });
    }
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if sigs.recv().await.is_none() {
            return;
        }
        eprintln!("\ncancelling: tearing down containers (signal again to force quit)...");
        let _ = tx.send(true);
        if sigs.recv().await.is_some() {
            eprintln!("force quit: removing this run's compose projects (signal again to skip)...");
            tokio::select! {
                _ = force_cleanup(trials()) => {}
                _ = sigs.recv() => {}
            }
            std::process::exit(130);
        }
    });
    rx
}

/// Best effort, bounded: `docker compose down` every project named after one of `trials`.
async fn force_cleanup(trials: Vec<String>) {
    use std::time::Duration;
    use tokio::process::Command;
    let prefixes: Vec<String> = trials.iter().map(|t| env::sanitize(t, false)).collect();
    if prefixes.is_empty() {
        return;
    }
    let ls = Command::new("docker").args(["compose", "ls", "--all", "--quiet"]).kill_on_drop(true).output();
    let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(10), ls).await else { return };
    let mut downs = tokio::task::JoinSet::new();
    for p in String::from_utf8_lossy(&out.stdout).lines().filter(|p| prefixes.iter().any(|x| p.starts_with(x.as_str()))) {
        let mut c = Command::new("docker");
        c.args(["compose", "-p", p, "down", "--volumes", "--remove-orphans", "-t", "0"]);
        c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true);
        downs.spawn(async move { c.status().await });
    }
    let _ = tokio::time::timeout(Duration::from_secs(30), downs.join_all()).await;
}

/// Exit 1 if every trial errored (or any did, with `fail_on_error`); else 0.
fn job_exit_code(s: &job::JobStats, fail_on_error: bool) -> ExitCode {
    let all = s.n_completed_trials > 0 && s.n_errored_trials == s.n_completed_trials;
    if all || (fail_on_error && s.n_errored_trials > 0) {
        eprintln!("{} of {} trial(s) errored", s.n_errored_trials, s.n_completed_trials);
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Run a job with signal cancellation and print its summary; `Err(Cancelled)` once cancelled.
async fn run_with_ctrl_c(cfg: JobConfig, trials: Option<Vec<TrialConfig>>, filter: &[String]) -> Result<job::JobResult> {
    env::preflight().await?;
    let job_dir = cfg.job_dir();
    let dir = job_dir.clone();
    let rx = cancel_on_signal(move || trial_names(std::slice::from_ref(&dir)));
    let jr = job::run_job(cfg, trials, filter, rx.clone()).await?;
    job::print_summary(&jr, &job_dir);
    if *rx.borrow() {
        return Err(Cancelled.into());
    }
    Ok(jr)
}

/// `hbench init`: task or dataset, with cross-type flag checks.
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
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(e) if e.is::<Cancelled>() => {
            eprintln!("cancelled");
            ExitCode::from(130)
        }
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode> {
    redact::register_host_env();
    let ok = |r: Result<()>| r.map(|_| ExitCode::SUCCESS);
    let cli = Cli::parse();
    if !matches!(cli.cmd, Cmd::Completions { .. } | Cmd::Update) {
        update::notify();
    }
    match cli.cmd {
        Cmd::Exec(a) => ok(exec::run(*a).await),
        Cmd::Update => update::run(),
        Cmd::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "hbench", &mut std::io::stdout());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Run(mut a) | Cmd::Jobs { cmd: JobsCmd::Start(mut a) } => {
            if a.config.is_none() && a.path.is_none() && a.dataset.is_none() && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                wizard::fill(&mut a).await?;
            }
            let mut cfg = build_job_config(&a)?;
            preflight_agents(&cfg)?;
            if !a.skip_preflight {
                preflight_auth(&cfg)?;
            }
            if cfg.job_dir().join("config.json").exists() {
                bail!("job {} already exists; use `hbench jobs resume -p {}`", cfg.job_name, cfg.job_dir().display());
            }
            oci::pin_datasets(&mut cfg.datasets, a.lock.mode()).await?;
            if a.diff.is_empty() {
                return Ok(job_exit_code(&run_with_ctrl_c(cfg, None, &[]).await?.stats, a.fail_on_error));
            }
            if cfg.install_only {
                bail!("--diff cannot use --install-only.");
            }
            let sources = diff::load_sources(&diff::source_dirs(&a.diff)?)?;
            let tasks = job::resolve_tasks(&cfg).await?;
            let targets = job::build_trial_configs(&cfg, &tasks, uuid::Uuid::new_v4());
            let diffs = diff::compare(&targets, &sources)?;
            diff::print_preview(&diffs, &sources);
            let trials = diff::trial_configs(targets, &diffs, &sources);
            if trials.is_empty() {
                println!("Nothing to run: every target trial can be reused.");
                return Ok(ExitCode::SUCCESS);
            }
            if !a.yes {
                eprint!("Proceed with {} trial(s)? [y/N] ", trials.len());
                let mut ans = String::new();
                std::io::stdin().read_line(&mut ans)?;
                if !matches!(ans.trim(), "y" | "Y" | "yes") {
                    bail!("aborted");
                }
            }
            cfg.source_jobs =
                a.diff.iter().map(|p| config::SourceJob { action: "diff".into(), kind: None, job_id: None, path: p.clone() }).collect();
            Ok(job_exit_code(&run_with_ctrl_c(cfg, Some(trials), &[]).await?.stats, a.fail_on_error))
        }
        Cmd::Jobs { cmd: JobsCmd::Resume { path, filter_error_types, skip_preflight, fail_on_error } } => {
            let cfg = job::load_config(&path)?;
            preflight_agents(&cfg)?;
            if !skip_preflight {
                preflight_auth(&cfg)?;
            }
            Ok(job_exit_code(&run_with_ctrl_c(cfg, None, &filter_error_types).await?.stats, fail_on_error))
        }
        Cmd::Jobs { cmd: JobsCmd::Summary { path, job_path, json } } => {
            let path = path.or(job_path).context("pass a job dir")?;
            let p = path.join("result.json");
            let s = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
            if json {
                let v: Value = serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display()))?;
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                job::print_summary(&serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display()))?, &path);
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Jobs { cmd: JobsCmd::Verify { path, job_path } } => {
            let path = path.or(job_path).context("pass a job dir")?;
            let problems = manifest::verify(&path)?;
            if problems.is_empty() {
                println!("{}: every file matches its manifest", path.display());
                return Ok(ExitCode::SUCCESS);
            }
            for p in &problems {
                println!("{p}");
            }
            eprintln!("{}: {} file(s) differ from the manifest", path.display(), problems.len());
            Ok(ExitCode::FAILURE)
        }
        Cmd::Jobs { cmd: JobsCmd::Regrade { source, path, jobs_dir, job_name, n_concurrent } } => {
            let path = path.canonicalize().with_context(|| format!("task path {}", path.display()))?;
            let task_dirs: Vec<PathBuf> = if Task::is_task_dir(&path) {
                vec![path.clone()]
            } else {
                let mut v: Vec<PathBuf> = std::fs::read_dir(&path)
                    .with_context(|| format!("reading {}", path.display()))?
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| Task::is_task_dir(p))
                    .collect();
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
            let source = source.canonicalize().with_context(|| format!("source job {}", source.display()))?;
            let mut cfg = JobConfig { jobs_dir, n_concurrent_trials: n_concurrent, ..Default::default() };
            if let Some(n) = job_name {
                cfg.job_name = n;
            }
            let mut trials = vec![];
            for e in std::fs::read_dir(&source).with_context(|| format!("reading {}", source.display()))?.flatten() {
                let Some(r) = std::fs::read_to_string(e.path().join("result.json"))
                    .ok()
                    .and_then(|s| serde_json::from_str::<trial::TrialResult>(&s).ok())
                else {
                    continue;
                };
                let Some(dir) = by_name.get(&r.task_name) else { continue };
                let tref = TaskRef { path: Some(dir.clone()), source: r.source.clone(), ..Default::default() };
                trials.push(TrialConfig {
                    agent: r.config.agent.clone(),
                    source_trial: Some(SourceTrial::regrade_local(r.id.to_string(), e.path())),
                    ..TrialConfig::new(tref.clone(), job::trial_name(&tref), cfg.job_dir())
                });
            }
            if trials.is_empty() {
                bail!("no trials in {} match the provided tasks by name", source.display());
            }
            cfg.tasks = by_name.values().map(|p| TaskRef::local(p.clone(), None)).collect();
            Ok(job_exit_code(&run_with_ctrl_c(cfg, Some(trials), &[]).await?.stats, false))
        }
        Cmd::Trials { cmd: TrialsCmd::Start { config, path, agent, model, agent_kwargs, agent_env, trials_dir, skip_preflight } } => {
            let mut tc: TrialConfig = match config {
                Some(c) => TrialConfig::load(&c)?,
                None => {
                    let p = path.context("pass -p <task dir> or -c <trial.yaml>")?;
                    let p = p.canonicalize().with_context(|| format!("path {}", p.display()))?;
                    let tref = TaskRef::local(p, None);
                    let agent = AgentConfig {
                        name: Some(agent),
                        model_name: model,
                        kwargs: agent_kwargs.iter().map(|(k, v)| (k.clone(), kwarg_value(v))).collect(),
                        env: agent_env.into_iter().collect(),
                        ..Default::default()
                    };
                    TrialConfig { agent, ..TrialConfig::new(tref.clone(), job::trial_name(&tref), trials_dir) }
                }
            };
            if tc.trial_name.is_empty() {
                tc.trial_name = job::trial_name(&tc.task);
            }
            tc.task.validate()?;
            let cfg = JobConfig { agents: vec![tc.agent.clone()], ..Default::default() };
            preflight_agents(&cfg)?;
            if !skip_preflight {
                preflight_auth(&cfg)?;
            }
            registry::download(std::slice::from_ref(&tc.task)).await?;
            env::preflight().await?;
            let task = std::sync::Arc::new(Task::load(&registry::local_path(&tc.task))?);
            let name = tc.trial_name.clone();
            let rx = cancel_on_signal(move || vec![name.clone()]);
            let r = Box::pin(trial::Trial::new(tc, task)?.run(rx.clone())).await;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "trial_name": r.trial_name, "rewards": r.reward(), "exception": r.exception_info.as_ref().map(|e| &e.exception_type), "trial_uri": r.trial_uri
                }))?
            );
            if *rx.borrow() {
                return Err(Cancelled.into());
            }
            Ok(if r.exception_info.is_some() { ExitCode::FAILURE } else { ExitCode::SUCCESS })
        }
        Cmd::Datasets { cmd: DatasetsCmd::List(r) } => {
            if r.registry_url.is_none() && r.registry_path.is_none() {
                let spec = r.registry.unwrap_or_else(registry::default_registry);
                for d in oci::list(&spec).await? {
                    println!("{}@{}  ({} tasks)  {}", d.config.name, d.config.version, d.layers.len(), d.config.description);
                }
                return Ok(ExitCode::SUCCESS);
            }
            for d in registry::load(r.registry_url.as_deref(), r.registry_path.as_deref())? {
                println!("{}@{}  ({} tasks)  {}", d.name, d.version, d.tasks.len(), d.description);
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Datasets { cmd: DatasetsCmd::Pack { tasks_dir, descriptor, oci_layout } } => {
            let packed = oci::publish::pack(&tasks_dir, &oci::descriptor::load(&descriptor)?, &oci_layout)?;
            eprintln!(
                "packed {}@{}: {} task(s) into {}",
                packed.name,
                packed.version,
                packed.tasks,
                oci_layout.join(&packed.name).display()
            );
            println!("{}", packed.digest);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Datasets { cmd: DatasetsCmd::Push { tasks_dir, descriptor, to, allow_overwrite } } => {
            let docker_config = oci::credentials::docker_config_path();
            let staging = env::cache_root().join("staging");
            let (packed, pushed) =
                oci::publish::pack_and_push(&tasks_dir, &descriptor, &to, allow_overwrite, docker_config.as_deref(), &staging).await?;
            eprintln!("{}@{}: {} task(s); {}", packed.name, packed.version, packed.tasks, push_summary(&pushed));
            println!("{}", pushed.copied.digest);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Datasets {
            cmd: DatasetsCmd::Convert { registry, dataset, oci_layout, to, descriptor, check_equivalence, allow_overwrite },
        } => {
            let output = match (oci_layout, to) {
                (Some(dir), _) => oci::publish::Output::Layout(dir),
                (None, Some(to)) => oci::publish::Output::Registry(oci::publish::Target::parse(&to)?),
                (None, None) => bail!("name an output: --oci-layout <dir> or --to oci://<registry>/<repository>:<version>"),
            };
            let args = oci::publish::Convert {
                registry,
                dataset,
                descriptor,
                output,
                check_equivalence,
                allow_overwrite,
                docker_config: oci::credentials::docker_config_path(),
                download_dir: None,
                staging: env::cache_root().join("staging"),
            };
            let converted = oci::publish::convert(&args).await?;
            let checked = if check_equivalence { ", equivalent to the checkouts" } else { "" };
            match &converted.pushed {
                Some(p) => eprintln!("{} task(s){checked}; {}", converted.tasks, push_summary(p)),
                None => eprintln!("{} task(s){checked}", converted.tasks),
            }
            println!("{}@{} {}", converted.name, converted.version, converted.digest);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Datasets { cmd: DatasetsCmd::Export { dataset, registry, oci_layout } } => {
            let location = oci::Location::parse(&registry)?;
            let auth = oci::credentials::registry_auth(oci::credentials::docker_config_path().as_deref(), &location.registry)?;
            let copied = oci::publish::export(&dataset, &registry, &oci_layout, auth).await?;
            eprintln!("exported {dataset} ({} blob(s)) to {}", copied.blobs, copied.reference);
            println!("{}", copied.digest);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Datasets { cmd: DatasetsCmd::Download { dataset, registry: r, lock, output_dir } } => {
            let (name, version) = match dataset.split_once('@') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (dataset.clone(), None),
            };
            let mut d = DatasetConfig {
                name: Some(name),
                version,
                registry_url: r.registry_url,
                registry_path: r.registry_path,
                registry: r.registry,
                download_dir: output_dir,
                ..Default::default()
            };
            oci::pin_datasets(std::slice::from_mut(&mut d), lock.mode()).await?;
            let tasks = registry::dataset_tasks(&d, false).await?;
            registry::download(&tasks).await?;
            for t in &tasks {
                println!("{}", registry::local_path(t).display());
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Tasks {
            cmd:
                TasksCmd::Init {
                    name,
                    tasks_dir,
                    org,
                    description,
                    no_pytest,
                    no_solution,
                    include_canary_strings,
                    metadata_template,
                    include_standard_metadata,
                    no_package,
                    author,
                    steps,
                },
        } => {
            if let Some(t) = &metadata_template
                && !t.exists()
            {
                bail!("Template file not found: {}", t.display());
            }
            let name = authoring::resolve_name(name, org)?;
            let opts = authoring::TaskOpts {
                no_pytest,
                no_solution,
                include_canary_strings,
                include_standard_metadata,
                no_package,
                description,
                authors: authoring::parse_authors(&author),
                metadata_template,
                steps,
            };
            ok(authoring::init_task(&name, &tasks_dir, &opts).map(|_| ()))
        }
        Cmd::Init(a) => ok(run_init(*a)),
        Cmd::Add(a) | Cmd::Datasets { cmd: DatasetsCmd::Add(a) } => ok(authoring::add(&a.packages, &a.to, a.scan)),
        Cmd::Remove(a) | Cmd::Datasets { cmd: DatasetsCmd::Remove(a) } => ok(authoring::remove(&a.package, &a.from, a.scan)),
        Cmd::Sync { path } => ok(authoring::sync(&path)),
        Cmd::Datasets { cmd: DatasetsCmd::Init { name, output_dir, org, description, with_metric, author } } => {
            let name = authoring::resolve_name(name, org)?;
            ok(authoring::init_dataset(&name, &output_dir, &description, with_metric, &authoring::parse_authors(&author)))
        }
        Cmd::Tasks { cmd: TasksCmd::Digest { path } } => {
            let rows: Vec<Value> = task_dirs(&path)?
                .iter()
                .filter(|d| Task::is_task_dir(d))
                .map(|d| -> Result<Value> {
                    let t = Task::load(d)?;
                    Ok(serde_json::json!({"name": t.name(), "checksum": t.tree_hash()?, "digest": crate::task::package_digest(d)}))
                })
                .collect::<Result<_>>()?;
            println!("{}", serde_json::to_string_pretty(&rows)?);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Tasks { cmd: TasksCmd::Check { path } } => {
            let dirs = task_dirs(&path)?;
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
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Sweeps { cmd: SweepsCmd::Run { config, max_sweeps, trials_per_task, hint, hints_file, skip_preflight } } => {
            let cfg = JobConfig::load(&config)?;
            preflight_agents(&cfg)?;
            if !skip_preflight {
                preflight_auth(&cfg)?;
            }
            env::preflight().await?;
            let hints: BTreeMap<String, String> = match hints_file {
                Some(p) => serde_json::from_str(&std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?)
                    .context("--hints-file must be a JSON object {task_name: hint}")?,
                None => BTreeMap::new(),
            };
            // Sweep job dirs are `<jobs_dir>/<job_name>.sweep-*`.
            let (jobs_dir, prefix) = (cfg.jobs_dir.clone(), format!("{}.sweep-", cfg.job_name));
            let rx = cancel_on_signal(move || {
                let dirs: Vec<PathBuf> = std::fs::read_dir(&jobs_dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                    .map(|e| e.path())
                    .collect();
                trial_names(&dirs)
            });
            let jobs = sweeps::run(cfg, max_sweeps, trials_per_task, hint, hints, rx.clone()).await?;
            for (dir, _) in &jobs {
                println!("{}", dir.display());
            }
            if *rx.borrow() {
                return Err(Cancelled.into());
            }
            Ok(job_exit_code(&sweeps::trial_counts(jobs.iter().map(|(_, r)| r)), false))
        }
        Cmd::Tasks { cmd: TasksCmd::Migrate { input, output, cpus, memory_mb, storage_mb, gpus } } => {
            println!(
                "⚠️  WARNING: This migration tool is not foolproof. Please review migrated tasks carefully and make manual adjustments as needed.\n"
            );
            if !input.exists() {
                bail!("Input path does not exist: {}", input.display());
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
                Err(e) if single => return Err(e.context("Failed to migrate task")),
                Err(e) => return Err(e),
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Cache { cmd: CacheCmd::Clean { force, dry, no_docker, no_cache_dir } } => {
            ok(tools::cache_clean(force, dry, no_docker, no_cache_dir).await)
        }
        Cmd::Tasks { cmd: TasksCmd::StartEnv { path, no_all, non_interactive, agent, model, agent_kwargs } } => {
            env::preflight().await?;
            let a = agent.map(|name| AgentConfig {
                name: Some(name),
                model_name: model,
                kwargs: agent_kwargs.iter().map(|(k, v)| (k.clone(), kwarg_value(v))).collect(),
                ..Default::default()
            });
            ok(tools::start_env(&path, !no_all, !non_interactive, a).await)
        }
        Cmd::Trials { cmd: TrialsCmd::Handoff { trial_dir } } => {
            let dir = trial_dir.canonicalize().with_context(|| format!("trial dir {}", trial_dir.display()))?;
            let cmd = tools::handoff_command(&dir, &std::env::current_dir()?)?;
            println!("Resuming session from trial {} with: {}", trial_dir.display(), cmd.join(" "));
            use std::os::unix::process::CommandExt;
            Err(std::process::Command::new(&cmd[0]).args(&cmd[1..]).exec().into())
        }
        Cmd::Agents { cmd: None | Some(AgentsCmd::List { json: false }) } => {
            for n in agents::agent_names() {
                println!("{n}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Agents { cmd: Some(AgentsCmd::List { json: true }) } => {
            println!("{}", serde_json::to_string_pretty(&agents::agent_names())?);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Agents { cmd: Some(AgentsCmd::Schema { name, json }) } => {
            let Some(s) = schemas::agent(&name) else { bail!("Agent '{name}' has no options schema") };
            if json {
                println!("{s}");
            } else {
                print!("{}", schemas::options_table(&name, &serde_json::from_str(s)?));
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Tasks { cmd: TasksCmd::Schema } => {
            println!("{}", schemas::TASK);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Trials { cmd: TrialsCmd::Schema } => {
            println!("{}", schemas::TRIAL);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Analyze(a) => ok(analyze::analyze_command(*a).await),
        Cmd::Check(a) => ok(analyze::check_command(*a).await),
        Cmd::View { folder, path, port, host, tasks, jobs, .. } => {
            //  `-p` is the folder flag but is also used as a port flag; a port-shaped value that isn't a dir is a port.
            let is_port = |p: &str| !std::path::Path::new(p).is_dir() && viewer::parse_port_range(p).is_ok();
            let (folder, port) = match path {
                Some(p) if is_port(&p) => (folder, p),
                Some(p) => (folder.or(Some(PathBuf::from(p))), port),
                None => (folder, port),
            };
            let mode = if tasks {
                Some(viewer::Mode::Tasks)
            } else if jobs {
                Some(viewer::Mode::Jobs)
            } else {
                None
            };
            ok(viewer::serve(&folder.unwrap_or_else(|| "jobs".into()), mode, &host, viewer::parse_port_range(&port)?))
        }
        Cmd::Traces {
            cmd: TracesCmd::Export { path, output, no_recursive, episodes, filter, sharegpt, instruction_metadata, verifier_metadata },
        } => {
            let o = traces::ExportOpts {
                recursive: !no_recursive,
                episodes,
                filter,
                sharegpt,
                include_instruction: instruction_metadata,
                include_verifier_output: verifier_metadata,
            };
            ok(traces::run(&path, output.as_deref(), &o))
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
    fn datasets_publishing_flags() {
        let mut cli = Cli::command();
        let datasets = cli.find_subcommand_mut("datasets").unwrap();
        let help = datasets.find_subcommand_mut("push").unwrap().render_help().to_string();
        assert!(["--descriptor", "--to", "--allow-overwrite"].iter().all(|f| help.contains(f)), "{help}");
        let parses = |args: &[&str]| Cli::try_parse_from(["hbench", "datasets"].iter().chain(args)).is_ok();
        assert!(parses(&["push", "tasks", "--descriptor", "source.yaml", "--to", "oci://ghcr.io/ns/hello:1.0"]));
        assert!(parses(&["pack", "tasks", "--descriptor", "source.yaml", "--oci-layout", "out"]));
        assert!(parses(&["convert", "registry.json", "-d", "hello@1.0", "--oci-layout", "out", "--check-equivalence"]));
        assert!(!parses(&["convert", "registry.json", "-d", "hello@1.0"]), "convert needs --oci-layout or --to");
        assert!(!parses(&["convert", "registry.json", "-d", "hello@1.0", "--oci-layout", "out", "--to", "oci://localhost:5000/hello"]));
        assert!(parses(&["export", "-d", "hello@1.0", "--registry", "oci://ghcr.io/ns", "--oci-layout", "offline"]));
    }

    #[test]
    fn cli_builds_job_config() {
        let dir = std::env::temp_dir().join(format!("hbench-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cli = Cli::parse_from([
            "hbench",
            "run",
            "-p",
            dir.to_str().unwrap(),
            "-a",
            "claude-code",
            "-m",
            "anthropic/a",
            "-m",
            "anthropic/b",
            "--ak",
            "max_turns=5",
            "--ae",
            "X=1",
            "-k",
            "3",
            "-i",
            "t*",
            "--no-delete",
            "--artifact",
            "/app/x",
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

    #[test]
    fn agent_flags_apply_to_every_configured_agent() {
        let dir = std::env::temp_dir().join(format!("hbench-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let job = dir.join("job.yaml");
        std::fs::write(&job, "agents:\n  - name: claude-code\n    model_name: anthropic/a\n  - name: codex\n    model_name: openai/b\n")
            .unwrap();
        let cfg = |extra: &[&str]| {
            let mut args = vec!["hbench", "run", "-c", job.to_str().unwrap(), "-p", dir.to_str().unwrap()];
            args.extend(extra);
            let Cmd::Run(a) = Cli::parse_from(args).cmd else { panic!() };
            build_job_config(&a).unwrap()
        };
        let c = cfg(&["--ak", "max_turns=5", "--ae", "X=1", "--skill", "/skills/s", "--n-concurrent-agent", "2"]);
        let agents: Vec<(&str, Option<&str>)> = c.agents.iter().map(|a| (a.name(), a.model_name.as_deref())).collect();
        assert_eq!(agents, [("claude-code", Some("anthropic/a")), ("codex", Some("openai/b"))]);
        for a in &c.agents {
            assert_eq!((&a.kwargs["max_turns"], a.env["X"].as_str(), a.n_concurrent), (&Value::from(5), "1", Some(2)));
            assert_eq!(a.skills, ["/skills/s"]);
        }
        let c = cfg(&["-m", "anthropic/c", "--ak", "max_turns=5"]);
        let agents: Vec<(&str, Option<&str>)> = c.agents.iter().map(|a| (a.name(), a.model_name.as_deref())).collect();
        assert_eq!(agents, [("claude-code", Some("anthropic/c"))]);
        assert_eq!(c.agents[0].kwargs["max_turns"], 5);
    }

    #[test]
    fn task_filters_apply_to_single_tasks() {
        let dir = std::env::temp_dir().join(format!("hbench-cli-{}", uuid::Uuid::new_v4())).join("hello");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("task.toml"), "").unwrap();
        let cfg = |extra: &[&str]| {
            let mut args = vec!["hbench", "run", "-p", dir.to_str().unwrap()];
            args.extend(extra);
            let Cmd::Run(a) = Cli::parse_from(args).cmd else { panic!() };
            build_job_config(&a)
        };
        assert_eq!(cfg(&["-i", "hel*"]).unwrap().tasks.len(), 1);
        assert!(cfg(&["-i", "nope"]).unwrap_err().to_string().contains("No tasks matched"));
        assert!(cfg(&["-x", "hello"]).is_err());
    }

    #[test]
    fn preflight_checks_kwargs_and_keys() {
        let agent = |name: &str, kw: Value, env: &[(&str, &str)]| JobConfig {
            agents: vec![AgentConfig {
                name: Some(name.into()),
                kwargs: serde_json::from_value(kw).unwrap(),
                env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(preflight_agents(&agent("claude-code", serde_json::json!({"bogus": 1}), &[])).is_err());
        assert!(preflight_auth(&agent("claude-code", serde_json::json!({}), &[("CLAUDE_CODE_OAUTH_TOKEN", "t")])).is_ok());
        assert!(preflight_auth(&agent("aider", serde_json::json!({}), &[])).is_ok()); // undecidable: skipped
        if auth_envs("codex").unwrap().iter().all(|n| std::env::var(n).is_err()) {
            let e = preflight_auth(&agent("codex", serde_json::json!({}), &[])).unwrap_err().to_string();
            assert!(e.contains("OPENAI_API_KEY"), "{e}");
        }
    }

    #[test]
    fn preflight_accepts_acp_registry_agents() {
        let agent = |name: &str, kw: Value| JobConfig {
            agents: vec![AgentConfig { name: Some(name.into()), kwargs: serde_json::from_value(kw).unwrap(), ..Default::default() }],
            ..Default::default()
        };
        assert!(preflight_agents(&agent("acp:goose", serde_json::json!({}))).is_ok());
        assert!(preflight_agents(&agent("acp:opencode@1.2.0", serde_json::json!({"permission_mode": "deny"}))).is_ok());
        // kwargs are still checked against the ACP options schema.
        assert!(preflight_agents(&agent("acp:goose", serde_json::json!({"bogus": 1}))).is_err());
        // The registry spec must name an agent (and a version, when `@` is given).
        assert!(preflight_agents(&agent("acp:", serde_json::json!({}))).is_err());
        assert!(preflight_agents(&agent("acp:goose@", serde_json::json!({}))).is_err());
        assert!(preflight_agents(&agent("no-such-agent", serde_json::json!({}))).is_err());
    }

    #[test]
    fn exit_codes() {
        let jr = |done: usize, errored: usize| job::JobResult {
            id: uuid::Uuid::nil(),
            started_at: chrono::Utc::now(),
            updated_at: None,
            finished_at: None,
            n_total_trials: done,
            stats: job::JobStats { n_completed_trials: done, n_errored_trials: errored, ..Default::default() },
        };
        assert_eq!(job_exit_code(&jr(2, 1).stats, false), ExitCode::SUCCESS);
        assert_eq!(job_exit_code(&jr(2, 1).stats, true), ExitCode::FAILURE);
        assert_eq!(job_exit_code(&jr(2, 2).stats, false), ExitCode::FAILURE);
        assert_eq!(job_exit_code(&jr(0, 0).stats, true), ExitCode::SUCCESS);
        let sweeps = [jr(2, 2), jr(1, 0)];
        assert_eq!(job_exit_code(&sweeps::trial_counts(sweeps.iter()), false), ExitCode::SUCCESS);
        let sweeps = [jr(2, 2), jr(1, 1)];
        assert_eq!(job_exit_code(&sweeps::trial_counts(sweeps.iter()), false), ExitCode::FAILURE);
    }

    #[test]
    fn summary_takes_a_positional_path() {
        for args in [&["hbench", "jobs", "summary", "jobs/x"][..], &["hbench", "jobs", "summary", "-p", "jobs/x"]] {
            let Cmd::Jobs { cmd: JobsCmd::Summary { path, job_path, .. } } = Cli::parse_from(args).cmd else { panic!() };
            assert_eq!(path.or(job_path), Some(PathBuf::from("jobs/x")));
        }
    }

    #[tokio::test]
    async fn sighup_cancels() {
        let mut rx = cancel_on_signal(Vec::new); // handlers are registered before it returns
        let pid = std::process::id().to_string();
        assert!(std::process::Command::new("kill").args(["-HUP", &pid]).status().unwrap().success());
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.changed()).await.unwrap().unwrap();
        assert!(*rx.borrow());
    }
}
