// hbench driver for an Eve app: boots `eve start`, sends the instruction over Eve's
// session API, then writes the event log and a result summary under the logs dir.
import { spawn } from "node:child_process";
import { createWriteStream } from "node:fs";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

const env = process.env;
const must = (k) => {
  if (!env[k]) throw new Error(`${k} must be set`);
  return env[k];
};

const cfg = {
  project: must("EVE_PROJECT_DIR"),
  instruction: must("EVE_INSTRUCTION_PATH"),
  logs: env.EVE_AGENT_LOGS_DIR ?? "/logs/agent",
  host: env.EVE_HOST ?? "127.0.0.1",
  port: Number(env.EVE_PORT ?? "3024"),
  bootMs: Number(env.EVE_STARTUP_TIMEOUT_MS ?? "60000"),
};
const base = `http://${cfg.host}:${cfg.port}`;
const out = (name) => join(cfg.logs, name);

// Status values that mean the session settled normally.
const SETTLED = new Set(["waiting", "completed", "session.waiting", "session.completed"]);

// Usage field aliases seen across Eve providers; first finite number wins.
const ALIASES = {
  inputTokens: ["inputTokens", "input_tokens", "promptTokens", "prompt_tokens", "totalPromptTokens"],
  outputTokens: ["outputTokens", "output_tokens", "completionTokens", "completion_tokens", "totalCompletionTokens"],
  cachedTokens: ["cachedTokens", "cached_tokens", "cachedInputTokens", "cached_input_tokens"],
  costUsd: ["costUsd", "cost_usd", "totalCostUsd", "total_cost_usd", "totalCost"],
};

function tally(events) {
  const sum = Object.fromEntries(Object.keys(ALIASES).map((k) => [k, 0]));
  for (const ev of events) {
    if (ev.type !== "step.completed") continue;
    const src = ev.data?.usage ?? ev.data ?? {};
    for (const [k, names] of Object.entries(ALIASES)) {
      const hit = names.map((n) => src[n]).find((v) => typeof v === "number" && Number.isFinite(v));
      sum[k] += hit ?? 0;
    }
  }
  // Zero totals are reported as absent.
  return Object.fromEntries(Object.entries(sum).map(([k, v]) => [k, v || undefined]));
}

function diagnose(events, status) {
  for (const ev of events) {
    if (ev.type === "input.requested") {
      return { type: ev.type, message: "Eve requested human input during a non-interactive hbench run", data: ev.data };
    }
    if (ev.type === "turn.failed" || ev.type === "session.failed") {
      const message = ev.data?.message ?? ev.data?.error?.message ?? `${ev.type} emitted during Eve run`;
      return { type: ev.type, message, data: ev.data };
    }
  }
  if (SETTLED.has(status)) return null;
  return { type: "session.unsettled", message: `Eve run ended with unexpected status: ${status ?? "unknown"}` };
}

function boot() {
  const log = createWriteStream(out("eve-server.log"), { flags: "a" });
  const proc = spawn(join(cfg.project, "node_modules", ".bin", "eve"), ["start", "--host", cfg.host, "--port", String(cfg.port)], {
    cwd: cfg.project,
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  proc.stdout.pipe(log, { end: false });
  proc.stderr.pipe(log, { end: false });
  proc.once("exit", (code, signal) => log.write(`\n[eve-server exited code=${code ?? ""} signal=${signal ?? ""}]\n`));
  return proc;
}

async function healthy(proc) {
  const stopAt = Date.now() + cfg.bootMs;
  let why;
  while (Date.now() < stopAt) {
    if (proc.exitCode !== null) throw new Error(`eve start exited before becoming healthy (code ${proc.exitCode})`);
    try {
      const r = await fetch(`${base}/eve/v1/health`);
      if (r.ok) return;
      why = new Error(`health returned ${r.status}`);
    } catch (e) {
      why = e;
    }
    await sleep(500);
  }
  throw new Error(`Timed out waiting for eve start health: ${why?.message ?? "unknown"}`);
}

async function shutdown(proc) {
  if (proc.exitCode !== null) return;
  proc.kill("SIGTERM");
  const gone = await Promise.race([new Promise((r) => proc.once("exit", () => r(true))), sleep(5000, false)]);
  if (!gone && proc.exitCode === null) proc.kill("SIGKILL");
}

// The client ships inside the app's own `eve` dependency.
async function loadClient() {
  const req = createRequire(join(cfg.project, "package.json"));
  return import(pathToFileURL(req.resolve("eve/client")).href);
}

const saveResult = (obj) => writeFile(out("eve-result.json"), `${JSON.stringify(obj, null, 2)}\n`);

await mkdir(cfg.logs, { recursive: true });
let proc;
let saved = false;
try {
  proc = boot();
  await healthy(proc);
  const text = await readFile(cfg.instruction, "utf8");
  const { Client } = await loadClient();
  const reply = await new Client({ host: base }).session().send(text);
  const res = await reply.result();
  const events = res.events ?? [];
  await writeFile(out("eve-events.ndjson"), events.map((e) => JSON.stringify(e)).join("\n") + "\n");

  const failure = diagnose(events, res.status);
  await saveResult({
    status: res.status,
    sessionId: res.sessionId,
    continuationToken: reply.continuationToken,
    message: res.message,
    data: res.data,
    inputRequests: res.inputRequests ?? [],
    usage: tally(events),
    eventCount: events.length,
    failure,
  });
  saved = true;
  if (failure) throw new Error(failure.message);
} catch (e) {
  if (!saved) {
    await saveResult({ status: "failed", inputRequests: [], usage: {}, eventCount: 0, failure: { type: "runner.error", message: e?.message ?? String(e) } });
  }
  throw e;
} finally {
  if (proc) await shutdown(proc);
}
