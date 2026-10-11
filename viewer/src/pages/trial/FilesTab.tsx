// File browser for the trial (or step) root and its artifacts: collapsible tree + viewer pane. Path lives in the URL.
import { useMemo, useState } from "react";
import { ApiError, useApi } from "../../lib/api";
import * as fmt from "../../lib/fmt";
import { useUrlState } from "../../lib/state";
import type { Artifacts, FileInfo } from "../../lib/types";
import { CodeView, JsonTree } from "../../ui/content";
import { Empty, ErrorBox, SkeletonRows } from "../../ui/kit";
import * as Icon from "../../ui/icons";
import { api, fileUrl, mediaOf, type TrialRef } from "./util";

export function FilesTab({ t, source }: { t: TrialRef; source: "files" | "artifacts" }) {
  const res = useApi<FileInfo[] | Artifacts>(api(t, source));
  const url = useUrlState();
  const selected = url.get("path");
  if (res.error) return <ErrorBox error={res.error} onRetry={res.reload} />;
  if (!res.data) return <SkeletonRows />;
  const files = Array.isArray(res.data) ? res.data : res.data.files;
  const manifest = Array.isArray(res.data) ? null : res.data.manifest;
  const prefix = source === "artifacts" ? "artifacts/" : "";
  return (
    <>
      {manifest != null && (
        <details className="panel tfiles__manifest">
          <summary className="panel__head label">Artifact manifest</summary>
          <div className="panel__body">
            <JsonTree value={manifest} open={3} />
          </div>
        </details>
      )}
      {files.length === 0 ? (
        <Empty title={source === "artifacts" ? "No artifacts collected" : "No files"}>{source === "artifacts" ? "The trial's artifact directories were empty." : "This trial directory is empty."}</Empty>
      ) : (
        <div className="tfiles">
          <Tree files={files} selected={selected} onSelect={(path) => url.set({ path })} />
          <div className="tfiles__view">
            {selected ? <FileView t={t} path={prefix + selected} name={selected} /> : <Empty title="Select a file">{fmt.int(files.filter((f) => !f.is_dir).length)} files in this {source === "artifacts" ? "artifact set" : "trial"}.</Empty>}
          </div>
        </div>
      )}
    </>
  );
}

function Tree({ files, selected, onSelect }: { files: FileInfo[]; selected: string; onSelect: (p: string) => void }) {
  const [closed, setClosed] = useState<Set<string>>(() => new Set(files.length > 150 ? files.filter((f) => f.is_dir).map((f) => f.path) : []));
  const visible = useMemo(() => files.filter((f) => !f.path.split("/").slice(0, -1).some((_, i, a) => closed.has(a.slice(0, i + 1).join("/")))), [files, closed]);
  const toggle = (path: string) =>
    setClosed((c) => {
      const n = new Set(c);
      if (n.has(path)) n.delete(path);
      else n.add(path);
      return n;
    });
  return (
    <ul className="tfiles__tree" aria-label="Files">
      {visible.map((f) => {
        const depth = f.path.split("/").length - 1;
        return (
          <li key={f.path}>
            <button
              className={`tfiles__item${f.path === selected ? " is-on" : ""}`}
              style={{ paddingLeft: `calc(${depth} * 14px + 8px)` }}
              onClick={() => (f.is_dir ? toggle(f.path) : onSelect(f.path))}
              aria-expanded={f.is_dir ? !closed.has(f.path) : undefined}
              aria-current={f.path === selected || undefined}
              title={f.path}
            >
              {f.is_dir ? <Icon.Folder size={13} /> : <Icon.File size={13} />}
              <span className="trunc">{f.name}</span>
              {!f.is_dir && <span className="num faint tfiles__size">{fmt.bytes(f.size)}</span>}
            </button>
          </li>
        );
      })}
    </ul>
  );
}

export function FileView({ t, path, name }: { t: TrialRef; path: string; name: string }) {
  const src = fileUrl(t, path);
  const media = mediaOf(path);
  const head = (
    <div className="tfiles__head">
      <span className="mono trunc ink" title={path}>
        {name}
      </span>
      <a className="btn btn--sm btn--ghost" href={src} target="_blank" rel="noreferrer" download={media ? undefined : name}>
        <Icon.Download size={13} /> Raw
      </a>
    </div>
  );
  if (media)
    return (
      <>
        {head}
        <div className="tfiles__media">
          {media === "image" && <img src={src} alt={name} />}
          {media === "audio" && <audio controls src={src} />}
          {media === "video" && <video controls src={src} />}
        </div>
      </>
    );
  return (
    <>
      {head}
      <TextView src={src} path={path} />
    </>
  );
}

function TextView({ src, path }: { src: string; path: string }) {
  const res = useApi<string>(src, { text: true });
  const pretty = useMemo(() => {
    if (res.data == null || !path.endsWith(".json")) return null;
    try {
      return JSON.stringify(JSON.parse(res.data), null, 2);
    } catch {
      return null;
    }
  }, [res.data, path]);
  if (res.error) {
    const st = res.error instanceof ApiError ? res.error.status : 0;
    if (st === 413 || st === 415)
      return (
        <Empty title={st === 413 ? "Too large to preview" : "Binary file"} action={<a className="link" href={src} download>Download it</a>}>
          {res.error.message}
        </Empty>
      );
    return <ErrorBox error={res.error} />;
  }
  if (res.data == null || res.loading) return <SkeletonRows rows={8} />;
  if (!res.data) return <Empty title="Empty file" />;
  return <CodeView code={pretty ?? res.data} path={path} wrap maxHeight="72vh" />;
}
