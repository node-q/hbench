// The launcher's draft (all strings, as typed) and its conversion to/from hbench's JobConfig JSON (src/config.rs).
import type { KwargSpec, Obj } from "../../lib/types";

export type Source = "dataset" | "path" | "git";
export interface KV {
  k: string;
  v: string;
}

export interface AgentDraft {
  name: string;
  model: string;
  /** Raw input per declared kwarg key ("" = unset → agent default). */
  kwargs: Record<string, string>;
  custom: KV[];
  env: KV[];
  n_concurrent: string;
  override_timeout_sec: string;
  override_setup_timeout_sec: string;
  max_timeout_sec: string;
  skills: string;
  extra_allowed_hosts: string;
}

export interface Draft {
  source: Source;
  dataset: { name: string; version: string; registry_url: string; registry_path: string; task_names: string; exclude_task_names: string; n_tasks: string; overwrite: boolean; download_dir: string };
  path: string;
  git: { git_url: string; git_commit_id: string; path: string };
  agents: AgentDraft[];
  env: { type: string; force_build: boolean; delete: boolean; override_cpus: string; override_memory_mb: string; override_storage_mb: string; override_gpus: string; cpu_policy: string; memory_policy: string; kwargs: string; extra_allowed_hosts: string; env: KV[] };
  job: {
    job_name: string;
    n_attempts: string;
    n_concurrent_trials: string;
    timeout_multiplier: string;
    agent_timeout_multiplier: string;
    verifier_timeout_multiplier: string;
    agent_setup_timeout_multiplier: string;
    environment_build_timeout_multiplier: string;
    debug: boolean;
    install_only: boolean;
    extra_instructions: string;
    extra_instruction_paths: string;
  };
  retry: { max_retries: string; include_exceptions: string; exclude_exceptions: string; wait_multiplier: string; min_wait_sec: string; max_wait_sec: string };
  verifier: { disable: boolean; override_timeout_sec: string; max_timeout_sec: string; kwargs: string; env: KV[] };
}

export const DEFAULT_EXCLUDE = [
  "AgentAuthenticationError",
  "AgentSafetyRefusalError",
  "AgentTimeoutError",
  "ApiUsageLimitError",
  "ModelNotFoundError",
  "RewardFileEmptyError",
  "RewardFileNotFoundError",
  "UnsupportedModelParametersError",
  "VerifierOutputParseError",
  "VerifierTimeoutError",
].join(", ");

export const newAgent = (name = "oracle"): AgentDraft => ({
  name,
  model: "",
  kwargs: {},
  custom: [],
  env: [],
  n_concurrent: "",
  override_timeout_sec: "",
  override_setup_timeout_sec: "",
  max_timeout_sec: "",
  skills: "",
  extra_allowed_hosts: "",
});

export const emptyDraft = (): Draft => ({
  source: "dataset",
  dataset: { name: "", version: "", registry_url: "", registry_path: "", task_names: "", exclude_task_names: "", n_tasks: "", overwrite: false, download_dir: "" },
  path: "",
  git: { git_url: "", git_commit_id: "", path: "" },
  agents: [newAgent()],
  env: { type: "docker", force_build: false, delete: true, override_cpus: "", override_memory_mb: "", override_storage_mb: "", override_gpus: "", cpu_policy: "auto", memory_policy: "auto", kwargs: "", extra_allowed_hosts: "", env: [] },
  job: {
    job_name: "",
    n_attempts: "1",
    n_concurrent_trials: "4",
    timeout_multiplier: "1",
    agent_timeout_multiplier: "",
    verifier_timeout_multiplier: "",
    agent_setup_timeout_multiplier: "",
    environment_build_timeout_multiplier: "",
    debug: false,
    install_only: false,
    extra_instructions: "",
    extra_instruction_paths: "",
  },
  retry: { max_retries: "0", include_exceptions: "", exclude_exceptions: DEFAULT_EXCLUDE, wait_multiplier: "1", min_wait_sec: "1", max_wait_sec: "60" },
  verifier: { disable: false, override_timeout_sec: "", max_timeout_sec: "", kwargs: "", env: [] },
});

// ---------------------------------------------------------------- validation

const list = (s: string) =>
  s
    .split(/[,\n]/)
    .map((x) => x.trim())
    .filter(Boolean);
const lines = (s: string) =>
  s
    .split("\n")
    .map((x) => x.trim())
    .filter(Boolean);

export function numError(s: string, { int = false, min }: { int?: boolean; min?: number } = {}): string | undefined {
  if (s.trim() === "") return undefined;
  const n = Number(s);
  if (!Number.isFinite(n)) return "Enter a number.";
  if (int && !Number.isInteger(n)) return "Enter a whole number.";
  if (min != null && n < min) return `Must be at least ${min}.`;
  return undefined;
}

export function jsonError(s: string, objectOnly = false): string | undefined {
  if (s.trim() === "") return undefined;
  try {
    const v = JSON.parse(s);
    if (objectOnly && (typeof v !== "object" || v === null || Array.isArray(v))) return "Must be a JSON object, e.g. {\"key\": 1}.";
  } catch (e) {
    return `Invalid JSON: ${(e as Error).message}`;
  }
  return undefined;
}

export function kwargError(spec: KwargSpec, raw: string | undefined): string | undefined {
  const s = raw ?? "";
  if (s.trim() === "") return spec.required ? "Required by this agent." : undefined;
  if (spec.kind === "int") return numError(s, { int: true });
  if (spec.kind === "float") return numError(s);
  if (spec.kind === "json") return jsonError(s);
  return undefined;
}

export function sourceError(d: Draft): string | undefined {
  if (d.source === "dataset" && !d.dataset.name.trim() && !d.dataset.registry_path.trim()) return "Name a registry dataset.";
  if (d.source === "path" && !d.path.trim()) return "Enter a task or dataset folder.";
  if (d.source === "git" && (!d.git.git_url.trim() || !d.git.path.trim())) return "A git task needs both the repository URL and the task path.";
  return undefined;
}

/** All blocking errors (for the submit gate). */
export function draftErrors(d: Draft, specs: Record<string, KwargSpec[]>): string[] {
  const errs: (string | undefined)[] = [
    sourceError(d),
    numError(d.dataset.n_tasks, { int: true, min: 1 }),
    numError(d.job.n_attempts, { int: true, min: 1 }),
    numError(d.job.n_concurrent_trials, { int: true, min: 1 }),
    numError(d.job.timeout_multiplier, { min: 0 }),
    jsonError(d.env.kwargs, true),
    jsonError(d.verifier.kwargs, true),
    numError(d.retry.max_retries, { int: true, min: 0 }),
  ];
  for (const a of d.agents) {
    if (!a.name) errs.push("Pick an agent.");
    for (const s of specs[a.name] ?? []) errs.push(kwargError(s, a.kwargs[s.key]));
    errs.push(numError(a.n_concurrent, { int: true, min: 1 }));
  }
  return errs.filter((e): e is string => !!e);
}

// ---------------------------------------------------------------- draft → JobConfig

const num = (s: string) => (s.trim() === "" ? null : Number(s));
const str = (s: string) => (s.trim() === "" ? null : s.trim());
const kvObj = (kv: KV[]) => Object.fromEntries(kv.filter((x) => x.k.trim()).map((x) => [x.k.trim(), x.v]));
const jsonObj = (s: string): Obj => {
  try {
    return s.trim() ? JSON.parse(s) : {};
  } catch {
    return {};
  }
};

/** Parse a custom kwarg value: JSON when it parses (numbers, bools, objects), else the raw string. */
function loose(v: string): unknown {
  try {
    return JSON.parse(v);
  } catch {
    return v;
  }
}

function kwargValue(spec: KwargSpec | undefined, raw: string): unknown {
  switch (spec?.kind) {
    case "int":
    case "float":
      return Number(raw);
    case "bool":
      return raw === "true";
    case "json":
      return loose(raw);
    case "string":
    case "enum":
      return raw;
    default:
      return loose(raw);
  }
}

export function toConfig(d: Draft, specs: Record<string, KwargSpec[]>): Obj {
  const cfg: Obj = {
    n_attempts: num(d.job.n_attempts) ?? 1,
    n_concurrent_trials: num(d.job.n_concurrent_trials) ?? 4,
    timeout_multiplier: num(d.job.timeout_multiplier) ?? 1,
    agent_timeout_multiplier: num(d.job.agent_timeout_multiplier),
    verifier_timeout_multiplier: num(d.job.verifier_timeout_multiplier),
    agent_setup_timeout_multiplier: num(d.job.agent_setup_timeout_multiplier),
    environment_build_timeout_multiplier: num(d.job.environment_build_timeout_multiplier),
    debug: d.job.debug,
    install_only: d.job.install_only,
    extra_instructions: lines(d.job.extra_instructions),
    extra_instruction_paths: lines(d.job.extra_instruction_paths),
    retry: {
      max_retries: num(d.retry.max_retries) ?? 0,
      include_exceptions: list(d.retry.include_exceptions).length ? list(d.retry.include_exceptions) : null,
      exclude_exceptions: list(d.retry.exclude_exceptions),
      wait_multiplier: num(d.retry.wait_multiplier) ?? 1,
      min_wait_sec: num(d.retry.min_wait_sec) ?? 1,
      max_wait_sec: num(d.retry.max_wait_sec) ?? 60,
    },
    environment: {
      type: d.env.type || "docker",
      force_build: d.env.force_build,
      delete: d.env.delete,
      cpu_enforcement_policy: d.env.cpu_policy,
      memory_enforcement_policy: d.env.memory_policy,
      override_cpus: num(d.env.override_cpus),
      override_memory_mb: num(d.env.override_memory_mb),
      override_storage_mb: num(d.env.override_storage_mb),
      override_gpus: num(d.env.override_gpus),
      kwargs: jsonObj(d.env.kwargs),
      extra_allowed_hosts: list(d.env.extra_allowed_hosts),
      env: kvObj(d.env.env),
    },
    verifier: {
      disable: d.verifier.disable,
      override_timeout_sec: num(d.verifier.override_timeout_sec),
      max_timeout_sec: num(d.verifier.max_timeout_sec),
      kwargs: jsonObj(d.verifier.kwargs),
      env: kvObj(d.verifier.env),
    },
    agents: d.agents.map((a) => {
      const sp = specs[a.name] ?? [];
      const kwargs: Obj = {};
      for (const [k, raw] of Object.entries(a.kwargs)) {
        if (raw.trim() === "") continue;
        kwargs[k] = kwargValue(sp.find((s) => s.key === k), raw);
      }
      for (const { k, v } of a.custom) if (k.trim()) kwargs[k.trim()] = loose(v);
      return {
        name: a.name,
        model_name: str(a.model),
        n_concurrent: num(a.n_concurrent),
        override_timeout_sec: num(a.override_timeout_sec),
        override_setup_timeout_sec: num(a.override_setup_timeout_sec),
        max_timeout_sec: num(a.max_timeout_sec),
        skills: list(a.skills),
        extra_allowed_hosts: list(a.extra_allowed_hosts),
        kwargs,
        env: kvObj(a.env),
      };
    }),
    datasets: [],
    tasks: [],
  };
  if (str(d.job.job_name)) cfg.job_name = d.job.job_name.trim();
  if (d.source === "dataset") {
    const ds = d.dataset;
    cfg.datasets = [
      {
        name: str(ds.name),
        version: str(ds.version),
        registry_url: str(ds.registry_url),
        registry_path: str(ds.registry_path),
        task_names: list(ds.task_names).length ? list(ds.task_names) : null,
        exclude_task_names: list(ds.exclude_task_names).length ? list(ds.exclude_task_names) : null,
        n_tasks: num(ds.n_tasks),
        overwrite: ds.overwrite,
        download_dir: str(ds.download_dir),
      },
    ];
  } else if (d.source === "path") {
    // The server turns a bare dataset path that is a task dir into a `tasks` entry.
    cfg.datasets = [{ path: d.path.trim() }];
  } else {
    cfg.tasks = [{ git_url: str(d.git.git_url), git_commit_id: str(d.git.git_commit_id), path: str(d.git.path) }];
  }
  return cfg;
}

// ---------------------------------------------------------------- JobConfig → draft (history "Load")

const s = (v: unknown) => (v == null ? "" : typeof v === "string" ? v : JSON.stringify(v));
const kvFrom = (o: unknown): KV[] => Object.entries((o as Obj) ?? {}).map(([k, v]) => ({ k, v: s(v) }));
const join = (v: unknown, sep = ", ") => (Array.isArray(v) ? v.join(sep) : "");
const objText = (o: unknown) => (o && typeof o === "object" && Object.keys(o).length ? JSON.stringify(o, null, 2) : "");

export function fromConfig(c: Obj, specs: Record<string, KwargSpec[]>): Draft {
  const d = emptyDraft();
  const ds = c.datasets?.[0];
  const task = c.tasks?.[0];
  if (ds?.path) {
    d.source = "path";
    d.path = ds.path;
  } else if (ds) {
    d.source = "dataset";
    d.dataset = {
      name: s(ds.name),
      version: s(ds.version),
      registry_url: s(ds.registry_url),
      registry_path: s(ds.registry_path),
      task_names: join(ds.task_names),
      exclude_task_names: join(ds.exclude_task_names),
      n_tasks: s(ds.n_tasks),
      overwrite: !!ds.overwrite,
      download_dir: s(ds.download_dir),
    };
  } else if (task?.git_url) {
    d.source = "git";
    d.git = { git_url: s(task.git_url), git_commit_id: s(task.git_commit_id), path: s(task.path) };
  } else if (task?.path) {
    d.source = "path";
    d.path = task.path;
  }
  if (Array.isArray(c.agents) && c.agents.length) {
    d.agents = c.agents.map((a: Obj) => {
      const declared = new Set((specs[a.name] ?? []).map((x) => x.key));
      const kw: Record<string, string> = {};
      const custom: KV[] = [];
      for (const [k, v] of Object.entries((a.kwargs as Obj) ?? {})) {
        if (declared.has(k)) kw[k] = s(v);
        else custom.push({ k, v: s(v) });
      }
      return {
        ...newAgent(a.name ?? "oracle"),
        model: s(a.model_name),
        kwargs: kw,
        custom,
        env: kvFrom(a.env),
        n_concurrent: s(a.n_concurrent),
        override_timeout_sec: s(a.override_timeout_sec),
        override_setup_timeout_sec: s(a.override_setup_timeout_sec),
        max_timeout_sec: s(a.max_timeout_sec),
        skills: join(a.skills),
        extra_allowed_hosts: join(a.extra_allowed_hosts),
      };
    });
  }
  const e = c.environment ?? {};
  d.env = {
    type: e.type ?? "docker",
    force_build: !!e.force_build,
    delete: e.delete ?? true,
    override_cpus: s(e.override_cpus),
    override_memory_mb: s(e.override_memory_mb),
    override_storage_mb: s(e.override_storage_mb),
    override_gpus: s(e.override_gpus),
    cpu_policy: e.cpu_enforcement_policy ?? "auto",
    memory_policy: e.memory_enforcement_policy ?? "auto",
    kwargs: objText(e.kwargs),
    extra_allowed_hosts: join(e.extra_allowed_hosts),
    env: kvFrom(e.env),
  };
  d.job = {
    job_name: "", // a loaded run gets a fresh name; reusing the old one would 409
    n_attempts: s(c.n_attempts ?? 1),
    n_concurrent_trials: s(c.n_concurrent_trials ?? 4),
    timeout_multiplier: s(c.timeout_multiplier ?? 1),
    agent_timeout_multiplier: s(c.agent_timeout_multiplier),
    verifier_timeout_multiplier: s(c.verifier_timeout_multiplier),
    agent_setup_timeout_multiplier: s(c.agent_setup_timeout_multiplier),
    environment_build_timeout_multiplier: s(c.environment_build_timeout_multiplier),
    debug: !!c.debug,
    install_only: !!c.install_only,
    extra_instructions: join(c.extra_instructions, "\n"),
    extra_instruction_paths: join(c.extra_instruction_paths, "\n"),
  };
  const r = c.retry ?? {};
  d.retry = {
    max_retries: s(r.max_retries ?? 0),
    include_exceptions: join(r.include_exceptions),
    exclude_exceptions: r.exclude_exceptions ? join(r.exclude_exceptions) : DEFAULT_EXCLUDE,
    wait_multiplier: s(r.wait_multiplier ?? 1),
    min_wait_sec: s(r.min_wait_sec ?? 1),
    max_wait_sec: s(r.max_wait_sec ?? 60),
  };
  const v = c.verifier ?? {};
  d.verifier = { disable: !!v.disable, override_timeout_sec: s(v.override_timeout_sec), max_timeout_sec: s(v.max_timeout_sec), kwargs: objText(v.kwargs), env: kvFrom(v.env) };
  return d;
}

/** One-line description of a config's task source (history list). */
export function sourceLabel(c: Obj): string {
  const ds = c.datasets?.[0];
  const t = c.tasks?.[0];
  if (ds?.name) return ds.version ? `${ds.name}@${ds.version}` : ds.name;
  if (ds?.path) return ds.path.split("/").pop() || ds.path;
  if (t?.path) return t.path.split("/").pop() || t.path;
  return "—";
}
