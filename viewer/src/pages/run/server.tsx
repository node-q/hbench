// The launcher's "Self-hosted server" model choice: point an agent at an OpenAI-compatible server (llama.cpp, vLLM,
// LM Studio, TensorFold…) by its URL, and pick one of the models it lists, instead of naming provider env vars.
// The app asks the server through `POST /api/run/server-models`; form.ts turns the answer into the agent's env.
import { useEffect, useRef, useState } from "react";
import * as Icon from "../../ui/icons";
import { send } from "../../lib/api";
import { Field, Text } from "./fields";
import { fromContainer, serverApi, serverRoot, type AgentDraft, type ServerDraft } from "./form";

interface Probe {
  root: string;
  container_root: string;
  models?: string[];
  server?: string | null;
  served_by?: string | null;
  needs_key?: boolean;
  key_refused?: boolean;
}

type Status = { kind: "idle" } | { kind: "checking" } | { kind: "ok"; probe: Probe } | { kind: "key"; refused: boolean } | { kind: "error"; message: string };

export const NEW_SERVER: ServerDraft = { url: "http://localhost:8080", key: "", served_by: "" };

export function ServerPanel({ agent, onChange }: { agent: AgentDraft; onChange: (a: AgentDraft) => void }) {
  const server = agent.server ?? NEW_SERVER;
  const [status, setStatus] = useState<Status>({ kind: "idle" });
  // The latest draft, for answers that arrive after more typing.
  const latest = useRef(agent);
  latest.current = agent;
  const setServer = (patch: Partial<ServerDraft>) => onChange({ ...agent, server: { ...server, ...patch } });

  const check = async () => {
    const s = latest.current.server ?? NEW_SERVER;
    if (!s.url.trim()) return;
    setStatus({ kind: "checking" });
    try {
      const probe = await send<Probe>("POST", "/api/run/server-models", { url: s.url, key: s.key });
      if (probe.needs_key) return setStatus({ kind: "key", refused: !!probe.key_refused });
      setStatus({ kind: "ok", probe });
      // Fill what the answer settles: the only model, or what the server says it is.
      const a = latest.current;
      const cur = a.server ?? NEW_SERVER;
      const models = probe.models ?? [];
      const model = models.length === 1 || (models.length && !models.includes(a.model.trim())) ? models[0] : a.model;
      onChange({ ...a, model, server: { ...cur, served_by: cur.served_by || probe.served_by || "" } });
    } catch (e) {
      setStatus({ kind: "error", message: (e as Error).message });
    }
  };
  // Ask once when the panel opens; after that, on Connect, Enter or leaving the URL/key fields.
  useEffect(() => {
    void check();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const models = status.kind === "ok" ? (status.probe.models ?? []) : [];
  const root = serverRoot(server.url);
  const inside = fromContainer(root);
  const api = serverApi(agent.name);
  const onEnter = (e: React.KeyboardEvent) => e.key === "Enter" && (e.preventDefault(), void check());

  return (
    <div className="run-server">
      <div className="run-row run-row--browse">
        <Text
          label="Server URL"
          value={server.url}
          onChange={(url) => setServer({ url })}
          onBlur={() => void check()}
          onKeyDown={onEnter}
          placeholder="http://localhost:8080"
          mono
          autoComplete="off"
          spellCheck={false}
          help={inside !== root ? `The agent's container reaches it at ${inside}.` : "OpenAI-compatible: it serves /v1/models."}
        />
        <button type="button" className="btn run-browse" onClick={() => void check()} disabled={status.kind === "checking" || !server.url.trim()}>
          {status.kind === "checking" ? <span className="dk-spin" aria-hidden="true" /> : <Icon.Refresh size={13} />} Connect
        </button>
      </div>
      <ServerStatus status={status} />
      <div className="run-row">
        {models.length ? (
          <Field label="Model" required help={`${models.length} served; the agent asks for ${api.provider}/<model>.`}>
            {(p) => (
              <select {...p} className="select input--mono" value={agent.model} onChange={(e) => onChange({ ...agent, model: e.target.value })}>
                {!models.includes(agent.model) && <option value={agent.model}>{agent.model || "Pick a model"}</option>}
                {models.map((m) => (
                  <option key={m} value={m}>
                    {m}
                  </option>
                ))}
              </select>
            )}
          </Field>
        ) : (
          <Text label="Model" required value={agent.model} onChange={(model) => onChange({ ...agent, model })} placeholder="as the server names it" mono autoComplete="off" help="Connect to list the server's models." />
        )}
        <Text
          label="API key"
          type="password"
          value={server.key}
          onChange={(key) => setServer({ key })}
          onBlur={() => status.kind === "key" && void check()}
          onKeyDown={onEnter}
          placeholder="none"
          mono
          autoComplete="off"
          help={status.kind === "key" ? (status.refused ? "The server refused this key." : "The server wants a key.") : "If the server checks one. ${VAR} reads it from hbench's environment."}
          error={status.kind === "key" ? (status.refused ? "The server refused this key." : "The server wants a key.") : undefined}
        />
        <Text
          label="Served by"
          value={server.served_by}
          onChange={(served_by) => setServer({ served_by })}
          placeholder="e.g. vllm, llama.cpp"
          mono
          autoComplete="off"
          help="Recorded with each trial."
          error={server.served_by.trim() && !/^[A-Za-z0-9._\-:+]{1,64}$/.test(server.served_by.trim()) ? "1-64 letters, digits and . _ - : +" : undefined}
        />
      </div>
    </div>
  );
}

function ServerStatus({ status }: { status: Status }) {
  if (status.kind === "ok") {
    const n = status.probe.models?.length ?? 0;
    return (
      <p className="run-server__status is-ok" role="status">
        <Icon.Check size={12} /> Connected{status.probe.server ? <> to <span className="mono">{status.probe.server}</span></> : null}
        {" · "}
        {n ? `${n} model${n === 1 ? "" : "s"}` : "it lists no models; type the model's name"}
      </p>
    );
  }
  if (status.kind === "error")
    return (
      <p className="run-server__status is-bad" role="alert">
        <Icon.Alert size={12} /> {status.message}
      </p>
    );
  return null;
}
