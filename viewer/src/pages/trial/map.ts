// Trajectory map model (no React): collapse an ATIF step list into groups of the same kind of work, in order.
import type { TrajectoryStep } from "../../lib/types";

export const LANES = ["agent", "explore", "run", "install", "edit", "web"] as const;
export type Lane = (typeof LANES)[number];

export interface MapGroup {
  /** Indexes into the step list, inclusive. */
  from: number;
  to: number;
  lane: Lane;
  title: string;
  /** Distinct commands, files or URLs, in first-seen order. */
  targets: string[];
  /** Files read or changed, for revisit edges. */
  files: string[];
  calls: number;
  errors: number;
  /** Reached the benchmark's own sources over the network. */
  benchmark: boolean;
  costUsd: number | null;
  durationMs: number | null;
}

export interface MapEdge {
  from: number;
  to: number;
  revisit: boolean;
}

export const SHELL_TOOLS = /^(bash|shell|sh|exec|exec_command|run|run_command|run_shell_command|execute_command|terminal|local_shell|command)$/i;
const INSTALL = /\b(pip3?|uv|apt(-get)?|apk|yum|dnf|brew|npm|pnpm|yarn|cargo|gem|go)\s+(\S+\s+)?(install|add|get)\b/;
const FETCH = /\b(curl|wget|git\s+clone|urllib|httpx|requests\.(get|post))\b/;
const INSPECT = /^(ls|cat|head|tail|grep|rg|find|file|wc|pwd|tree|stat|sed\s+-n)\b/;
// ponytail: names the sources of the datasets seen so far; a new benchmark needs its host here or as the trial's source.
const BENCHMARK = "harbor-framework|laude-institute|terminal-bench|tbench\\.ai|solution/solve\\.sh|tests/test_outputs\\.py";
const TITLES: Record<Lane, string> = { agent: "Reply", explore: "Explore", run: "Run", install: "Install", edit: "Edit", web: "Web" };

function argText(args: unknown): string {
  return typeof args === "string" ? args : JSON.stringify(args ?? "");
}

function field(args: unknown, keys: string[]): string {
  if (!args || typeof args !== "object") return "";
  const v = keys.map((k) => (args as Record<string, unknown>)[k]).find((x) => typeof x === "string" && x);
  return (v as string | undefined) ?? "";
}

const commandOf = (args: unknown) => (typeof args === "string" ? args : field(args, ["command", "cmd", "script"]));

/** The program a shell line runs: skips `cd x &&` and `VAR=1` prefixes. */
function program(cmd: string): string {
  const parts = cmd.trim().split(/\s*(?:&&|;|\n)\s*/).filter((p) => !/^(cd|set|export|source)\b/.test(p));
  const words = (parts[0] ?? cmd).trim().split(/\s+/).filter((w) => !/^\w+=/.test(w));
  return words[0] ?? "";
}

function callLane(name: string, args: unknown): Lane {
  if (SHELL_TOOLS.test(name)) {
    const cmd = commandOf(args).trim();
    if (INSTALL.test(cmd)) return "install";
    if (FETCH.test(cmd)) return "web";
    return INSPECT.test(cmd) ? "explore" : "run";
  }
  const n = name.toLowerCase();
  if (/web|fetch|browser|url/.test(n)) return "web";
  if (/todo/.test(n)) return "explore";
  if (/write|edit|patch|replace|create|insert|delete/.test(n)) return "edit";
  if (/read|view|list|glob|grep|search|find|open|^ls$|^cat$/.test(n)) return "explore";
  return "run";
}

function target(name: string, args: unknown, lane: Lane): string {
  const file = field(args, ["file_path", "filePath", "path", "notebook_path"]);
  const url = field(args, ["url", "uri"]);
  if (file || url) return file || url;
  const queries = (args as { queries?: unknown } | null)?.queries;
  if (Array.isArray(queries)) return String(queries[0] ?? name);
  const query = field(args, ["query", "pattern"]);
  if (query) return query;
  if (!SHELL_TOOLS.test(name)) return name;
  const cmd = commandOf(args);
  if (lane === "web") return cmd.match(/https?:\/\/[^\s'"]+/)?.[0] ?? program(cmd);
  return (lane === "run" ? program(cmd) : cmd.trim().split("\n")[0]).slice(0, 90);
}

const RANK: Lane[] = ["edit", "install", "web", "run", "explore"];

/** Group steps. An agent step with no tool calls joins the running group, unless it's the last word. */
export function buildGroups(steps: TrajectoryStep[], source?: string | null): MapGroup[] {
  const known = new RegExp(source ? `${BENCHMARK}|${source.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}` : BENCHMARK, "i");
  const groups: MapGroup[] = [];
  steps.forEach((s, i) => {
    const calls = s.tool_calls ?? [];
    const lanes = calls.map((c) => callLane(c.function_name, c.arguments));
    const lane: Lane = s.source !== "agent" ? "agent" : (RANK.find((l) => lanes.includes(l)) ?? "agent");
    const fetches = calls.some((c, k) => lanes[k] === "web" || FETCH.test(commandOf(c.arguments)));
    const benchmark = fetches && calls.some((c) => known.test(argText(c.arguments)));
    const last = groups[groups.length - 1];
    const thinking = s.source === "agent" && !calls.length && i < steps.length - 1;
    const group =
      last && (thinking || (last.lane === lane && last.benchmark === benchmark && lane !== "agent"))
        ? last
        : (groups[groups.push({ from: i, to: i, lane, title: s.source === "agent" ? TITLES[lane] : "Task", targets: [], files: [], calls: 0, errors: 0, benchmark, costUsd: null, durationMs: null }) - 1]);
    group.to = i;
    group.calls += calls.length;
    group.errors += (s.observation?.results ?? []).filter((r) => r.extra?.is_error).length;
    const cost = s.metrics?.cost_usd;
    if (typeof cost === "number") group.costUsd = (group.costUsd ?? 0) + cost;
    calls.forEach((c, k) => {
      const t = target(c.function_name, c.arguments, lanes[k]);
      if (t && !group.targets.includes(t)) group.targets.push(t);
      if ((lanes[k] === "explore" || lanes[k] === "edit") && field(c.arguments, ["file_path", "filePath", "path"]) && !group.files.includes(t)) group.files.push(t);
    });
  });
  // A group runs until the next one starts, so a long command counts towards the group that ran it.
  groups.forEach((g, i) => {
    const start = Date.parse(steps[g.from].timestamp ?? "");
    const end = Date.parse(steps[groups[i + 1]?.from ?? g.to].timestamp ?? "");
    if (Number.isFinite(start) && Number.isFinite(end)) g.durationMs = end - start;
  });
  return groups;
}

/** The order of work, plus a dashed edge back to the latest earlier group that touched the same file. */
export function buildEdges(groups: MapGroup[]): MapEdge[] {
  const edges: MapEdge[] = groups.slice(1).map((_, i) => ({ from: i, to: i + 1, revisit: false }));
  groups.forEach((g, i) => {
    for (let j = i - 2; j >= 0; j--) {
      if (g.files.some((f) => groups[j].files.includes(f))) {
        edges.push({ from: i, to: j, revisit: true });
        break;
      }
    }
  });
  return edges;
}
