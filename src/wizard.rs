//! `hbench run` with no tasks given: pick the tasks, agent and model on the terminal (arrow keys), then print the
//! equivalent command so the next run can skip the questions.

use crate::{RunArgs, agents, auth_envs, registry};
use anyhow::{Result, bail};
use dialoguer::console::{Term, style};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Select};
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

enum Source {
    Path(String),
    /// Registry URL or registry.json path, and `name@version`.
    Registry {
        registry: String,
        is_path: bool,
        dataset: String,
    },
}

struct Answers {
    source: Source,
    agent: String,
    model: Option<String>,
    n_tasks: Option<usize>,
}

/// Ask for the task source, agent, model and task count, and fill them into `a`.
pub fn fill(a: &mut RunArgs) -> Result<()> {
    // Ctrl-C ends the process mid-prompt: show the cursor the prompts hid, exit 130.
    let on_ctrl_c = tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = Term::stderr().show_cursor();
            eprintln!();
            std::process::exit(130);
        }
    });
    let r = ask(a);
    on_ctrl_c.abort();
    let _ = Term::stderr().show_cursor();
    // The signal can also land as an interrupted read.
    r.map_err(|e| match e.downcast_ref::<dialoguer::Error>() {
        Some(dialoguer::Error::IO(io)) if io.kind() == std::io::ErrorKind::Interrupted => crate::Cancelled.into(),
        _ => e,
    })
}

fn ask(a: &mut RunArgs) -> Result<()> {
    let t = ColorfulTheme::default();
    eprintln!();
    for l in LOGO.lines() {
        eprintln!("  {}", style(l).cyan().bold());
    }
    eprintln!("\n  {}\n", style("set up a run · ↑↓ to move, Enter to pick").dim());

    let source = match Select::with_theme(&t)
        .with_prompt("What to run")
        .items(["Tasks in a local folder", "A dataset from a registry"])
        .default(0)
        .interact()?
    {
        0 => {
            let def = if Path::new("examples/tasks/hello-world").is_dir() { "examples/tasks/hello-world" } else { "." };
            Source::Path(Input::with_theme(&t).with_prompt("Folder (a task, or a folder of tasks)").default(def.into()).interact_text()?)
        }
        _ => {
            let r: String = Input::with_theme(&t).with_prompt("Registry (URL or path of a registry.json)").interact_text()?;
            let is_path = Path::new(&r).is_file();
            let datasets = if is_path { registry::load(None, Some(Path::new(&r)))? } else { registry::load(Some(&r), None)? };
            if datasets.is_empty() {
                bail!("registry {r} has no datasets");
            }
            let labels: Vec<String> =
                datasets.iter().map(|d| format!("{}@{}  {}", d.name, d.version, style(format!("{} tasks", d.tasks.len())).dim())).collect();
            let d = &datasets[Select::with_theme(&t).with_prompt("Dataset").items(&labels).default(0).max_length(12).interact()?];
            Source::Registry { registry: r, is_path, dataset: format!("{}@{}", d.name, d.version) }
        }
    };

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
    let model = if matches!(agent.as_str(), "oracle" | "nop") {
        None
    } else {
        let mut i = Input::<String>::with_theme(&t).with_prompt("Model (provider/name)");
        if let Some(m) = suggested {
            i = i.default(m.into());
        }
        Some(i.interact_text()?)
    };

    let n: String = Input::with_theme(&t)
        .with_prompt("How many tasks (a number, or all)")
        .default("all".into())
        .validate_with(|s: &String| if s == "all" || s.parse::<usize>().is_ok_and(|n| n > 0) { Ok(()) } else { Err("a number, or all") })
        .interact_text()?;

    let cmd = apply(a, Answers { source, agent, model, n_tasks: n.parse().ok() });
    eprintln!("\n  {}\n  {}\n", style("Same run without the questions:").dim(), style(cmd.join(" ")).cyan());
    if !Confirm::with_theme(&t).with_prompt("Start").default(true).interact()? {
        bail!("aborted");
    }
    eprintln!();
    Ok(())
}

/// Fill the answers into `a`; returns the equivalent command line.
fn apply(a: &mut RunArgs, ans: Answers) -> Vec<String> {
    let mut cmd: Vec<String> = vec!["hbench".into(), "run".into()];
    match ans.source {
        Source::Path(p) => {
            cmd.extend(["-p".into(), shell_quote(&p)]);
            a.path = Some(p.into());
        }
        Source::Registry { registry, is_path, dataset } => {
            let flag = if is_path { "--registry-path" } else { "--registry-url" };
            cmd.extend(["-d".into(), dataset.clone(), flag.into(), shell_quote(&registry)]);
            a.dataset = Some(dataset);
            if is_path {
                a.registry.registry_path = Some(PathBuf::from(registry));
            } else {
                a.registry.registry_url = Some(registry);
            }
        }
    }
    cmd.extend(["-a".into(), ans.agent.clone()]);
    a.agent = Some(ans.agent);
    if let Some(m) = ans.model {
        cmd.extend(["-m".into(), shell_quote(&m)]);
        a.models = vec![m];
    }
    if let Some(n) = ans.n_tasks {
        cmd.extend(["-l".into(), n.to_string()]);
        a.n_tasks = Some(n);
    }
    cmd
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
    fn answers_fill_the_run_args() {
        let crate::Cmd::Run(mut a) = crate::Cli::parse_from(["hbench", "run"]).cmd else { panic!() };
        let ans = Answers {
            source: Source::Registry { registry: "https://x/registry.json".into(), is_path: false, dataset: "tb@2.0".into() },
            agent: "codex".into(),
            model: Some("openai/gpt-5".into()),
            n_tasks: Some(3),
        };
        let cmd = apply(&mut a, ans).join(" ");
        assert_eq!(cmd, "hbench run -d tb@2.0 --registry-url https://x/registry.json -a codex -m openai/gpt-5 -l 3");
        assert_eq!((a.dataset.as_deref(), a.registry.registry_url.as_deref()), (Some("tb@2.0"), Some("https://x/registry.json")));
        assert_eq!((a.agent.as_deref(), a.models.as_slice(), a.n_tasks), (Some("codex"), &["openai/gpt-5".to_string()][..], Some(3)));

        let crate::Cmd::Run(mut a) = crate::Cli::parse_from(["hbench", "run"]).cmd else { panic!() };
        let ans = Answers { source: Source::Path("my tasks".into()), agent: "oracle".into(), model: None, n_tasks: None };
        assert_eq!(apply(&mut a, ans).join(" "), "hbench run -p 'my tasks' -a oracle");
        assert_eq!(a.path, Some(PathBuf::from("my tasks")));
    }
}
