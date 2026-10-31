// Check for map.ts (not bundled): `npx tsx src/pages/trial/map.check.ts` from viewer/.
import { buildEdges, buildGroups } from "./map";
import type { TrajectoryStep } from "../../lib/types";

const step = (i: number, source: string, calls: [string, unknown][] = [], extra: Partial<TrajectoryStep> = {}): TrajectoryStep => ({
  step_id: i,
  source,
  timestamp: new Date(1000 * i).toISOString(),
  tool_calls: calls.map(([function_name, args]) => ({ tool_call_id: "", function_name, arguments: args as any })),
  ...extra,
});
const bash = (command: string): [string, unknown] => ["bash", { command }];
const steps = [
  step(0, "user"),
  step(1, "agent", [["read", { file_path: "/app/a.py" }]]),
  step(2, "agent", [bash("ls -la")]),
  step(3, "agent", [bash("cd /app && python3 x.py")], { metrics: { cost_usd: 0.5 }, observation: { results: [{ extra: { is_error: true } }] } }),
  step(4, "agent"),
  step(5, "agent", [bash("X=1 python3 y.py")], { metrics: { cost_usd: 0.25 } }),
  step(6, "agent", [bash("pip install --quiet numpy")]),
  step(7, "agent", [["web_search", { queries: ["chess"] }]]),
  step(8, "agent", [["web_fetch", { url: "https://raw.githubusercontent.com/harbor-framework/terminal-bench-2/main/x/README.md" }]]),
  step(9, "agent", [bash("curl -sSfL https://example.com/my-bench/solve && cat README")]),
  step(10, "agent", [["write", { file_path: "/app/a.py", content: "curl terminal-bench" }]]),
  step(11, "agent"),
];
const g = buildGroups(steps, "my-bench");
const view = g.map((x) => `${x.lane}:${x.from}-${x.to}`);
console.assert(view.join() === "agent:0-0,explore:1-2,run:3-5,install:6-6,web:7-7,web:8-9,edit:10-10,agent:11-11", view.join());
console.assert(g[2].calls === 2 && g[2].errors === 1 && g[2].costUsd === 0.75 && g[2].durationMs === 3000 && g[2].targets.join() === "python3", JSON.stringify(g[2]));
console.assert(g.map((x) => x.benchmark).join() === "false,false,false,false,false,true,false,false", "benchmark flag: fetches only, source included");
console.assert(g[0].title === "Task" && g[7].title === "Reply", "titles");
console.assert(g[5].targets[1] === "https://example.com/my-bench/solve", "a fetch shows its URL", g[5].targets.join());
const e = buildEdges(g);
console.assert(e.filter((x) => !x.revisit).length === g.length - 1, "one forward edge per gap");
console.assert(JSON.stringify(e.filter((x) => x.revisit)) === JSON.stringify([{ from: 6, to: 1, revisit: true }]), "revisit edge", JSON.stringify(e));
console.assert(buildGroups([]).length === 0, "empty");
console.log("map.ts ok");
