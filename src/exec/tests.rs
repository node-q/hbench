use super::*;
use clap::Parser;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    a: ExecArgs,
}

fn args(v: &[&str]) -> ExecArgs {
    Cli::try_parse_from(std::iter::once("exec").chain(v.iter().copied())).expect("cli parse").a
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hbench-exec-{tag}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Writes `{"files": {rel: content}}`; `*.sh` get 0755 like the oracle.
fn write_fixture(spec: &Value, root: &Path) {
    for (rel, content) in spec["files"].as_object().unwrap() {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        write_file(&p, content.as_str().unwrap(), if rel.ends_with(".sh") { 0o755 } else { 0o644 }).unwrap();
    }
}

#[test]
fn artifact_inference_matches_hbench() {
    let p = "Write result.json, reports/summary.txt, /tmp/output.PNG, ../out/data.csv, docs/README.md, and .env.local.";
    assert_eq!(
        infer_artifacts_from_prompt(p, "/app"),
        ["/app/result.json", "/app/reports/summary.txt", "/tmp/output.PNG", "/out/data.csv", "/app/docs/README.md", "/app/.env.local"]
    );
    assert_eq!(
        infer_artifacts_from_prompt("Write result.json and reports/summary.txt.", "workspace"),
        ["/workspace/result.json", "/workspace/reports/summary.txt"]
    );
    assert_eq!(infer_artifacts_from_prompt("a.md then ./a.md and x/../a.md", "/w"), ["/w/a.md"]);
    assert!(infer_artifacts_from_prompt("Run python3.12 and explain the result.", "/app").is_empty());
    assert_eq!(prompt_file_to_artifact_path("out.json)\"", "/"), "/out.json");
}

#[test]
fn python_path_helpers() {
    assert_eq!(normpath("/work/../up.csv"), "/up.csv");
    assert_eq!(normpath("/../a//./b/"), "/a/b");
    assert_eq!(normpath("../a/../../b"), "../../b");
    assert_eq!(normpath("//x/y"), "//x/y");
    assert_eq!(py_path("./td/"), "td");
    assert_eq!(py_path(""), ".");
    assert_eq!(py_path("///a//b"), "/a/b");
    assert_eq!(posix_split("a/b/"), ("a/b".into(), "".into()));
    assert_eq!(posix_split("/x"), ("/".into(), "x".into()));
    assert_eq!(slugify(" My Job! "), "my-job");
    assert_eq!(slugify("--"), "task");
}

fn print_output(a: &ExecArgs) -> String {
    match resolve_config(a) {
        Ok(c) => serde_json::to_string_pretty(&print_value(&c)).unwrap(),
        Err(e) => format!("ERROR Error: {}", format!("{e:#}").split_whitespace().collect::<Vec<_>>().join(" ")),
    }
}

/// `hbench exec ... --print-config` goldens (tests/fixtures/exec, produced by upstream 0.24.0).
#[test]
fn print_config_matches_hbench_golden() {
    let spec: Value = serde_json::from_str(include_str!("../../tests/fixtures/exec/print_config_cases.json")).unwrap();
    let golden = include_str!("../../tests/fixtures/exec/print_config_golden.txt");
    let root = tmp("print");
    write_fixture(&spec, &root);
    let r = root.to_string_lossy().into_owned();
    let want: Vec<(&str, &str)> = golden.split("### ").filter(|b| !b.is_empty()).map(|b| b.split_once('\n').unwrap()).collect();
    let cases = spec["cases"].as_array().unwrap();
    assert_eq!(cases.len(), want.len());
    for (case, (head, body)) in cases.iter().zip(want) {
        let argv: Vec<String> = case.as_array().unwrap().iter().map(|a| a.as_str().unwrap().replace("{ROOT}", &r)).collect();
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let got = print_output(&args(&argv)).replace(&r, "{ROOT}");
        assert_eq!(got, body.trim_end(), "case {head}");
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn default_job_names_and_temp_task_dir() {
    let c = resolve_config(&args(&["-i", "make a.md", "--ri", "combine"])).unwrap();
    let (m, r) = (c.map.job.job_name.clone().unwrap(), c.reduce.as_ref().unwrap().job.job_name.clone().unwrap());
    assert!(m.ends_with("-map") && r.ends_with("-reduce"));
    assert_eq!(m.trim_end_matches("-map"), r.trim_end_matches("-reduce"));
    let out = c.map.compile.output_dir.clone().unwrap().0;
    assert!(out.starts_with(&format!("{}/hbench-exec-map-tasks-", temp_root().display())), "{out}");
    assert_eq!(c.reduce.unwrap().task.output_dir.0, out);
}

/// Dump a task dir the way the oracle script does.
fn dump_tree(dir: &Path, out: &mut Vec<String>) {
    use std::os::unix::fs::PermissionsExt;
    let assets = [
        (TPL_AUTO_VERIFY, "auto-verify-artifacts.sh"),
        (TPL_AUTO_VERIFY_SCHEMAS, "auto-verify-artifacts-with-schemas.sh"),
        (TPL_SCHEMA_VALIDATOR, "validate_artifact_schemas.py"),
        (TPL_PROMOTE_REWARD, "promote_reward_artifact.py"),
    ];
    fn walk(base: &Path, d: &Path, acc: &mut Vec<(String, Option<PathBuf>)>) {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                acc.push((rel, None));
                walk(base, &p, acc);
            } else {
                acc.push((rel, Some(p)));
            }
        }
    }
    let mut entries = vec![];
    walk(dir, dir, &mut entries);
    entries.sort();
    for (rel, file) in entries {
        match file {
            None => out.push(format!("DIR {rel}")),
            Some(p) => {
                let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                out.push(format!("== {rel} 0o{mode:o}"));
                let text = std::fs::read_to_string(&p).unwrap();
                match assets.iter().find(|(c, _)| *c == text) {
                    Some((_, n)) => out.push(format!("<asset {n}>")),
                    None => out.push(text.trim_end_matches('\n').to_string()),
                }
            }
        }
    }
}

/// Compiled task trees match upstream's Compiler byte for byte (tests/fixtures/exec goldens).
#[test]
fn compile_matches_hbench_golden() {
    let spec: Value = serde_json::from_str(include_str!("../../tests/fixtures/exec/compile_cases.json")).unwrap();
    let golden = include_str!("../../tests/fixtures/exec/compile_golden.txt");
    let root = tmp("compile");
    write_fixture(&spec, &root);
    let r = root.to_string_lossy().into_owned();
    let mut out = vec![];
    for (name, case) in spec["cases"].as_object().unwrap() {
        out.push(format!("## {name}"));
        let mut cfg: Value = serde_json::from_str(&case.to_string().replace("{ROOT}", &r)).unwrap();
        cfg["output_dir"] = root.join("out").join(name).to_string_lossy().into_owned().into();
        let cfg: CompileConfig = serde_json::from_value(cfg).unwrap();
        cfg.validate().unwrap();
        match compile(&cfg) {
            Err(e) => out.push(format!("ERROR {}", e.to_string().replace(&r, "{ROOT}").lines().next().unwrap_or_default())),
            Ok(tasks) => {
                for t in tasks {
                    out.push(format!("TASK {}", t.file_name().unwrap().to_string_lossy()));
                    dump_tree(&t, &mut out);
                }
            }
        }
    }
    let got = out.join("\n");
    for (i, (g, w)) in got.lines().zip(golden.lines()).enumerate() {
        assert_eq!(g, w, "first differing line {}", i + 1);
    }
    assert_eq!(got.trim_end(), golden.trim_end());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn compile_rejects_bad_inputs() {
    let root = tmp("bad");
    let out = Some(PyPath(root.join("o").to_string_lossy().into_owned()));
    assert!(compile(&CompileConfig::default()).unwrap_err().to_string().contains("output_dir"));
    let missing = CompileConfig {
        output_dir: out.clone(),
        instructions: vec![CompileInstruction { text: None, path: Some(PyPath::new("/nonexistent/i.md")) }],
        ..Default::default()
    };
    assert!(compile(&missing).unwrap_err().to_string().starts_with("Instruction path must be a file"));
    let no_instruction = CompileConfig { output_dir: out, ..Default::default() };
    assert!(compile(&no_instruction).unwrap_err().to_string().starts_with("Compiled task is not a valid task directory"));
    let both: CompileInstruction = serde_json::from_value(json!({"text": "a", "path": "b"})).unwrap();
    assert!(both.validate().is_err());
    std::fs::remove_dir_all(&root).ok();
}

fn write_trial(job: &Path, name: &str, artifacts: &[(&str, &str)]) {
    let dir = job.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    for (rel, content) in artifacts {
        let p = dir.join("artifacts").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    let r = json!({
        "id": uuid::Uuid::nil(), "task_name": "t", "trial_name": name, "trial_uri": format!("file://{}", dir.display()),
        "task_id": {"path": "t"}, "task_checksum": "", "agent_info": {"name": "oracle", "version": "1", "model_info": null},
        "config": {"task": {"path": "t"}, "trial_name": name, "trials_dir": job},
    });
    std::fs::write(dir.join("result.json"), r.to_string()).unwrap();
}

/// Mirrors upstream's test_executor_stages_map_artifacts_into_reduce_task_environment.
#[test]
fn reduce_task_stages_map_artifacts() {
    let root = tmp("reduce");
    let job = root.join("jobs/map");
    write_trial(&job, "Task B__1", &[("out.md", "b")]);
    write_trial(&job, "task-a__1", &[("x/y.txt", "a")]);
    write_trial(&job, "task-c__1", &[]);
    let task = ExecReduceTaskConfig {
        task_name: "reduce".into(),
        output_dir: PyPath(root.join("tasks").to_string_lossy().into_owned()),
        task_template: None,
        instruction: CompileInstruction::text("Combine into /app/final.md"),
        artifacts: vec!["/app/final.md".into()],
        environment: ExecReduceEnvironment::default(),
        verifier: None,
    };
    let dir = compile_reduce_task(&task, &job).unwrap();
    assert_eq!(dir.file_name().unwrap(), "reduce-0001");
    let staged = dir.join("environment/artifacts");
    assert_eq!(std::fs::read_to_string(staged.join("0001-task-b__1/out.md")).unwrap(), "b");
    assert_eq!(std::fs::read_to_string(staged.join("0002-task-a__1/x/y.txt")).unwrap(), "a");
    assert_eq!(std::fs::read_dir(&staged).unwrap().count(), 2);
    assert_eq!(std::fs::read_to_string(dir.join("instruction.md")).unwrap(), "Combine into /app/final.md\n");

    let empty = root.join("jobs/empty");
    write_trial(&empty, "t__1", &[]);
    assert_eq!(compile_reduce_task(&task, &empty).unwrap_err().to_string(), "Cannot reduce a map job with no trial artifacts.");
    std::fs::create_dir_all(root.join("jobs/none")).unwrap();
    assert_eq!(
        compile_reduce_task(&task, &root.join("jobs/none")).unwrap_err().to_string(),
        "Cannot reduce a map job with no trial results."
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn job_config_feeds_existing_runner() {
    let c = resolve_config(&args(&[
        "-i",
        "make a.md",
        "-a",
        "oracle",
        "-n",
        "2",
        "-k",
        "3",
        "-r",
        "1",
        "--job-name",
        "J",
        "--jobs-dir",
        "jd",
        "--tasks-dir",
        "td",
        "--ae",
        "K=V",
    ]))
    .unwrap();
    let cfg = job_config(&c.map.job, &[PathBuf::from("/t/a")]).unwrap();
    assert_eq!(
        (cfg.job_name.as_str(), cfg.jobs_dir.as_path(), cfg.n_attempts, cfg.n_concurrent_trials, cfg.retry.max_retries),
        ("J", Path::new("jd"), 3, 2, 1)
    );
    assert_eq!(cfg.agents[0].env["K"], "V");
    assert_eq!(cfg.tasks[0].path.as_deref(), Some(Path::new("/t/a")));
    assert!(!cfg.verifier.disable);
    let daytona = resolve_config(&args(&["-i", "x", "-e", "daytona", "--tasks-dir", "td"])).unwrap();
    assert!(job_config(&daytona.map.job, &[PathBuf::from("/t/a")]).is_err());
}
