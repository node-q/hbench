//! `hbench run` with no tasks given: pick the tasks, agent and model on the terminal (arrow keys), then print the
//! equivalent command so the next run can skip the questions.

use crate::config::{DatasetConfig, TaskRef};
use crate::task::Task;
use crate::{RunArgs, agents, auth_envs, oci, registry};
use anyhow::{Context, Result, bail};
use dialoguer::console::{Term, style};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, MultiSelect, Select};
use std::path::{Path, PathBuf};

/// Same logo as the README.
const LOGO: &str = "\
██╗  ██╗██████╗ ███████╗███╗   ██╗ ██████╗██╗  ██╗
██║  ██║██╔══██╗██╔════╝████╗  ██║██╔════╝██║  ██║
███████║██████╔╝█████╗  ██╔██╗ ██║██║     ███████║
██╔══██║██╔══██╗██╔══╝  ██║╚██╗██║██║     ██╔══██║
██║  ██║██████╔╝███████╗██║ ╚████║╚██████╗██║  ██║
╚═╝  ╚═╝╚═════╝ ╚══════╝╚═╝  ╚═══╝ ╚═════╝╚═╝  ╚═╝";

/// Agents offered first, with the model suggested for each (None = the agent takes no model or has no default).
const TOP_AGENTS: &[(&str, &str, Option<&str>)] = &[
    ("claude-code", "Claude Code · Anthropic", Some("anthropic/claude-opus-5-5")),
    ("codex", "Codex · OpenAI", Some("openai/gpt-5")),
    ("gemini-cli", "Gemini CLI · Google", Some("google/gemini-3-pro")),
    ("opencode", "OpenCode · any provider", None),
    ("terminus-2", "Terminus 2 · any provider", None),
    ("deepseek-harness", "DeepSeek harness", Some("deepseek/deepseek-v4-flash")),
    ("oracle", "Oracle · runs each task's reference solution", None),
];

/// Providers offered for the model, with their models; an empty list asks for the model name.
// ponytail: a hand-picked list (the bundled price table is stale and has thousands of entries); "other…" and
// "custom…" reach every other provider and model.
const PROVIDERS: &[(&str, &[&str])] = &[
    ("anthropic", &["claude-opus-5-5", "claude-fable-5-1", "claude-sonnet-5-5", "claude-haiku-5-5"]),
    ("openai", &["gpt-5.6", "gpt-5.6-sol", "gpt-5.5", "gpt-5"]),
    ("google", &["gemini-3-pro", "gemini-3.5-flash"]),
    ("deepseek", &["deepseek-v4-flash", "deepseek-v4-pro"]),
    ("moonshot", &["kimi-k2.6"]),
    ("xai", &["grok-4"]),
    ("mistral", &["devstral-latest", "mistral-large-latest"]),
    ("openrouter", &[]),
    ("ollama", &[]),
];

/// node-q's dataset registry, offered first.
const NODEQ_REGISTRY: &str = "oci://ghcr.io/node-q/hbench-datasets";

enum Source {
    Path(String),
    /// `flag` (`--registry`, `--registry-url` or `--registry-path`) with `registry`, and `name@version`.
    Dataset {
        flag: &'static str,
        registry: String,
        dataset: String,
    },
}

enum Tasks {
    All,
    First(usize),
    Pick(Vec<String>),
}

struct Answers {
    source: Source,
    tasks: Tasks,
    agent: String,
    model: Option<String>,
}

/// Ask for the task source, tasks, agent and model, and fill them into `a`.
pub async fn fill(a: &mut RunArgs) -> Result<()> {
    // Ctrl-C ends the process mid-prompt: show the cursor the prompts hid, exit 130.
    let on_ctrl_c = tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = Term::stderr().show_cursor();
            eprintln!();
            std::process::exit(130);
        }
    });
    let r = ask(a).await;
    on_ctrl_c.abort();
    let _ = Term::stderr().show_cursor();
    // The signal can also land as an interrupted read.
    r.map_err(|e| match e.downcast_ref::<dialoguer::Error>() {
        Some(dialoguer::Error::IO(io)) if io.kind() == std::io::ErrorKind::Interrupted => crate::Cancelled.into(),
        _ => e,
    })
}

/// `f`'s result, with `what` shown on a line of its own while it runs.
async fn busy<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    eprintln!("  {}", style(format!("{what}…")).dim());
    let r = f.await;
    let _ = Term::stderr().clear_last_lines(1);
    r
}

/// The task names of OCI dataset `name@version` in `registry`.
async fn oci_tasks(registry: &str, name: &str, version: &str) -> Result<Vec<String>> {
    let d = DatasetConfig { name: Some(name.into()), version: Some(version.into()), registry: Some(registry.into()), ..Default::default() };
    Ok(oci::dataset_tasks(&d).await?.iter().map(TaskRef::display_name).collect())
}

/// Pick `name@version` among the versions of dataset `name` in OCI registry `registry`, with its task names.
async fn pick_oci_version(t: &ColorfulTheme, registry: &str, name: &str) -> Result<(String, Vec<String>)> {
    let versions = busy("looking up its versions", oci::versions(registry, name)).await?;
    let v = match versions.as_slice() {
        [] => bail!("{registry} has no versions of {name}"),
        [v] => v.clone(),
        _ => versions[Select::with_theme(t).with_prompt("Version").items(&versions).default(versions.len() - 1).interact()?].clone(),
    };
    let tasks = busy("reading the dataset", oci_tasks(registry, name, &v)).await?;
    Ok((format!("{name}@{v}"), tasks))
}

/// Pick a dataset in OCI registry `registry` (`oci://` or `oci-layout:`): one it lists (a layout dir lists itself,
/// a registry its dataset index), or any other by name.
async fn pick_oci(t: &ColorfulTheme, registry: &str) -> Result<(String, Vec<String>)> {
    let listed = if registry.starts_with(oci::LAYOUT_SCHEME) {
        busy("reading the datasets", oci::list(registry)).await?
    } else {
        match busy("reading the dataset index", oci::indexed(registry)).await {
            Ok(Some(datasets)) => datasets,
            Ok(None) => {
                eprintln!("  {}", style(format!("{registry} lists no datasets (no public dataset index); name one")).dim());
                vec![]
            }
            Err(e) => {
                eprintln!("  {}", style(format!("could not read {registry}'s dataset index: {e:#}")).dim());
                vec![]
            }
        }
    };
    let found: Vec<(String, Vec<String>)> = listed
        .into_iter()
        .map(|d| (format!("{}@{}", d.config.name, d.config.version), d.layers.into_iter().map(|l| l.name).collect()))
        .collect();
    let mut labels: Vec<String> = found.iter().map(|(id, ts)| format!("{id:<28}{}", style(format!("{} tasks", ts.len())).dim())).collect();
    let other = !registry.starts_with(oci::LAYOUT_SCHEME);
    if other {
        labels.push(format!("{:<28}{}", "Another dataset…", style("by name").dim()));
    }
    if found.is_empty() && !other {
        bail!("{registry} has no datasets");
    }
    let i = Select::with_theme(t).with_prompt("Dataset").items(&labels).default(0).max_length(12).interact()?;
    match found.get(i) {
        Some(f) => Ok(f.clone()),
        None => {
            let name: String = Input::with_theme(t).with_prompt("Dataset name").interact_text()?;
            pick_oci_version(t, registry, name.trim()).await
        }
    }
}

async fn ask(a: &mut RunArgs) -> Result<()> {
    let t = ColorfulTheme::default();
    eprintln!();
    for l in LOGO.lines() {
        eprintln!("  {}", style(l).cyan().bold());
    }
    eprintln!("\n  {}\n", style("set up a run · ↑↓ to move, Enter to pick").dim());

    let sources = [
        format!("{:<26}{}", "Default datasets", style(NODEQ_REGISTRY.trim_start_matches(oci::REGISTRY_SCHEME)).dim()),
        format!("{:<26}{}", "Tasks in a local folder", style("a task, or a folder of tasks").dim()),
        format!("{:<26}{}", "Another OCI registry", style("oci://<registry>/<namespace> or oci-layout:<dir>").dim()),
        format!("{:<26}{}", "A registry.json", style("URL or path").dim()),
    ];
    let (source, tasks) = match Select::with_theme(&t).with_prompt("What to run").items(&sources).default(0).interact()? {
        0 => {
            let (dataset, tasks) = pick_oci(&t, NODEQ_REGISTRY).await?;
            (Source::Dataset { flag: "--registry", registry: NODEQ_REGISTRY.into(), dataset }, tasks)
        }
        1 => {
            let def = if Path::new("examples/tasks").is_dir() { "examples/tasks" } else { "." };
            let p: String = Input::with_theme(&t).with_prompt("Folder").default(def.into()).interact_text()?;
            let tasks = if Task::is_task_dir(Path::new(&p)) {
                vec![]
            } else {
                let mut v: Vec<String> = std::fs::read_dir(&p)
                    .with_context(|| format!("reading {p}"))?
                    .flatten()
                    .filter(|e| Task::is_task_dir(&e.path()))
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                v.sort();
                if v.is_empty() {
                    bail!("{p} has no tasks (directories with a task.toml)");
                }
                v
            };
            (Source::Path(p), tasks)
        }
        2 => {
            let r: String = Input::with_theme(&t)
                .with_prompt("Registry")
                .validate_with(|s: &String| oci::Location::parse(s).map(|_| ()).map_err(|e| e.to_string()))
                .interact_text()?;
            let (dataset, tasks) = pick_oci(&t, &r).await?;
            (Source::Dataset { flag: "--registry", registry: r, dataset }, tasks)
        }
        _ => {
            let r: String = Input::with_theme(&t).with_prompt("registry.json (URL or path)").interact_text()?;
            let is_path = Path::new(&r).is_file();
            let datasets = busy("reading the registry", async {
                if is_path { registry::load(None, Some(Path::new(&r))) } else { registry::load(Some(&r), None) }
            })
            .await?;
            if datasets.is_empty() {
                bail!("registry {r} has no datasets");
            }
            let labels: Vec<String> = datasets
                .iter()
                .map(|d| format!("{:<28}{}", format!("{}@{}", d.name, d.version), style(format!("{} tasks", d.tasks.len())).dim()))
                .collect();
            let d = &datasets[Select::with_theme(&t).with_prompt("Dataset").items(&labels).default(0).max_length(12).interact()?];
            let flag = if is_path { "--registry-path" } else { "--registry-url" };
            let tasks = d.tasks.iter().map(|t| t.name.clone()).collect();
            (Source::Dataset { flag, registry: r, dataset: format!("{}@{}", d.name, d.version) }, tasks)
        }
    };
    let tasks = pick_tasks(&t, tasks)?;

    let mut labels: Vec<String> = TOP_AGENTS.iter().map(|(n, d, _)| format!("{n:<17}{}", style(d).dim())).collect();
    labels.push(format!("{:<17}{}", "more…", style("every other agent").dim()));
    let i = Select::with_theme(&t).with_prompt("Agent").items(&labels).default(0).interact()?;
    let (agent, suggested) = match TOP_AGENTS.get(i) {
        Some((n, _, m)) => (n.to_string(), *m),
        None => {
            let all = agents::agent_names();
            let n = all[Select::with_theme(&t).with_prompt("Agent").items(&all).default(0).max_length(12).interact()?];
            (n.to_string(), None)
        }
    };
    if let Some(names) = auth_envs(&agent)
        && !names.iter().any(|n| std::env::var(n).is_ok_and(|v| !v.is_empty()))
    {
        bail!("{agent} needs credentials: `export {}=...` (or one of {}), then run again", names[0], names[1..].join(", "));
    }
    let model = if matches!(agent.as_str(), "oracle" | "nop") { None } else { Some(pick_model(&t, suggested)?) };

    let cmd = apply(a, Answers { source, tasks, agent, model });
    eprintln!("\n  {}\n  {}\n", style("Same run without the questions:").dim(), style(cmd.join(" ")).cyan());
    if !Confirm::with_theme(&t).with_prompt("Start").default(true).interact()? {
        bail!("aborted");
    }
    eprintln!();
    Ok(())
}

/// All of `names`, the first few, or the ones ticked; nothing to ask for a single task (no names).
fn pick_tasks(t: &ColorfulTheme, names: Vec<String>) -> Result<Tasks> {
    if names.len() <= 1 {
        return Ok(Tasks::All);
    }
    let items = [format!("All {} tasks", names.len()), "The first few".into(), "Pick tasks…".into()];
    Ok(match Select::with_theme(t).with_prompt("Tasks").items(&items).default(0).interact()? {
        0 => Tasks::All,
        1 => {
            let max = names.len();
            let n: usize = Input::with_theme(t)
                .with_prompt("How many")
                .default(5.min(max))
                .validate_with(move |n: &usize| if (1..=max).contains(n) { Ok(()) } else { Err(format!("1 to {max}")) })
                .interact_text()?;
            Tasks::First(n)
        }
        _ => loop {
            let picked = MultiSelect::with_theme(t)
                .with_prompt(format!("Tasks {}", style("(space ticks, Enter confirms)").dim()))
                .items(&names)
                .max_length(15)
                .interact()?;
            if !picked.is_empty() {
                break Tasks::Pick(picked.into_iter().map(|i| names[i].clone()).collect());
            }
            eprintln!("  {}", style("tick at least one task").yellow());
        },
    })
}

/// Fill the answers into `a`; returns the equivalent command line.
fn apply(a: &mut RunArgs, ans: Answers) -> Vec<String> {
    let mut cmd: Vec<String> = vec!["hbench".into(), "run".into()];
    match ans.source {
        Source::Path(p) => {
            cmd.extend(["-p".into(), shell_quote(&p)]);
            a.path = Some(p.into());
        }
        Source::Dataset { flag, registry, dataset } => {
            cmd.extend(["-d".into(), dataset.clone(), flag.into(), shell_quote(&registry)]);
            a.dataset = Some(dataset);
            match flag {
                "--registry-path" => a.registry.registry_path = Some(PathBuf::from(registry)),
                "--registry-url" => a.registry.registry_url = Some(registry),
                _ => a.registry.registry = Some(registry),
            }
        }
    }
    match ans.tasks {
        Tasks::All => {}
        Tasks::First(n) => {
            cmd.extend(["-l".into(), n.to_string()]);
            a.n_tasks = Some(n);
        }
        Tasks::Pick(names) => {
            for n in &names {
                cmd.extend(["-i".into(), shell_quote(n)]);
            }
            a.include = names;
        }
    }
    cmd.extend(["-a".into(), ans.agent.clone()]);
    a.agent = Some(ans.agent);
    if let Some(m) = ans.model {
        cmd.extend(["-m".into(), shell_quote(&m)]);
        a.models = vec![m];
    }
    cmd
}

/// A provider from [`PROVIDERS`] (marked when its key is set), then one of its models; "other…" and "custom…" are typed.
fn pick_model(t: &ColorfulTheme, suggested: Option<&str>) -> Result<String> {
    let (sp, sm) = suggested.and_then(|m| m.split_once('/')).unzip();
    let mut labels: Vec<String> = PROVIDERS
        .iter()
        .map(|(p, _)| {
            let envs = agents::model_connection::provider(p).map_or(&[][..], |a| a.api_key_envs);
            match envs.iter().find(|n| std::env::var(n).is_ok_and(|v| !v.is_empty())) {
                Some(n) => format!("{p:<12}{}", style(format!("{n} set")).green()),
                None => p.to_string(),
            }
        })
        .collect();
    labels.push(format!("{:<12}{}", "other…", style("type provider/name").dim()));
    let def = PROVIDERS.iter().position(|(p, _)| Some(*p) == sp).unwrap_or(0);
    let Some((p, models)) = PROVIDERS.get(Select::with_theme(t).with_prompt("Provider").items(&labels).default(def).interact()?) else {
        return Ok(Input::with_theme(t).with_prompt("Model (provider/name)").interact_text()?);
    };
    let mut items: Vec<&str> = models.to_vec();
    items.push("custom…");
    let i = if models.is_empty() {
        models.len()
    } else {
        let def = models.iter().position(|m| Some(*m) == sm).unwrap_or(0);
        Select::with_theme(t).with_prompt("Model").items(&items).default(def).interact()?
    };
    let name = match models.get(i) {
        Some(m) => m.to_string(),
        None => Input::<String>::with_theme(t).with_prompt(format!("Model ({p}/…)")).interact_text()?,
    };
    Ok(format!("{p}/{name}"))
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./@:=".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn suggested_models_are_listed_and_providers_known() {
        for (agent, _, m) in TOP_AGENTS {
            if let Some((p, name)) = m.and_then(|m| m.split_once('/')) {
                assert!(PROVIDERS.iter().any(|(q, ms)| *q == p && ms.contains(&name)), "{agent}: {p}/{name} not in PROVIDERS");
            }
        }
        for (p, _) in PROVIDERS {
            assert!(agents::model_connection::provider(p).is_some(), "{p} has no model_connection entry");
        }
    }

    fn run_args() -> Box<RunArgs> {
        let crate::Cmd::Run(a) = crate::Cli::parse_from(["hbench", "run"]).cmd else { panic!() };
        a
    }

    #[test]
    fn answers_fill_the_run_args() {
        let mut a = run_args();
        let ans = Answers {
            source: Source::Dataset { flag: "--registry", registry: NODEQ_REGISTRY.into(), dataset: "terminal-bench@2.0.1".into() },
            tasks: Tasks::First(3),
            agent: "codex".into(),
            model: Some("openai/gpt-5".into()),
        };
        let cmd = apply(&mut a, ans).join(" ");
        assert_eq!(cmd, "hbench run -d terminal-bench@2.0.1 --registry oci://ghcr.io/node-q/hbench-datasets -l 3 -a codex -m openai/gpt-5");
        assert_eq!((a.dataset.as_deref(), a.registry.registry.as_deref()), (Some("terminal-bench@2.0.1"), Some(NODEQ_REGISTRY)));
        assert_eq!((a.agent.as_deref(), a.models.as_slice(), a.n_tasks), (Some("codex"), &["openai/gpt-5".to_string()][..], Some(3)));

        let mut a = run_args();
        let ans = Answers {
            source: Source::Dataset { flag: "--registry-url", registry: "https://x/registry.json".into(), dataset: "tb@2.0".into() },
            tasks: Tasks::Pick(vec!["fix-git".into(), "a b".into()]),
            agent: "oracle".into(),
            model: None,
        };
        assert_eq!(
            apply(&mut a, ans).join(" "),
            "hbench run -d tb@2.0 --registry-url https://x/registry.json -i fix-git -i 'a b' -a oracle"
        );
        assert_eq!((a.registry.registry_url.as_deref(), a.include.len(), a.n_tasks), (Some("https://x/registry.json"), 2, None));

        let mut a = run_args();
        let ans = Answers { source: Source::Path("my tasks".into()), tasks: Tasks::All, agent: "oracle".into(), model: None };
        assert_eq!(apply(&mut a, ans).join(" "), "hbench run -p 'my tasks' -a oracle");
        assert_eq!(a.path, Some(PathBuf::from("my tasks")));
    }
}
