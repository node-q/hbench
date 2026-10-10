//! `hbench view`: serves upstream 0.24.0's viewer SPA (assets/hbench-viewer, embedded by build.rs) and a Rust port
//! of its API (`hbench/viewer/{server,scanner,task_scanner,trial_utils,models,stream}.py`) over a jobs or tasks folder.
//! Response shapes, ordering, filters, sorting and error codes follow upstream; upstream Hub (auth/upload) and the
//! `--stream` editor need upstream's cloud services, so they answer the way upstream does when signed out / unavailable.

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/viewer_files.rs"));
}

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
const STREAM_HANDLE_NAME: &str = "stream.json";
const NOT_AUTHENTICATED: &str = "Not authenticated: hub sign-in is not supported by hbench.";
/// upstream's `EnvironmentType` values.
const ENVIRONMENTS: &[&str] = &[
    "docker", "podman", "daytona", "e2b", "modal", "runloop", "langsmith", "ec2", "gke", "ack", "openshift", "novita", "apple-container",
    "singularity", "islo", "tensorlake", "cwsandbox", "use-computer", "cua-cloud", "blaxel", "opensandbox", "beam", "skypilot", "hf-sandbox",
    "hyperbrowser", "vercel", "runta", "prime", "kata", "mosaic", "smol",
];
const RESOURCE_MODES: &[&str] = &["auto", "limit", "request", "guarantee", "ignore"];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Jobs,
    Tasks,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Jobs => "jobs",
            Mode::Tasks => "tasks",
        }
    }
}

pub struct Resp {
    pub status: u16,
    pub ctype: String,
    pub body: Vec<u8>,
    pub headers: Vec<(&'static str, String)>,
}

/// (status, FastAPI `detail`).
type Err = (u16, Value);
type Res = std::result::Result<Resp, Err>;

fn err(status: u16, detail: impl Into<String>) -> Err {
    (status, Value::String(detail.into()))
}

fn ok_json(v: Value) -> Res {
    Ok(Resp { status: 200, ctype: "application/json".into(), body: serde_json::to_vec(&v).unwrap(), headers: vec![] })
}

fn ok_text(s: String) -> Res {
    Ok(Resp { status: 200, ctype: "text/plain; charset=utf-8".into(), body: s.into_bytes(), headers: vec![] })
}

pub struct Req<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub origin: Option<&'a str>,
    pub host: Option<&'a str>,
    pub client_loopback: bool,
    pub body: &'a [u8],
}

struct LaunchedRun {
    child: std::process::Child,
    log_path: PathBuf,
}

pub struct App {
    pub folder: PathBuf,
    pub mode: Mode,
    runs: Mutex<HashMap<String, LaunchedRun>>,
}

impl App {
    pub fn new(folder: PathBuf, mode: Mode) -> App {
        App { folder, mode, runs: Mutex::new(HashMap::new()) }
    }
}

// ---------------------------------------------------------------- python-compatible helpers

/// `%XX` decoding; `plus` also maps `+` to space (query strings).
fn pct_decode(s: &str, plus: bool) -> String {
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

struct Query(Vec<(String, String)>);

impl Query {
    fn parse(q: &str) -> Query {
        Query(q.split('&').filter(|kv| !kv.is_empty()).map(|kv| kv.split_once('=').unwrap_or((kv, ""))).map(|(k, v)| (pct_decode(k, true), pct_decode(v, true))).collect())
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().rev().find(|(x, _)| x == k).map(|(_, v)| v.as_str())
    }
    fn all(&self, k: &str) -> Vec<String> {
        self.0.iter().filter(|(x, _)| x == k).map(|(_, v)| v.clone()).collect()
    }
    /// FastAPI `int = Query(default, ge=.., le=..)` validation, with its 422 detail shape.
    fn int(&self, k: &str, default: i64, ge: Option<i64>, le: Option<i64>) -> std::result::Result<i64, Err> {
        let Some(raw) = self.get(k) else { return Ok(default) };
        let detail = |t: &str, msg: String, ctx: Option<Value>| {
            let mut d = json!({"type": t, "loc": ["query", k], "msg": msg, "input": raw});
            if let Some(c) = ctx {
                d["ctx"] = c;
            }
            (422, json!([d]))
        };
        let n: i64 = raw.trim().parse().map_err(|_| detail("int_parsing", "Input should be a valid integer, unable to parse string as an integer".into(), None))?;
        if let Some(g) = ge.filter(|g| n < *g) {
            return Err(detail("greater_than_equal", format!("Input should be greater than or equal to {g}"), Some(json!({"ge": g}))));
        }
        if let Some(l) = le.filter(|l| n > *l) {
            return Err(detail("less_than_equal", format!("Input should be less than or equal to {l}"), Some(json!({"le": l}))));
        }
        Ok(n)
    }
    fn required(&self, k: &str) -> std::result::Result<String, Err> {
        self.get(k).map(String::from).ok_or_else(|| (422, json!([{"type": "missing", "loc": ["query", k], "msg": "Field required", "input": null}])))
    }
}

/// `Path.read_text()`: strict UTF-8 with universal-newline translation. `Ok(None)` if missing.
fn read_text(p: &Path) -> std::io::Result<Option<String>> {
    if !p.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(p)?;
    let s = String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(if s.contains('\r') { s.replace("\r\n", "\n").replace('\r', "\n") } else { s }))
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&read_text(p).ok()??).ok()
}

fn read_json_object(p: &Path) -> Option<Value> {
    read_json(p).filter(Value::is_object)
}

fn s(v: &Value) -> Option<&str> {
    v.as_str()
}

fn str_or_null(o: Option<&str>) -> Value {
    o.map_or(Value::Null, |x| Value::String(x.into()))
}

/// Python truthiness for the JSON values the viewer inspects.
fn truthy(v: &Value) -> bool {
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
fn parse_dt(v: &Value) -> Option<DateTime<Utc>> {
    let t = v.as_str()?;
    DateTime::parse_from_rfc3339(t).map(|d| d.with_timezone(&Utc)).ok().or_else(|| NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f").ok().map(|n| n.and_utc()))
}

fn ts(d: &DateTime<Utc>) -> f64 {
    d.timestamp() as f64 + d.timestamp_subsec_micros() as f64 / 1e6
}

/// pydantic's JSON form of an aware UTC datetime.
fn py_dt(d: DateTime<Utc>) -> String {
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
        let tz = if off == 0 { "Z".to_string() } else { format!("{}{:02}:{:02}", if off < 0 { '-' } else { '+' }, off.abs() / 3600, off.abs() % 3600 / 60) };
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
fn norm_dts(v: &mut Value) {
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
fn mtime_dt(p: &Path) -> Option<DateTime<Utc>> {
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
fn as_uri(p: &Path) -> String {
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

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// `sorted(dir.iterdir())` names.
fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort_by_key(|p| name_of(p));
    v
}

/// `(base / rel).resolve()` without requiring existence: symlinks resolved where the path exists, `..` lexically.
fn resolve(base: &Path, rel: &str) -> PathBuf {
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
fn strictly_under(child: &Path, parent: &Path) -> bool {
    child != parent && child.starts_with(parent)
}

fn uncached(n_input: Option<i64>, n_cache: Option<i64>) -> Option<i64> {
    let i = n_input?;
    Some(match n_cache {
        Some(c) => (i - c).max(0),
        None => i,
    })
}

fn num_or_null<T: Into<Value>>(o: Option<T>) -> Value {
    o.map_or(Value::Null, Into::into)
}

fn paginate(items: Vec<Value>, page: i64, page_size: i64) -> Value {
    let total = items.len() as i64;
    let total_pages = if total > 0 { (total + page_size - 1) / page_size } else { 0 };
    let start = ((page - 1) * page_size).min(total) as usize;
    let page_items: Vec<Value> = items.into_iter().skip(start).take(page_size as usize).collect();
    json!({"items": page_items, "total": total, "page": page, "page_size": page_size, "total_pages": total_pages})
}

/// `Counter` -> sorted `[FilterOption]`.
fn filter_options(counts: BTreeMap<String, i64>) -> Value {
    Value::Array(counts.into_iter().map(|(v, c)| json!({"value": v, "count": c})).collect())
}

/// upstream's `metadata_filter_value`: encodes None without colliding with real values.
fn metadata_filter_value(v: Option<&str>) -> String {
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
fn serve_file(full: &Path) -> Res {
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
        return Ok(Resp { status: 200, ctype: m.into(), body, headers: vec![("Content-Disposition", format!("attachment; filename=\"{}\"", name_of(full)))] });
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
fn scan_dir(dir: &Path, base: &str, with_dirs: bool, skip_top_manifest: bool, out: &mut Vec<Value>) {
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

// ---------------------------------------------------------------- model loading (pydantic validate + dump)

/// `JobConfig.model_validate(...)` dumped in full (`model_dump(mode="json")`).
fn job_config_value(raw: Value) -> Option<Value> {
    let mut raw = raw;
    // scanner._normalize_viewer_job_config: numeric dataset refs -> strings.
    if let Some(ds) = raw.get_mut("datasets").and_then(Value::as_array_mut) {
        for d in ds.iter_mut().filter_map(Value::as_object_mut) {
            for k in ["version", "ref"] {
                if let Some(Value::Number(n)) = d.get(k) {
                    let s = n.to_string();
                    d.insert(k.into(), Value::String(s));
                }
            }
        }
    }
    crate::config::migrate_legacy(&mut raw);
    let cfg: crate::config::JobConfig = serde_json::from_value(raw).ok()?;
    serde_json::to_value(cfg).ok()
}

fn job_config(job_dir: &Path) -> Option<Value> {
    job_config_value(read_json(&job_dir.join("config.json"))?)
}

/// `JobResult` as pydantic loads it: required fields present, `updated_at` derived, `trial_results` defaulted.
fn job_result(job_dir: &Path) -> Option<Value> {
    let mut r = read_json_object(&job_dir.join("result.json"))?;
    let o = r.as_object_mut()?;
    let status = o.remove("status");
    if o.get("updated_at").is_none_or(Value::is_null) {
        let from_status = status.as_ref().and_then(|s| s.get("updated_at")).filter(|v| truthy(v)).cloned();
        let v = from_status.or_else(|| o.get("finished_at").filter(|v| truthy(v)).cloned()).or_else(|| o.get("started_at").cloned()).unwrap_or(Value::Null);
        o.insert("updated_at".into(), v);
    }
    if !(o.contains_key("id") && o.contains_key("started_at") && o.contains_key("n_total_trials")) {
        return None;
    }
    let stats = o.get("stats").cloned().unwrap_or(json!({}));
    o.insert("stats".into(), job_stats(&stats));
    let mut out = Map::new();
    for k in ["id", "started_at", "updated_at", "finished_at", "n_total_trials", "stats"] {
        out.insert(k.into(), o.get(k).cloned().unwrap_or(Value::Null));
    }
    out.insert("trial_results".into(), o.get("trial_results").cloned().unwrap_or(json!([])));
    let mut out = Value::Object(out);
    norm_dts(&mut out);
    Some(out)
}

/// `JobStats` with defaults filled and legacy `n_trials`/`n_errors` migrated.
fn job_stats(s: &Value) -> Value {
    let g = |k: &str| s.get(k).cloned();
    let int = |k: &str, legacy: Option<&str>| g(k).or_else(|| legacy.and_then(|l| g(l))).unwrap_or(json!(0));
    let evals: Map<String, Value> = s
        .get("evals")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, e)| {
                    let d = |f: &str, def: Value| e.get(f).cloned().unwrap_or(def);
                    (
                        k.clone(),
                        json!({"n_trials": d("n_trials", json!(0)), "n_errors": d("n_errors", json!(0)), "metrics": d("metrics", json!([])), "pass_at_k": d("pass_at_k", json!({})),
                               "reward_stats": d("reward_stats", json!({})), "exception_stats": d("exception_stats", json!({}))}),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "n_completed_trials": int("n_completed_trials", Some("n_trials")),
        "n_errored_trials": int("n_errored_trials", Some("n_errors")),
        "n_running_trials": int("n_running_trials", None),
        "n_pending_trials": int("n_pending_trials", None),
        "n_cancelled_trials": int("n_cancelled_trials", None),
        "n_retries": int("n_retries", None),
        "evals": evals,
        "n_input_tokens": g("n_input_tokens").unwrap_or(Value::Null),
        "n_cache_tokens": g("n_cache_tokens").unwrap_or(Value::Null),
        "n_output_tokens": g("n_output_tokens").unwrap_or(Value::Null),
        "cost_usd": g("cost_usd").unwrap_or(Value::Null),
    })
}

/// A trial result that validates as upstream's `TrialResult` (required fields present); the file is its dump.
fn trial_result(trial_dir: &Path) -> Option<Value> {
    let mut r = read_json_object(&trial_dir.join("result.json"))?;
    norm_dts(&mut r);
    ["id", "task_name", "trial_name", "trial_uri", "task_id", "task_checksum", "config", "agent_info"].iter().all(|k| r.get(k).is_some()).then_some(r)
}

/// `TrialConfig.model_validate_json(config.json)`: pruned defaults restored.
fn trial_config(trial_dir: &Path) -> Option<crate::config::TrialConfig> {
    let mut v = read_json_object(&trial_dir.join("config.json"))?;
    let o = v.as_object_mut()?;
    o.entry("trial_name").or_insert(json!(""));
    o.entry("trials_dir").or_insert(json!("trials"));
    serde_json::from_value(v).ok()
}

fn agent_name_from_config(agent: &Value) -> String {
    match (agent.get("name").and_then(s), agent.get("import_path").and_then(s)) {
        (Some(n), _) if !n.is_empty() => n.into(),
        (_, Some(ip)) if !ip.is_empty() => ip.into(),
        (None, None) => "oracle".into(),
        _ => "unknown".into(),
    }
}

fn agent_name_from_result(r: &Value) -> String {
    let agent = &r["config"]["agent"];
    if agent.get("import_path").is_some_and(truthy) { agent_name_from_config(agent) } else { r["agent_info"]["name"].as_str().unwrap_or_default().into() }
}

fn reasoning_effort(config: &Value) -> Option<String> {
    config["agent"]["kwargs"].get("reasoning_effort").and_then(s).filter(|x| !x.is_empty()).map(String::from)
}

/// `model_info_from_model_name`: (provider, name).
fn split_model(model: Option<&str>) -> Option<(Option<String>, String)> {
    let m = model.filter(|m| !m.is_empty())?;
    Some(match m.split_once('/') {
        Some((p, n)) => (Some(p.into()), n.into()),
        None => (None, m.into()),
    })
}

fn task_name_from_config(task: &Value) -> String {
    if let Some(n) = task.get("name").and_then(s).filter(|n| !n.is_empty()) {
        return n.into();
    }
    // LocalTaskId/GitTaskId.get_name(): the path's (resolved, for local tasks) final component.
    let p = PathBuf::from(task.get("path").and_then(s).unwrap_or_default());
    let p = if task.get("git_url").is_some_and(truthy) { p } else { std::path::absolute(&p).map(|a| a.canonicalize().unwrap_or(a)).unwrap_or(p) };
    name_of(&p)
}

/// `TrialResult.compute_token_cost_totals()`.
fn token_totals(r: &Value) -> (Option<i64>, Option<i64>, Option<i64>, Option<f64>) {
    let ctxs: Vec<&Value> = if r.get("agent_result").is_some_and(|a| !a.is_null()) {
        vec![&r["agent_result"]]
    } else {
        r.get("step_results").and_then(Value::as_array).map(|s| s.iter().map(|x| &x["agent_result"]).filter(|a| !a.is_null()).collect()).unwrap_or_default()
    };
    let sum = |k: &str| ctxs.iter().filter_map(|c| c.get(k).and_then(Value::as_i64)).fold(None, |acc: Option<i64>, x| Some(acc.unwrap_or(0) + x));
    let cost = ctxs.iter().filter_map(|c| c.get("cost_usd").and_then(Value::as_f64)).fold(None, |acc: Option<f64>, x| Some(acc.unwrap_or(0.0) + x));
    (sum("n_input_tokens"), sum("n_cache_tokens"), sum("n_output_tokens"), cost)
}

/// Numeric (non-bool) rewards as floats.
fn numeric_rewards(rewards: &Value) -> Vec<(String, f64)> {
    rewards.as_object().map(|m| m.iter().filter_map(|(k, v)| if v.is_number() { v.as_f64().map(|f| (k.clone(), f)) } else { None }).collect()).unwrap_or_default()
}

/// `evals_from_rewards`: one `{key: {metrics: [{key: value}]}}` per numeric reward, `reward` first.
fn evals_from_rewards(rewards: Vec<(String, f64)>) -> Value {
    let mut numeric = rewards;
    numeric.sort_by(|a, b| (a.0 != "reward", &a.0).cmp(&(b.0 != "reward", &b.0)));
    Value::Object(numeric.into_iter().map(|(k, v)| (k.clone(), json!({"metrics": [{k: v}]}))).collect())
}

// ---------------------------------------------------------------- jobs mode

impl App {
    /// `JobScanner.list_jobs()`: every subdirectory, newest (reverse-sorted) first.
    fn list_jobs(&self) -> Vec<String> {
        let mut v: Vec<String> = sorted_entries(&self.folder).into_iter().filter(|p| p.is_dir()).map(|p| name_of(&p)).collect();
        v.reverse();
        v
    }

    fn list_trials(&self, job: &str) -> Vec<String> {
        let dir = self.folder.join(job);
        if job.is_empty() || job.contains('/') || !dir.is_dir() {
            return vec![];
        }
        sorted_entries(&dir).into_iter().filter(|d| d.is_dir() && (d.join("config.json").exists() || d.join("result.json").exists())).map(|p| name_of(&p)).collect()
    }

    fn validate_job(&self, job: &str) -> std::result::Result<PathBuf, Err> {
        let d = resolve(&self.folder, job);
        if job.contains('/') || !strictly_under(&d, &self.folder) {
            return Err(err(400, "Invalid job name"));
        }
        Ok(d)
    }

    fn validate_trial(&self, job: &str, trial: &str) -> std::result::Result<PathBuf, Err> {
        let jd = self.validate_job(job)?;
        let d = resolve(&jd, trial);
        if trial.contains('/') || !strictly_under(&d, &jd) {
            return Err(err(400, "Invalid trial name"));
        }
        Ok(d)
    }

    fn existing_trial(&self, job: &str, trial: &str) -> std::result::Result<PathBuf, Err> {
        let d = self.validate_trial(job, trial)?;
        if !d.exists() {
            return Err(err(404, format!("Trial '{trial}' not found in job '{job}'")));
        }
        Ok(d)
    }

    fn existing_job(&self, job: &str) -> std::result::Result<PathBuf, Err> {
        let d = self.validate_job(job)?;
        if !d.exists() {
            return Err(err(404, format!("Job '{job}' not found")));
        }
        Ok(d)
    }

    fn require_listed_job(&self, job: &str) -> std::result::Result<(), Err> {
        if self.list_jobs().iter().any(|j| j == job) { Ok(()) } else { Err(err(404, format!("Job '{job}' not found"))) }
    }

    fn job_summaries(&self) -> Vec<Value> {
        let mut out: Vec<(Option<f64>, Value)> = vec![];
        for name in self.list_jobs() {
            let dir = self.folder.join(&name);
            let result = job_result(&dir);
            let config = job_config(&dir);
            let (mut agents, mut providers, mut models, mut sources): (Vec<String>, Vec<String>, Vec<String>, Vec<String>) = Default::default();
            let mut environment_type = Value::Null;
            if let Some(c) = &config {
                let mut src: HashSet<String> = HashSet::new();
                for d in c["datasets"].as_array().into_iter().flatten() {
                    let local = d.get("path").is_some_and(|p| !p.is_null()) && d.get("repo").is_none_or(Value::is_null);
                    if local {
                        let p = d["path"].as_str().unwrap_or_default();
                        src.insert(path_label(p));
                    } else if let Some(n) = d.get("name").and_then(s) {
                        src.insert(n.into());
                    }
                }
                for t in c["tasks"].as_array().into_iter().flatten() {
                    if let Some(n) = t.get("name").and_then(s) {
                        src.insert(n.into());
                    } else if let Some(p) = t.get("path").and_then(s) {
                        src.insert(path_label(p));
                    }
                }
                sources = src.into_iter().collect();
                sources.sort();
                let mut a: Vec<String> = c["agents"].as_array().into_iter().flatten().filter_map(|a| a.get("name").and_then(s).map(String::from)).collect();
                a.sort();
                a.dedup();
                agents = a;
                for ag in c["agents"].as_array().into_iter().flatten() {
                    if let Some(m) = ag.get("model_name").and_then(s).filter(|m| !m.is_empty()) {
                        match m.split_once('/') {
                            Some((p, n)) => {
                                providers.push(p.into());
                                models.push(n.into());
                            }
                            None => models.push(m.into()),
                        }
                    }
                }
                providers.sort();
                providers.dedup();
                models.sort();
                models.dedup();
                environment_type = c["environment"]["type"].clone();
            }
            let summary = match &result {
                Some(r) => {
                    let st = &r["stats"];
                    let evals: Map<String, Value> =
                        st["evals"].as_object().into_iter().flatten().filter(|(_, e)| truthy(&e["metrics"])).map(|(k, e)| (k.clone(), json!({"metrics": e["metrics"]}))).collect();
                    json!({
                        "name": name, "id": r["id"], "started_at": r["started_at"], "updated_at": r["updated_at"], "finished_at": r["finished_at"],
                        "n_total_trials": r["n_total_trials"], "n_completed_trials": st["n_completed_trials"], "n_errored_trials": st["n_errored_trials"],
                        "datasets": sources, "agents": agents, "providers": providers, "models": models, "environment_type": environment_type, "evals": evals,
                        "total_input_tokens": num_or_null(uncached(st["n_input_tokens"].as_i64(), st["n_cache_tokens"].as_i64())),
                        "total_cached_input_tokens": st["n_cache_tokens"], "total_output_tokens": st["n_output_tokens"], "total_cost_usd": st["cost_usd"],
                    })
                }
                None => json!({
                    "name": name, "id": null, "started_at": null, "updated_at": null, "finished_at": null, "n_total_trials": 0, "n_completed_trials": 0,
                    "n_errored_trials": 0, "datasets": sources, "agents": agents, "providers": providers, "models": models, "environment_type": environment_type,
                    "evals": {}, "total_input_tokens": null, "total_cached_input_tokens": null, "total_output_tokens": null, "total_cost_usd": null,
                }),
            };
            out.push((parse_dt(&summary["started_at"]).map(|d| ts(&d)), summary));
        }
        // Most recent first; jobs without started_at last (stable, like Python's reverse sort).
        out.sort_by(|a, b| (b.0.is_some(), b.0.unwrap_or(0.0)).partial_cmp(&(a.0.is_some(), a.0.unwrap_or(0.0))).unwrap_or(std::cmp::Ordering::Equal));
        out.into_iter().map(|x| x.1).collect()
    }

    fn task_summaries(&self, job: &str) -> Vec<Value> {
        #[derive(Default)]
        struct G {
            n_trials: i64,
            n_completed: i64,
            n_errors: i64,
            exception_types: Vec<String>,
            total_reward: f64,
            reward_count: i64,
            reward_totals: Vec<(String, f64, i64)>,
            total_duration_ms: f64,
            duration_count: i64,
            total_tokens: i64,
            tokens_count: i64,
            totals: [(i64, i64); 3],
            total_cost: f64,
            cost_count: i64,
        }
        type Key = (Option<String>, Option<String>, Option<String>, Option<String>, String);
        let mut groups: Vec<(Key, G)> = vec![];
        let mut slot = |k: Key| -> usize {
            match groups.iter().position(|(x, _)| *x == k) {
                Some(i) => i,
                None => {
                    groups.push((k, G::default()));
                    groups.len() - 1
                }
            }
        };
        let mut entries: Vec<(usize, Option<Value>)> = vec![];
        for name in self.list_trials(job) {
            let dir = self.folder.join(job).join(&name);
            match trial_result(&dir) {
                Some(r) => {
                    let mi = &r["agent_info"]["model_info"];
                    let key = (
                        Some(agent_name_from_result(&r)),
                        mi.get("provider").and_then(s).map(String::from),
                        mi.get("name").and_then(s).map(String::from),
                        r.get("source").and_then(s).map(String::from),
                        r["task_name"].as_str().unwrap_or_default().to_string(),
                    );
                    entries.push((slot(key), Some(r)));
                }
                None => {
                    let Some(cfg) = trial_config(&dir) else { continue };
                    let c = serde_json::to_value(&cfg).unwrap_or_default();
                    let mi = split_model(cfg.agent.model_name.as_deref());
                    let key = (
                        Some(agent_name_from_config(&c["agent"])),
                        mi.as_ref().and_then(|m| m.0.clone()),
                        mi.map(|m| m.1),
                        cfg.task.source.clone(),
                        task_name_from_config(&c["task"]),
                    );
                    entries.push((slot(key), None));
                }
            }
        }
        for (i, r) in entries {
            let g = &mut groups[i].1;
            g.n_trials += 1;
            let Some(r) = r else { continue };
            let finished = parse_dt(&r["finished_at"]);
            if let Some(f) = finished {
                g.n_completed += 1;
                if let Some(st) = parse_dt(&r["started_at"]) {
                    g.total_duration_ms += (f - st).num_microseconds().unwrap_or(0) as f64 / 1e6 * 1000.0;
                    g.duration_count += 1;
                }
            }
            if let Some(t) = r["exception_info"].get("exception_type").and_then(s) {
                g.n_errors += 1;
                if !g.exception_types.iter().any(|x| x == t) {
                    g.exception_types.push(t.into());
                }
            }
            if finished.is_some() {
                let rewards = r["verifier_result"].get("rewards").filter(|v| truthy(v)).cloned().unwrap_or(json!({}));
                g.total_reward += rewards.get("reward").and_then(Value::as_f64).unwrap_or(0.0);
                g.reward_count += 1;
                for (k, v) in numeric_rewards(&rewards) {
                    match g.reward_totals.iter_mut().find(|x| x.0 == k) {
                        Some(x) => {
                            x.1 += v;
                            x.2 += 1;
                        }
                        None => g.reward_totals.push((k, v, 1)),
                    }
                }
            }
            let (ni, nc, no, cost) = token_totals(&r);
            let unc = uncached(ni, nc);
            if let (true, Some(u), Some(c), Some(o)) = (finished.is_some(), unc, nc, no) {
                g.total_tokens += u + c + o;
                g.tokens_count += 1;
            }
            for (slot, v) in g.totals.iter_mut().zip([unc, nc, no]) {
                if let Some(v) = v {
                    slot.0 += v;
                    slot.1 += 1;
                }
            }
            if let Some(c) = cost {
                g.total_cost += c;
                g.cost_count += 1;
            }
        }
        let avg = |t: f64, n: i64| if n > 0 { json!(t / n as f64) } else { Value::Null };
        groups
            .into_iter()
            .map(|((agent, provider, model, source, task), mut g)| {
                let avg_reward = if g.n_completed < g.n_trials {
                    Value::Null
                } else if g.reward_count > 0 {
                    json!(g.total_reward / g.reward_count as f64)
                } else {
                    json!(0.0)
                };
                g.exception_types.sort();
                let evals = evals_from_rewards(g.reward_totals.iter().map(|(k, t, n)| (k.clone(), t / *n as f64)).collect());
                json!({
                    "task_name": task, "source": source, "agent_name": agent, "model_provider": provider, "model_name": model,
                    "n_trials": g.n_trials, "n_completed": g.n_completed, "n_errors": g.n_errors, "exception_types": g.exception_types,
                    "avg_reward": avg_reward, "evals": evals, "avg_duration_ms": avg(g.total_duration_ms, g.duration_count),
                    "avg_tokens": avg(g.total_tokens as f64, g.tokens_count),
                    "avg_input_tokens": avg(g.totals[0].0 as f64, g.totals[0].1), "avg_cached_input_tokens": avg(g.totals[1].0 as f64, g.totals[1].1),
                    "avg_output_tokens": avg(g.totals[2].0 as f64, g.totals[2].1), "avg_cost_usd": avg(g.total_cost, g.cost_count),
                })
            })
            .collect()
    }

    fn trial_summaries(&self, job: &str) -> std::result::Result<Vec<Value>, Err> {
        self.require_listed_job(job)?;
        let mut out = vec![];
        for name in self.list_trials(job) {
            let dir = self.folder.join(job).join(&name);
            let Some(r) = trial_result(&dir) else {
                let Some(cfg) = trial_config(&dir) else { continue };
                let c = serde_json::to_value(&cfg).unwrap_or_default();
                let mi = split_model(cfg.agent.model_name.as_deref());
                out.push(json!({
                    "name": name, "task_name": task_name_from_config(&c["task"]), "id": c["job_id"], "source": cfg.task.source,
                    "agent_name": agent_name_from_config(&c["agent"]), "reported_agent_name": null, "reasoning_effort": reasoning_effort(&c),
                    "model_provider": mi.as_ref().and_then(|m| m.0.clone()), "model_name": mi.map(|m| m.1), "reward": null, "evals": {},
                    "error_type": null, "started_at": null, "finished_at": null, "input_tokens": null, "cached_input_tokens": null,
                    "output_tokens": null, "cost_usd": null,
                }));
                continue;
            };
            let mi = &r["agent_info"]["model_info"];
            let evals = evals_from_rewards(numeric_rewards(&r["verifier_result"]["rewards"]));
            let reward = evals.as_object().and_then(|m| m.values().next()).map(|e| e["metrics"][0].as_object().and_then(|m| m.values().next().cloned()).unwrap_or(Value::Null)).unwrap_or(Value::Null);
            let (ni, nc, no, cost) = token_totals(&r);
            out.push(json!({
                "name": name, "task_name": r["task_name"], "id": r["id"], "source": r.get("source").cloned().unwrap_or(Value::Null),
                "agent_name": agent_name_from_result(&r), "reported_agent_name": r["agent_info"]["name"], "reasoning_effort": reasoning_effort(&r["config"]),
                "model_provider": mi.get("provider").cloned().unwrap_or(Value::Null), "model_name": mi.get("name").cloned().unwrap_or(Value::Null),
                "reward": reward, "evals": evals, "error_type": r["exception_info"].get("exception_type").cloned().unwrap_or(Value::Null),
                "started_at": r.get("started_at").cloned().unwrap_or(Value::Null), "finished_at": r.get("finished_at").cloned().unwrap_or(Value::Null),
                "input_tokens": num_or_null(uncached(ni, nc)), "cached_input_tokens": num_or_null(nc), "output_tokens": num_or_null(no), "cost_usd": num_or_null(cost),
            }));
        }
        Ok(out)
    }

    fn step_root(&self, trial_dir: &Path, step: Option<&str>) -> std::result::Result<PathBuf, Err> {
        let Some(step) = step else { return Ok(trial_dir.to_path_buf()) };
        let d = resolve(&trial_dir.join("steps"), step);
        if step.contains('/') || !strictly_under(&d, trial_dir) {
            return Err(err(400, "Invalid step name"));
        }
        if !d.exists() {
            return Err(err(404, format!("Step '{step}' not found")));
        }
        Ok(d)
    }

    fn jobs_get(&self, segs: &[&str], q: &Query, req: &Req) -> Res {
        let step = q.get("step");
        match segs {
            ["jobs"] => {
                let page = q.int("page", 1, Some(1), None)?;
                let page_size = q.int("page_size", 100, Some(1), Some(100))?;
                let mut items = self.job_summaries();
                let lower = |v: &Value| v.as_str().unwrap_or_default().to_lowercase();
                let any_in = |v: &Value, needle: &str| v.as_array().into_iter().flatten().any(|x| lower(x).contains(needle));
                if let Some(qs) = q.get("q").filter(|x| !x.is_empty()) {
                    let n = qs.to_lowercase();
                    items.retain(|s| lower(&s["name"]).contains(&n) || ["datasets", "agents", "providers", "models"].iter().any(|k| any_in(&s[*k], &n)));
                }
                for (param, field) in [("agent", "agents"), ("provider", "providers"), ("model", "models")] {
                    let want = q.all(param);
                    if !want.is_empty() {
                        items.retain(|s| s[field].as_array().into_iter().flatten().any(|x| want.iter().any(|w| x.as_str() == Some(w))));
                    }
                }
                let dates = q.all("date");
                if !dates.is_empty() {
                    let now = chrono::Local::now().naive_local();
                    let cutoffs: Vec<NaiveDateTime> = dates
                        .iter()
                        .filter_map(|d| match d.as_str() {
                            "today" => Some(now - chrono::Duration::days(1)),
                            "week" => Some(now - chrono::Duration::weeks(1)),
                            "month" => Some(now - chrono::Duration::days(30)),
                            _ => None,
                        })
                        .collect();
                    if let Some(cutoff) = cutoffs.into_iter().min() {
                        // `started_at.replace(tzinfo=None) >= cutoff`: the timestamp's own wall clock vs local now.
                        let wall = |t: &str| DateTime::parse_from_rfc3339(t).map(|d| d.naive_local()).ok().or_else(|| NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f").ok());
                        items.retain(|s| s["started_at"].as_str().and_then(wall).is_some_and(|d| d >= cutoff));
                    }
                }
                ok_json(paginate(items, page, page_size))
            }
            ["jobs", "filters"] => {
                let (mut a, mut p, mut m) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
                for sm in self.job_summaries() {
                    for (field, counts) in [("agents", &mut a), ("providers", &mut p), ("models", &mut m)] {
                        for v in sm[field].as_array().into_iter().flatten().filter_map(Value::as_str) {
                            *counts.entry(v.to_string()).or_insert(0) += 1;
                        }
                    }
                }
                ok_json(json!({"agents": filter_options(a), "providers": filter_options(p), "models": filter_options(m)}))
            }
            ["jobs", job] => {
                let dir = self.existing_job(job)?;
                match job_result(&dir) {
                    Some(mut r) => {
                        r["job_uri"] = json!(as_uri(&dir));
                        ok_json(r)
                    }
                    None => {
                        let n = sorted_entries(&dir).iter().filter(|d| d.is_dir()).count();
                        ok_json(json!({
                            "id": job, "started_at": null, "updated_at": null, "finished_at": null, "n_total_trials": n,
                            "stats": job_stats(&json!({"n_pending_trials": n})), "job_uri": as_uri(&dir),
                        }))
                    }
                }
            }
            ["jobs", job, "analysis"] => {
                let dir = self.existing_job(job)?;
                let p = dir.join("analysis.json");
                if !p.exists() {
                    return ok_json(json!({}));
                }
                ok_json(read_json(&p).ok_or_else(|| err(500, "Error reading analysis.json"))?)
            }
            ["jobs", job, "upload"] => {
                let dir = self.existing_job(job)?;
                if !dir.join("result.json").exists() {
                    return ok_json(json!({"status": "in_progress", "job_id": null, "view_url": null}));
                }
                // upstream Hub isn't reachable from hbench: report what upstream reports when signed out.
                ok_json(match job_result(&dir) {
                    Some(r) => json!({"status": "unauthenticated", "job_id": r["id"], "view_url": null}),
                    None => json!({"status": "unknown", "job_id": null, "view_url": null}),
                })
            }
            ["jobs", job, "config"] => {
                let cfg = (!job.contains('/')).then(|| job_config(&self.folder.join(job))).flatten();
                ok_json(cfg.ok_or_else(|| err(404, format!("Config for job '{job}' not found")))?)
            }
            ["jobs", job, "tasks", "filters"] => {
                self.require_listed_job(job)?;
                let mut counts: [BTreeMap<String, i64>; 5] = Default::default();
                for sm in self.task_summaries(job) {
                    for (i, k) in ["agent_name", "model_provider", "model_name"].iter().enumerate() {
                        *counts[i].entry(metadata_filter_value(sm[*k].as_str())).or_insert(0) += 1;
                    }
                    if let Some(t) = sm["task_name"].as_str().filter(|t| !t.is_empty()) {
                        *counts[3].entry(t.into()).or_insert(0) += 1;
                    }
                    *counts[4].entry(sm["source"].as_str().unwrap_or("adhoc").into()).or_insert(0) += 1;
                }
                let mut efforts = BTreeMap::new();
                for name in self.list_trials(job) {
                    let dir = self.folder.join(job).join(&name);
                    let cfg = trial_result(&dir).map(|r| r["config"].clone()).or_else(|| trial_config(&dir).and_then(|c| serde_json::to_value(c).ok()));
                    if let Some(c) = cfg {
                        *efforts.entry(metadata_filter_value(reasoning_effort(&c).as_deref())).or_insert(0) += 1;
                    }
                }
                let [a, p, m, t, src] = counts;
                ok_json(json!({
                    "agents": filter_options(a), "providers": filter_options(p), "models": filter_options(m), "tasks": filter_options(t),
                    "sources": filter_options(src), "reasoning_efforts": filter_options(efforts),
                }))
            }
            ["jobs", job, "tasks"] => {
                let page = q.int("page", 1, Some(1), None)?;
                let page_size = q.int("page_size", 100, Some(1), Some(100))?;
                self.require_listed_job(job)?;
                let mut items = self.task_summaries(job);
                if let Some(qs) = q.get("q").filter(|x| !x.is_empty()) {
                    let n = qs.to_lowercase();
                    items.retain(|s| ["task_name", "agent_name", "model_provider", "model_name", "source"].iter().any(|k| s[*k].as_str().is_some_and(|v| v.to_lowercase().contains(&n))));
                }
                for (param, field) in [("agent", "agent_name"), ("provider", "model_provider"), ("model", "model_name")] {
                    let want = q.all(param);
                    if !want.is_empty() {
                        items.retain(|s| want.contains(&metadata_filter_value(s[field].as_str())));
                    }
                }
                let tasks = q.all("task");
                if !tasks.is_empty() {
                    items.retain(|s| s["task_name"].as_str().is_some_and(|t| tasks.iter().any(|x| x == t)));
                }
                if let Some(by) = q.get("sort_by").filter(|x| !x.is_empty()) {
                    sort_task_summaries(&mut items, by, q.get("sort_order") == Some("desc"));
                }
                ok_json(paginate(items, page, page_size))
            }
            ["jobs", job, "chart-trials"] => ok_json(Value::Array(self.trial_summaries(job)?)),
            ["jobs", job, "trials"] => {
                let page = q.int("page", 1, Some(1), None)?;
                let page_size = q.int("page_size", 100, Some(1), Some(100))?;
                let mut items = self.trial_summaries(job)?;
                if let Some(qs) = q.get("q").filter(|x| !x.is_empty()) {
                    let n = qs.to_lowercase();
                    items.retain(|s| ["name", "task_name", "agent_name", "model_provider", "model_name", "source"].iter().any(|k| s[*k].as_str().is_some_and(|v| v.to_lowercase().contains(&n))));
                }
                for (param, field) in [("reasoning_effort", "reasoning_effort"), ("agent", "agent_name"), ("provider", "model_provider"), ("model", "model_name")] {
                    let want = q.all(param);
                    if !want.is_empty() {
                        items.retain(|s| want.contains(&metadata_filter_value(s[field].as_str())));
                    }
                }
                let tasks = q.all("task");
                if !tasks.is_empty() {
                    items.retain(|s| s["task_name"].as_str().is_some_and(|t| tasks.iter().any(|x| x == t)));
                }
                let sources = q.all("source");
                if !sources.is_empty() {
                    items.retain(|s| sources.iter().any(|x| x == s["source"].as_str().unwrap_or("adhoc")));
                }
                if let Some(by) = q.get("sort_by").filter(|x| !x.is_empty()) {
                    sort_trial_summaries(&mut items, by, q.get("sort_order") == Some("desc"));
                }
                ok_json(paginate(items, page, page_size))
            }
            ["jobs", job, "trials", trial] => {
                if let Some(r) = (!job.contains('/') && !trial.contains('/')).then(|| trial_result(&self.folder.join(job).join(trial))).flatten() {
                    return ok_json(r);
                }
                let not_found = || err(404, format!("Trial '{trial}' not found in job '{job}'"));
                let raw_dir = self.folder.join(job).join(trial);
                let cfg = (!job.contains('/') && !trial.contains('/')).then(|| trial_config(&raw_dir)).flatten().ok_or_else(not_found)?;
                let dir = self.validate_trial(job, trial)?;
                ok_json(partial_trial_result(trial, &dir, &cfg))
            }
            ["jobs", job, "trials", trial, "navigation"] => {
                let names = self.list_trials(job);
                let i = names.iter().position(|n| n == trial).ok_or_else(|| err(404, "Trial not found"))?;
                let finished = job_result(&self.folder.join(job)).is_some_and(|r| truthy(&r["finished_at"]));
                ok_json(json!({
                    "previous": if i > 0 { json!(names[i - 1]) } else { Value::Null },
                    "next": names.get(i + 1).map_or(Value::Null, |n| json!(n)),
                    "index": i, "total": names.len(), "finished": finished,
                }))
            }
            ["jobs", job, "trials", trial, "trajectory"] => {
                let dir = self.existing_trial(job, trial)?;
                let agent = self.step_root(&dir, step)?.join("agent");
                let p = agent.join("trajectory.json");
                if !p.exists() {
                    // hbench extra: older claude-code runs only kept the native session; convert it like `hbench traces`.
                    return ok_json(crate::atif::claude_code_trajectory(&agent).unwrap_or(Value::Null));
                }
                ok_json(read_json(&p).ok_or_else(|| err(500, "Failed to parse trajectory.json"))?)
            }
            ["jobs", job, "trials", trial, "interaction"] => {
                let dir = self.existing_trial(job, trial)?;
                ok_json(interaction(&self.step_root(&dir, step)?)?)
            }
            ["jobs", job, "trials", trial, "verifier-output"] => {
                let dir = self.existing_trial(job, trial)?;
                let v = self.step_root(&dir, step)?.join("verifier");
                let text = |n: &str| match read_text(&v.join(n)) {
                    Ok(t) => str_or_null(t.as_deref()),
                    Err(_) => json!("[Error reading file]"),
                };
                ok_json(json!({
                    "stdout": text("test-stdout.txt"), "stderr": text("test-stderr.txt"), "ctrf": text("ctrf.json"),
                    "reward": read_json_object(&v.join("reward.json")), "reward_details": read_json_object(&v.join("reward-details.json")),
                }))
            }
            ["jobs", job, "trials", trial, "files"] => {
                let dir = self.existing_trial(job, trial)?;
                let mut out = vec![];
                scan_dir(&self.step_root(&dir, step)?, "", true, false, &mut out);
                ok_json(Value::Array(out))
            }
            ["jobs", job, "trials", trial, "recording"] => {
                let dir = self.existing_trial(job, trial)?;
                let p = dir.join("agent/recording.mp4");
                ok_json(if p.is_file() {
                    json!({"available": true, "file_path": "agent/recording.mp4", "media_type": "video/mp4", "size": p.metadata().map(|m| m.len()).unwrap_or(0)})
                } else {
                    json!({"available": false, "file_path": null, "media_type": null, "size": null})
                })
            }
            ["jobs", job, "trials", trial, "recording", "file"] => {
                let dir = self.existing_trial(job, trial)?;
                let p = dir.join("agent/recording.mp4");
                if !p.is_file() {
                    return Err(err(404, "Recording not found"));
                }
                let body = std::fs::read(&p).map_err(|e| err(500, e.to_string()))?;
                Ok(Resp { status: 200, ctype: "video/mp4".into(), body, headers: vec![("Content-Disposition", "inline; filename=\"recording.mp4\"".into())] })
            }
            ["jobs", job, "trials", trial, "files", rest @ ..] => {
                let dir = self.existing_trial(job, trial)?;
                let root = self.step_root(&dir, step)?;
                let full = resolve(&root, &rest.join("/"));
                if !strictly_under(&full, &dir) {
                    return Err(err(403, "Access denied"));
                }
                serve_file(&full)
            }
            ["jobs", job, "trials", trial, "artifacts"] => {
                let dir = self.existing_trial(job, trial)?;
                let a = self.step_root(&dir, step)?.join("artifacts");
                if !a.exists() {
                    return ok_json(json!({"files": [], "manifest": null}));
                }
                let mut files = vec![];
                scan_dir(&a, "", false, true, &mut files);
                ok_json(json!({"files": files, "manifest": read_json(&a.join("manifest.json"))}))
            }
            ["jobs", job, "trials", trial, "agent-logs"] => {
                let dir = self.existing_trial(job, trial)?;
                let agent = self.step_root(&dir, step)?.join("agent");
                let text = |p: &Path| match read_text(p) {
                    Ok(t) => str_or_null(t.as_deref()),
                    Err(_) => json!("[Error reading file]"),
                };
                let mut commands = vec![];
                let mut i = 0;
                while agent.join(format!("command-{i}")).exists() {
                    let p = agent.join(format!("command-{i}/stdout.txt"));
                    if p.exists() {
                        commands.push(json!({"index": i, "content": text(&p)}));
                    }
                    i += 1;
                }
                ok_json(json!({
                    "oracle": text(&agent.join("oracle.txt")), "setup": text(&agent.join("setup/stdout.txt")), "commands": commands,
                    "summary": text(&dir.join("analysis.md")), "analysis": read_json(&dir.join("analysis.json")),
                }))
            }
            ["jobs", job, "trials", trial, "stream"] => {
                let dir = self.stream_dir(job, trial, req)?;
                ok_json(json!({"state": "disconnected", "available": dir.join(STREAM_HANDLE_NAME).exists()}))
            }
            ["compare"] => {
                let jobs = q.all("job");
                if jobs.is_empty() {
                    q.required("job")?;
                }
                compare(self, &jobs)
            }
            ["run", "options"] => {
                let agents: Vec<&str> = {
                    let mut a = crate::agents::agent_names();
                    a.sort();
                    a
                };
                let kwargs: Map<String, Value> = agents.iter().map(|a| (a.to_string(), Value::Array(agent_kwarg_specs(a)))).collect();
                ok_json(json!({
                    "agents": agents, "agent_kwargs": kwargs, "environments": ENVIRONMENTS, "resource_modes": RESOURCE_MODES,
                    "defaults": serde_json::to_value(crate::config::JobConfig::default()).unwrap_or_default(), "jobs_dir": self.folder.to_string_lossy(),
                }))
            }
            ["run", "history"] => {
                let limit = q.int("limit", 50, None, None)?;
                let mut items: Vec<(std::time::SystemTime, String, Value)> = vec![];
                for entry in std::fs::read_dir(&self.folder).into_iter().flatten().flatten() {
                    let p = entry.path();
                    let cp = p.join("config.json");
                    if !p.is_dir() || !cp.exists() {
                        continue;
                    }
                    let Some(mtime) = cp.metadata().and_then(|m| m.modified()).ok() else { continue };
                    let Some(mut raw) = read_json_object(&cp) else { continue };
                    raw.as_object_mut().unwrap().entry("job_name").or_insert(json!(name_of(&p)));
                    let Some(cfg) = job_config_value(raw) else { continue };
                    items.push((mtime, name_of(&p), cfg));
                }
                items.sort_by(|a, b| b.0.cmp(&a.0));
                let n = items.len() as i64;
                let end = if limit >= 0 { limit.min(n) } else { (n + limit).max(0) };
                ok_json(Value::Array(items.into_iter().take(end as usize).map(|(_, name, config)| json!({"job_name": name, "config": config})).collect()))
            }
            ["run", "models"] => ok_json(json!({"models": serde_json::from_str::<Value>(include_str!("../assets/litellm/models.json")).unwrap_or_default()})),
            ["run", job, "status"] => {
                let job_ready = self.folder.join(job).join("result.json").exists();
                let mut runs = self.runs.lock().unwrap();
                let Some(run) = runs.get_mut(*job) else {
                    return ok_json(json!({"running": false, "returncode": null, "job_ready": job_ready, "log_tail": ""}));
                };
                let rc = run.child.try_wait().ok().flatten().map(|s| s.code().unwrap_or(-1));
                let lines: Vec<String> = std::fs::read_to_string(&run.log_path).unwrap_or_default().lines().map(String::from).collect();
                let tail = lines[lines.len().saturating_sub(40)..].join("\n");
                ok_json(json!({"running": rc.is_none(), "returncode": rc, "job_ready": job_ready, "log_tail": tail}))
            }
            _ => Err(err(404, "Not Found")),
        }
    }

    fn stream_dir(&self, job: &str, trial: &str, req: &Req) -> std::result::Result<PathBuf, Err> {
        // SSH forwards are loopback-only. The browser must run on this host.
        if !req.client_loopback {
            return Err(err(403, "Browser VS Code requires a browser on the hbench viewer host"));
        }
        let p = resolve(&self.folder, &format!("{job}/{trial}"));
        if p.parent().and_then(Path::parent) != Some(self.folder.as_path()) || !p.is_dir() {
            return Err(err(404, "Trial not found"));
        }
        Ok(p)
    }

    fn jobs_write(&self, method: &str, segs: &[&str], req: &Req) -> Res {
        let body = || -> std::result::Result<Value, Err> {
            if req.body.is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_slice(req.body).map_err(|e| (422, json!([{"type": "json_invalid", "loc": ["body", 0], "msg": format!("JSON decode error: {e}")}])))
        };
        match (method, segs) {
            ("DELETE", ["jobs", job]) => {
                let dir = self.existing_job(job)?;
                std::fs::remove_dir_all(&dir).map_err(|e| err(500, format!("Failed to delete job: {e}")))?;
                ok_json(json!({"status": "ok", "message": format!("Job '{job}' deleted")}))
            }
            ("POST", ["jobs", job, "upload"]) => {
                let dir = self.existing_job(job)?;
                if !dir.join("result.json").exists() || !dir.join("config.json").exists() {
                    return Err(err(400, format!("Job '{job}' is missing result.json / config.json — it may still be running or the run was interrupted.")));
                }
                let b = body()?;
                match b.get("visibility") {
                    None | Some(Value::Null) => {}
                    Some(v) if v == "public" || v == "private" => {}
                    Some(v) => return Err(err(400, format!("Invalid visibility {}; must be 'public', 'private', or omitted.", py_repr(v)))),
                }
                Err(err(401, NOT_AUTHENTICATED))
            }
            ("POST", ["jobs", job, "summarize"]) => {
                let dir = self.existing_job(job)?;
                let b = body()?;
                let mut args = analyze_args(&b);
                args.extend(["-n".into(), b.get("n_concurrent").and_then(Value::as_i64).unwrap_or(32).to_string()]);
                if b.get("only_failed").and_then(Value::as_bool).unwrap_or(false) {
                    args.push("--failing".into());
                }
                let report = self.run_analyze(&dir, args)?;
                let Some(report) = report else { return ok_json(json!({"n_trials_analyzed": 0})) };
                std::fs::write(dir.join("analysis.json"), serde_json::to_string_pretty(&report).unwrap()).map_err(|e| err(500, e.to_string()))?;
                let n = report["results"].as_array().into_iter().flatten().filter(|r| !truthy(&r["error"])).count();
                ok_json(json!({"n_trials_analyzed": n}))
            }
            ("POST", ["jobs", job, "trials", trial, "summarize"]) => {
                let dir = self.existing_trial(job, trial)?;
                let report = self.run_analyze(&dir, analyze_args(&body()?))?.ok_or_else(|| err(500, "analysis produced no report"))?;
                let r = &report["results"][0];
                if let Some(e) = r["error"].as_str().filter(|e| !e.is_empty()) {
                    return Err(err(500, e));
                }
                ok_json(json!({"summary": r["summary"]}))
            }
            ("POST", ["jobs", job, "trials", trial, "stream"]) => {
                self.stream_dir(job, trial, req)?;
                // hbench has no `--stream` SSH sandboxes, so there is never a valid handle to connect to.
                Err(err(400, "No valid stream handle. Run a supported environment with --stream."))
            }
            ("DELETE", ["jobs", job, "trials", trial, "stream"]) => {
                let dir = self.stream_dir(job, trial, req)?;
                ok_json(json!({"state": "disconnected", "available": dir.join(STREAM_HANDLE_NAME).exists()}))
            }
            ("POST", ["run"]) => self.launch(body()?),
            ("POST", ["run", "config.yaml"]) => {
                let b = body()?;
                if !b.is_object() {
                    return Err(err(422, "Body must be a JSON object."));
                }
                let text = serde_yaml_ng::to_string(&normalize_local_paths(b)).map_err(|e| err(500, e.to_string()))?;
                Ok(Resp { status: 200, ctype: "application/x-yaml".into(), body: text.into_bytes(), headers: vec![] })
            }
            ("POST", ["run", "pick-directory"]) => match native_pick_directory() {
                Ok(p) => ok_json(json!({"path": p})),
                Err(e) => Err(err(501, e.to_string())),
            },
            ("DELETE", ["run", job]) => {
                let mut runs = self.runs.lock().unwrap();
                let run = runs.get_mut(*job).ok_or_else(|| err(404, "No running launch for this job."))?;
                if run.child.try_wait().ok().flatten().is_some() {
                    return Err(err(404, "No running launch for this job."));
                }
                // SIGINT: hbench's graceful cancel (tears down containers), like upstream's SIGTERM.
                let _ = std::process::Command::new("kill").args(["-INT", &run.child.id().to_string()]).status();
                ok_json(json!({"stopped": true}))
            }
            _ => Err(err(405, "Method Not Allowed")),
        }
    }

    /// `hbench analyze <path>` (upstream's `run_analyze`); returns its `analysis.json` report.
    fn run_analyze(&self, path: &Path, args: Vec<String>) -> std::result::Result<Option<Value>, Err> {
        let exe = std::env::current_exe().map_err(|e| err(500, e.to_string()))?;
        let out = std::process::Command::new(exe).arg("analyze").arg(path).args(args).arg("-o").arg(&self.folder).output().map_err(|e| err(500, e.to_string()))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains("No trial directories found") || stdout.contains("trial directories found") && !out.status.success() && !stdout.contains("Report: ") {
            return Ok(None);
        }
        let report = stdout.lines().find_map(|l| l.strip_prefix("Report: ")).map(PathBuf::from).and_then(|p| read_json(&p));
        match report {
            Some(r) => Ok(Some(r)),
            None => Err(err(500, stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("analysis failed").trim().to_string())),
        }
    }

    fn launch(&self, data: Value) -> Res {
        if !data.is_object() {
            return Err(err(422, "Body must be a JSON object."));
        }
        let mut data = normalize_local_paths(data);
        data["jobs_dir"] = json!(self.folder.to_string_lossy());
        let mut raw = data.clone();
        crate::config::migrate_legacy(&mut raw);
        let cfg: crate::config::JobConfig = serde_json::from_value(raw).map_err(|e| err(422, format!("Invalid run configuration: {e}")))?;
        if cfg.datasets.is_empty() && cfg.tasks.is_empty() {
            return Err(err(422, "Specify a dataset, a task, or a local path to run."));
        }
        let job_name = cfg.job_name.clone();
        data["job_name"] = json!(job_name);
        let mut runs = self.runs.lock().unwrap();
        if self.folder.join(&job_name).exists() || runs.contains_key(&job_name) {
            return Err(err(409, format!("A job named '{job_name}' already exists.")));
        }
        let work = std::env::temp_dir().join(format!("hbench-launch-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&work).map_err(|e| err(500, e.to_string()))?;
        let config_path = work.join("config.json");
        std::fs::write(&config_path, data.to_string()).map_err(|e| err(500, e.to_string()))?;
        let log_path = work.join("launch.log");
        let log = std::fs::File::create(&log_path).map_err(|e| err(500, e.to_string()))?;
        let exe = std::env::current_exe().map_err(|e| err(500, e.to_string()))?;
        let child = std::process::Command::new(exe)
            .args(["run", "-c"])
            .arg(&config_path)
            .args(["-y", "--quiet"])
            .stdout(log.try_clone().map_err(|e| err(500, e.to_string()))?)
            .stderr(log)
            .stdin(std::process::Stdio::null())
            .spawn()
            .map_err(|e| err(500, e.to_string()))?;
        runs.insert(job_name.clone(), LaunchedRun { child, log_path });
        ok_json(json!({"job_name": job_name}))
    }
}

fn analyze_args(b: &Value) -> Vec<String> {
    let g = |k: &str, d: &str| b.get(k).and_then(s).unwrap_or(d).to_string();
    vec!["-a".into(), g("agent", "codex"), "-m".into(), g("model", "openai/gpt-6-luna"), "-e".into(), g("environment", "docker")]
}

fn py_repr(v: &Value) -> String {
    match v {
        Value::String(x) => format!("'{x}'"),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        x => x.to_string(),
    }
}

fn path_label(p: &str) -> String {
    let n = name_of(Path::new(p));
    if n.is_empty() { p.into() } else { n }
}

/// `_normalize_local_paths`: a bare dataset path that is a valid task dir becomes a `tasks` entry.
fn normalize_local_paths(mut data: Value) -> Value {
    let Some(datasets) = data.get("datasets").and_then(Value::as_array).cloned() else { return data };
    let mut remaining = vec![];
    let mut tasks: Vec<Value> = data.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default();
    for ds in datasets {
        let path = ds.get("path").and_then(s).filter(|p| !p.is_empty());
        let bare = path.is_some() && !ds.get("name").is_some_and(truthy) && !ds.get("repo").is_some_and(truthy);
        match path {
            Some(p) if bare && crate::analyze::is_valid_task_dir(Path::new(p)) => tasks.push(json!({"path": p})),
            _ => remaining.push(ds),
        }
    }
    data["datasets"] = Value::Array(remaining);
    if !tasks.is_empty() {
        data["tasks"] = Value::Array(tasks);
    }
    data
}

fn native_pick_directory() -> Result<Option<String>> {
    let run = |cmd: &str, args: &[&str]| -> Result<Option<String>> {
        let out = std::process::Command::new(cmd).args(args).output()?;
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((out.status.success() && !p.is_empty()).then_some(p))
    };
    if cfg!(target_os = "macos") {
        return run("osascript", &["-e", "POSIX path of (choose folder with prompt \"Select a dataset folder\")"]);
    }
    if cfg!(target_os = "linux") {
        let which = |c: &str| std::process::Command::new("which").arg(c).output().is_ok_and(|o| o.status.success());
        if which("zenity") {
            return run("zenity", &["--file-selection", "--directory"]);
        }
        if which("kdialog") {
            return run("kdialog", &["--getexistingdirectory"]);
        }
        bail!("Install zenity or kdialog to use the folder picker.");
    }
    bail!("No native folder picker for platform {:?}.", std::env::consts::OS)
}

/// `partial_trial_result_from_config`: an in-progress trial (config.json, no result.json).
fn partial_trial_result(trial_name: &str, dir: &Path, cfg: &crate::config::TrialConfig) -> Value {
    let c = serde_json::to_value(cfg).unwrap_or_default();
    let mi = split_model(cfg.agent.model_name.as_deref());
    let task = &c["task"];
    let task_id = match (task.get("name").and_then(s), task.get("git_url").and_then(s)) {
        (Some(n), _) if !n.contains('/') => json!({"path": n}),
        (Some(n), _) => {
            let (org, name) = n.split_once('/').unwrap();
            json!({"org": org, "name": name, "ref": task["ref"]})
        }
        (None, Some(g)) => json!({"git_url": g, "git_commit_id": task["git_commit_id"], "path": task["path"]}),
        _ => json!({"path": task["path"]}),
    };
    json!({
        "id": cfg.job_id.map_or_else(|| json!(uuid::Uuid::new_v4()), |u| json!(u)),
        "task_name": task_name_from_config(task),
        "trial_name": if cfg.trial_name.is_empty() { trial_name.to_string() } else { cfg.trial_name.clone() },
        "trial_uri": as_uri(dir), "task_id": task_id, "source": cfg.task.source, "task_checksum": "", "config": c,
        "agent_info": {"name": agent_name_from_config(&c["agent"]), "version": "", "model_info": mi.map(|(p, n)| json!({"name": n, "provider": p}))},
        "agent_result": null, "verifier_result": null, "verifier_environment_mode": null, "exception_info": null,
        "started_at": mtime_dt(&dir.join("config.json")).map(py_dt), "finished_at": null, "environment_setup": null, "agent_setup": null,
        "agent_execution": null, "verifier": null, "step_results": null,
    })
}

fn interaction(root: &Path) -> std::result::Result<Value, Err> {
    let user_traj = root.join("user-agent/trajectory.json");
    let bridge = root.join("agent/bridge-trajectory.json");
    let user_runtime = sorted_entries(&root.join("user-agent")).into_iter().find(|p| p.is_file() && *p != user_traj && p.extension().is_some_and(|e| e == "jsonl" || e == "txt"));
    let obj = |p: &Path, label: &str| -> std::result::Result<Value, Err> {
        if !p.exists() {
            return Ok(Value::Null);
        }
        let t = read_text(p).map_err(|e| err(500, format!("Failed to parse {label}: {e}")))?.unwrap_or_default();
        let v: Value = serde_json::from_str(&t).map_err(|e| err(500, format!("Failed to parse {label}: {e}")))?;
        if !v.is_object() {
            return Err(err(500, format!("Failed to parse {label}: expected a JSON object")));
        }
        Ok(v)
    };
    let user_trajectory = obj(&user_traj, "user-agent trajectory")?;
    let bridge_trajectory = obj(&bridge, "bridge trajectory export")?;
    let session = bridge_trajectory["session"]["state"]["acp_session_id"].as_str().filter(|x| !x.is_empty());
    let target_log = session.and_then(|id| {
        let mut found: Vec<PathBuf> = walk_files(&root.join("agent/sessions/projects")).into_iter().filter(|p| name_of(p) == format!("{id}.jsonl")).collect();
        found.sort();
        found.into_iter().next()
    });
    let lines = |p: &Option<PathBuf>, label: &str| -> std::result::Result<(Vec<Value>, Vec<Value>), Err> {
        let (mut events, mut errors) = (vec![], vec![]);
        let Some(p) = p else { return Ok((events, errors)) };
        let t = read_text(p).map_err(|e| err(500, format!("Failed to read {label}: {e}")))?.unwrap_or_default();
        for (i, line) in t.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(v) => events.push(v),
                Err(e) => errors.push(json!({"line_number": i + 1, "error": e.to_string(), "raw": line})),
            }
        }
        Ok((events, errors))
    };
    let (user_events, user_errors) = lines(&user_runtime, "user-agent runtime log")?;
    let (target_events, target_errors) = lines(&target_log, "target runtime log")?;
    let rel = |p: Option<&Path>| p.filter(|p| p.exists()).and_then(|p| p.strip_prefix(root).ok()).map(|r| r.to_string_lossy().replace('\\', "/"));
    Ok(json!({
        "available": !user_trajectory.is_null() || !bridge_trajectory.is_null(),
        "sources": {
            "user_trajectory": rel(Some(&user_traj)), "user_runtime": rel(user_runtime.as_deref()),
            "bridge_trajectory": rel(Some(&bridge)), "target_runtime": rel(target_log.as_deref()),
        },
        "user_trajectory": user_trajectory, "user_events": user_events, "user_parse_errors": user_errors,
        "bridge_trajectory": bridge_trajectory, "target_events": target_events, "target_parse_errors": target_errors,
    }))
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for p in sorted_entries(dir) {
        if p.is_dir() {
            out.extend(walk_files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn compare(app: &App, jobs: &[String]) -> Res {
    let existing = app.list_jobs();
    if let Some(j) = jobs.iter().find(|j| !existing.contains(j)) {
        return Err(err(404, format!("Job '{j}' not found")));
    }
    let mut tasks: Vec<(Option<String>, String)> = vec![];
    let mut ams: Vec<(String, Option<String>, Option<String>, Option<String>)> = vec![];
    let mut cells: Map<String, Value> = Map::new();
    let opt = |v: &Value| v.as_str().map(String::from);
    for job in jobs {
        for sm in app.task_summaries(job) {
            let (source, task) = (opt(&sm["source"]), sm["task_name"].as_str().unwrap_or_default().to_string());
            let (agent, provider, model) = (opt(&sm["agent_name"]), opt(&sm["model_provider"]), opt(&sm["model_name"]));
            let task_key = format!("{}::{task}", source.clone().unwrap_or_default());
            let am_key = format!("{job}::{}::{}::{}", agent.clone().unwrap_or_default(), provider.clone().unwrap_or_default(), model.clone().unwrap_or_default());
            if !tasks.contains(&(source.clone(), task.clone())) {
                tasks.push((source, task));
            }
            let am = (job.clone(), agent, provider, model);
            if !ams.contains(&am) {
                ams.push(am);
            }
            let cell = json!({
                "job_name": job, "avg_reward": sm["avg_reward"], "avg_duration_ms": sm["avg_duration_ms"], "avg_cost_usd": sm["avg_cost_usd"],
                "avg_tokens": sm["avg_tokens"], "n_trials": sm["n_trials"], "n_completed": sm["n_completed"],
            });
            cells.entry(task_key).or_insert_with(|| json!({})).as_object_mut().unwrap().insert(am_key, cell);
        }
    }
    let reward_of = |c: &Value| c["avg_reward"].as_f64().unwrap_or(0.0);
    let task_avg = |key: &str| -> f64 {
        let cs: Vec<f64> = cells.get(key).and_then(Value::as_object).map(|m| m.values().map(reward_of).collect()).unwrap_or_default();
        if cs.is_empty() { 0.0 } else { cs.iter().sum::<f64>() / cs.len() as f64 }
    };
    let mut task_list: Vec<(f64, Value)> = tasks
        .into_iter()
        .map(|(src, name)| {
            let key = format!("{}::{name}", src.clone().unwrap_or_default());
            (task_avg(&key), json!({"source": src, "task_name": name, "key": key}))
        })
        .collect();
    task_list.sort_by(|a, b| {
        (-a.0).partial_cmp(&-b.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1["source"].as_str().unwrap_or("").cmp(b.1["source"].as_str().unwrap_or(""))).then_with(|| a.1["task_name"].as_str().cmp(&b.1["task_name"].as_str()))
    });
    let mut am_list: Vec<(f64, Value)> = ams
        .into_iter()
        .map(|(job, agent, provider, model)| {
            let key = format!("{job}::{}::{}::{}", agent.clone().unwrap_or_default(), provider.clone().unwrap_or_default(), model.clone().unwrap_or_default());
            let rs: Vec<f64> = cells.values().filter_map(|tc| tc.get(&key)).map(reward_of).collect();
            let avg = if rs.is_empty() { 0.0 } else { rs.iter().sum::<f64>() / rs.len() as f64 };
            (avg, json!({"job_name": job, "agent_name": agent, "model_provider": provider, "model_name": model, "key": key}))
        })
        .collect();
    let k = |v: &Value, f: &str| v[f].as_str().unwrap_or("").to_string();
    am_list.sort_by(|a, b| {
        (-a.0)
            .partial_cmp(&-b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| [k(&a.1, "job_name"), k(&a.1, "agent_name"), k(&a.1, "model_provider"), k(&a.1, "model_name")].cmp(&[k(&b.1, "job_name"), k(&b.1, "agent_name"), k(&b.1, "model_provider"), k(&b.1, "model_name")]))
    });
    ok_json(json!({"tasks": task_list.into_iter().map(|x| x.1).collect::<Vec<_>>(), "agent_models": am_list.into_iter().map(|x| x.1).collect::<Vec<_>>(), "cells": cells}))
}

/// Python `sort(key=..., reverse=...)` (stable; reverse keeps equal items in order).
fn py_sort<K: PartialOrd>(items: &mut [Value], reverse: bool, key: impl Fn(&Value) -> K) {
    items.sort_by(|a, b| {
        let (x, y) = (key(a), key(b));
        let o = x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal);
        if reverse { o.reverse() } else { o }
    });
}

fn sort_task_summaries(items: &mut [Value], by: &str, reverse: bool) {
    let st = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    let opt = |v: &Value, k: &str| (v[k].is_null(), v[k].as_f64().unwrap_or(0.0));
    match by {
        "task_name" | "agent_name" | "model_provider" | "model_name" | "source" => py_sort(items, reverse, |v| st(v, by)),
        "n_trials" | "n_errors" => py_sort(items, reverse, |v| v[by].as_f64().unwrap_or(0.0)),
        "avg_duration_ms" | "avg_reward" | "avg_input_tokens" | "avg_cached_input_tokens" | "avg_output_tokens" | "avg_cost_usd" => py_sort(items, reverse, |v| opt(v, by)),
        "exception_types" => py_sort(items, reverse, |v| v["exception_types"][0].as_str().unwrap_or("").to_string()),
        _ => {}
    }
}

const TRIAL_SUMMARY_FIELDS: &[&str] = &[
    "name", "task_name", "id", "source", "agent_name", "reported_agent_name", "reasoning_effort", "model_provider", "model_name", "reward", "error_type",
    "started_at", "finished_at", "input_tokens", "cached_input_tokens", "output_tokens", "cost_usd",
];

/// A sortable form of a TrialSummary field (datetimes compare as instants, numbers numerically).
#[derive(PartialEq, PartialOrd)]
enum SortKey {
    Num(f64),
    Str(String),
}

fn sort_trial_summaries(items: &mut Vec<Value>, by: &str, reverse: bool) {
    let (mut present, missing): (Vec<Value>, Vec<Value>) = if by == "duration" {
        items.drain(..).partition(|s| !s["started_at"].is_null())
    } else if TRIAL_SUMMARY_FIELDS.contains(&by) {
        items.drain(..).partition(|s| !s[by].is_null())
    } else {
        return;
    };
    if by == "duration" {
        let now = ts(&Utc::now());
        py_sort(&mut present, reverse, |s| {
            let end = parse_dt(&s["finished_at"]).map_or(now, |d| ts(&d));
            end - parse_dt(&s["started_at"]).map_or(0.0, |d| ts(&d))
        });
    } else {
        py_sort(&mut present, reverse, |s| match (&s[by], by) {
            (v, "started_at" | "finished_at") => SortKey::Num(parse_dt(v).map_or(0.0, |d| ts(&d))),
            (Value::Number(n), _) => SortKey::Num(n.as_f64().unwrap_or(0.0)),
            (v, _) => SortKey::Str(v.as_str().unwrap_or_default().to_string()),
        });
    }
    *items = if reverse { missing.into_iter().chain(present).collect() } else { present.into_iter().chain(missing).collect() };
}

/// `_agent_kwarg_specs_for`: the launcher's form fields from the agent's options schema.
fn agent_kwarg_specs(name: &str) -> Vec<Value> {
    let Some(schema) = crate::schemas::agent(name).and_then(|s| serde_json::from_str::<Value>(s).ok()) else { return vec![] };
    let required: Vec<&str> = schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let mut specs = vec![];
    for (key, prop) in schema["properties"].as_object().into_iter().flatten() {
        let variants: Vec<&Value> = match prop.get("anyOf").and_then(Value::as_array) {
            Some(a) if !a.is_empty() => a.iter().collect(),
            _ => vec![prop],
        };
        let vs = variants.iter().copied().find(|v| v.get("type").and_then(s) != Some("null")).unwrap_or(prop);
        let choices = vs.get("enum").filter(|c| truthy(c));
        let kind = if choices.is_some() {
            "enum"
        } else if vs.get("$ref").is_some() || matches!(vs.get("type").and_then(s), Some("array" | "object")) {
            "json"
        } else {
            match vs.get("type").and_then(s) {
                Some("boolean") => "bool",
                Some("integer") => "int",
                Some("number") => "float",
                _ => "string",
            }
        };
        let mut spec = json!({"key": key, "kind": kind});
        for f in ["default", "description"] {
            if let Some(v) = prop.get(f) {
                spec[f] = v.clone();
            }
        }
        if let Some(c) = choices {
            spec["choices"] = c.clone();
        }
        if required.contains(&key.as_str()) {
            spec["required"] = json!(true);
        }
        specs.push(spec);
    }
    specs
}

/// LiteLLM pricing lookup (`/api/pricing`), from hbench's pinned LiteLLM price table.
fn pricing(model: &str) -> Res {
    let entry = [model, model.split_once('/').map_or(model, |x| x.1)].into_iter().find_map(crate::agents::pricing::model_cost);
    let Some(e) = entry else { return Err(err(404, format!("No pricing entry for model '{model}'"))) };
    let input = e.get("input_cost_per_token").cloned().unwrap_or(Value::Null);
    let cache = e.get("cache_read_input_token_cost").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| input.clone());
    ok_json(json!({"model_name": model, "input_cost_per_token": input, "cache_read_input_token_cost": cache, "output_cost_per_token": e.get("output_cost_per_token").cloned().unwrap_or(Value::Null)}))
}

// ---------------------------------------------------------------- tasks mode

/// `TaskConfig.model_validate_toml(...).model_dump(mode="json")`, driven by upstream's exported TaskConfig schema:
/// properties in field order, defaults (incl. default factories) filled, legacy fields migrated.
fn task_config_dump(toml_text: &str) -> Option<Value> {
    let mut data: Value = serde_json::to_value(toml::from_str::<toml::Table>(toml_text).ok()?).ok()?;
    let o = data.as_object_mut()?;
    if let Some(v) = o.remove("version") {
        o.entry("schema_version").or_insert(v);
    }
    let schema: Value = serde_json::from_str(crate::schemas::TASK).ok()?;
    let mut out = dump_model(&data, &schema, &schema)?;
    // allow_internet (deprecated) -> network_mode, unless set explicitly.
    let legacy = |env_in: Option<&Value>, env_out: &mut Value| {
        let Some(allow) = env_in.and_then(|e| e.get("allow_internet")).and_then(Value::as_bool) else { return };
        if env_in.is_some_and(|e| e.get("network_mode").is_some()) || !env_out["allowed_hosts"].is_null() {
            return;
        }
        env_out["network_mode"] = json!(if allow { "public" } else { "no-network" });
    };
    legacy(data.get("environment"), &mut out["environment"]);
    if !out["verifier"]["environment"].is_null() {
        legacy(data.get("verifier").and_then(|v| v.get("environment")), &mut out["verifier"]["environment"]);
    }
    if let Some(steps) = out["steps"].as_array_mut() {
        for (i, st) in steps.iter_mut().enumerate() {
            if !st["verifier"]["environment"].is_null() {
                legacy(data["steps"][i].get("verifier").and_then(|v| v.get("environment")), &mut st["verifier"]["environment"]);
            }
        }
    }
    Some(out)
}

fn deref<'a>(schema: &'a Value, root: &'a Value) -> &'a Value {
    match schema.get("$ref").and_then(s).and_then(|r| r.strip_prefix("#/$defs/")) {
        Some(name) => &root["$defs"][name],
        None => schema,
    }
}

fn dump_model(data: &Value, model: &Value, root: &Value) -> Option<Value> {
    let mut data = data.clone();
    if let Some(o) = data.as_object_mut() {
        // EnvironmentConfig._migrate_legacy_resource_fields
        for (old, new) in [("memory", "memory_mb"), ("storage", "storage_mb")] {
            if model["properties"].get(new).is_some()
                && let Some(Value::String(size)) = o.remove(old)
            {
                o.entry(new).or_insert(json!(parse_size_mb(&size)?));
            }
        }
        if let Some(Value::String(os)) = o.get_mut("os") {
            *os = os.to_lowercase();
        }
    }
    let mut out = Map::new();
    for (k, prop) in model["properties"].as_object()? {
        if k == "allow_internet" {
            continue; // Field(exclude=True)
        }
        let v = match data.get(k) {
            Some(v) => dump_value(v, prop, root)?,
            None => prop.get("default").cloned().unwrap_or_else(|| default_for(prop, root)),
        };
        out.insert(k.clone(), v);
    }
    Some(Value::Object(out))
}

/// Default-factory value for a schema property without an explicit `default`.
fn default_for(prop: &Value, root: &Value) -> Value {
    let p = deref(prop, root);
    if p.get("properties").is_some() {
        return dump_model(&json!({}), p, root).unwrap_or(Value::Null);
    }
    match p.get("type").and_then(s) {
        Some("array") => json!([]),
        Some("object") => json!({}),
        _ => Value::Null,
    }
}

fn dump_value(v: &Value, prop: &Value, root: &Value) -> Option<Value> {
    if v.is_null() {
        return Some(Value::Null);
    }
    if let Some(variants) = prop.get("anyOf").and_then(Value::as_array) {
        let fits = |var: &Value| {
            let d = deref(var, root);
            match (v, d.get("type").and_then(s)) {
                (Value::Object(_), _) if d.get("properties").is_some() => true,
                (Value::Object(_), Some("object")) | (Value::Array(_), Some("array")) | (Value::String(_), Some("string")) | (Value::Bool(_), Some("boolean")) => true,
                (Value::Number(n), Some("integer")) => n.is_i64() || n.is_u64(),
                (Value::Number(_), Some("number")) => true,
                (Value::String(_), None) => d.get("enum").is_some(),
                _ => false,
            }
        };
        return match variants.iter().find(|var| fits(var)) {
            Some(var) => dump_value(v, var, root),
            None => Some(v.clone()),
        };
    }
    let p = deref(prop, root);
    if p.get("properties").is_some() {
        return dump_model(v, p, root);
    }
    Some(match (v, p.get("type").and_then(s)) {
        (Value::Array(items), Some("array")) => match p.get("items") {
            Some(it) => Value::Array(items.iter().map(|x| dump_value(x, it, root)).collect::<Option<_>>()?),
            None => v.clone(),
        },
        (Value::Object(m), Some("object")) => match p.get("additionalProperties").filter(|a| a.is_object()) {
            Some(ap) => Value::Object(m.iter().map(|(k, x)| Some((k.clone(), dump_value(x, ap, root)?))).collect::<Option<_>>()?),
            None => v.clone(),
        },
        (Value::Number(n), Some("number")) if n.is_i64() || n.is_u64() => json!(n.as_f64()?),
        _ => v.clone(),
    })
}

/// `EnvironmentConfig._parse_size_to_mb`.
fn parse_size_mb(size: &str) -> Option<i64> {
    let s = size.trim().to_uppercase();
    let (n, unit) = s.split_at(s.len().checked_sub(1)?);
    let f: f64 = n.parse().ok()?;
    Some(match unit {
        "G" => (f * 1024.0) as i64,
        "M" => f as i64,
        "K" => (f / 1024.0) as i64,
        _ => return None,
    })
}

struct TaskInfo {
    config: Option<Value>,
    has_instruction: bool,
    has_environment: bool,
    has_tests: bool,
    has_solution: bool,
    has_docker_compose: bool,
}

fn task_info(dir: &Path) -> TaskInfo {
    let toml_text = std::fs::read_to_string(dir.join("task.toml")).ok();
    let has_steps = toml_text.as_deref().and_then(|t| toml::from_str::<toml::Table>(t).ok()).and_then(|t| t.get("steps").cloned()).is_some_and(|s| match s {
        toml::Value::Array(a) => !a.is_empty(),
        toml::Value::Table(t) => !t.is_empty(),
        toml::Value::String(x) => !x.is_empty(),
        _ => true,
    });
    TaskInfo {
        config: toml_text.as_deref().and_then(task_config_dump),
        has_instruction: dir.join("instruction.md").exists() || has_steps,
        has_environment: dir.join("environment").exists(),
        has_tests: dir.join("tests").exists(),
        has_solution: dir.join("solution").exists(),
        has_docker_compose: dir.join("environment/docker-compose.yaml").exists(),
    }
}

/// `TaskDefinitionScanner.get_instruction`.
fn task_instruction(dir: &Path, config: Option<&Value>) -> Value {
    let p = dir.join("instruction.md");
    if p.exists() {
        return read_text(&p).ok().flatten().map_or(Value::Null, Value::String);
    }
    let steps = config.and_then(|c| c["steps"].as_array()).filter(|s| !s.is_empty());
    let Some(steps) = steps else { return Value::Null };
    let sections: Vec<String> = steps
        .iter()
        .enumerate()
        .filter_map(|(i, st)| {
            let name = st["name"].as_str()?;
            let text = read_text(&dir.join("steps").join(name).join("instruction.md")).ok()??;
            Some(format!("## Step {}: {name}\n\n{}", i + 1, text.trim_end()))
        })
        .collect();
    if sections.is_empty() { Value::Null } else { json!(sections.join("\n\n---\n\n")) }
}

impl App {
    fn list_tasks(&self) -> Vec<String> {
        sorted_entries(&self.folder).into_iter().filter(|d| d.is_dir() && d.join("task.toml").exists()).map(|p| name_of(&p)).collect()
    }

    fn task_definition_summaries(&self) -> Vec<Value> {
        self.list_tasks()
            .into_iter()
            .map(|name| {
                let i = task_info(&self.folder.join(&name));
                let flags = |mut v: Value| {
                    for (k, b) in [("has_instruction", i.has_instruction), ("has_environment", i.has_environment), ("has_tests", i.has_tests), ("has_solution", i.has_solution), ("has_docker_compose", i.has_docker_compose)] {
                        v[k] = json!(b);
                    }
                    v
                };
                match &i.config {
                    Some(c) => {
                        let env = &c["environment"];
                        let mut v = json!({"name": name, "version": c["schema_version"], "source": c["source"], "metadata": c["metadata"]});
                        v = flags(v);
                        for (k, val) in [
                            ("agent_timeout_sec", &c["agent"]["timeout_sec"]),
                            ("verifier_timeout_sec", &c["verifier"]["timeout_sec"]),
                            ("os", &env["os"]),
                            ("cpus", &env["cpus"]),
                            ("memory_mb", &env["memory_mb"]),
                            ("storage_mb", &env["storage_mb"]),
                            ("gpus", &env["gpus"]),
                        ] {
                            v[k] = val.clone();
                        }
                        v
                    }
                    None => {
                        let mut v = json!({"name": name, "version": "1.0", "source": null, "metadata": {}});
                        v = flags(v);
                        for k in ["agent_timeout_sec", "verifier_timeout_sec", "os", "cpus", "memory_mb", "storage_mb", "gpus"] {
                            v[k] = Value::Null;
                        }
                        v
                    }
                }
            })
            .collect()
    }

    fn validate_task(&self, name: &str) -> std::result::Result<PathBuf, Err> {
        let d = resolve(&self.folder, name);
        if name.contains('/') || !strictly_under(&d, &self.folder) {
            return Err(err(400, "Invalid task name"));
        }
        Ok(d)
    }

    fn tasks_get(&self, segs: &[&str], q: &Query) -> Res {
        match segs {
            ["task-definitions", "filters"] => {
                let (mut diff, mut cat, mut tags) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
                for sm in self.task_definition_summaries() {
                    let m = &sm["metadata"];
                    if let Some(d) = m.get("difficulty").and_then(s) {
                        *diff.entry(d.to_string()).or_insert(0) += 1;
                    }
                    if let Some(c) = m.get("category").and_then(s) {
                        *cat.entry(c.to_string()).or_insert(0) += 1;
                    }
                    for t in m.get("tags").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                        *tags.entry(t.to_string()).or_insert(0) += 1;
                    }
                }
                ok_json(json!({"difficulties": filter_options(diff), "categories": filter_options(cat), "tags": filter_options(tags)}))
            }
            ["task-definitions"] => {
                let page = q.int("page", 1, Some(1), None)?;
                let page_size = q.int("page_size", 100, Some(1), Some(100))?;
                let mut items = self.task_definition_summaries();
                if let Some(qs) = q.get("q").filter(|x| !x.is_empty()) {
                    let n = qs.to_lowercase();
                    items.retain(|s| {
                        s["name"].as_str().is_some_and(|x| x.to_lowercase().contains(&n))
                            || s["source"].as_str().is_some_and(|x| x.to_lowercase().contains(&n))
                            || s["metadata"].as_object().into_iter().flatten().any(|(_, v)| py_str(v).to_lowercase().contains(&n))
                    });
                }
                for (param, field) in [("difficulty", "difficulty"), ("category", "category")] {
                    let want = q.all(param);
                    if !want.is_empty() {
                        items.retain(|s| s["metadata"].get(field).is_some_and(|v| want.iter().any(|w| v.as_str() == Some(w))));
                    }
                }
                let tags = q.all("tag");
                if !tags.is_empty() {
                    items.retain(|s| tags.iter().any(|t| s["metadata"].get("tags").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x.as_str() == Some(t)))));
                }
                ok_json(paginate(items, page, page_size))
            }
            ["task-definitions", name] => {
                let dir = self.validate_task(name)?;
                if !dir.exists() || !dir.join("task.toml").exists() {
                    return Err(err(404, format!("Task '{name}' not found")));
                }
                let i = task_info(&dir);
                ok_json(json!({
                    "name": name, "task_dir": dir.to_string_lossy(), "config": i.config.clone().unwrap_or(json!({})),
                    "instruction": task_instruction(&dir, i.config.as_ref()), "has_instruction": i.has_instruction, "has_environment": i.has_environment,
                    "has_tests": i.has_tests, "has_solution": i.has_solution, "has_docker_compose": i.has_docker_compose,
                }))
            }
            ["task-definitions", name, "files"] => {
                let dir = self.validate_task(name)?;
                if !dir.exists() {
                    return Err(err(404, format!("Task '{name}' not found")));
                }
                let mut out = vec![];
                scan_dir(&dir, "", true, false, &mut out);
                ok_json(Value::Array(out))
            }
            ["task-definitions", name, "files", rest @ ..] => {
                let dir = self.validate_task(name)?;
                if !dir.exists() {
                    return Err(err(404, format!("Task '{name}' not found")));
                }
                let full = resolve(&dir, &rest.join("/"));
                if !full.starts_with(&dir) {
                    return Err(err(403, "Access denied"));
                }
                serve_file(&full)
            }
            _ => Err(err(404, "Not Found")),
        }
    }
}

/// Python `str()` of a JSON value (metadata search).
fn py_str(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Null => "None".into(),
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        x => x.to_string(),
    }
}

// ---------------------------------------------------------------- routing / server

fn static_file(path: &str) -> Option<Resp> {
    let (_, body) = embedded::VIEWER_FILES.iter().find(|(p, _)| *p == path)?;
    let ctype = match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "ico" => "image/vnd.microsoft.icon",
        "ttf" => "font/ttf",
        _ => "application/octet-stream",
    };
    Some(Resp { status: 200, ctype: ctype.into(), body: body.to_vec(), headers: vec![] })
}

/// Origins allowed to make state-changing requests: the viewer's own origin, when its Host is loopback.
fn write_allowed(req: &Req) -> bool {
    let Some(origin) = req.origin else { return true };
    let Some(host) = req.host else { return false };
    let hostname = if host.starts_with('[') { host.split(']').next().map(|h| h.trim_start_matches('[')) } else { host.split(':').next() };
    matches!(hostname, Some("127.0.0.1" | "localhost" | "::1")) && origin == format!("http://{host}")
}

pub fn handle(app: &App, req: &Req) -> Resp {
    route(app, req).unwrap_or_else(|(status, detail)| Resp { status, ctype: "application/json".into(), body: serde_json::to_vec(&json!({"detail": detail})).unwrap(), headers: vec![] })
}

fn route(app: &App, req: &Req) -> Res {
    let (path, query) = req.url.split_once('?').unwrap_or((req.url, ""));
    let q = Query::parse(query);
    if !matches!(req.method, "GET" | "HEAD" | "OPTIONS") && !write_allowed(req) {
        return Err(err(403, "Cross-origin request rejected."));
    }
    let Some(api) = path.strip_prefix("/api/") else {
        if req.method != "GET" {
            return Err(err(405, "Method Not Allowed"));
        }
        if path.starts_with("/assets/") || path.starts_with("/fonts/") {
            return static_file(path).ok_or_else(|| err(404, "Not Found"));
        }
        if path == "/auth/callback" {
            let msg = q.get("error").unwrap_or("Hub sign-in is not supported by hbench");
            return Ok(Resp { status: 400, ctype: "text/html; charset=utf-8".into(), body: format!("<h1>Sign-in failed</h1><p>{}</p>", html_escape(msg)).into_bytes(), headers: vec![] });
        }
        return static_file(if path == "/favicon.ico" || path == "/favicon.svg" { path } else { "/index.html" }).ok_or_else(|| err(404, "Not Found"));
    };
    let decoded: Vec<String> = api.split('/').map(|x| pct_decode(x, false)).collect();
    let segs: Vec<&str> = decoded.iter().map(String::as_str).collect();
    match (req.method, segs.as_slice()) {
        ("GET", ["health"]) => ok_json(json!({"status": "ok"})),
        ("GET", ["config"]) => ok_json(json!({"folder": app.folder.to_string_lossy(), "mode": app.mode.as_str(), "environments": ENVIRONMENTS})),
        ("GET", ["pricing"]) => pricing(&q.required("model")?),
        ("GET", ["auth", "status"]) => ok_json(json!({"authenticated": false, "username": null})),
        ("GET", ["auth", "organizations"]) => Err(err(401, NOT_AUTHENTICATED)),
        ("GET", ["auth", "login-url"]) => Err(err(501, "Hub sign-in is not supported by hbench.")),
        ("POST", ["auth", "logout"]) => ok_json(json!({"status": "ok"})),
        ("GET", s) if app.mode == Mode::Tasks => app.tasks_get(s, &q),
        ("GET", s) => app.jobs_get(s, &q, req),
        (m, s) if app.mode == Mode::Jobs => app.jobs_write(m, s, req),
        _ => Err(err(405, "Method Not Allowed")),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// `hbench view`'s folder-type auto-detection.
pub fn detect_mode(folder: &Path) -> Result<Mode> {
    let subdirs: Vec<PathBuf> = sorted_entries(folder).into_iter().filter(|d| d.is_dir() && !name_of(d).starts_with('.')).collect();
    if subdirs.is_empty() {
        bail!("Folder has no subdirectories. Use --tasks or --jobs to specify the folder type.");
    }
    for d in subdirs {
        if d.join("config.json").exists() {
            return Ok(Mode::Jobs);
        }
        if d.join("task.toml").exists() {
            return Ok(Mode::Tasks);
        }
    }
    bail!("Could not auto-detect folder type. Use --tasks or --jobs to specify.")
}

/// `8080` or `8080-8089`.
pub fn parse_port_range(s: &str) -> Result<(u16, u16)> {
    let (a, b) = s.split_once('-').unwrap_or((s, s));
    Ok((a.trim().parse()?, b.trim().parse()?))
}

pub fn serve(folder: &Path, mode: Option<Mode>, host: &str, ports: (u16, u16)) -> Result<()> {
    let folder = std::path::absolute(folder)?;
    if !folder.exists() {
        bail!("Folder '{}' does not exist", folder.display());
    }
    let folder = folder.canonicalize()?;
    let mode = match mode {
        Some(m) => m,
        None => detect_mode(&folder)?,
    };
    let server = (ports.0..=ports.1)
        .find_map(|p| tiny_http::Server::http((host, p)).ok().map(|s| (s, p)))
        .ok_or_else(|| anyhow!("No available port found in range {}-{}", ports.0, ports.1))?;
    let (server, port) = (Arc::new(server.0), server.1);
    println!("Starting hbench viewer");
    println!("  {}: {}", if mode == Mode::Tasks { "Tasks folder" } else { "Jobs folder" }, folder.display());
    println!("  Mode: {}", mode.as_str());
    println!("  Server: http://{host}:{port}");
    let app = Arc::new(App::new(folder, mode));
    // ponytail: fixed worker pool; the SPA fires a handful of parallel requests per page.
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let (server, app) = (server.clone(), app.clone());
            std::thread::spawn(move || {
                for mut rq in server.incoming_requests() {
                    let header = |name: &str| rq.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str().to_string());
                    let (origin, host) = (header("origin"), header("host"));
                    let loopback = rq.remote_addr().is_some_and(|a| a.ip().is_loopback());
                    let method = rq.method().as_str().to_uppercase();
                    let url = rq.url().to_string();
                    let mut body = vec![];
                    let _ = rq.as_reader().read_to_end(&mut body);
                    let r = handle(&app, &Req { method: &method, url: &url, origin: origin.as_deref(), host: host.as_deref(), client_loopback: loopback, body: &body });
                    let mut resp = tiny_http::Response::from_data(r.body).with_status_code(r.status);
                    resp.add_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], r.ctype.as_bytes()).unwrap());
                    for (k, v) in &r.headers {
                        if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                            resp.add_header(h);
                        }
                    }
                    // Without `Connection: close`, a reused keep-alive connection stalls ~4s in tiny_http.
                    resp.add_header(tiny_http::Header::from_bytes(&b"Connection"[..], &b"close"[..]).unwrap());
                    let _ = rq.respond(resp);
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(app: &App, url: &str) -> (u16, Value) {
        let r = handle(app, &Req { method: "GET", url, origin: None, host: Some("127.0.0.1:8080"), client_loopback: true, body: b"" });
        (r.status, serde_json::from_slice(&r.body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&r.body).into_owned())))
    }

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    fn trial(dir: &Path, task: &str, reward: Option<f64>, err: Option<&str>, started: &str, finished: Option<&str>, cost: f64) {
        let r = json!({
            "id": uuid::Uuid::new_v4(), "task_name": task, "trial_name": name_of(dir), "trial_uri": "", "task_id": {"path": task}, "source": "ds", "task_checksum": "",
            "config": {"task": {"path": task}, "agent": {"name": "claude-code", "kwargs": {"reasoning_effort": "high"}}},
            "agent_info": {"name": "claude-code", "version": "2", "model_info": {"name": "m", "provider": "anthropic"}},
            "agent_result": {"n_input_tokens": 100, "n_cache_tokens": 30, "n_output_tokens": 5, "cost_usd": cost},
            "verifier_result": reward.map(|r| json!({"rewards": {"reward": r, "aux": 2}})),
            "exception_info": err.map(|e| json!({"exception_type": e})), "started_at": started, "finished_at": finished,
        });
        write(&dir.join("result.json"), &r.to_string());
    }

    /// jobs/{old,new}: new has 3 trials over 2 tasks (one errored), plus one in-progress trial (config only).
    fn fixture() -> App {
        let root = std::env::temp_dir().join(format!("hbench-view-{}", uuid::Uuid::new_v4()));
        let new = root.join("2026-01-02__00-00-00");
        write(&new.join("config.json"), r#"{"job_name":"n","agents":[{"name":"claude-code","model_name":"anthropic/m"}],"datasets":[{"name":"ds","version":2}]}"#);
        write(&new.join("result.json"), r#"{"id":"00000000-0000-0000-0000-000000000001","started_at":"2026-01-02T00:00:00Z","finished_at":null,"n_total_trials":4,"stats":{"n_trials":3,"n_errors":1,"evals":{"k":{"metrics":[{"mean":0.5}]}},"n_input_tokens":300,"n_cache_tokens":90,"cost_usd":0.6}}"#);
        trial(&new.join("a__1"), "a", Some(1.0), None, "2026-01-02T00:00:00Z", Some("2026-01-02T00:00:10Z"), 0.1);
        trial(&new.join("a__2"), "a", Some(0.0), Some("AgentTimeoutError"), "2026-01-02T00:00:00Z", Some("2026-01-02T00:00:30Z"), 0.2);
        trial(&new.join("b__1"), "b", Some(0.75), None, "2026-01-02T00:00:00Z", Some("2026-01-02T00:00:20Z"), 0.3);
        write(&new.join("c__1/config.json"), r#"{"task":{"path":"/t/c"},"trial_name":"c__1","agent":{"name":"oracle"}}"#);
        write(&new.join("a__1/agent/x.txt"), "a\r\nb");
        write(&root.join("2025-01-01__00-00-00/config.json"), "{}");
        write(&root.join("secret.txt"), "top secret");
        App::new(root.canonicalize().unwrap(), Mode::Jobs)
    }

    #[test]
    fn jobs_list_sort_filter_paginate() {
        let app = fixture();
        let (_, j) = get(&app, "/api/jobs");
        assert_eq!((j["total"].as_i64(), j["total_pages"].as_i64()), (Some(2), Some(1)));
        let first = &j["items"][0];
        assert_eq!(first["name"], "2026-01-02__00-00-00");
        assert_eq!((first["n_completed_trials"].as_i64(), first["n_errored_trials"].as_i64(), first["total_input_tokens"].as_i64()), (Some(3), Some(1), Some(210)));
        assert_eq!((first["datasets"].clone(), first["providers"].clone(), first["models"].clone()), (json!(["ds"]), json!(["anthropic"]), json!(["m"])));
        assert_eq!(j["items"][1]["agents"], json!(["oracle"])); // default agent, no result -> sorted last
        assert_eq!(get(&app, "/api/jobs?q=ANTHROPIC").1["total"], 1);
        assert_eq!(get(&app, "/api/jobs?agent=oracle&agent=nope").1["items"][0]["name"], "2025-01-01__00-00-00");
        let p = get(&app, "/api/jobs?page=2&page_size=1").1;
        assert_eq!((p["items"][0]["name"].as_str(), p["total_pages"].as_i64()), (Some("2025-01-01__00-00-00"), Some(2)));
        assert_eq!(get(&app, "/api/jobs?page_size=101").0, 422);
        assert_eq!(get(&app, "/api/jobs/filters").1["agents"], json!([{"value": "claude-code", "count": 1}, {"value": "oracle", "count": 1}]));
    }

    #[test]
    fn task_and_trial_summaries() {
        let app = fixture();
        let job = "2026-01-02__00-00-00";
        let (_, t) = get(&app, &format!("/api/jobs/{job}/tasks?sort_by=avg_reward&sort_order=desc"));
        let names: Vec<&str> = t["items"].as_array().unwrap().iter().map(|x| x["task_name"].as_str().unwrap()).collect();
        assert_eq!(names, ["c", "b", "a"]); // in-progress group (avg None) first when descending
        let a = &t["items"][2];
        assert_eq!((a["n_trials"].as_i64(), a["n_errors"].as_i64(), a["avg_reward"].as_f64(), a["avg_duration_ms"].as_f64()), (Some(2), Some(1), Some(0.5), Some(20000.0)));
        assert_eq!((a["avg_input_tokens"].as_f64(), a["avg_tokens"].as_f64(), a["evals"]["aux"]["metrics"][0]["aux"].as_f64()), (Some(70.0), Some(105.0), Some(2.0)));
        assert_eq!(a["exception_types"], json!(["AgentTimeoutError"]));
        let (_, tr) = get(&app, &format!("/api/jobs/{job}/trials?sort_by=cost_usd&sort_order=desc"));
        let names: Vec<&str> = tr["items"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["c__1", "b__1", "a__2", "a__1"]); // None first when descending
        assert_eq!((tr["items"][1]["reward"].as_f64(), tr["items"][1]["reasoning_effort"].as_str()), (Some(0.75), Some("high")));
        assert_eq!(get(&app, &format!("/api/jobs/{job}/trials?reasoning_effort=__missing__")).1["total"], 1);
        assert_eq!(get(&app, &format!("/api/jobs/{job}/tasks/filters")).1["reasoning_efforts"], json!([{"value": "__missing__", "count": 1}, {"value": "high", "count": 3}]));
        let nav = get(&app, &format!("/api/jobs/{job}/trials/a__2/navigation")).1;
        assert_eq!((nav["previous"].as_str(), nav["next"].as_str(), nav["total"].as_i64()), (Some("a__1"), Some("b__1"), Some(4)));
        let partial = get(&app, &format!("/api/jobs/{job}/trials/c__1")).1;
        assert_eq!((partial["task_name"].as_str(), partial["agent_info"]["name"].as_str(), partial["finished_at"].is_null()), (Some("c"), Some("oracle"), true));
        assert_eq!(get(&app, &format!("/api/jobs/{job}/trials/a__1/files/agent/x.txt")).1, "a\nb");
        let cmp = get(&app, &format!("/api/compare?job={job}")).1;
        assert_eq!(cmp["tasks"][0]["task_name"], "b"); // highest avg reward first
    }

    #[test]
    fn path_guards_and_spa() {
        let app = fixture();
        let job = "2026-01-02__00-00-00";
        for url in [format!("/api/jobs/{job}/trials/a__1/files/../../../secret.txt"), format!("/api/jobs/{job}/trials/a__1/files/%2E%2E/%2E%2E/%2E%2E/secret.txt")] {
            let (s, body) = get(&app, &url);
            assert_eq!(s, 403, "{url} -> {body}");
        }
        assert_eq!(get(&app, "/api/jobs/..").0, 400);
        assert_eq!(get(&app, "/api/jobs/nope").0, 404);
        assert_eq!(get(&app, &format!("/api/jobs/{job}/trials/a__1/verifier-output?step=../../..")).0, 400);
        let r = handle(&app, &Req { method: "DELETE", url: &format!("/api/jobs/{job}"), origin: Some("http://evil.example"), host: Some("127.0.0.1:8080"), client_loopback: true, body: b"" });
        assert_eq!(r.status, 403);
        assert!(app.folder.join(job).exists());
        let page = handle(&app, &Req { method: "GET", url: "/jobs/x/trials/y", origin: None, host: None, client_loopback: true, body: b"" });
        assert!(String::from_utf8(page.body).unwrap().contains("__reactRouterContext"));
    }

    #[test]
    fn task_config_dump_fills_defaults_and_migrates() {
        let c = task_config_dump("version = \"1.0\"\n[environment]\nmemory = \"2G\"\nos = \"LINUX\"\nallow_internet = false\n[agent]\ntimeout_sec = 600\n").unwrap();
        assert_eq!((c["schema_version"].as_str(), c["environment"]["memory_mb"].as_i64(), c["environment"]["os"].as_str()), (Some("1.0"), Some(2048), Some("linux")));
        assert_eq!(c["environment"]["network_mode"], "no-network");
        assert!(c["environment"].get("allow_internet").is_none() && c["agent"]["timeout_sec"].is_f64());
        assert_eq!(c["artifacts"], json!([]));
    }
}
