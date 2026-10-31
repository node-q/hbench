// Trajectory as a map: one node per group of similar work, laid out in a lane per kind of work.
import { memo, useMemo } from "react";
import { Background, Controls, Handle, MarkerType, Position, ReactFlow, type Edge, type Node, type NodeProps } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import * as fmt from "../../lib/fmt";
import { useUrlState } from "../../lib/state";
import { Empty, ErrorBox, SkeletonRows } from "../../ui/kit";
import { buildEdges, buildGroups, LANES, type MapGroup } from "./map";
import { useTrajectory } from "./TrajectoryTab";
import type { TrialRef } from "./util";

const LANE_W = 232;
const ROW_H = 104;
const NODE_W = 204;
const LANE_LABELS = { agent: "Agent", explore: "Explore", run: "Run", install: "Install", edit: "Edit", web: "Web" };

type WorkNode = Node<{ g: MapGroup; open: (step: number) => void }, "work">;
type LaneNode = Node<{ label: string }, "lane">;

const GroupCard = memo(function GroupCard({ data: { g, open } }: NodeProps<WorkNode>) {
  const range = g.from === g.to ? `#${g.from + 1}` : `#${g.from + 1}–${g.to + 1}`;
  return (
    <>
      <Handle type="target" position={Position.Top} isConnectable={false} />
      <button className={`tmap__node tmap__node--${g.lane}${g.benchmark ? " is-flagged" : ""}`} onClick={() => open(g.from)} title={g.targets.join("\n")}>
        <span className="tmap__head">
          <span className="tmap__title">{g.benchmark ? "Benchmark source" : g.title}</span>
          {g.calls > 1 && <span className="num muted">×{g.calls}</span>}
          {g.errors > 0 && <span className="tmap__err">{g.errors} failed</span>}
        </span>
        <span className="tmap__target trunc mono">{g.targets[0] ?? " "}</span>
        <span className="tmap__meta num">
          <span>{range}</span>
          {g.durationMs != null && <span>{fmt.duration(g.durationMs)}</span>}
          {g.costUsd != null && <span>{fmt.usd(g.costUsd)}</span>}
        </span>
      </button>
      <Handle type="source" position={Position.Bottom} isConnectable={false} />
    </>
  );
});

const LaneLabel = ({ data }: NodeProps<LaneNode>) => <span className="label">{data.label}</span>;
const nodeTypes = { work: GroupCard, lane: LaneLabel };

export function MapTab({ t, running, source }: { t: TrialRef; running: boolean; source?: string | null }) {
  const res = useTrajectory(t, running);
  const url = useUrlState();
  const steps = res.data?.steps;
  const { nodes, edges, flagged, total } = useMemo(() => {
    const groups = buildGroups(steps ?? [], source);
    const open = (step: number) => url.set({ tab: null, at: step + 1 });
    const nodes: Node[] = [
      ...LANES.map((l, i) => ({ id: `lane-${l}`, type: "lane", position: { x: i * LANE_W, y: -36 }, data: { label: LANE_LABELS[l] }, selectable: false, focusable: false })),
      ...groups.map((g, i) => ({ id: String(i), type: "work", position: { x: LANES.indexOf(g.lane) * LANE_W, y: i * ROW_H }, data: { g, open }, style: { width: NODE_W } })),
    ];
    const edges: Edge[] = buildEdges(groups).map((e) => ({
      id: `${e.from}-${e.to}`,
      source: String(e.from),
      target: String(e.to),
      type: e.revisit ? "default" : "smoothstep",
      className: e.revisit ? "tmap__revisit" : undefined,
      markerEnd: { type: MarkerType.ArrowClosed, color: "var(--color-muted)" },
    }));
    return { nodes, edges, flagged: groups.filter((g) => g.benchmark), total: groups.length };
  }, [steps, source, url.set]);

  if (res.error && !res.data) return <ErrorBox error={res.error} title="Couldn't load the trajectory" onRetry={res.reload} />;
  if (res.data === undefined) return <SkeletonRows rows={10} />;
  if (!steps?.length) return <Empty title={running ? "Waiting for the first step" : "No trajectory"}>{running ? "The map grows here as the agent works." : "This trial has no trajectory to map."}</Empty>;
  return (
    <div className="tmap">
      {flagged.length > 0 && (
        <p className="tmap__warn" role="status">
          This run reached the benchmark's own sources in step {flagged.map((g) => g.from + 1).join(", ")}. A pass may not be the agent's own work. Open the steps to check.
        </p>
      )}
      <p className="muted tmap__note">
        {fmt.int(steps.length)} steps in {fmt.int(total)} groups. Click a group to open its first step.
      </p>
      <div className="tmap__canvas">
        <ReactFlow
          nodes={nodes}
          edges={edges}
          nodeTypes={nodeTypes}
          colorMode={document.documentElement.dataset.mode === "dark" ? "dark" : "light"}
          defaultViewport={{ x: 24, y: 56, zoom: 0.85 }}
          minZoom={0.1}
          nodesDraggable={false}
          nodesConnectable={false}
          panOnScroll
          zoomOnScroll={false}
        >
          <Background gap={24} />
          <Controls showInteractive={false} />
        </ReactFlow>
      </div>
    </div>
  );
}
