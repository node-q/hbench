//! File mention extraction from instructions.

const KNOWN_EXTS: &[&str] = &[
    "md", "txt", "pdf", "png", "jpg", "jpeg", "gif", "svg", "csv", "json", "jsonl", "xml", "yaml", "yml", "toml", "py", "js", "ts", "tsx",
    "jsx", "sh", "html", "css", "sql", "rs", "go", "java", "c", "cpp", "h", "rb", "ipynb", "parquet", "xlsx", "docx", "pptx", "zip", "tar",
    "gz", "lock", "cfg", "ini", "log",
];
const KNOWN_DOTFILES: &[&str] = &["gitignore", "gitattributes", "env", "envrc", "bashrc", "zshrc", "editorconfig", "npmrc", "prettierrc"];

/// Tiny backtracking matcher: the file-mention pattern uses lookbehind/lookahead, which
/// the `regex` crate lacks, so the pattern is hand-built from these nodes with backtracking
/// regex exploration order (greedy, alternatives left to right).
enum Node {
    Class(fn(char) -> bool),
    Lit(&'static str),
    /// Case-insensitive alternatives, tried in order.
    Words(&'static [&'static str]),
    Seq(Vec<Node>),
    Alt(Vec<Node>),
    Star(Box<Node>),
    Opt(Box<Node>),
    NotAhead(Box<Node>),
    /// `$` without multiline mode: end, or before a final newline.
    End,
}

type K<'a> = dyn FnMut(usize) -> bool + 'a;

impl Node {
    fn m(&self, s: &[char], i: usize, k: &mut K<'_>) -> bool {
        match self {
            Node::Class(f) => i < s.len() && f(s[i]) && k(i + 1),
            Node::Lit(l) => {
                let n = l.chars().count();
                i + n <= s.len() && s[i..i + n].iter().copied().eq(l.chars()) && k(i + n)
            }
            Node::Words(ws) => ws.iter().any(|w| {
                let n = w.chars().count();
                i + n <= s.len() && s[i..i + n].iter().zip(w.chars()).all(|(a, b)| a.to_lowercase().eq(b.to_lowercase())) && k(i + n)
            }),
            Node::Seq(ns) => seq(ns, s, i, k),
            Node::Alt(ns) => ns.iter().any(|n| n.m(s, i, k)),
            Node::Star(n) => star(n, s, i, k),
            Node::Opt(n) => n.m(s, i, k) || k(i),
            Node::NotAhead(n) => !n.m(s, i, &mut |_| true) && k(i),
            Node::End => (i == s.len() || (i + 1 == s.len() && s[i] == '\n')) && k(i),
        }
    }
}

fn seq(ns: &[Node], s: &[char], i: usize, k: &mut K<'_>) -> bool {
    match ns.split_first() {
        None => k(i),
        Some((first, rest)) => first.m(s, i, &mut |j| seq(rest, s, j, k)),
    }
}

fn star(n: &Node, s: &[char], i: usize, k: &mut K<'_>) -> bool {
    n.m(s, i, &mut |j| j > i && star(n, s, j, k)) || k(i)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn file_mention_pattern() -> Node {
    use Node::*;
    let w = || Class(is_word);
    let w_dash = || Class(|c| is_word(c) || c == '-');
    let w_dot_dash = || Class(|c| is_word(c) || c == '.' || c == '-');
    let upper = || Class(|c| c.is_ascii_uppercase());
    let dot = || Lit(".");
    Seq(vec![
        // (?!(?-i:[A-Z]\.[A-Z])(?:$|[^\w/.-]|\.(?!\w)))  D.C. / U.S. initials
        NotAhead(Box::new(Seq(vec![
            upper(),
            dot(),
            upper(),
            Alt(vec![End, Class(|c| !(is_word(c) || c == '/' || c == '.' || c == '-')), Seq(vec![dot(), NotAhead(Box::new(w()))])]),
        ]))),
        Opt(Box::new(Lit("/"))),
        Star(Box::new(Seq(vec![dot(), Opt(Box::new(dot())), Lit("/")]))),
        Star(Box::new(Seq(vec![w(), Star(Box::new(w_dot_dash())), Lit("/")]))),
        Alt(vec![
            Seq(vec![
                w(),
                Star(Box::new(w_dash())),
                Star(Box::new(Seq(vec![dot(), w_dash(), Star(Box::new(w_dash()))]))),
                dot(),
                Words(KNOWN_EXTS),
            ]),
            Seq(vec![dot(), Words(KNOWN_DOTFILES), Star(Box::new(Seq(vec![dot(), w(), Star(Box::new(w()))])))]),
        ]),
        NotAhead(Box::new(Class(|c| is_word(c) || c == '/' || c == '-'))),
        NotAhead(Box::new(Seq(vec![dot(), w()]))),
    ])
}

/// `FILE_MENTION_PATTERN.finditer(text)` group 1 values.
pub fn file_mentions(text: &str) -> Vec<String> {
    let s: Vec<char> = text.chars().collect();
    let pat = file_mention_pattern();
    let mut out = vec![];
    let mut pos = 0;
    while pos < s.len() {
        let found = (pos..s.len()).find_map(|start| {
            // (?<![\w~./-])
            if start > 0 && (is_word(s[start - 1]) || "~./-".contains(s[start - 1])) {
                return None;
            }
            let mut end = None;
            pat.m(&s, start, &mut |e| {
                end = Some(e);
                true
            });
            end.map(|e| (start, e))
        });
        let Some((start, end)) = found else { break };
        out.push(s[start..end].iter().collect());
        pos = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file-mention test suite plus extra edge cases.
    #[test]
    fn file_mention_pattern_suite() {
        let suite: &[(&str, &[&str])] = &[
            ("Open notes.md when you can", &["notes.md"]),
            ("notes.md starts the sentence", &["notes.md"]),
            ("the file is notes/lessons/2026-07-05.md", &["notes/lessons/2026-07-05.md"]),
            ("two files: a.py and b.py", &["a.py", "b.py"]),
            ("absolute /etc/hosts.cfg path", &["/etc/hosts.cfg"]),
            ("run ./build.sh then ../deploy.sh", &["./build.sh", "../deploy.sh"]),
            ("deep ../../scripts/build.sh path", &["../../scripts/build.sh"]),
            ("home ~/Developer/lessons/foo.md path", &[]),
            ("edit .gitignore and src/.env.local", &[".gitignore", "src/.env.local"]),
            ("the .env file holds secrets", &[".env"]),
            ("unknown dotfile .venv should NOT match", &[]),
            (".NET is a framework not a file", &[]),
            ("unpack foo.tar.gz and app.config.py", &["foo.tar.gz", "app.config.py"]),
            ("release v1.2.3.tar.gz is out", &["v1.2.3.tar.gz"]),
            ("jquery.min.js is legacy", &["jquery.min.js"]),
            (
                "outputs events.jsonl layout.xml notebook.ipynb data.parquet",
                &["events.jsonl", "layout.xml", "notebook.ipynb", "data.parquet"],
            ),
            ("see README.MD and photo.JPG", &["README.MD", "photo.JPG"]),
            ("I renamed foo.txt.", &["foo.txt"]),
            ("is it in foo.md?", &["foo.md"]),
            ("files (see notes.md) here", &["notes.md"]),
            ("he said \"read plan.md\".", &["plan.md"]),
            ("check [design.md] and `impl.py`", &["design.md", "impl.py"]),
            ("link: [lesson](notes/2026-07-05.md)", &["notes/2026-07-05.md"]),
            ("foo.md,bar.md are both stale", &["foo.md", "bar.md"]),
            ("error at src/app.py:42 today", &["src/app.py"]),
            ("see notes.md#history for context", &["notes.md"]),
            ("foo.md's contents changed", &["foo.md"]),
            ("semicolon test notes.md; done", &["notes.md"]),
            ("python 3.14 and semver 1.2.3 are versions", &[]),
            ("use e.g. numpy.array or os.path.join", &[]),
            ("born in the U.S. in 1990, i.e. long ago", &[]),
            ("visit example.com or www.google.org", &[]),
            ("email alexgshaw64@gmail.com please", &[]),
            ("call obj.method() and this.props.name", &[]),
            ("Washington D.C. is not a path", &[]),
            ("the amount is 1,234.56 dollars", &[]),
            ("unknown exts data.feather notes.org skip", &[]),
            ("extension prefix trap: notes.mdx file.pyc", &[]),
            ("urls https://example.com/report.pdf skip", &[]),
            ("open My Document.pdf now", &["Document.pdf"]),
            ("Makefile and Dockerfile have no extension", &[]),
            ("windows C:\\Users\\alex\\file.txt path", &["file.txt"]),
            ("ellipsis right before...foo.md blocks it", &[]),
            ("bare extension mention: .md files", &[]),
            ("résumé.pdf and 中文.txt exist", &["résumé.pdf", "中文.txt"]),
            ("A.B\n", &[]),
            ("x.json.", &["x.json"]),
            ("a.c.cpp b.jsonl.", &["a.c.cpp", "b.jsonl"]),
            ("/abs/.env.local.bak ok", &["/abs/.env.local.bak"]),
            ("../../a/b.c/d.md", &["../../a/b.c/d.md"]),
        ];
        for (text, want) in suite {
            assert_eq!(file_mentions(text), *want, "{text:?}");
        }
    }
}
