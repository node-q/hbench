//! Embeds the viewer SPA (viewer/dist, built from viewer/ with `npm run build`) as `VIEWER_FILES: &[(url_path, bytes)]`,
//! and the commit the binary is built from as `HBENCH_COMMIT`: `HBENCH_COMMIT` or `GITHUB_SHA` if set, else
//! `git rev-parse HEAD`, else none.

use std::path::Path;
use std::process::Command;

fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e} (run `npm run build` in viewer/)", dir.display()))
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

fn main() {
    embed_commit();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("viewer/dist");
    println!("cargo:rerun-if-changed={}", root.display());
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
