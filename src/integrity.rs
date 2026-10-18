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
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

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
    for (op, args) in instructions(&dockerfile) {
        match op.as_str() {
            "WORKDIR" => {
                if let Some(dir) = args.split_whitespace().next() {
                    workdir = workdir.join(dir);
                }
            }
            "COPY" | "ADD" => {
                let (flags, paths) = copy_args(&args);
                // `--from` copies from another image, not from this task's files.
                if flags.iter().any(|f| f.starts_with("--from")) {
                    continue;
                }
                let Some((dest, sources)) = paths.split_last() else { continue };
                let into_dir = dest.ends_with('/') || sources.len() > 1;
                let dest = workdir.join(dest);
                for src in sources {
                    let host = env.join(src);
                    if !inside(&env, &host) {
                        continue;
                    }
                    for f in copied(&env, &host, op == "ADD") {
                        let target = normalize(&if f.unpacked || into_dir { dest.join(&f.rel) } else { dest.clone() });
                        let name = f.rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        if f.hidden_tests {
                            out.push(Leak::HiddenTests(target));
                        } else if has_extension(&name, "enc") && solve.contains(&name) {
                            out.push(Leak::EncryptedSolution(target));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The Dockerfile's instructions as (upper-case op, arguments), with `\` continuations joined and
/// comment lines dropped.
fn instructions(dockerfile: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut cur = String::new();
    for line in dockerfile.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
        if let Some(head) = line.strip_suffix('\\') {
            cur.push_str(head);
            cur.push(' ');
            continue;
        }
        cur.push_str(line);
        let inst = std::mem::take(&mut cur);
        let (op, args) = inst.trim().split_once(char::is_whitespace).unwrap_or((inst.trim(), ""));
        if !op.is_empty() {
            out.push((op.to_ascii_uppercase(), args.trim().to_string()));
        }
    }
    out
}

/// A COPY/ADD's leading `--flags` and its paths, in shell or JSON (`["src", "dest"]`) form.
fn copy_args(args: &str) -> (Vec<&str>, Vec<String>) {
    let mut flags = vec![];
    let mut rest = args.trim_start();
    while rest.starts_with("--") {
        let (flag, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        flags.push(flag);
        rest = tail.trim_start();
    }
    let paths = if rest.starts_with('[') {
        serde_json::from_str(rest).unwrap_or_default()
    } else {
        rest.split_whitespace().map(String::from).collect()
    };
    (flags, paths)
}

/// A file the image receives from one COPY/ADD source.
struct Copied {
    /// Path below the destination when the source is a directory or an unpacked archive; else the file name.
    rel: PathBuf,
    unpacked: bool,
    hidden_tests: bool,
}

fn copied(env: &Path, host: &Path, add: bool) -> Vec<Copied> {
    if add
        && host.is_file()
        && let Some(members) = tar_members(host)
    {
        return members
            .into_iter()
            .map(|rel| {
                let hidden_tests = rel.starts_with("tests")
                    && is_json(&rel)
                    && Command::new("tar").arg("-xOf").arg(host).arg(&rel).output().is_ok_and(|o| holds_hidden_tests(&o.stdout));
                Copied { rel, unpacked: true, hidden_tests }
            })
            .collect();
    }
    let unpacked = host.is_dir();
    files(host)
        .into_iter()
        .map(|(file, rel)| Copied {
            rel: if unpacked { rel } else { PathBuf::from(file.file_name().unwrap_or_default()) },
            unpacked,
            hidden_tests: is_hidden_tests(env, &file),
        })
        .collect()
}

/// The files in a tar archive ADD would unpack (plain or compressed; Docker goes by content, not name).
fn tar_members(p: &Path) -> Option<Vec<PathBuf>> {
    let mut head = vec![];
    std::fs::File::open(p).and_then(|f| f.take(262).read_to_end(&mut head)).ok()?;
    let compressed = [&[0x1f, 0x8b][..], b"BZh", &[0xfd, b'7', b'z', b'X', b'Z', 0], &[0x28, 0xb5, 0x2f, 0xfd]];
    if !compressed.iter().any(|m| head.starts_with(m)) && head.get(257..262) != Some(b"ustar") {
        return None;
    }
    let out = Command::new("tar").arg("-tf").arg(p).output().ok().filter(|o| o.status.success())?;
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.ends_with('/'))
            .map(|l| PathBuf::from(l.trim_start_matches("./")))
            .collect(),
    )
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
    file.strip_prefix(env).is_ok_and(|r| r.starts_with("tests"))
        && is_json(file)
        && std::fs::read(file).is_ok_and(|b| holds_hidden_tests(&b))
}

fn holds_hidden_tests(json: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(json).is_ok_and(|v| v.get("private_test_cases").is_some())
}

fn is_json(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"))
}

fn has_extension(name: &str, ext: &str) -> bool {
    Path::new(name).extension().is_some_and(|e| e.eq_ignore_ascii_case(ext))
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

    struct TaskDir(PathBuf);

    impl Drop for TaskDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl std::ops::Deref for TaskDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    fn task(dockerfile: &str, files: &[(&str, &str)]) -> TaskDir {
        let d = std::env::temp_dir().join(format!("hbench-integrity-{}", uuid::Uuid::new_v4()));
        for (p, body) in files.iter().chain(&[("environment/Dockerfile", dockerfile)]) {
            let p = d.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        TaskDir(d)
    }

    const CONFIG: &str = r#"{"public_test_cases": "[]", "private_test_cases": "eJy..."}"#;

    #[test]
    fn livecodebench_hidden_tests() {
        let d = task(
            "FROM python:3.11-slim\nWORKDIR /app\nCOPY check_solution.py /app/check_solution.py\nCOPY tests/config.json /app/config.json\n",
            &[("environment/check_solution.py", ""), ("environment/tests/config.json", CONFIG), ("tests/config.json", CONFIG)],
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

    #[test]
    fn continuation_and_json_form_copies() {
        let d = task(
            "FROM python:3.11-slim\n# copy the checker's config\nCOPY --chown=1000:1000 \\\n    tests/config.json \\\n    /app/config.json\nCOPY [\"tests/config.json\", \"/srv/\"]\n",
            &[("environment/tests/config.json", CONFIG)],
        );
        assert_eq!(leaks(&d), vec![Leak::HiddenTests("/app/config.json".into()), Leak::HiddenTests("/srv/config.json".into())]);
    }

    #[test]
    fn upper_case_encrypted_solution() {
        let d = task(
            "FROM ubuntu:24.04\nCOPY SOLUTION.ENC /opt/\n",
            &[("environment/SOLUTION.ENC", "x"), ("solution/solve.sh", "openssl -in /opt/SOLUTION.ENC")],
        );
        assert_eq!(leaks(&d), vec![Leak::EncryptedSolution("/opt/SOLUTION.ENC".into())]);
    }

    #[test]
    fn add_unpacks_archives() {
        let d = task(
            "FROM ubuntu:24.04\nWORKDIR /app\nADD bundle.tgz ./\nCOPY bundle.tgz /keep/\n",
            &[("bundle/tests/config.json", CONFIG), ("bundle/.oracle/solution.enc", "x"), ("solution/solve.sh", "solution.enc")],
        );
        let tar = Command::new("tar")
            .arg("-czf")
            .arg(d.join("environment/bundle.tgz"))
            .args(["-C", d.join("bundle").to_str().unwrap(), "tests", ".oracle"])
            .status()
            .unwrap();
        assert!(tar.success());
        let mut got = leaks(&d);
        got.sort_by_key(|l| format!("{l:?}"));
        assert_eq!(
            got,
            vec![Leak::EncryptedSolution("/app/.oracle/solution.enc".into()), Leak::HiddenTests("/app/tests/config.json".into())]
        );
    }
}
