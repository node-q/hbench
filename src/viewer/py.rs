//! Python-compatible helpers.

use super::*;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

const MAX_FILE_SIZE: u64 = 1024 * 1024;
const MAX_MEDIA_FILE_SIZE: u64 = 64 * 1024 * 1024;
const MEDIA_EXTENSIONS: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
    ("wav", "audio/wav"),
    ("mp3", "audio/mpeg"),
    ("m4a", "audio/mp4"),
    ("aac", "audio/aac"),
    ("oga", "audio/ogg"),
    ("ogg", "audio/ogg"),
    ("flac", "audio/flac"),
    ("weba", "audio/webm"),
    ("aiff", "audio/aiff"),
    ("aif", "audio/aiff"),
];

/// `%XX` decoding; `plus` also maps `+` to space (query strings).
pub(super) fn pct_decode(s: &str, plus: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(if plus && b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(super) struct Query(Vec<(String, String)>);

impl Query {
    pub(super) fn parse(q: &str) -> Query {
        Query(
            q.split('&')
                .filter(|kv| !kv.is_empty())
                .map(|kv| kv.split_once('=').unwrap_or((kv, "")))
                .map(|(k, v)| (pct_decode(k, true), pct_decode(v, true)))
                .collect(),
        )
    }
    pub(super) fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().rev().find(|(x, _)| x == k).map(|(_, v)| v.as_str())
    }
    pub(super) fn all(&self, k: &str) -> Vec<String> {
        self.0.iter().filter(|(x, _)| x == k).map(|(_, v)| v.clone()).collect()
    }
    /// FastAPI `int = Query(default, ge=.., le=..)` validation, with its 422 detail shape.
    pub(super) fn int(&self, k: &str, default: i64, ge: Option<i64>, le: Option<i64>) -> std::result::Result<i64, Err> {
        let Some(raw) = self.get(k) else { return Ok(default) };
        let detail = |t: &str, msg: String, ctx: Option<Value>| {
            let mut d = json!({"type": t, "loc": ["query", k], "msg": msg, "input": raw});
            if let Some(c) = ctx {
                d["ctx"] = c;
            }
            (422, json!([d]))
        };
        let n: i64 = raw
            .trim()
            .parse()
            .map_err(|_| detail("int_parsing", "Input should be a valid integer, unable to parse string as an integer".into(), None))?;
        if let Some(g) = ge.filter(|g| n < *g) {
            return Err(detail("greater_than_equal", format!("Input should be greater than or equal to {g}"), Some(json!({"ge": g}))));
        }
        if let Some(l) = le.filter(|l| n > *l) {
            return Err(detail("less_than_equal", format!("Input should be less than or equal to {l}"), Some(json!({"le": l}))));
        }
        Ok(n)
    }
    pub(super) fn required(&self, k: &str) -> std::result::Result<String, Err> {
        self.get(k)
            .map(String::from)
            .ok_or_else(|| (422, json!([{"type": "missing", "loc": ["query", k], "msg": "Field required", "input": null}])))
    }
}

/// `Path.read_text()`: strict UTF-8 with universal-newline translation. `Ok(None)` if missing.
pub(super) fn read_text(p: &Path) -> std::io::Result<Option<String>> {
    if !p.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(p)?;
    let s = String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(if s.contains('\r') { s.replace("\r\n", "\n").replace('\r', "\n") } else { s }))
}

pub(super) fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&read_text(p).ok()??).ok()
}

pub(super) fn read_json_object(p: &Path) -> Option<Value> {
    read_json(p).filter(Value::is_object)
}

pub(super) fn s(v: &Value) -> Option<&str> {
    v.as_str()
}

pub(super) fn str_or_null(o: Option<&str>) -> Value {
    o.map_or(Value::Null, |x| Value::String(x.into()))
}

/// Python truthiness for the JSON values the viewer inspects.
pub(super) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// A pydantic datetime: aware timestamps as given, naive ones taken as UTC (upstream's `_started_at_sort_key`).
pub(super) fn parse_dt(v: &Value) -> Option<DateTime<Utc>> {
    let t = v.as_str()?;
    DateTime::parse_from_rfc3339(t)
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f").ok().map(|n| n.and_utc()))
}

pub(super) fn ts(d: &DateTime<Utc>) -> f64 {
    d.timestamp() as f64 + d.timestamp_subsec_micros() as f64 / 1e6
}

/// pydantic's JSON form of an aware UTC datetime.
pub(super) fn py_dt(d: DateTime<Utc>) -> String {
    let base = d.format("%Y-%m-%dT%H:%M:%S").to_string();
    match d.timestamp_subsec_micros() {
        0 => format!("{base}Z"),
        us => format!("{base}.{us:06}Z"),
    }
}

/// A datetime string re-serialized the way pydantic dumps it (microseconds, `Z` for UTC), else unchanged.
fn norm_dt(v: &Value) -> Value {
    let Some(t) = v.as_str() else { return v.clone() };
    if let Ok(d) = DateTime::parse_from_rfc3339(t) {
        let us = d.timestamp_subsec_micros();
        let frac = if us == 0 { String::new() } else { format!(".{us:06}") };
        let off = d.offset().local_minus_utc();
        let tz = if off == 0 {
            "Z".to_string()
        } else {
            format!("{}{:02}:{:02}", if off < 0 { '-' } else { '+' }, off.abs() / 3600, off.abs() % 3600 / 60)
        };
        return json!(format!("{}{frac}{tz}", d.format("%Y-%m-%dT%H:%M:%S")));
    }
    match NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f") {
        Ok(n) => {
            let us = n.and_utc().timestamp_subsec_micros();
            json!(format!("{}{}", n.format("%Y-%m-%dT%H:%M:%S"), if us == 0 { String::new() } else { format!(".{us:06}") }))
        }
        Err(_) => v.clone(),
    }
}

/// Normalize every `*_at` datetime in a dumped model (TrialResult/JobResult timings, exception `occurred_at`).
pub(super) fn norm_dts(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                if k.ends_with("_at") && x.is_string() {
                    *x = norm_dt(x);
                } else {
                    norm_dts(x);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(norm_dts),
        _ => {}
    }
}

/// `datetime.fromtimestamp(path.stat().st_mtime, tz=timezone.utc)`.
pub(super) fn mtime_dt(p: &Path) -> Option<DateTime<Utc>> {
    let d = p.metadata().ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    let t = d.as_secs() as f64 + 1e-9 * d.subsec_nanos() as f64; // st_mtime
    let (mut sec, frac) = (t.trunc() as i64, t.fract());
    let mut us = (frac * 1e6).round_ties_even() as i64;
    if us >= 1_000_000 {
        sec += 1;
        us -= 1_000_000;
    }
    DateTime::from_timestamp(sec, (us * 1000) as u32)
}

/// `Path.as_uri()` for an absolute path.
pub(super) fn as_uri(p: &Path) -> String {
    let mut out = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/_.-~".contains(&b) {
            out.push(b as char);
        } else {
            out += &format!("%{b:02X}");
        }
    }
    out
}

pub(super) fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// `sorted(dir.iterdir())` names.
pub(super) fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort_by_key(|p| name_of(p));
    v
}

/// `(base / rel).resolve()` without requiring existence: symlinks resolved where the path exists, `..` lexically.
pub(super) fn resolve(base: &Path, rel: &str) -> PathBuf {
    let joined = base.join(rel);
    if let Ok(c) = joined.canonicalize() {
        return c;
    }
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => {
                out.push(c);
                if let Ok(real) = out.canonicalize() {
                    out = real;
                }
            }
        }
    }
    out
}

/// `parent in child.parents`.
pub(super) fn strictly_under(child: &Path, parent: &Path) -> bool {
    child != parent && child.starts_with(parent)
}

pub(super) fn uncached(n_input: Option<i64>, n_cache: Option<i64>) -> Option<i64> {
    let i = n_input?;
    Some(match n_cache {
        Some(c) => (i - c).max(0),
        None => i,
    })
}

pub(super) fn num_or_null<T: Into<Value>>(o: Option<T>) -> Value {
    o.map_or(Value::Null, Into::into)
}

pub(super) fn paginate(items: Vec<Value>, page: i64, page_size: i64) -> Value {
    let total = items.len() as i64;
    let total_pages = if total > 0 { (total + page_size - 1) / page_size } else { 0 };
    let start = ((page - 1) * page_size).min(total) as usize;
    let page_items: Vec<Value> = items.into_iter().skip(start).take(page_size as usize).collect();
    json!({"items": page_items, "total": total, "page": page, "page_size": page_size, "total_pages": total_pages})
}

/// `Counter` -> sorted `[FilterOption]`.
pub(super) fn filter_options(counts: BTreeMap<String, i64>) -> Value {
    Value::Array(counts.into_iter().map(|(v, c)| json!({"value": v, "count": c})).collect())
}

/// upstream's `metadata_filter_value`: encodes None without colliding with real values.
pub(super) fn metadata_filter_value(v: Option<&str>) -> String {
    match v {
        None => "__missing__".into(),
        Some(x) if x == "__missing__" || x.starts_with('\\') => format!("\\{x}"),
        Some(x) => x.into(),
    }
}

fn format_size(n: u64) -> String {
    if n < 1024 {
        format!("{n} bytes")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// FileResponse (media) or PlainTextResponse with upstream's size caps and errors.
pub(super) fn serve_file(full: &Path) -> Res {
    if !full.exists() {
        return Err(err(404, "File not found"));
    }
    if full.is_dir() {
        return Err(err(400, "Cannot read directory"));
    }
    let ext = full.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let media = MEDIA_EXTENSIONS.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m);
    let limit = if media.is_some() { MAX_MEDIA_FILE_SIZE } else { MAX_FILE_SIZE };
    let size = full.metadata().map(|m| m.len()).unwrap_or(0);
    if size > limit {
        return Err(err(413, format!("File too large: {} (max {})", format_size(size), format_size(limit))));
    }
    if let Some(m) = media {
        let body = std::fs::read(full).map_err(|e| err(500, e.to_string()))?;
        return Ok(Resp {
            status: 200,
            ctype: m.into(),
            body,
            headers: vec![("Content-Disposition", format!("attachment; filename=\"{}\"", name_of(full)))],
        });
    }
    match read_text(full) {
        Ok(Some(t)) => ok_text(t),
        Ok(None) => Err(err(404, "File not found")),
        Err(_) => Err(err(415, "File is binary and cannot be displayed")),
    }
}

fn file_info(path: String, p: &Path, is_dir: bool) -> Value {
    json!({"path": path, "name": name_of(p), "is_dir": is_dir, "size": if is_dir { Value::Null } else { p.metadata().map(|m| m.len()).unwrap_or(0).into() }})
}

/// Recursive listing in `sorted(iterdir())` order; `with_dirs` false = files only (artifacts).
pub(super) fn scan_dir(dir: &Path, base: &str, with_dirs: bool, skip_top_manifest: bool, out: &mut Vec<Value>) {
    for item in sorted_entries(dir) {
        let name = name_of(&item);
        let rel = if base.is_empty() { name.clone() } else { format!("{base}/{name}") };
        if skip_top_manifest && base.is_empty() && name == "manifest.json" {
            continue;
        }
        if item.is_dir() {
            if with_dirs {
                out.push(file_info(rel.clone(), &item, true));
            }
            scan_dir(&item, &rel, with_dirs, skip_top_manifest, out);
        } else {
            out.push(file_info(rel, &item, false));
        }
    }
}

pub(super) fn py_repr(v: &Value) -> String {
    match v {
        Value::String(x) => format!("'{x}'"),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        x => x.to_string(),
    }
}
