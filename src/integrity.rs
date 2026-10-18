//! Beyond upstream: verifier-only data that some registry datasets build into the agent's image.
//!
//! - LiveCodeBench copies `tests/config.json` to `/app/config.json` for its public-test checker, and that file
//!   also holds the hidden tests (`private_test_cases`, base64 + zlib + pickle).
//! - Aider Polyglot (`/app/.oracle/solution.enc`) and TB2's adaptive-rejection-sampler
//!   (`/protected/protected.tar.gz.enc`) ship the reference solution encrypted, with the password in the
//!   task's public `solution/solve.sh`.
//!
//! The verifier gets its own copy of `tests/` when it runs, so taking these out of the running container
//! before the agent starts changes nothing for grading. Only the image's Dockerfile `COPY` lines are read;
//! anything a task's compose file or a prebuilt image adds is not seen.

use crate::env::quote;
use std::path::{Component, Path, PathBuf};

/// What to remove from the agent's container, as found in the task's Dockerfile.
#[derive(Debug, PartialEq)]
pub enum Leak {
    /// A copy of a verifier JSON file carrying hidden tests; strip the key, keep the public cases.
    HiddenTests(String),
    /// An encrypted reference solution the oracle decrypts.
    EncryptedSolution(String),
}

pub fn leaks(task_dir: &Path) -> Vec<Leak> {
    let env = task_dir.join("environment");
    let Ok(dockerfile) = std::fs::read_to_string(env.join("Dockerfile")) else { return vec![] };
    let solve = std::fs::read_to_string(task_dir.join("solution/solve.sh")).unwrap_or_default();
    let mut out = vec![];
    let mut workdir = PathBuf::from("/");
    for line in dockerfile.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some((op, args)) = words.split_first() else { continue };
        match op.to_ascii_uppercase().as_str() {
            "WORKDIR" if !args.is_empty() => workdir = workdir.join(args[0]),
            "COPY" | "ADD" => {
                // `--from` copies from another image, not from this task's files.
                if args.iter().any(|a| a.starts_with("--from")) {
                    continue;
                }
                let paths: Vec<&str> = args.iter().copied().filter(|a| !a.starts_with("--")).collect();
                let Some((dest, sources)) = paths.split_last() else { continue };
                let into_dir = dest.ends_with('/') || sources.len() > 1;
                let dest = workdir.join(dest);
                for src in sources {
                    let host = env.join(src);
                    if !inside(&env, &host) {
                        continue;
                    }
                    let target = |rel: &Path| -> String {
                        let p = if host.is_dir() || into_dir { dest.join(rel) } else { dest.clone() };
                        normalize(&p)
                    };
                    for (file, rel) in files(&host) {
                        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let rel = if host.is_dir() { rel } else { PathBuf::from(&name) };
                        if is_hidden_tests(&env, &file) {
                            out.push(Leak::HiddenTests(target(&rel)));
                        } else if name.ends_with(".enc") && solve.contains(&name) {
                            out.push(Leak::EncryptedSolution(target(&rel)));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// One root shell command that removes every leak, or None when there are none.
pub fn scrub_command(leaks: &[Leak]) -> Option<String> {
    if leaks.is_empty() {
        return None;
    }
    let cmds: Vec<String> = leaks
        .iter()
        .map(|l| match l {
            Leak::HiddenTests(p) => format!(
                "if command -v python3 >/dev/null; then python3 -c {} {p}; else rm -f {p}; fi",
                quote(STRIP_HIDDEN_TESTS),
                p = quote(p)
            ),
            Leak::EncryptedSolution(p) => format!("rm -f {}", quote(p)),
        })
        .collect();
    Some(format!("set -e; {}", cmds.join("; ")))
}

const STRIP_HIDDEN_TESTS: &str = "import json, sys
p = sys.argv[1]
c = json.load(open(p))
c.pop('private_test_cases', None)
json.dump(c, open(p, 'w'))";

/// A JSON file from the task's `tests/` (copied into the build context) that holds hidden tests.
fn is_hidden_tests(env: &Path, file: &Path) -> bool {
    let in_tests = file.strip_prefix(env).is_ok_and(|r| r.starts_with("tests"));
    in_tests
        && file.extension().is_some_and(|e| e == "json")
        && std::fs::read_to_string(file)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .is_some_and(|v| v.get("private_test_cases").is_some())
}

fn files(root: &Path) -> Vec<(PathBuf, PathBuf)> {
    if root.is_file() {
        return vec![(root.to_path_buf(), PathBuf::new())];
    }
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.push((p.clone(), rel.to_path_buf()));
            }
        }
    }
    out
}

fn inside(root: &Path, p: &Path) -> bool {
    p.strip_prefix(root).is_ok_and(|r| !r.components().any(|c| c == Component::ParentDir))
}

fn normalize(p: &Path) -> String {
    let mut out = PathBuf::from("/");
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    out.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(dockerfile: &str, files: &[(&str, &str)]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hbench-integrity-{}", uuid::Uuid::new_v4()));
        for (p, body) in files.iter().chain(&[("environment/Dockerfile", dockerfile)]) {
            let p = d.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        d
    }

    #[test]
    fn livecodebench_hidden_tests() {
        let config = r#"{"public_test_cases": "[]", "private_test_cases": "eJy..."}"#;
        let d = task(
            "FROM python:3.11-slim\nWORKDIR /app\nCOPY check_solution.py /app/check_solution.py\nCOPY tests/config.json /app/config.json\n",
            &[("environment/check_solution.py", ""), ("environment/tests/config.json", config), ("tests/config.json", config)],
        );
        assert_eq!(leaks(&d), vec![Leak::HiddenTests("/app/config.json".into())]);
        let cmd = scrub_command(&leaks(&d)).unwrap();
        assert!(cmd.contains("private_test_cases") && cmd.contains("python3 -c") && cmd.contains(" /app/config.json;"), "{cmd}");
    }

    #[test]
    fn encrypted_solutions_the_oracle_decrypts() {
        let polyglot = task(
            "FROM buildpack-deps:jammy\nWORKDIR /app\nCOPY workspace/ ./\n",
            &[
                ("environment/workspace/sgf_parsing.py", ""),
                ("environment/workspace/.oracle/solution.enc", "x"),
                ("solution/solve.sh", "ENC_FILE=\"$ORACLE_DIR/solution.enc\""),
            ],
        );
        assert_eq!(leaks(&polyglot), vec![Leak::EncryptedSolution("/app/.oracle/solution.enc".into())]);
        let tb2 = task(
            "FROM ubuntu:24.04\nWORKDIR /app\nCOPY protected.tar.gz.enc /protected/protected.tar.gz.enc\n",
            &[("environment/protected.tar.gz.enc", "x"), ("solution/solve.sh", "openssl ... -in /protected/protected.tar.gz.enc")],
        );
        assert_eq!(leaks(&tb2), vec![Leak::EncryptedSolution("/protected/protected.tar.gz.enc".into())]);
        assert_eq!(scrub_command(&leaks(&tb2)).unwrap(), "set -e; rm -f /protected/protected.tar.gz.enc");
    }

    #[test]
    fn task_inputs_stay() {
        // TB2 tasks that copy test files on purpose (the agent is told to run them), an .enc the
        // oracle never mentions, and copies from another image.
        let d = task(
            "FROM ubuntu:24.04\nWORKDIR /app\nCOPY tests/test_outputs.py /app\nCOPY data.enc /app/\nCOPY --from=ghcr.io/astral-sh/uv:0.8.14 /uv /bin/\n",
            &[("environment/tests/test_outputs.py", ""), ("environment/data.enc", "x"), ("solution/solve.sh", "echo")],
        );
        assert_eq!(leaks(&d), vec![]);
        assert_eq!(scrub_command(&[]), None);
    }
}
