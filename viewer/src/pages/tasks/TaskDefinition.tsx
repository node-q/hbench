import { useMemo, useState, type ReactNode } from "react";
import { useParams } from "react-router";
import { ApiError, p, useApi } from "../../lib/api";
import { useUrlState } from "../../lib/state";
import { bytes, duration } from "../../lib/fmt";
import type { FileInfo, Obj, TaskDefinition as TaskDef } from "../../lib/types";
import { CopyButton, Empty, ErrorBox, PageHead, SkeletonRows, Tabs } from "../../ui/kit";
import { CodeView, JsonTree, Markdown } from "../../ui/content";
import * as Icon from "../../ui/icons";
import { NotFound, useCommands, useTitle, type Command } from "../../shell";
import { Flags, Tags } from "./TaskDefinitions";
import "./tasks.css";

const TABS = [
  { id: "instruction", label: "Instruction" },
  { id: "config", label: "Configuration" },
  { id: "files", label: "Files" },
];

export default function TaskDefinition() {
  const { taskName = "" } = useParams();
  useTitle(taskName);
  const url = useUrlState();
  const tab = TABS.some((t) => t.id === url.get("tab")) ? url.get("tab") : "instruction";
  const setTab = (id: string) => url.set({ tab: id === "instruction" ? null : id, file: id === "files" ? url.get("file") : null });
  const task = useApi<TaskDef>(p("task-definitions", taskName));

  const cmds: Command[] = useMemo(() => TABS.map((t) => ({ id: `tab:${t.id}`, label: `Show ${t.label.toLowerCase()}`, group: "This task", run: () => setTab(t.id) })), [url.set]); // eslint-disable-line react-hooks/exhaustive-deps
  useCommands(cmds);

  if (task.error instanceof ApiError && task.error.status === 404) return <NotFound what="Task" />;
  const t = task.data;
  const md: Obj = t?.config?.metadata ?? {};
  return (
    <div className="page">
      <PageHead
        crumbs={[{ to: "/task-definitions", label: "Tasks" }]}
        title={taskName}
        meta={
          t && (
            <>
              {md.difficulty && <span className={`chip diff--${md.difficulty}`}>{md.difficulty}</span>}
              {md.category && <span className="chip">{md.category}</span>}
              <Tags tags={(md.tags as string[]) ?? []} max={6} />
              {md.author_name && <span>by {md.author_name}</span>}
              <Flags t={t} />
              <span className="task-dir mono" title={t.task_dir}>
                <span className="trunc">{t.task_dir}</span>
                <CopyButton text={t.task_dir} label="Copy task path" />
              </span>
            </>
          )
        }
      />
      {task.error ? (
        <ErrorBox error={task.error} onRetry={task.reload} />
      ) : !t ? (
        <SkeletonRows />
      ) : (
        <>
          <Tabs label="Task sections" tabs={TABS} value={tab} onChange={setTab} />
          {tab === "instruction" &&
            (t.instruction ? <Markdown text={t.instruction} /> : <Empty title="No instruction.md">This task has no instruction file or step instructions.</Empty>)}
          {tab === "config" && <ConfigView config={t.config} />}
          {tab === "files" && <Files task={taskName} selected={url.get("file")} onSelect={(f) => url.set({ file: f })} />}
        </>
      )}
    </div>
  );
}

// ---------------------------------------------------------------- configuration

const secs = (n: unknown) => (typeof n === "number" ? duration(n * 1000) : null);
const mb = (n: unknown) => (typeof n !== "number" ? null : n >= 1024 ? `${+(n / 1024).toFixed(1)} GB` : `${n} MB`);
const net = (o: Obj | null | undefined) => {
  if (!o) return null;
  const hosts = Array.isArray(o.allowed_hosts) && o.allowed_hosts.length ? ` · ${o.allowed_hosts.join(", ")}` : "";
  return o.network_mode || hosts ? `${o.network_mode ?? "allow-list"}${hosts}` : null;
};

function Group({ title, rows }: { title: string; rows: [string, ReactNode][] }) {
  const shown = rows.filter(([, v]) => v != null && v !== "");
  return (
    <section className="cfg-group">
      <h3 className="label">{title}</h3>
      {shown.length ? (
        <dl className="kv">
          {shown.map(([k, v]) => (
            <div key={k} style={{ display: "contents" }}>
              <dt>{k}</dt>
              <dd className="num">{v}</dd>
            </div>
          ))}
        </dl>
      ) : (
        <p className="faint" style={{ fontSize: "var(--text-sm)" }}>
          Defaults
        </p>
      )}
    </section>
  );
}

function ConfigView({ config }: { config: Obj }) {
  const env: Obj = config.environment ?? {};
  const ver: Obj = config.verifier ?? {};
  const agent: Obj = config.agent ?? {};
  const steps: Obj[] = Array.isArray(config.steps) ? config.steps : [];
  return (
    <div className="cfg">
      <div className="cfg-grid">
        <Group
          title="Timeouts"
          rows={[
            ["Agent", secs(agent.timeout_sec)],
            ["Verifier", secs(ver.timeout_sec)],
            ["Environment build", secs(env.build_timeout_sec)],
          ]}
        />
        <Group
          title="Resources"
          rows={[
            ["OS", env.os],
            ["CPUs", env.cpus],
            ["Memory", mb(env.memory_mb)],
            ["Storage", mb(env.storage_mb)],
            ["GPUs", env.gpus ? `${env.gpus}${env.gpu_types?.length ? ` × ${env.gpu_types.join(", ")}` : ""}` : env.gpus],
            ["TPU", env.tpu ? JSON.stringify(env.tpu) : null],
          ]}
        />
        <Group
          title="Network"
          rows={[
            ["Environment", net(env)],
            ["Agent", net(agent)],
            ["Verifier", net(ver)],
            ["Verifier environment", net(ver.environment)],
          ]}
        />
        <Group
          title="Build"
          rows={[
            ["Docker image", env.docker_image],
            ["Workdir", env.workdir],
            ["Skills directory", env.skills_dir],
            ["MCP servers", env.mcp_servers?.length ? env.mcp_servers.map((s: Obj) => s.name ?? "?").join(", ") : null],
            ["Schema version", config.schema_version],
          ]}
        />
      </div>
      {steps.length > 0 && (
        <section className="section">
          <h2>Steps</h2>
          <ol className="steps">
            {steps.map((s, i) => (
              <li key={i}>
                <span className="mono ink">{s.name ?? `step ${i + 1}`}</span>
                {secs(s.agent?.timeout_sec) && <span className="muted"> · agent {secs(s.agent.timeout_sec)}</span>}
                {secs(s.verifier?.timeout_sec) && <span className="muted"> · verifier {secs(s.verifier.timeout_sec)}</span>}
              </li>
            ))}
          </ol>
          {config.multi_step_reward_strategy && <p className="muted">Reward strategy: {String(config.multi_step_reward_strategy)}</p>}
        </section>
      )}
      <section className="section">
        <div className="section__head">
          <h2>task.toml</h2>
          <CopyButton text={JSON.stringify(config, null, 2)} label="Copy config as JSON" />
        </div>
        <div className="panel panel__body">
          <JsonTree value={config} open={1} />
        </div>
      </section>
    </div>
  );
}

// ---------------------------------------------------------------- files

const IMG = /\.(png|jpe?g|gif|webp|svg)$/i;

function Files({ task, selected, onSelect }: { task: string; selected: string; onSelect: (path: string) => void }) {
  const files = useApi<FileInfo[]>(p("task-definitions", task, "files"));
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  if (files.error) return <ErrorBox error={files.error} onRetry={files.reload} />;
  if (!files.data) return <SkeletonRows />;
  if (!files.data.length) return <Empty title="Empty task directory" />;
  // Hidden = any ancestor directory collapsed.
  const hidden = (path: string) => [...collapsed].some((d) => path.startsWith(d + "/"));
  return (
    <div className="files">
      <nav className="files__tree panel" aria-label="Task files">
        {files.data
          .filter((f) => !hidden(f.path))
          .map((f) => {
            const depth = f.path.split("/").length - 1;
            const open = !collapsed.has(f.path);
            return (
              <button
                key={f.path}
                className="menu-item files__item"
                data-active={f.path === selected}
                aria-expanded={f.is_dir ? open : undefined}
                aria-current={f.path === selected ? "true" : undefined}
                style={{ paddingLeft: 8 + depth * 14 }}
                onClick={() => {
                  if (!f.is_dir) return onSelect(f.path);
                  const next = new Set(collapsed);
                  open ? next.add(f.path) : next.delete(f.path);
                  setCollapsed(next);
                }}
              >
                {f.is_dir ? <Icon.Chevron size={11} style={{ transform: open ? "rotate(90deg)" : undefined }} /> : <Icon.File size={12} className="faint" />}
                <span className="trunc">{f.name}</span>
                {!f.is_dir && <span className="menu-item__count">{bytes(f.size)}</span>}
              </button>
            );
          })}
      </nav>
      <div className="files__view">
        {selected ? <FileView task={task} path={selected} /> : <Empty title="Select a file">Pick a file on the left to read it.</Empty>}
      </div>
    </div>
  );
}

function FileView({ task, path }: { task: string; path: string }) {
  const src = p("task-definitions", task, "files", ...path.split("/"));
  const isImg = IMG.test(path);
  const text = useApi<string>(isImg ? null : src, { text: true });
  return (
    <div className="file-view">
      <div className="file-view__head">
        <span className="mono trunc ink">{path}</span>
        <a className="btn btn--ghost btn--sm" href={src} target="_blank" rel="noreferrer">
          <Icon.External size={13} /> Raw
        </a>
      </div>
      {isImg ? (
        <img src={src} alt={path} className="file-view__img" />
      ) : text.error ? (
        text.error instanceof ApiError && (text.error.status === 413 || text.error.status === 415) ? (
          <Empty title={text.error.status === 413 ? "File too large to preview" : "Binary file"}>
            {text.error.message}. Use Raw to download it.
          </Empty>
        ) : (
          <ErrorBox error={text.error} onRetry={text.reload} />
        )
      ) : text.data == null ? (
        <SkeletonRows rows={10} />
      ) : (
        <CodeView code={text.data} path={path} maxHeight="calc(100vh - 260px)" />
      )}
    </div>
  );
}
