//! Python `toml` 0.10 encoder.

use anyhow::{Result, bail};
use serde_json::{Map, Value};

/// `toml.dumps(o)` from the Python `toml` package, quirks included (task.toml is written with it).
pub(super) fn toml_dumps(o: &Map<String, Value>) -> Result<String> {
    let (mut retval, mut sections) = dump_sections(o, "")?;
    while !sections.is_empty() {
        let mut next: Vec<(String, Map<String, Value>)> = vec![];
        for (section, table) in &sections {
            let (add, subs) = dump_sections(table, section)?;
            if !add.is_empty() || subs.is_empty() {
                if !retval.is_empty() && !retval.ends_with("\n\n") {
                    retval.push('\n');
                }
                retval.push_str(&format!("[{section}]\n"));
                retval.push_str(&add);
            }
            next.extend(subs.into_iter().map(|(k, v)| (format!("{section}.{k}"), v)));
        }
        sections = next;
    }
    Ok(retval)
}

fn bare_key(k: &str) -> String {
    if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') { k.to_string() } else { dump_str(k) }
}

type Sections = Vec<(String, Map<String, Value>)>;

fn dump_sections(o: &Map<String, Value>, sup: &str) -> Result<(String, Sections)> {
    let sup = if !sup.is_empty() && !sup.ends_with('.') { format!("{sup}.") } else { sup.to_string() };
    let mut retstr = String::new();
    let mut retdict: Sections = vec![];
    let mut arraystr = String::new();
    for (key, val) in o {
        let q = bare_key(key);
        match val {
            Value::Object(m) => retdict.push((q, m.clone())),
            Value::Array(items) if items.iter().any(Value::is_object) => {
                for a in items {
                    let Value::Object(a) = a else { bail!("TypeError: cannot dump mixed table/value array {key:?} to TOML") };
                    let mut arraytabstr = String::from("\n");
                    arraystr.push_str(&format!("[[{sup}{q}]]\n"));
                    let (s, mut d) = dump_sections(a, &format!("{sup}{q}"))?;
                    if !s.is_empty() {
                        if s.starts_with('[') {
                            arraytabstr.push_str(&s);
                        } else {
                            arraystr.push_str(&s);
                        }
                    }
                    while !d.is_empty() {
                        let mut newd = vec![];
                        for (dsec, table) in &d {
                            let (s1, d1) = dump_sections(table, &format!("{sup}{q}.{dsec}"))?;
                            if !s1.is_empty() {
                                arraytabstr.push_str(&format!("[{sup}{q}.{dsec}]\n"));
                                arraytabstr.push_str(&s1);
                            }
                            newd.extend(d1.into_iter().map(|(k, v)| (format!("{dsec}.{k}"), v)));
                        }
                        d = newd;
                    }
                    arraystr.push_str(&arraytabstr);
                }
            }
            Value::Null => {}
            v => retstr.push_str(&format!("{q} = {}\n", dump_value(v))),
        }
    }
    retstr.push_str(&arraystr);
    Ok((retstr, retdict))
}

fn dump_value(v: &Value) -> String {
    match v {
        Value::String(s) => dump_str(s),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) if n.is_f64() => dump_float(n.as_f64().unwrap_or_default()),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => format!("[{}]", a.iter().map(|x| format!(" {},", dump_value(x))).collect::<String>()),
        // A dict nested in a list is iterated like one (its keys).
        Value::Object(m) => format!("[{}]", m.keys().map(|k| format!(" {},", dump_str(k))).collect::<String>()),
        Value::Null => dump_str("None"),
    }
}

/// Python `str(float)` with the encoder's exponent tweak.
fn dump_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let a = f.abs();
    if a != 0.0 && !(1e-4..1e16).contains(&a) {
        let s = format!("{f:e}");
        let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        return format!("{mant}e{}{}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    let s = format!("{f}");
    if s.contains('.') { s } else { format!("{s}.0") }
}

/// Python `repr(str)`.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if !is_printable(c) => {
                let n = c as u32;
                if n <= 0xff {
                    out.push_str(&format!("\\x{n:02x}"));
                } else if n <= 0xffff {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    out.push_str(&format!("\\U{n:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// ponytail: approximates Python's `str.isprintable` (control, line/para separators, common
/// format chars, non-ASCII spaces); exact needs Unicode category tables.
fn is_printable(c: char) -> bool {
    !(c.is_control()
        || matches!(c as u32, 0xa0 | 0xad | 0x1680 | 0x2000..=0x200f | 0x2028..=0x202f | 0x205f..=0x2064 | 0x3000 | 0xfeff | 0xd800..=0xdfff | 0xe000..=0xf8ff))
}

/// `toml.encoder._dump_str`, including its `\x` and backslash handling.
fn dump_str(s: &str) -> String {
    let r = py_repr(s);
    let single = r.starts_with('\'');
    let mut v = r[1..r.len() - 1].to_string();
    if single {
        v = v.replace("\\'", "'").replace('"', "\\\"");
    }
    let mut parts: Vec<String> = v.split("\\x").map(String::from).collect();
    while parts.len() > 1 {
        if parts[0].is_empty() {
            parts.remove(0);
            if parts.len() < 2 {
                break;
            }
        }
        parts[0] = parts[0].replace("\\\\", "\\");
        let chars: Vec<char> = parts[0].chars().collect();
        let mut i = chars.len();
        let mut joinx = chars.last() != Some(&'\\');
        while i > 1 && chars[i - 1] == '\\' {
            joinx = !joinx;
            i -= 1;
        }
        let joiner = if joinx { "x" } else { "u00" };
        let merged = format!("{}{joiner}{}", parts[0], parts[1]);
        parts.splice(0..2, [merged]);
    }
    format!("\"{}\"", parts[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Python `toml` encoder quirks (outputs captured from toml 0.10.2).
    #[test]
    fn python_toml_encoder() {
        assert_eq!(dump_str("q\"'\\ \t\n\u{1}é中"), "\"q\\\"'\\ \\t\\nx01é中\"");
        assert_eq!(dump_str("it's \"x\""), "\"it's \\\"x\\\"\"");
        assert_eq!(dump_str("a\\b"), "\"a\\\\b\"");
        for (f, s) in [(1e-5, "1e-5"), (1e16, "1e+16"), (1.5e300, "1.5e+300"), (0.1, "0.1"), (600.0, "600.0"), (-3.0, "-3.0")] {
            assert_eq!(dump_float(f), s);
        }
        assert_eq!(dump_value(&json!([[1, 2], ["a"]])), "[ [ 1, 2,], [ \"a\",],]");
        let m = json!({"a": 1e-5, "k y": "v", "m": {"x": {"y": 1}, "z": [{"q": 1, "w": {"r": 2}}]}, "t": true});
        assert_eq!(
            toml_dumps(m.as_object().unwrap()).unwrap(),
            "a = 1e-5\n\"k y\" = \"v\"\nt = true\n\n[m]\n[[m.z]]\nq = 1\n\n[m.z.w]\nr = 2\n\n[m.x]\ny = 1\n"
        );
    }
}
