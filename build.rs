//! Embeds the viewer SPA (viewer/dist, built from viewer/ with `npm run build`) as `VIEWER_FILES: &[(url_path, bytes)]`.

use std::path::Path;

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

fn main() {
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
