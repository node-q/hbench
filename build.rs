//! Embeds the viewer SPA (viewer/dist, built from viewer/ with `npm run build`) as `VIEWER_FILES: &[(url_path, bytes)]`,
//! and the commit the binary is built from as `HBENCH_COMMIT`: `HBENCH_COMMIT` or `GITHUB_SHA` if set, else
//! `git rev-parse HEAD`, else none.
//!
//! viewer/dist is not committed. When it is missing or older than the viewer's sources, this builds it with npm
//! (`npm ci` if node_modules is missing or older than the lockfile, then `npm run build`), so `cargo build` and
//! `cargo install --path .` work from a fresh clone with Node on the PATH. CI and the release build the viewer in
//! a step of their own first and set `HBENCH_VIEWER_PREBUILT=1`, which makes a missing or stale bundle an error
//! here instead of a build.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// What `npm run build` reads, besides node_modules.
const VIEWER_INPUTS: &[&str] = &["src", "public", "index.html", "package.json", "package-lock.json", "tsconfig.json", "vite.config.ts"];

fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e} (run `npm ci && npm run build` in viewer/)", dir.display()))
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).current_dir(env!("CARGO_MANIFEST_DIR")).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

fn embed_commit() {
    println!("cargo:rerun-if-env-changed=HBENCH_COMMIT");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    let given = ["HBENCH_COMMIT", "GITHUB_SHA"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
    if given.is_none() {
        // A new commit moves the branch's ref; switching branches moves HEAD.
        for path in [
            git(&["rev-parse", "--git-path", "HEAD"]),
            git(&["symbolic-ref", "-q", "HEAD"]).and_then(|r| git(&["rev-parse", "--git-path", &r])),
        ]
        .into_iter()
        .flatten()
        {
            println!("cargo:rerun-if-changed={}", Path::new(env!("CARGO_MANIFEST_DIR")).join(path).display());
        }
    }
    if let Some(commit) = given.or_else(|| git(&["rev-parse", "HEAD"])) {
        println!("cargo:rustc-env=HBENCH_COMMIT={commit}");
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The newest modification time of a file, or of a directory and everything in it (a directory's own time moves
/// when an entry is added or removed).
fn newest(path: &Path) -> Option<SystemTime> {
    let own = mtime(path);
    let Ok(entries) = std::fs::read_dir(path) else { return own };
    entries.flatten().map(|e| newest(&e.path())).fold(own, |a, b| a.max(b))
}

fn npm(viewer: &Path, args: &[&str]) {
    // npm is a .cmd script on Windows, which Command runs only by its full name.
    let program = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let status = Command::new(program).args(args).current_dir(viewer).status().unwrap_or_else(|e| {
        panic!(
            "viewer/dist is missing or out of date, and building it needs Node 22+ and npm on the PATH ({program}: {e}).\n\
             Install Node (the version CI uses is in .node-version) or build it yourself: cd viewer && npm ci && npm run build"
        )
    });
    assert!(status.success(), "`npm {}` in viewer/ failed ({status}); see its output above", args.join(" "));
}

/// Makes sure viewer/dist is there and no older than the viewer's sources, building it if not (see the top).
fn build_viewer(viewer: &Path) -> PathBuf {
    let dist = viewer.join("dist");
    println!("cargo:rerun-if-env-changed=HBENCH_VIEWER_PREBUILT");
    println!("cargo:rerun-if-changed={}", dist.display());
    for input in VIEWER_INPUTS {
        println!("cargo:rerun-if-changed={}", viewer.join(input).display());
    }
    let built = mtime(&dist.join("index.html"));
    let sources = VIEWER_INPUTS.iter().filter_map(|i| newest(&viewer.join(i))).max();
    let problem = match built {
        None => "missing",
        Some(built) if sources.is_some_and(|s| s > built) => "older than viewer/'s sources",
        Some(_) => return dist,
    };
    if std::env::var_os("HBENCH_VIEWER_PREBUILT").is_some_and(|v| !v.is_empty()) {
        panic!(
            "viewer/dist is {problem}, and HBENCH_VIEWER_PREBUILT is set, so it is not built here.\n\
             Build it first: cd viewer && npm ci && npm run build"
        );
    }
    println!("cargo:warning=viewer/dist is {problem}: building it with npm (cd viewer && npm ci && npm run build)");
    let installed = mtime(&viewer.join("node_modules/.package-lock.json"));
    if installed.is_none() || mtime(&viewer.join("package-lock.json")) > installed {
        npm(viewer, &["ci"]);
    }
    npm(viewer, &["run", "build"]);
    assert!(dist.join("index.html").is_file(), "`npm run build` in viewer/ left no dist/index.html");
    dist
}

fn main() {
    embed_commit();
    let root = build_viewer(&Path::new(env!("CARGO_MANIFEST_DIR")).join("viewer"));
    let mut files = vec![];
    walk(&root, &mut files);
    let mut src = String::from("pub static VIEWER_FILES: &[(&str, &[u8])] = &[\n");
    for f in &files {
        let rel = f.strip_prefix(&root).unwrap().to_str().unwrap().replace('\\', "/");
        src += &format!("    ({:?}, include_bytes!({:?})),\n", format!("/{rel}"), f.to_str().unwrap());
    }
    src += "];\n";
    std::fs::write(Path::new(&std::env::var("OUT_DIR").unwrap()).join("viewer_files.rs"), src).unwrap();
}
