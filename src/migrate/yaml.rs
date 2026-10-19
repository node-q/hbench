//! Block-style YAML writer for migrated docker-compose files, with stable byte-for-byte output.

use super::{float_repr, yaml_text};
use regex::Regex;
use serde_yaml_ng::Value;
use std::sync::LazyLock;

/// `v` as block-style YAML: indent 2, width 80, ASCII only, keys in order. Handles the node shapes a
/// YAML load produces (no anchors/tags).
pub(super) fn to_yaml(v: &Value) -> String {
    let mut e = Emitter { out: String::new(), col: 0, ws: true, indention: true, indent: None, indents: vec![] };
    e.node(v, false, false);
    e.write_indent();
    e.out
}

static NON_STR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(?:yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF",
        r"|[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?",
        r"|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN)",
        r"|[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+",
        r"|<<|~|null|Null|NULL|=|",
        r"|[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]",
        r"|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?(?:[Tt]|[ \t]+)[0-9][0-9]?:[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?",
        r")$"
    ))
    .unwrap()
});

fn is_break(c: char) -> bool {
    matches!(c, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

fn is_ws0(c: char) -> bool {
    matches!(c, '\0' | ' ' | '\t' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

struct Analysis {
    empty: bool,
    multiline: bool,
    allow_block_plain: bool,
    allow_single_quoted: bool,
}

fn analyze_scalar(s: &[char]) -> Analysis {
    if s.is_empty() {
        return Analysis { empty: true, multiline: false, allow_block_plain: true, allow_single_quoted: true };
    }
    let (mut block_ind, mut line_breaks, mut special) = (false, false, false);
    let (mut lead_space, mut lead_break, mut trail_space, mut trail_break, mut break_space, mut space_break) =
        (false, false, false, false, false, false);
    if s.starts_with(&['-', '-', '-']) || s.starts_with(&['.', '.', '.']) {
        block_ind = true;
    }
    let mut preceded_ws = true;
    let mut followed_ws = s.len() == 1 || is_ws0(s[1]);
    let (mut prev_space, mut prev_break) = (false, false);
    for (i, &ch) in s.iter().enumerate() {
        // Block indicators (flow-plain is never used: no flow context with scalars).
        let ind = if i == 0 {
            "#,[]{}&*!|>'\"%@`".contains(ch) || ((ch == '?' || ch == ':' || ch == '-') && followed_ws)
        } else {
            (ch == ':' && followed_ws) || (ch == '#' && preceded_ws)
        };
        block_ind |= ind;
        if is_break(ch) {
            line_breaks = true;
        }
        if !(ch == '\n' || (' '..='~').contains(&ch)) {
            special = true; // allow_unicode=False: every non-ASCII char is special
        }
        if ch == ' ' {
            lead_space |= i == 0;
            trail_space |= i == s.len() - 1;
            break_space |= prev_break;
            prev_space = true;
            prev_break = false;
        } else if is_break(ch) {
            lead_break |= i == 0;
            trail_break |= i == s.len() - 1;
            space_break |= prev_space;
            prev_space = false;
            prev_break = true;
        } else {
            prev_space = false;
            prev_break = false;
        }
        preceded_ws = is_ws0(ch);
        followed_ws = i + 2 >= s.len() || is_ws0(s[i + 2]);
    }
    let abp =
        !(lead_space || lead_break || trail_space || trail_break || break_space || space_break || special || line_breaks || block_ind);
    let asq = !(break_space || space_break || special);
    Analysis { empty: false, multiline: line_breaks, allow_block_plain: abp, allow_single_quoted: asq }
}

/// A scalar's text, and whether it can be written plain (unquoted) without reading back as another type.
fn scalar_text(v: &Value) -> (String, bool) {
    match v {
        Value::Null => ("null".into(), true),
        Value::Bool(b) => (b.to_string(), true),
        Value::Number(n) if n.is_f64() => {
            let f = n.as_f64().unwrap();
            let s = if f.is_nan() {
                ".nan".into()
            } else if f.is_infinite() {
                if f > 0.0 { ".inf" } else { "-.inf" }.into()
            } else {
                let r = float_repr(f).to_lowercase();
                if !r.contains('.') && r.contains('e') { r.replacen('e', ".0e", 1) } else { r }
            };
            (s, true)
        }
        Value::Number(n) => (n.to_string(), true),
        Value::String(s) => (s.clone(), !NON_STR_RE.is_match(s)),
        _ => (yaml_text(v), true),
    }
}

struct Emitter {
    out: String,
    col: usize,
    ws: bool,
    indention: bool,
    indent: Option<usize>,
    indents: Vec<Option<usize>>,
}

impl Emitter {
    fn node(&mut self, v: &Value, mapping_ctx: bool, simple_key: bool) {
        match v {
            Value::Tagged(t) => self.node(&t.value, mapping_ctx, simple_key),
            Value::Sequence(s) if s.is_empty() => self.flow_empty("[", "]"),
            Value::Mapping(m) if m.is_empty() => self.flow_empty("{", "}"),
            Value::Sequence(s) => {
                let indentless = mapping_ctx && !self.indention;
                self.increase_indent(false, indentless);
                for item in s {
                    self.write_indent();
                    self.write_indicator("-", true, false, true);
                    self.node(item, false, false);
                }
                self.indent = self.indents.pop().unwrap();
            }
            Value::Mapping(m) => {
                self.increase_indent(false, false);
                for (k, val) in m {
                    self.write_indent();
                    if check_simple_key(k) {
                        self.node(k, true, true);
                        self.write_indicator(":", false, false, false);
                    } else {
                        self.write_indicator("?", true, false, true);
                        self.node(k, true, false);
                        self.write_indent();
                        self.write_indicator(":", true, false, true);
                    }
                    self.node(val, true, false);
                }
                self.indent = self.indents.pop().unwrap();
            }
            _ => {
                let (text, implicit) = scalar_text(v);
                let chars: Vec<char> = text.chars().collect();
                self.increase_indent(true, false);
                let a = analyze_scalar(&chars);
                let split = !simple_key;
                if implicit && !(simple_key && (a.empty || a.multiline)) && a.allow_block_plain {
                    self.write_plain(&chars, split);
                } else if a.allow_single_quoted && !(simple_key && a.multiline) {
                    self.write_single_quoted(&chars, split);
                } else {
                    self.write_double_quoted(&chars, split);
                }
                self.indent = self.indents.pop().unwrap();
            }
        }
    }

    // Only empty collections go flow style (all safe_dump(default_flow_style=False) emits).
    fn flow_empty(&mut self, open: &str, close: &str) {
        self.write_indicator(open, true, true, false);
        self.write_indicator(close, false, false, false);
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.indents.push(self.indent);
        match self.indent {
            None => self.indent = Some(if flow { 2 } else { 0 }),
            Some(i) if !indentless => self.indent = Some(i + 2),
            _ => {}
        }
    }

    fn write(&mut self, s: &str) {
        self.col += s.chars().count();
        self.out += s;
    }

    fn write_indicator(&mut self, ind: &str, need_ws: bool, whitespace: bool, indention: bool) {
        let data = if self.ws || !need_ws { ind.to_string() } else { format!(" {ind}") };
        self.ws = whitespace;
        self.indention = self.indention && indention;
        self.write(&data);
    }

    fn write_indent(&mut self) {
        let indent = self.indent.unwrap_or(0);
        if !self.indention || self.col > indent || (self.col == indent && !self.ws) {
            self.write_line_break(None);
        }
        if self.col < indent {
            self.ws = true;
            self.out += &" ".repeat(indent - self.col);
            self.col = indent;
        }
    }

    fn write_line_break(&mut self, br: Option<char>) {
        self.ws = true;
        self.indention = true;
        self.col = 0;
        self.out.push(br.unwrap_or('\n'));
    }

    // No line-break branch; multiline scalars are never written plain.
    fn write_plain(&mut self, t: &[char], split: bool) {
        if t.is_empty() {
            return;
        }
        if !self.ws {
            self.write(" ");
        }
        self.ws = false;
        self.indention = false;
        let (mut spaces, mut start, mut end) = (false, 0, 0);
        while end <= t.len() {
            let ch = t.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.col > 80 && split {
                        self.write_indent();
                        self.ws = false;
                        self.indention = false;
                    } else {
                        self.write(&t[start..end].iter().collect::<String>());
                    }
                    start = end;
                }
            } else if ch.is_none_or(|c| c == ' ' || is_break(c)) {
                self.write(&t[start..end].iter().collect::<String>());
                start = end;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
            }
            end += 1;
        }
    }

    fn write_single_quoted(&mut self, t: &[char], split: bool) {
        self.write_indicator("'", true, false, false);
        let (mut spaces, mut breaks, mut start, mut end) = (false, false, 0, 0);
        while end <= t.len() {
            let ch = t.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.col > 80 && split && start != 0 && end != t.len() {
                        self.write_indent();
                    } else {
                        self.write(&t[start..end].iter().collect::<String>());
                    }
                    start = end;
                }
            } else if breaks {
                if ch.is_none_or(|c| !is_break(c)) {
                    if t[start] == '\n' {
                        self.write_line_break(None);
                    }
                    for &br in &t[start..end] {
                        self.write_line_break(if br == '\n' { None } else { Some(br) });
                    }
                    self.write_indent();
                    start = end;
                }
            } else if ch.is_none_or(|c| c == ' ' || c == '\'' || is_break(c)) && start < end {
                self.write(&t[start..end].iter().collect::<String>());
                start = end;
            }
            if ch == Some('\'') {
                self.write("''");
                start = end + 1;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = is_break(c);
            }
            end += 1;
        }
        self.write_indicator("'", false, false, false);
    }

    fn write_double_quoted(&mut self, t: &[char], split: bool) {
        self.write_indicator("\"", true, false, false);
        let n = t.len() as isize;
        let (mut start, mut end): (isize, isize) = (0, 0);
        let slice = |a: isize, b: isize| -> String { if a < b { t[a as usize..b as usize].iter().collect() } else { String::new() } };
        while end <= n {
            let ch = t.get(end as usize).copied();
            if ch.is_none_or(|c| matches!(c, '"' | '\\' | '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{feff}') || !(' '..='~').contains(&c)) {
                if start < end {
                    self.write(&slice(start, end));
                    start = end;
                }
                if let Some(c) = ch {
                    let data = match c {
                        '\0' => "\\0".into(),
                        '\u{7}' => "\\a".into(),
                        '\u{8}' => "\\b".into(),
                        '\t' => "\\t".into(),
                        '\n' => "\\n".into(),
                        '\u{b}' => "\\v".into(),
                        '\u{c}' => "\\f".into(),
                        '\r' => "\\r".into(),
                        '\u{1b}' => "\\e".into(),
                        '"' => "\\\"".into(),
                        '\\' => "\\\\".into(),
                        '\u{85}' => "\\N".into(),
                        '\u{a0}' => "\\_".into(),
                        '\u{2028}' => "\\L".into(),
                        '\u{2029}' => "\\P".into(),
                        c if (c as u32) <= 0xff => format!("\\x{:02X}", c as u32),
                        c if (c as u32) <= 0xffff => format!("\\u{:04X}", c as u32),
                        c => format!("\\U{:08X}", c as u32),
                    };
                    self.write(&data);
                    start = end + 1;
                }
            }
            if 0 < end && end < n - 1 && (ch == Some(' ') || start >= end) && self.col as isize + (end - start) > 80 && split {
                let data = slice(start, end) + "\\";
                if start < end {
                    start = end;
                }
                self.write(&data);
                self.write_indent();
                self.ws = false;
                self.indention = false;
                if t[start as usize] == ' ' {
                    self.write("\\");
                }
            }
            end += 1;
        }
        self.write_indicator("\"", false, false, false);
    }
}

fn check_simple_key(k: &Value) -> bool {
    match k {
        Value::Sequence(s) => s.is_empty(),
        Value::Mapping(m) => m.is_empty(),
        _ => {
            let chars: Vec<char> = scalar_text(k).0.chars().collect();
            let a = analyze_scalar(&chars);
            chars.len() < 128 && !a.empty && !a.multiline
        }
    }
}

// Expected outputs below are golden files for these fixtures.
