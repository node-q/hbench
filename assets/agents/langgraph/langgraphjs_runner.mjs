// Runs one LangGraph.js graph on a task instruction inside the task container.
// Invoked by hbench's `langgraph` agent; writes the final state, the answer text and an
// answer/token-usage summary. Same CLI as langgraph_runner.py plus --runner-tools-dir,
// where hbench installs tsx and a fallback copy of langsmith.
import { createRequire } from "node:module";
import { readFile, realpath, writeFile } from "node:fs/promises";
import { dirname, isAbsolute, relative, resolve, sep } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

const REQUIRED = ["project-dir", "graph", "graph-path", "instruction-file", "result-path", "output-path"];
const OPTIONAL = ["runner-tools-dir", "summary-path", "model", "model-kwargs-json", "configurable-json"];
const NO_TRACE = "LangSmith trace context unavailable; continuing without parent trace";
const IDENT = /^[A-Za-z_$][A-Za-z0-9_$]*$/;

const camel = (flag) => flag.replace(/-(\w)/g, (_, c) => c.toUpperCase());
const isObj = (v) => v !== null && typeof v === "object";
const isPlain = (v) => isObj(v) && [Object.prototype, null].includes(Object.getPrototypeOf(v));
const jsonOut = (v) => `${JSON.stringify(v, null, 2)}\n`;

function readOptions(argv) {
  const { values } = parseArgs({
    args: argv,
    strict: true,
    options: Object.fromEntries([...REQUIRED, ...OPTIONAL].map((f) => [f, { type: "string" }])),
  });
  const missing = REQUIRED.find((f) => !values[f]);
  if (missing) throw new Error(`Missing required argument: --${missing}`);
  return Object.fromEntries(Object.entries(values).map(([k, v]) => [camel(k), v]));
}

function objectArg(raw, what) {
  if (!raw) return {};
  let v;
  try {
    v = JSON.parse(raw);
  } catch {
    v = undefined;
  }
  if (!isPlain(v)) throw new TypeError(`${what} must be a JSON object`);
  return v;
}

const outside = (root, p) => {
  const rel = relative(root, p);
  return isAbsolute(rel) || rel === ".." || rel.startsWith(`..${sep}`);
};

function splitGraphPath(spec) {
  const parts = spec.split(":");
  const [file, attr] = parts;
  if (parts.length !== 2 || !file || !attr || !attr.split(".").every((p) => IDENT.test(p))) {
    throw new Error("Invalid graph path. Expected './path/to/file.mjs:attribute'");
  }
  return [file, attr.split(".")];
}

async function importTarget(root, spec) {
  const [file, attrs] = splitGraphPath(spec);
  const path = await realpath(resolve(root, file));
  if (outside(root, path)) throw new Error("Graph module must resolve within the LangGraph project directory");
  let v = await import(pathToFileURL(path).href);
  for (const a of attrs) {
    if (v == null || !(a in Object(v))) throw new Error("Graph export was not found");
    v = v[a];
  }
  return v;
}

// Accept a compiled graph, a StateGraph builder (compiled here) or a factory taking the
// run config; at most three unwrapping steps.
async function invoker(target, config) {
  let g = target;
  for (let i = 0; i < 3; i++) {
    if (typeof g?.invoke === "function") return (input, cfg) => g.invoke(input, cfg);
    if (typeof g?.compile === "function") g = await g.compile();
    else if (typeof g === "function") g = await g(config);
    else break;
  }
  throw new TypeError("Selected graph must expose invoke()");
}

// ---------------------------------------------------------------- LangSmith parent trace

// The tools dir is only used to require() langsmith, so anything but an absolute,
// canonical, `..`-free path is ignored (the run just goes untraced).
async function trustedToolsDir(dir) {
  if (!dir || !isAbsolute(dir) || dir.split(sep).includes("..")) return undefined;
  const real = await realpath(dir).catch(() => undefined);
  return real === dir ? real : undefined;
}

function langsmithPaths(req) {
  return { runTrees: req.resolve("langsmith/run_trees"), traceable: req.resolve("langsmith/traceable") };
}

async function locateLangsmith(root, toolsDir) {
  try {
    return langsmithPaths(createRequire(resolve(root, "package.json")));
  } catch (err) {
    if (!toolsDir) throw err;
    const pkg = await realpath(resolve(toolsDir, "node_modules", "langsmith", "package.json"));
    if (dirname(dirname(dirname(pkg))) !== toolsDir) throw new Error("langsmith must be installed directly in the runner tools directory");
    return langsmithPaths(createRequire(pkg));
  }
}

async function parentTrace(root, toolsDir) {
  const trace = process.env.HBENCH_LANGSMITH_PARENT;
  if (!trace) return undefined;
  try {
    const paths = await locateLangsmith(root, toolsDir);
    const { RunTree } = await import(pathToFileURL(paths.runTrees).href);
    const { withRunTree } = await import(pathToFileURL(paths.traceable).href);
    const headers = { "langsmith-trace": trace };
    if (process.env.HBENCH_LANGSMITH_BAGGAGE) headers.baggage = process.env.HBENCH_LANGSMITH_BAGGAGE;
    const parent = RunTree.fromHeaders(headers);
    return parent && typeof withRunTree === "function" ? (fn) => withRunTree(parent, fn) : undefined;
  } catch {
    // Never print the header values.
    console.error(NO_TRACE);
    return undefined;
  }
}

async function traced(wrap, fn) {
  if (!wrap) return fn();
  let entered = false;
  try {
    return await wrap(() => {
      entered = true;
      return fn();
    });
  } catch (err) {
    // A failure inside the graph is the graph's; only tracing set-up failures fall back.
    if (entered) throw err;
    console.error(NO_TRACE);
    return fn();
  }
}

// ---------------------------------------------------------------- serialisation

// `stack` holds only the current ancestors, so shared (non-cyclic) references are
// serialised every time they appear and only real cycles become "[Circular]".
function plain(v, stack = new WeakSet()) {
  switch (typeof v) {
    case "string":
    case "boolean":
      return v;
    case "number":
      return Number.isFinite(v) ? v : null;
    case "bigint":
      return v.toString();
    case "undefined":
      return null;
    case "symbol":
    case "function":
      return String(v);
  }
  if (v === null) return null;
  if (stack.has(v)) return "[Circular]";
  stack.add(v);
  try {
    if (Array.isArray(v)) return v.map((x) => plain(x, stack));
    if (v instanceof Date) return Number.isNaN(v.valueOf()) ? null : v.toISOString();
    if (isPlain(v) || ("type" in v && "content" in v)) {
      const out = {};
      for (const k of Object.keys(v)) {
        try {
          out[k] = plain(v[k], stack);
        } catch {
          out[k] = "[Unreadable]";
        }
      }
      return out;
    }
  } finally {
    stack.delete(v);
  }
  try {
    return String(v);
  } catch {
    return "[Unserializable]";
  }
}

// Only text blocks count: tool calls, reasoning and images stay in result.json and must
// not leak into the graded answer (or make a tool-call-only message look non-blank).
function textOf(content) {
  if (content == null) return undefined;
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return JSON.stringify(plain(content));
  return content
    .map((b) => (typeof b === "string" ? b : typeof b?.text === "string" ? b.text : typeof b?.content === "string" ? b.content : ""))
    .join("");
}

function answerOf(state, serialised) {
  const msgs = Array.isArray(state?.messages) ? state.messages : [];
  for (let i = msgs.length - 1; i >= 0; i--) {
    const t = textOf(msgs[i]?.content);
    if (t?.trim()) return t;
  }
  return JSON.stringify(serialised, null, 2);
}

// ---------------------------------------------------------------- token usage

// Providers spell usage fields differently; each total takes the first finite one.
const PICKS = {
  input_tokens: (u) => [u.input_tokens, u.inputTokens, u.promptTokens],
  output_tokens: (u) => [u.output_tokens, u.outputTokens, u.completionTokens],
  total_tokens: (u) => [u.total_tokens, u.totalTokens],
  cache_read_tokens: (u) => {
    const d = u.input_token_details;
    const D = u.inputTokenDetails;
    return [u.cache_read_tokens, u.cacheReadTokens, d?.cache_read, d?.cacheRead, D?.cache_read, D?.cacheRead];
  },
};

const zeroUsage = () => Object.fromEntries(Object.keys(PICKS).map((k) => [k, 0]));

function firstNumber(values) {
  for (const v of values) {
    if (v == null) continue;
    const n = Number(v);
    if (Number.isFinite(n)) return n;
  }
  return undefined;
}

/** Add `usage` into `into`; false when it carried no recognised field. */
function addUsage(into, usage) {
  if (!isObj(usage)) return false;
  let hit = false;
  for (const [k, pick] of Object.entries(PICKS)) {
    const n = firstNumber(pick(usage));
    if (n !== undefined) {
      into[k] += n;
      hit = true;
    }
  }
  return hit;
}

const usageOf = (msg) => msg?.usage_metadata ?? msg?.usageMetadata;

/**
 * Callback handler that sums usage over every model call in the run tree. Summing the
 * final state's messages misses subagents (their messages never reach the parent state)
 * and anything history-trimming middleware dropped; callbacks are inherited by child
 * runs, so this sees all of them.
 */
function usageMeter() {
  const total = zeroUsage();
  const seen = new Set();
  let runs = 0;
  return {
    total,
    runs: () => runs,
    // No `name`: langchain only wraps nameless plain-object handlers. awaitHandlers makes
    // handleLLMEnd run inline, so the last call is counted before summary.json is written.
    handler: {
      awaitHandlers: true,
      handleLLMEnd(output, runId) {
        // Nested runs reach this handler once per enclosing callback manager.
        if (typeof runId === "string") {
          if (seen.has(runId)) return;
          seen.add(runId);
        }
        let hit = false;
        for (const gens of Array.isArray(output?.generations) ? output.generations : []) {
          for (const g of Array.isArray(gens) ? gens : []) hit = addUsage(total, usageOf(g?.message)) || hit;
        }
        // Run-level usage only when no message carried any, so nothing is counted twice.
        if (!hit) hit = addUsage(total, output?.llmOutput?.tokenUsage);
        if (hit) runs++;
      },
    },
  };
}

// ---------------------------------------------------------------- main

async function run(opts) {
  const root = await realpath(opts.projectDir);
  const toolsDir = await trustedToolsDir(opts.runnerToolsDir);
  const prompt = await readFile(opts.instructionFile, "utf8");
  const target = await importTarget(root, opts.graphPath);

  const kwargs = objectArg(opts.modelKwargsJson, "model_kwargs_json");
  const configurable = objectArg(opts.configurableJson, "configurable_json");
  configurable.thread_id ??= process.env.HBENCH_SESSION_ID ?? crypto.randomUUID();
  if (opts.model) configurable.model ??= opts.model;
  if (Object.keys(kwargs).length) configurable.model_kwargs ??= kwargs;

  const meter = usageMeter();
  const config = {
    configurable,
    callbacks: [meter.handler],
    metadata: { ls_runner: "hbench", hbench_agent: "langgraph", langgraph_graph: opts.graph },
  };
  const invoke = await invoker(target, config);
  const input = { messages: [{ role: "user", content: prompt }] };
  const state = await traced(await parentTrace(root, toolsDir), () => invoke(input, config));

  const serialised = plain(state);
  const answer = answerOf(state, serialised);
  await writeFile(opts.resultPath, jsonOut(serialised));
  await writeFile(opts.outputPath, answer);
  if (opts.summaryPath) {
    // Callback totals and message totals overlap; use one or the other, never both.
    const fromCallbacks = meter.runs() > 0;
    let usage = { ...meter.total };
    if (!fromCallbacks) {
      usage = zeroUsage();
      for (const m of Array.isArray(state?.messages) ? state.messages : []) addUsage(usage, usageOf(m));
    }
    await writeFile(
      opts.summaryPath,
      jsonOut({ answer_written: answer, usage, usage_source: fromCallbacks ? "callbacks" : "messages" }),
    );
  }
}

Promise.resolve()
  .then(() => run(readOptions(process.argv.slice(2))))
  .catch((err) => {
    console.error(err instanceof Error ? err.message : "LangGraph.js runner failed");
    process.exitCode = 1;
  });
