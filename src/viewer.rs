//! `hbench view`: local results viewer (upstream's `hbench view`, essentials only).
//! One embedded HTML/JS page + a small JSON API over the jobs dir.

use crate::trial::{TimingInfo, TrialResult};
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

const MAX_TEXT: u64 = 1024 * 1024;
const MAX_MEDIA: u64 = 50 * 1024 * 1024;

pub struct Resp {
    pub status: u16,
    pub ctype: &'static str,
    pub body: Vec<u8>,
}

type Res = std::result::Result<Resp, (u16, String)>;

fn ok_json(v: Value) -> Res {
    Ok(Resp { status: 200, ctype: "application/json", body: serde_json::to_vec(&v).unwrap() })
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn read_text(p: &Path) -> Value {
    if !p.is_file() {
        return Value::Null;
    }
    std::fs::read(p).map(|b| String::from_utf8_lossy(&b).into_owned().into()).unwrap_or_else(|_| "[Error reading file]".into())
}

fn pct_decode(s: &str) -> String {
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
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `base/parts...`, guaranteed (after resolving symlinks) to stay under `base`.
/// Rejects `..`, absolute and multi-component parts outright (400); escapes via symlink are 403.
pub fn under(base: &Path, parts: &[&str]) -> std::result::Result<PathBuf, (u16, String)> {
    let mut p = base.to_path_buf();
    for part in parts {
        let mut comps = Path::new(part).components();
        match (comps.next(), comps.next()) {
            (Some(Component::Normal(c)), None) if !part.contains(['/', '\\', '\0']) => p.push(c),
            _ => return Err((400, format!("invalid path component {part:?}"))),
        }
    }
    let c = p.canonicalize().map_err(|_| (404, "not found".to_string()))?;
    let base = base.canonicalize().map_err(|_| (404, "not found".to_string()))?;
    if !c.starts_with(&base) {
        return Err((403, "access denied".into()));
    }
    Ok(c)
}

fn secs(t: &Option<TimingInfo>) -> Option<f64> {
    let t = t.as_ref()?;
    Some((t.finished_at? - t.started_at?).num_milliseconds() as f64 / 1000.0)
}

fn trial_summary(dir: &Path) -> Value {
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    let Some(r) = std::fs::read_to_string(dir.join("result.json")).ok().and_then(|s| serde_json::from_str::<TrialResult>(&s).ok()) else {
        let cfg = read_json(&dir.join("config.json")).unwrap_or_default();
        return json!({"name": name, "status": "running", "agent_name": cfg["agent"]["name"], "model_name": cfg["agent"]["model_name"]});
    };
    let (i, c, o, cost) = r.usage_totals();
    let rewards = r.reward();
    let total = match (r.started_at, r.finished_at) {
        (Some(s), Some(f)) => Some((f - s).num_milliseconds() as f64 / 1000.0),
        _ => None,
    };
    json!({
        "name": name,
        "status": if r.finished_at.is_some() { "completed" } else { "running" },
        "task_name": r.task_name,
        "source": r.source,
        "agent_name": r.agent_info.name,
        "agent_version": r.agent_info.version,
        "model_name": r.agent_info.model_info.as_ref().map(|m| &m.name),
        "model_provider": r.agent_info.model_info.as_ref().and_then(|m| m.provider.as_ref()),
        "rewards": rewards,
        "reward": rewards.and_then(|m| m.get("reward").or(m.values().next())),
        "error_type": r.exception_info.as_ref().map(|e| &e.exception_type),
        "started_at": r.started_at,
        "finished_at": r.finished_at,
        "duration_sec": total,
        "durations": {
            "environment_setup": secs(&r.environment_setup),
            "agent_setup": secs(&r.agent_setup),
            "agent_execution": secs(&r.agent_execution),
            "verifier": secs(&r.verifier),
        },
        "n_input_tokens": i, "n_cache_tokens": c, "n_output_tokens": o, "cost_usd": cost,
        "n_steps": r.step_results.as_ref().map(Vec::len),
    })
}

fn files(root: &Path, skip_top_manifest: bool) -> Vec<Value> {
    crate::atif::walk(root)
        .into_iter()
        .filter_map(|p| {
            let rel = p.strip_prefix(root).ok()?.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            if skip_top_manifest && (rel == "manifest.json" || p.is_dir()) {
                return None;
            }
            let is_dir = p.is_dir();
            Some(json!({"path": rel, "name": p.file_name()?.to_string_lossy(), "is_dir": is_dir, "size": (!is_dir).then(|| p.metadata().map(|m| m.len()).unwrap_or(0))}))
        })
        .collect()
}

fn serve_file(p: &Path) -> Res {
    if p.is_dir() {
        return Err((400, "cannot read directory".into()));
    }
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let media = match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "mp4" => Some("video/mp4"),
        _ => None,
    };
    let size = p.metadata().map(|m| m.len()).unwrap_or(0);
    if size > if media.is_some() { MAX_MEDIA } else { MAX_TEXT } {
        return Err((413, format!("file too large: {size} bytes")));
    }
    let body = std::fs::read(p).map_err(|e| (500, e.to_string()))?;
    match media {
        Some(ctype) => Ok(Resp { status: 200, ctype, body }),
        None if std::str::from_utf8(&body).is_ok() => Ok(Resp { status: 200, ctype: "text/plain; charset=utf-8", body }),
        None => Err((415, "file is binary and cannot be displayed".into())),
    }
}

fn route(root: &Path, url: &str) -> Res {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let step = query.split('&').find_map(|kv| kv.strip_prefix("step=")).map(pct_decode).filter(|s| !s.is_empty());
    let segs: Vec<String> = path.trim_matches('/').split('/').map(pct_decode).collect();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    let scoped = |trial: &Path| match &step {
        Some(s) => under(trial, &["steps", s]),
        None => Ok(trial.to_path_buf()),
    };
    match segs.as_slice() {
        [""] | ["index.html"] => Ok(Resp { status: 200, ctype: "text/html; charset=utf-8", body: PAGE.as_bytes().to_vec() }),
        ["api", "jobs"] => {
            let mut dirs: Vec<PathBuf> = std::fs::read_dir(root).map_err(|e| (500, e.to_string()))?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            dirs.sort();
            dirs.reverse(); // newest (timestamped names) first, like upstream
            ok_json(Value::Array(
                dirs.iter()
                    .map(|d| json!({"name": d.file_name().unwrap().to_string_lossy(), "result": read_json(&d.join("result.json")), "config": read_json(&d.join("config.json"))}))
                    .collect(),
            ))
        }
        ["api", "jobs", job] => {
            let d = under(root, &[job])?;
            ok_json(json!({"name": job, "result": read_json(&d.join("result.json")), "config": read_json(&d.join("config.json"))}))
        }
        ["api", "jobs", job, "trials"] => {
            let d = under(root, &[job])?;
            let mut dirs: Vec<PathBuf> = std::fs::read_dir(&d).map_err(|e| (500, e.to_string()))?.flatten().map(|e| e.path()).filter(|p| p.is_dir() && (p.join("config.json").exists() || p.join("result.json").exists())).collect();
            dirs.sort();
            ok_json(Value::Array(dirs.iter().map(|p| trial_summary(p)).collect()))
        }
        ["api", "jobs", job, "trials", trial] => {
            let d = under(root, &[job, trial])?;
            ok_json(read_json(&d.join("result.json")).or_else(|| read_json(&d.join("config.json")).map(|c| json!({"trial_name": trial, "config": c}))).unwrap_or_default())
        }
        ["api", "jobs", job, "trials", trial, "files"] => ok_json(Value::Array(files(&scoped(&under(root, &[job, trial])?)?, false))),
        ["api", "jobs", job, "trials", trial, "files", rest @ ..] => serve_file(&under(&scoped(&under(root, &[job, trial])?)?, rest)?),
        ["api", "jobs", job, "trials", trial, "verifier-output"] => {
            let v = scoped(&under(root, &[job, trial])?)?.join("verifier");
            ok_json(json!({
                "stdout": read_text(&v.join("test-stdout.txt")),
                "stderr": read_text(&v.join("test-stderr.txt")),
                "ctrf": read_text(&v.join("ctrf.json")),
                "reward": read_json(&v.join("reward.json")).filter(Value::is_object),
                "reward_txt": read_text(&v.join("reward.txt")),
                "reward_details": read_json(&v.join("reward-details.json")).filter(Value::is_object),
            }))
        }
        ["api", "jobs", job, "trials", trial, "artifacts"] => {
            let a = scoped(&under(root, &[job, trial])?)?.join("artifacts");
            ok_json(json!({"files": files(&a, true), "manifest": read_json(&a.join("manifest.json"))}))
        }
        ["api", "jobs", job, "trials", trial, "trajectory"] => {
            let agent = scoped(&under(root, &[job, trial])?)?.join("agent");
            // Older claude-code runs have only the native session: convert on the fly.
            ok_json(read_json(&agent.join("trajectory.json")).or_else(|| crate::atif::claude_code_trajectory(&agent)).unwrap_or_default())
        }
        _ => Err((404, "not found".into())),
    }
}

pub fn handle(root: &Path, url: &str) -> Resp {
    route(root, url).unwrap_or_else(|(status, detail)| Resp { status, ctype: "application/json", body: serde_json::to_vec(&json!({"detail": detail})).unwrap() })
}

pub fn serve(jobs_dir: &Path, host: &str, port: u16) -> Result<()> {
    let root = jobs_dir.canonicalize().with_context(|| format!("jobs dir {}", jobs_dir.display()))?;
    let server = tiny_http::Server::http((host, port)).map_err(|e| anyhow!("binding {host}:{port}: {e}"))?;
    println!("hbench viewer on http://{host}:{port}  (jobs dir: {})", root.display());
    // ponytail: one request at a time; fine for a local viewer, use a thread pool if it ever serves many users.
    for req in server.incoming_requests() {
        let r = if *req.method() == tiny_http::Method::Get { handle(&root, req.url()) } else { Resp { status: 405, ctype: "text/plain", body: b"method not allowed".to_vec() } };
        let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], r.ctype.as_bytes()).unwrap();
        // Without `Connection: close`, a reused keep-alive connection stalls ~4s in tiny_http.
        let close = tiny_http::Header::from_bytes(&b"Connection"[..], &b"close"[..]).unwrap();
        let _ = req.respond(tiny_http::Response::from_data(r.body).with_status_code(r.status).with_header(header).with_header(close));
    }
    Ok(())
}

const PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>hbench viewer</title>
<style>
body{font:14px/1.45 system-ui,sans-serif;margin:0;color:#1d1d1f;background:#fafafa}
header{background:#1d1d1f;color:#fff;padding:10px 20px}header a{color:#fff;text-decoration:none;font-weight:600}
main{padding:16px 20px}a{color:#0b5cad}h2{margin:4px 0 12px}h3{margin:18px 0 6px}
table{border-collapse:collapse;background:#fff;width:100%;margin-bottom:12px}
th,td{border:1px solid #ddd;padding:4px 8px;text-align:left;vertical-align:top}th{background:#f0f0f0;cursor:pointer;white-space:nowrap}
pre{background:#fff;border:1px solid #ddd;padding:8px;overflow:auto;max-height:480px;white-space:pre-wrap;word-break:break-word;margin:4px 0}
details{background:#fff;border:1px solid #ddd;margin:8px 0;padding:4px 10px}summary{cursor:pointer;font-weight:600;padding:4px 0}
.muted{color:#888}.err{color:#b00020}.ok{color:#137333}.kv td:first-child{font-weight:600;width:200px}
.step{border-left:4px solid #ccc;background:#fff;margin:8px 0;padding:6px 10px}.step.user{border-color:#0b5cad}.step.agent{border-color:#137333}.step.system{border-color:#888}
.sh{font-size:12px;color:#555;margin-bottom:4px}.tc{margin:6px 0 6px 10px}.tree div{cursor:pointer;padding:1px 0}.tree div:hover{background:#eef}
.split{display:grid;grid-template-columns:minmax(220px,1fr) 3fr;gap:10px}input{padding:4px 8px;width:300px}
</style></head><body>
<header><a href="#/">hbench viewer</a></header><main id="app">Loading...</main>
<script>
const app = document.getElementById('app');
const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const enc = encodeURIComponent;
async function api(p) { const r = await fetch(p); if (!r.ok) throw new Error(p + ': ' + r.status + ' ' + await r.text()); return r.json(); }
async function txt(p) { const r = await fetch(p); return r.ok ? r.text() : null; }
const num = v => v == null ? '' : (typeof v === 'number' && !Number.isInteger(v) ? v.toFixed(3) : String(v));
const dur = s => s == null ? '' : s < 120 ? s.toFixed(1) + 's' : (s / 60).toFixed(1) + 'm';
const tok = n => n == null ? '' : n >= 1e6 ? (n / 1e6).toFixed(2) + 'M' : n >= 1e3 ? (n / 1e3).toFixed(1) + 'k' : String(n);
const usd = c => c == null ? '' : '$' + c.toFixed(4);
const when = t => t ? new Date(t).toLocaleString() : '';
const pretty = v => JSON.stringify(v, null, 2);
const metrics = ev => (ev.metrics || []).flatMap(m => Object.entries(m).map(([k, v]) => k + '=' + num(v))).join(' ');
const passk = ev => Object.entries(ev.pass_at_k || {}).map(([k, v]) => 'pass@' + k + '=' + num(v)).join(' ');
const pre = (s, cls) => s == null || s === '' ? '<p class="muted">(none)</p>' : `<pre class="${cls || ''}">${esc(s)}</pre>`;
const sect = (title, body, open) => `<details${open ? ' open' : ''}><summary>${title}</summary>${body}</details>`;
const filePath = p => p.split('/').map(enc).join('/');

function table(cols, rows, id) {
  // cols: [label, fn(row) -> html, sortKey(row)]
  const head = cols.map((c, i) => `<th data-i="${i}">${c[0]}</th>`).join('');
  const body = rows.map(r => '<tr>' + cols.map(c => `<td>${c[1](r)}</td>`).join('') + '</tr>').join('');
  return `<table id="${id || ''}"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table>`;
}
function sortable(id, cols, rows, render) {
  let dir = 1;
  document.querySelectorAll(`#${id} th`).forEach(th => th.onclick = () => {
    const c = cols[+th.dataset.i], key = c[2] || (r => '');
    rows.sort((a, b) => { const x = key(a), y = key(b); return (x == null) - (y == null) || (x < y ? -dir : x > y ? dir : 0); });
    dir = -dir; render();
  });
}

async function jobsView() {
  const jobs = await api('/api/jobs');
  const cols = [
    ['Job', j => `<a href="#/job/${enc(j.name)}">${esc(j.name)}</a>`, j => j.name],
    ['Started', j => esc(when(j.result?.started_at)), j => j.result?.started_at],
    ['Status', j => !j.result ? '<span class="muted">no result</span>' : j.result.finished_at ? 'finished' : 'running'],
    ['Trials', j => j.result ? `${j.result.stats.n_completed_trials}/${j.result.n_total_trials}` : '', j => j.result?.n_total_trials],
    ['Errors', j => j.result ? `<span class="${j.result.stats.n_errored_trials ? 'err' : ''}">${j.result.stats.n_errored_trials}</span>` : '', j => j.result?.stats.n_errored_trials],
    ['Agents / models', j => esc((j.config?.agents || []).map(a => a.name + (a.model_name ? ' / ' + a.model_name : '')).join(', '))],
    ['Evals (metrics, pass@k)', j => Object.entries(j.result?.stats.evals || {}).map(([k, e]) => `<div><b>${esc(k)}</b>: ${esc(metrics(e))} ${esc(passk(e))}</div>`).join('')],
    ['Cost', j => usd(j.result?.stats.cost_usd), j => j.result?.stats.cost_usd],
    ['Tokens in/cache/out', j => { const s = j.result?.stats; return s ? [s.n_input_tokens, s.n_cache_tokens, s.n_output_tokens].map(tok).join(' / ') : ''; }],
  ];
  const render = () => { app.innerHTML = `<h2>Jobs</h2>` + (jobs.length ? table(cols, jobs, 'jobs') : '<p class="muted">No jobs found.</p>'); sortable('jobs', cols, jobs, render); };
  render();
}

async function jobView(job) {
  const [j, trials] = await Promise.all([api(`/api/jobs/${enc(job)}`), api(`/api/jobs/${enc(job)}/trials`)]);
  const s = j.result?.stats;
  const evals = Object.entries(s?.evals || {}).map(([k, e]) => ({k, ...e}));
  const ecols = [['Agent / model / dataset', e => esc(e.k)], ['Trials', e => e.n_trials], ['Errors', e => e.n_errors], ['Metrics', e => esc(metrics(e))], ['pass@k', e => esc(passk(e))],
    ['Exceptions', e => Object.entries(e.exception_stats || {}).map(([x, n]) => `${esc(x)}: ${n.length}`).join('<br>')]];
  const tcols = [
    ['Trial', t => `<a href="#/job/${enc(job)}/trial/${enc(t.name)}">${esc(t.name)}</a>`, t => t.name],
    ['Task', t => esc(t.task_name), t => t.task_name],
    ['Agent', t => esc(t.agent_name), t => t.agent_name],
    ['Model', t => esc(t.model_name), t => t.model_name],
    ['Reward', t => t.status === 'running' ? '<span class="muted">running</span>' : `<span class="${t.reward > 0 ? 'ok' : ''}">${esc(num(t.reward))}</span>` + (t.rewards && Object.keys(t.rewards).length > 1 ? `<div class="muted">${esc(Object.entries(t.rewards).map(([k, v]) => k + '=' + num(v)).join(' '))}</div>` : ''), t => t.reward],
    ['Error', t => `<span class="err">${esc(t.error_type)}</span>`, t => t.error_type],
    ['Duration', t => dur(t.duration_sec), t => t.duration_sec],
    ['Env / setup / agent / verify', t => t.durations ? ['environment_setup', 'agent_setup', 'agent_execution', 'verifier'].map(k => dur(t.durations[k]) || '-').join(' / ') : ''],
    ['Cost', t => usd(t.cost_usd), t => t.cost_usd],
    ['Tokens in/out', t => t.n_input_tokens == null ? '' : tok(t.n_input_tokens) + ' / ' + tok(t.n_output_tokens), t => t.n_input_tokens],
  ];
  let filter = '';
  const shown = () => trials.filter(t => !filter || [t.name, t.task_name, t.error_type, t.agent_name, t.model_name].join(' ').toLowerCase().includes(filter));
  const render = () => {
    const rows = shown();
    document.getElementById('trials').innerHTML = table(tcols, rows, 'ttable') + `<p class="muted">${rows.length} of ${trials.length} trials</p>`;
    sortable('ttable', tcols, trials, render);
  };
  app.innerHTML = `<h2><a href="#/">Jobs</a> / ${esc(job)}</h2>` +
    (s ? `<p>Completed ${s.n_completed_trials}/${j.result.n_total_trials} &middot; errored <span class="err">${s.n_errored_trials}</span> &middot; running ${s.n_running_trials} &middot; retries ${s.n_retries}` +
      (s.cost_usd != null ? ` &middot; cost ${usd(s.cost_usd)}` : '') + (s.n_input_tokens != null ? ` &middot; tokens in ${tok(s.n_input_tokens)} cache ${tok(s.n_cache_tokens)} out ${tok(s.n_output_tokens)}` : '') +
      ` &middot; started ${esc(when(j.result.started_at))}${j.result.finished_at ? ', finished ' + esc(when(j.result.finished_at)) : ''}</p>` : '<p class="muted">No result.json yet.</p>') +
    (evals.length ? table(ecols, evals) : '') +
    `<h3>Trials</h3><input id="filter" placeholder="filter by task, trial, error, agent, model"><div id="trials"></div>` +
    sect('Job config.json', pre(pretty(j.config))) + sect('Job result.json', pre(pretty(j.result)));
  document.getElementById('filter').oninput = e => { filter = e.target.value.toLowerCase(); render(); };
  render();
}

const content = c => typeof c === 'string' ? c : Array.isArray(c) ? c.map(p => p.type === 'text' ? p.text : '[' + p.type + ']').join('\n') : c == null ? '' : pretty(c);
function trajectoryHtml(t) {
  if (!t) return '<p class="muted">No trajectory.json for this trial.</p>';
  const fm = t.final_metrics || {};
  let h = `<p>${esc(t.agent?.name)} ${esc(t.agent?.version)} &middot; model ${esc(t.agent?.model_name)} &middot; ${esc(t.schema_version)} &middot; session ${esc(t.session_id)}<br>` +
    `steps ${t.steps.length} &middot; prompt ${tok(fm.total_prompt_tokens)} (cached ${tok(fm.total_cached_tokens)}) &middot; completion ${tok(fm.total_completion_tokens)}${fm.total_cost_usd != null ? ' &middot; ' + usd(fm.total_cost_usd) : ''}</p>`;
  for (const s of t.steps) {
    const m = s.metrics;
    const results = s.observation?.results || [];
    const calls = (s.tool_calls || []).map(tc => {
      const r = results.find(r => r.source_call_id === tc.tool_call_id);
      return `<div class="tc"><b>${esc(tc.function_name)}</b> <span class="muted">${esc(tc.tool_call_id)}</span>${pre(pretty(tc.arguments))}` +
        (r ? sect('result', pre(content(r.content)), true) : '<div class="muted">no result</div>') + '</div>';
    }).join('');
    const loose = results.filter(r => !r.source_call_id || !(s.tool_calls || []).some(tc => tc.tool_call_id === r.source_call_id)).map(r => sect('observation', pre(content(r.content)), true)).join('');
    h += `<div class="step ${esc(s.source)}"><div class="sh">#${s.step_id} <b>${esc(s.source)}</b> ${esc(s.model_name || '')} ${esc(s.timestamp || '')}` +
      (m ? ` &middot; in ${tok(m.prompt_tokens)} out ${tok(m.completion_tokens)} cached ${tok(m.cached_tokens)}${m.cost_usd != null ? ' ' + usd(m.cost_usd) : ''}` : '') +
      (s.extra?.is_sidechain ? ' &middot; <i>subagent ' + esc(s.extra.agent_id || '') + '</i>' : '') + '</div>' +
      (s.reasoning_content ? sect('reasoning', pre(s.reasoning_content)) : '') +
      (content(s.message) ? pre(content(s.message)) : '') + calls + loose + '</div>';
  }
  return h;
}

function treeHtml(list, prefix, id) {
  const items = list.filter(f => f.path.startsWith(prefix) && f.path !== prefix.replace(/\/$/, ''));
  if (!items.length) return '<p class="muted">(empty)</p>';
  return `<div class="split"><div class="tree">` + items.map(f => {
    const depth = f.path.slice(prefix.length).split('/').length - 1;
    return `<div style="padding-left:${depth * 14}px" ${f.is_dir ? '' : `data-file="${esc(f.path)}" data-view="${id}"`}>${f.is_dir ? '&#128193; ' : ''}${esc(f.name)}${f.is_dir ? '' : ` <span class="muted">${tok(f.size)}B</span>`}</div>`;
  }).join('') + `</div><div id="${id}"><p class="muted">Select a file.</p></div></div>`;
}

// `hbench analyze` writes analysis.json into the trial dir: summary text + per-criterion checks.
function analysisHtml(raw) {
  let a;
  try { a = JSON.parse(raw); } catch (e) { return pre(raw); }
  const texts = Object.entries(a).filter(([, v]) => typeof v === 'string').map(([k, v]) => `<p><b>${esc(k)}</b>: ${esc(v)}</p>`).join('');
  const checks = Object.entries(a).filter(([, v]) => v && typeof v === 'object' && !Array.isArray(v))
    .map(([k, v]) => sect(esc(k), table([['Criterion', ([c]) => esc(c)], ['Outcome', ([, x]) => esc(x && typeof x === 'object' ? (x.outcome ?? x.result ?? '') : x)],
      ['Explanation', ([, x]) => esc(x && typeof x === 'object' ? (x.explanation ?? '') : '')]], Object.entries(v)), true)).join('');
  return texts + checks + sect('analysis.json', pre(pretty(a)));
}

async function trialView(job, trial) {
  const base = `/api/jobs/${enc(job)}/trials/${enc(trial)}`;
  const [r, log, exc, an] = await Promise.all([api(base), txt(base + '/files/trial.log'), txt(base + '/files/exception.txt'), txt(base + '/files/analysis.json')]);
  const steps = (r.step_results || []).map(s => s.step_name);
  const span = t => t && t.started_at && t.finished_at ? dur((new Date(t.finished_at) - new Date(t.started_at)) / 1000) : '';
  const rw = rs => rs ? Object.entries(rs).map(([k, v]) => `${esc(k)}=${esc(num(v))}`).join(' ') : '';
  const ex = r.exception_info;
  let h = `<h2><a href="#/">Jobs</a> / <a href="#/job/${enc(job)}">${esc(job)}</a> / ${esc(trial)}</h2>` +
    `<table class="kv">` + [
      ['Task', esc(r.task_name)], ['Source', esc(r.source)], ['Agent', esc(r.agent_info?.name) + ' ' + esc(r.agent_info?.version)],
      ['Model', esc([r.agent_info?.model_info?.provider, r.agent_info?.model_info?.name].filter(Boolean).join('/'))],
      ['Rewards', rw(r.verifier_result?.rewards) || '<span class="muted">none</span>'],
      ['Exception', ex ? `<span class="err">${esc(ex.exception_type)}</span>: ${esc((ex.exception_message || '').slice(0, 300))}` : ''],
      ['Started / finished', esc(when(r.started_at)) + ' &rarr; ' + esc(when(r.finished_at))],
      ['Env setup / agent setup / agent / verifier', [r.environment_setup, r.agent_setup, r.agent_execution, r.verifier].map(t => span(t) || '-').join(' / ')],
      ['Tokens in / cache / out, cost', r.agent_result ? [r.agent_result.n_input_tokens, r.agent_result.n_cache_tokens, r.agent_result.n_output_tokens].map(tok).join(' / ') + ' ' + usd(r.agent_result.cost_usd) : ''],
    ].map(([k, v]) => `<tr><td>${k}</td><td>${v}</td></tr>`).join('') + `</table>`;
  if (steps.length) {
    h += `<h3>Steps</h3>` + table([['Step', s => esc(s.step_name)], ['Rewards', s => rw(s.verifier_result?.rewards)], ['Exception', s => `<span class="err">${esc(s.exception_info?.exception_type)}</span>`],
      ['Agent', s => span(s.agent_execution)], ['Verifier', s => span(s.verifier)], ['Tokens in/out, cost', s => s.agent_result ? tok(s.agent_result.n_input_tokens) + ' / ' + tok(s.agent_result.n_output_tokens) + ' ' + usd(s.agent_result.cost_usd) : '']], r.step_results);
  }
  h += `<p>Showing: <select id="step"><option value="">trial root</option>${steps.map(s => `<option${s === steps[steps.length - 1] ? ' selected' : ''}>${esc(s)}</option>`).join('')}</select></p><div id="scoped">Loading...</div>` +
    (ex || exc ? sect('Exception', (ex ? pre(ex.exception_message, 'err') + sect('traceback', pre(ex.exception_traceback)) : '') + (exc ? sect('exception.txt', pre(exc)) : ''), true) : '') +
    (an ? sect('Analysis', analysisHtml(an), true) : '') +
    sect('trial.log', pre(log)) + sect('result.json', pre(pretty(r)));
  app.innerHTML = h;
  const sel = document.getElementById('step');
  const load = async () => {
    const step = sel.value, q = step ? '?step=' + enc(step) : '';
    const el = document.getElementById('scoped');
    el.innerHTML = 'Loading...';
    try {
      const [v, a, t, files] = await Promise.all([api(base + '/verifier-output' + q), api(base + '/artifacts' + q), api(base + '/trajectory' + q), api(base + '/files' + q)]);
      el.innerHTML =
        sect(`Trajectory${t ? ' (' + t.steps.length + ' steps)' : ''}`, trajectoryHtml(t), !!t) +
        sect('Verifier', `<b>reward</b> ${v.reward ? pre(pretty(v.reward)) : pre(v.reward_txt)}` + (v.reward_details ? sect('reward-details.json', pre(pretty(v.reward_details))) : '') +
          `<b>test-stdout.txt</b>${pre(v.stdout)}` + (v.stderr ? `<b>test-stderr.txt</b>${pre(v.stderr)}` : '') + (v.ctrf ? sect('ctrf.json', pre(v.ctrf)) : ''), true) +
        sect('Agent logs', treeHtml(files, 'agent/', 'agentview')) +
        sect(`Artifacts (${a.files.length})`, (a.files.length ? treeHtml(a.files.map(f => ({...f, path: 'artifacts/' + f.path})), 'artifacts/', 'artview') : '<p class="muted">No artifacts.</p>') +
          (a.manifest ? sect('manifest.json', pre(pretty(a.manifest)), true) : '')) +
        sect('All files', treeHtml(files, '', 'allview'));
      el.querySelectorAll('[data-file]').forEach(d => d.onclick = async () => {
        const target = document.getElementById(d.dataset.view), p = d.dataset.file, url = base + '/files/' + filePath(p) + q;
        if (/\.(png|jpe?g|gif|webp)$/i.test(p)) { target.innerHTML = `<b>${esc(p)}</b><br><img style="max-width:100%" src="${esc(url)}">`; return; }
        const res = await fetch(url), body = await res.text();
        let shown = body;
        if (res.ok && /\.json$/i.test(p)) { try { shown = pretty(JSON.parse(body)); } catch (e) {} }
        if (res.ok && /\.jsonl$/i.test(p)) { shown = body.split('\n').filter(Boolean).map(l => { try { return pretty(JSON.parse(l)); } catch (e) { return l; } }).join('\n'); }
        target.innerHTML = `<b>${esc(p)}</b>` + pre(res.ok ? shown : res.status + ' ' + body, res.ok ? '' : 'err');
      });
    } catch (e) { el.innerHTML = pre(String(e), 'err'); }
  };
  sel.onchange = load;
  load();
}

async function route() {
  const parts = location.hash.replace(/^#\/?/, '').split('/').filter(Boolean).map(decodeURIComponent);
  try {
    if (parts[0] === 'job' && parts[2] === 'trial') await trialView(parts[1], parts[3]);
    else if (parts[0] === 'job') await jobView(parts[1]);
    else await jobsView();
  } catch (e) { app.innerHTML = pre(String(e), 'err'); }
}
window.onhashchange = route;
route();
</script></body></html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobResult;

    fn trial_result(task: &str, trial: &str, reward: Option<f64>, err: Option<&str>) -> Value {
        let now = chrono::Utc::now();
        let t = json!({"started_at": now, "finished_at": now + chrono::Duration::seconds(3)});
        let r: TrialResult = serde_json::from_value(json!({
            "id": uuid::Uuid::nil(), "task_name": task, "trial_name": trial, "trial_uri": "", "task_id": {"path": task}, "source": "ds", "task_checksum": "",
            "config": {"task": {"path": task}, "trial_name": trial, "trials_dir": "j", "agent": {"name": "claude-code", "model_name": "anthropic/m"}},
            "agent_info": {"name": "claude-code", "version": "2.1", "model_info": {"name": "m", "provider": "anthropic"}},
            "agent_result": {"n_input_tokens": 100, "n_cache_tokens": 10, "n_output_tokens": 5, "cost_usd": 0.5, "metadata": null},
            "verifier_result": reward.map(|r| json!({"rewards": {"reward": r}})),
            "exception_info": err.map(|e| json!({"exception_type": e, "exception_message": "boom", "exception_traceback": "tb", "occurred_at": now})),
            "started_at": now, "finished_at": now + chrono::Duration::seconds(10),
            "environment_setup": t, "agent_setup": t, "agent_execution": t, "verifier": t,
        }))
        .unwrap();
        serde_json::to_value(r).unwrap()
    }

    /// jobs/<job>/{result.json, config.json, <trial>/...} built from the real structs.
    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!("hbench-view-{}", uuid::Uuid::new_v4()));
        let job = root.join("job-1");
        let ok = job.join("t1__abc");
        let bad = job.join("t2__def");
        for d in [ok.join("agent/sub"), ok.join("verifier"), ok.join("artifacts/out"), bad.join("agent"), job.join("t3__run")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let results = [trial_result("t1", "t1__abc", Some(1.0), None), trial_result("t2", "t2__def", None, Some("AgentTimeoutError"))];
        let trs: Vec<TrialResult> = results.iter().map(|v| serde_json::from_value(v.clone()).unwrap()).collect();
        let jr = JobResult { id: uuid::Uuid::nil(), started_at: chrono::Utc::now(), updated_at: None, finished_at: None, n_total_trials: 3, stats: crate::job::compute_stats(&trs, 3, 1, 0, &[]) };
        std::fs::write(job.join("result.json"), serde_json::to_string(&jr).unwrap()).unwrap();
        std::fs::write(job.join("config.json"), r#"{"agents":[{"name":"claude-code","model_name":"anthropic/m"}]}"#).unwrap();
        std::fs::write(ok.join("result.json"), results[0].to_string()).unwrap();
        std::fs::write(ok.join("config.json"), "{}").unwrap();
        std::fs::write(ok.join("trial.log"), "log line\n").unwrap();
        std::fs::write(ok.join("agent/sub/out.txt"), "agent says hi").unwrap();
        std::fs::write(ok.join("agent/blob.bin"), [0xff, 0xfe, 0x00]).unwrap();
        std::fs::write(ok.join("agent/trajectory.json"), r#"{"schema_version":"ATIF-v1.7","agent":{"name":"x","version":"1"},"steps":[]}"#).unwrap();
        std::fs::write(ok.join("verifier/test-stdout.txt"), "PASSED").unwrap();
        std::fs::write(ok.join("verifier/reward.txt"), "1").unwrap();
        std::fs::write(ok.join("artifacts/manifest.json"), r#"[{"source":"/app/out","ok":true}]"#).unwrap();
        std::fs::write(ok.join("artifacts/out/a.txt"), "art").unwrap();
        std::fs::write(bad.join("result.json"), results[1].to_string()).unwrap();
        std::fs::write(bad.join("exception.txt"), "Traceback").unwrap();
        std::fs::write(job.join("t3__run/config.json"), r#"{"agent":{"name":"oracle"}}"#).unwrap();
        std::fs::create_dir_all(bad.join("steps/one/verifier")).unwrap();
        std::fs::write(bad.join("steps/one/verifier/test-stdout.txt"), "step out").unwrap();
        std::fs::write(root.join("secret.txt"), "top secret").unwrap();
        root.canonicalize().unwrap()
    }

    fn get(root: &Path, url: &str) -> (u16, Value) {
        let r = handle(root, url);
        (r.status, serde_json::from_slice(&r.body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&r.body).into_owned())))
    }

    #[test]
    fn api_handlers() {
        let root = fixture();
        assert!(String::from_utf8(handle(&root, "/").body).unwrap().contains("hbench viewer"));
        let (s, jobs) = get(&root, "/api/jobs");
        assert_eq!((s, jobs[0]["name"].as_str()), (200, Some("job-1")));
        let st = &jobs[0]["result"]["stats"];
        assert_eq!((st["n_completed_trials"].as_u64(), st["n_errored_trials"].as_u64(), st["cost_usd"].as_f64()), (Some(2), Some(1), Some(1.0)));
        assert_eq!(st["evals"]["claude-code__m__ds"]["metrics"][0]["mean"], 0.5);
        assert_eq!(get(&root, "/api/jobs/job-1").1["config"]["agents"][0]["name"], "claude-code");

        let (_, trials) = get(&root, "/api/jobs/job-1/trials");
        let t = trials.as_array().unwrap();
        assert_eq!(t.iter().map(|t| t["name"].as_str().unwrap()).collect::<Vec<_>>(), ["t1__abc", "t2__def", "t3__run"]);
        assert_eq!((t[0]["reward"].as_f64(), t[0]["model_name"].as_str(), t[0]["duration_sec"].as_f64(), t[0]["durations"]["agent_execution"].as_f64()), (Some(1.0), Some("m"), Some(10.0), Some(3.0)));
        assert_eq!((t[1]["error_type"].as_str(), t[1]["reward"].is_null(), t[1]["cost_usd"].as_f64()), (Some("AgentTimeoutError"), true, Some(0.5)));
        assert_eq!((t[2]["status"].as_str(), t[2]["agent_name"].as_str()), (Some("running"), Some("oracle")));

        let b = "/api/jobs/job-1/trials/t1__abc";
        assert_eq!(get(&root, b).1["task_name"], "t1");
        let (_, files) = get(&root, &format!("{b}/files"));
        assert!(files.as_array().unwrap().iter().any(|f| f["path"] == "agent/sub/out.txt" && f["size"] == 13 && f["is_dir"] == false));
        assert!(files.as_array().unwrap().iter().any(|f| f["path"] == "agent/sub" && f["is_dir"] == true));
        assert_eq!(get(&root, &format!("{b}/files/agent/sub/out.txt")), (200, Value::String("agent says hi".into())));
        assert_eq!(get(&root, &format!("{b}/files/trial.log")).1, "log line\n");
        assert_eq!(get(&root, &format!("{b}/files/agent/blob.bin")).0, 415);
        assert_eq!(get(&root, &format!("{b}/files/agent")).0, 400);
        assert_eq!(get(&root, &format!("{b}/files/nope.txt")).0, 404);
        let (_, v) = get(&root, &format!("{b}/verifier-output"));
        assert_eq!((v["stdout"].as_str(), v["reward_txt"].as_str(), v["stderr"].is_null()), (Some("PASSED"), Some("1"), true));
        let (_, a) = get(&root, &format!("{b}/artifacts"));
        assert_eq!((a["files"][0]["path"].as_str(), a["files"].as_array().unwrap().len(), a["manifest"][0]["ok"].as_bool()), (Some("out/a.txt"), 1, Some(true)));
        assert_eq!(get(&root, &format!("{b}/trajectory")).1["schema_version"], "ATIF-v1.7");
        assert_eq!(get(&root, "/api/jobs/job-1/trials/t2__def/trajectory"), (200, Value::Null));
        assert_eq!(get(&root, "/api/jobs/job-1/trials/t2__def/verifier-output?step=one").1["stdout"], "step out");
        assert_eq!(get(&root, "/api/jobs/job-1/trials/t2__def/verifier-output?step=two").0, 404);
        assert_eq!(get(&root, "/api/jobs/nope").0, 404);
        assert_eq!(get(&root, "/api/whatever").0, 404);
    }

    #[test]
    fn path_traversal_guard() {
        let root = fixture();
        let job = root.join("job-1");
        let b = "/api/jobs/job-1/trials/t1__abc";
        for url in [
            "/api/jobs/..".to_string(),
            "/api/jobs/%2E%2E".into(),
            "/api/jobs/job-1/trials/..%2F..%2Fsecret.txt".into(),
            format!("{b}/files/../../../secret.txt"),
            format!("{b}/files/%2E%2E/%2E%2E/%2E%2E/secret.txt"),
            format!("{b}/files/%2Fetc%2Fpasswd"),
            format!("{b}/files/sub%2F..%2F..%2F..%2Fsecret.txt"),
            format!("{b}/verifier-output?step=../../.."),
        ] {
            let (s, body) = get(&root, &url);
            assert!(s == 400 || s == 403, "{url} -> {s} {body}");
            assert!(!body.to_string().contains("top secret"));
        }
        // a symlink escaping the jobs dir is refused even though it is lexically inside
        std::os::unix::fs::symlink(root.join("secret.txt"), job.join("t1__abc/agent/link.txt")).unwrap();
        assert_eq!(get(&root, &format!("{b}/files/agent/link.txt")).0, 403);
        assert_eq!(under(&job, &["t1__abc", "trial.log"]).unwrap(), job.join("t1__abc/trial.log"));
        assert!(under(&job, &["/etc"]).is_err() && under(&job, &["a/b"]).is_err() && under(&job, &[".."]).is_err());
    }
}
