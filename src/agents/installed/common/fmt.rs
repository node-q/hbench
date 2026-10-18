//! Value formatting for the config files, env vars and messages the installed agents write.

use serde_json::Value;

/// `s` quoted for a source literal: single quotes, or double quotes when `s` has a single quote but no double one;
/// backslash, the chosen quote, `\n`, `\r` and `\t` are backslash-escaped.
pub fn single_quoted(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::from(q);
    for c in s.chars() {
        match c {
            '\\' => out += "\\\\",
            '\n' => out += "\\n",
            '\r' => out += "\\r",
            '\t' => out += "\\t",
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

/// A float that always shows a decimal point for whole values (`1.0`, `0.7`).
pub fn float_with_point(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 { format!("{f:.1}") } else { f.to_string() }
}

/// JSON with `, ` and `: ` separators and non-ASCII characters escaped as `\uXXXX`.
pub fn json_spaced(v: &Value) -> String {
    match v {
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter().map(|(k, v)| format!("{}: {}", json_spaced(&Value::String(k.clone())), json_spaced(v))).collect::<Vec<_>>().join(", ")
        ),
        Value::Array(a) => format!("[{}]", a.iter().map(json_spaced).collect::<Vec<_>>().join(", ")),
        Value::String(s) => serde_json::to_string(s).unwrap().chars().fold(String::new(), |mut o, c| {
            if c.is_ascii() {
                o.push(c);
            } else {
                for u in c.encode_utf16(&mut [0; 2]) {
                    o += &format!("\\u{u:04x}");
                }
            }
            o
        }),
        v => v.to_string(),
    }
}

/// A string as-is, any other value as JSON.
pub fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// Type name of a JSON value for error messages (`NoneType`, `bool`, `int`, `float`, `str`, `list`, `dict`).
pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}
