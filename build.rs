//! Embeds upstream's viewer SPA (assets/hbench-viewer/static) as `VIEWER_FILES: &[(url_path, bytes)]`,
//! with hbench's theme (assets/hbench-viewer/hbench-theme.css) and the small bundle patches below applied.

use std::path::Path;

/// (file name prefix, exact text, replacement). Every patch must match, so a upstream update that moves the
/// text fails the build instead of silently dropping the patch.
const PATCHES: &[(&str, &str, &str)] = &[
    ("index.html", r#"<link rel="stylesheet" href="/assets/root-2wvw6FQ5.css"/>"#,
     r#"<link rel="stylesheet" href="/assets/root-2wvw6FQ5.css"/><link rel="stylesheet" href="/assets/hbench-theme.css"/><title>hbench</title>"#),
    // upstream Hub isn't supported: drop the sign-in buttons instead of offering a login that can't work.
    ("root-", r#"i.jsxs(z,{variant:"default",size:"sm",onClick:()=>r.mutate(),disabled:r.isPending,children:[i.jsx(R,{icon:Ue,strokeWidth:2,className:"h-4 w-4"}),"Sign in"]})"#, "null"),
    ("job-", r#"t.jsxs(Ee,{variant:"secondary",onClick:()=>Ht.mutate(),disabled:Ht.isPending,children:[t.jsx(Re,{icon:Us,strokeWidth:2,className:"h-4 w-4"}),"Sign in to upload"]})"#, "null"),
    ("root-", r#"className:"bg-black text-white hover:bg-black/90 dark:bg-white dark:text-black dark:hover:bg-white/90""#,
     r#"className:"bg-primary text-primary-foreground hover:bg-primary/90""#),
    // Reward heatmaps: shade toward the theme's "good" colour instead of the foreground grey.
    ("job-", "color-mix(in oklch, var(--foreground) ${Ke}%, var(--card))", "color-mix(in oklch, var(--hb-good) ${Ke}%, var(--card))"),
    ("job-", "color-mix(in oklch, var(--foreground) ${Q(x)}%, var(--card))", "color-mix(in oklch, var(--hb-good) ${Q(x)}%, var(--card))"),
    ("job-", "color-mix(in oklch, var(--foreground) 12%, var(--card)), var(--foreground))", "color-mix(in oklch, var(--hb-good) 12%, var(--card)), var(--hb-good))"),
    ("compare-BQ", "color-mix(in oklch, var(--foreground) ${ie(O,z,E)}%, var(--card))", "color-mix(in oklch, var(--hb-good) ${ie(O,z,E)}%, var(--card))"),
    ("compare-BQ", "color-mix(in oklch, var(--foreground) 12%, var(--card)), var(--foreground))", "color-mix(in oklch, var(--hb-good) 12%, var(--card)), var(--hb-good))"),
    ("home-", r#"writeText("hbench run -d <dataset>")"#, r#"writeText("hbench run -d <dataset>")"#),
    ("home-", r#"children:"hbench run -d <dataset>""#, r#"children:"hbench run -d <dataset>""#),
    ("run-", r#"children:"hbench run"}"#, r#"children:"hbench run"}"#),
    ("trial-", r#"const r=["hbench run"]"#, r#"const r=["hbench run"]"#),
    // No upstream branding: own favicon (the .ico isn't embedded), titles and Hub wording.
    ("index.html", r#"<link rel="icon" href="/favicon.ico" sizes="any"/>"#, ""),
    ("run-", r#"{title:"New run · upstream"}"#, r#"{title:"New run · hbench"}"#),
    ("job-", r#""upstream Hub is unreachable; upload may still work""#, r#""The hub is unreachable; upload may still work""#),
    ("job-", r#""Share jobs or store for later on upstream Hub""#, r#""Share jobs or store them for later""#),
    ("job-", r#"children:"Upload to upstream Hub"}"#, r#"children:"Upload job"}"#),
];

/// upstream mentions allowed to remain in the served UI: data, not branding (the run form's default task id).
const ALLOWED_HBENCH: &[&str] = &["hbench/hello-world"];

fn walk(dir: &Path, out: &mut Vec<String>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p.to_str().unwrap().to_string());
        }
    }
}

fn main() {
    let viewer = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/hbench-viewer");
    let root = viewer.join("static");
    let theme = viewer.join("hbench-theme.css");
    println!("cargo:rerun-if-changed={}", root.display());
    println!("cargo:rerun-if-changed={}", theme.display());
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut files = vec![];
    walk(&root, &mut files);
    let mut used = vec![0usize; PATCHES.len()];
    let mut src = String::from("pub static VIEWER_FILES: &[(&str, &[u8])] = &[\n");
    src += &format!("    (\"/assets/hbench-theme.css\", include_bytes!({:?})),\n", theme.to_str().unwrap());
    let favicon = viewer.join("hbench-favicon.svg");
    println!("cargo:rerun-if-changed={}", favicon.display());
    src += &format!("    (\"/favicon.svg\", include_bytes!({:?})),\n", favicon.to_str().unwrap());
    for f in &files {
        let path = Path::new(f);
        let rel = path.strip_prefix(&root).unwrap().to_str().unwrap().replace('\\', "/");
        let name = path.file_name().unwrap().to_str().unwrap();
        if rel == "favicon.ico" || rel == "favicon.svg" {
            continue;
        }
        let mut file = f.clone();
        if let Ok(orig) = std::fs::read_to_string(path) {
            let mut text = orig.clone();
            for (i, (prefix, from, to)) in PATCHES.iter().enumerate() {
                if name.starts_with(prefix) && text.contains(from) {
                    text = text.replace(from, to);
                    used[i] += 1;
                }
            }
            let mut rest = text.to_lowercase();
            for a in ALLOWED_HBENCH {
                rest = rest.replace(a, "");
            }
            assert!(!rest.contains("hbench"), "upstream branding left in viewer file {rel}: add a patch");
            if text != orig {
                let patched = out_dir.join(format!("viewer-{}", rel.replace('/', "_")));
                std::fs::write(&patched, text).unwrap();
                file = patched.to_str().unwrap().to_string();
            }
        }
        src += &format!("    ({:?}, include_bytes!({:?})),\n", format!("/{rel}"), file);
    }
    for (i, n) in used.iter().enumerate() {
        assert!(*n > 0, "viewer patch no longer applies: {:?}", PATCHES[i]);
    }
    src += "];\n";
    std::fs::write(out_dir.join("viewer_files.rs"), src).unwrap();
}
