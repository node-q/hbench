// Response shapes of src/viewer.rs. Fields mirror the JSON exactly (snake_case).

export type Json = null | boolean | number | string | Json[] | { [k: string]: Json };
export type Obj = Record<string, any>;

export interface Page<T> {
  items: T[];
  total: number;
  page: number;
  page_size: number;
  total_pages: number;
}

export interface FilterOption {
  value: string;
  count: number;
}

/** `metadata_filter_value`: the placeholder value for a missing agent/provider/model/effort. */
export const MISSING = "__missing__";

export interface AppConfig {
  folder: string;
  mode: "jobs" | "tasks";
  environments: string[];
}

/** `{metric_name: value}` maps; one per metric in an eval. */
export type Metric = Record<string, number | null>;
export interface EvalSummary {
  metrics: Metric[];
}

export interface JobSummary {
  name: string;
  id: string | null;
  started_at: string | null;
  updated_at: string | null;
  finished_at: string | null;
  n_total_trials: number;
  n_completed_trials: number;
  n_errored_trials: number;
  datasets: string[];
  agents: string[];
  providers: string[];
  models: string[];
  environment_type: string | null;
  evals: Record<string, EvalSummary>;
  total_input_tokens: number | null;
  total_cached_input_tokens: number | null;
  total_output_tokens: number | null;
  total_cost_usd: number | null;
}

export interface JobFilters {
  agents: FilterOption[];
  providers: FilterOption[];
  models: FilterOption[];
}

export interface EvalStats {
  n_trials: number;
  n_errors: number;
  metrics: Metric[];
  pass_at_k: Record<string, number>;
  reward_stats: Record<string, Record<string, string[]>>;
  exception_stats: Record<string, string[]>;
}

export interface JobStats {
  n_completed_trials: number;
  n_errored_trials: number;
  n_running_trials: number;
  n_pending_trials: number;
  n_cancelled_trials: number;
  n_retries: number;
  evals: Record<string, EvalStats>;
  n_input_tokens: number | null;
  n_cache_tokens: number | null;
  n_output_tokens: number | null;
  cost_usd: number | null;
}

export interface JobResult {
  id: string;
  started_at: string | null;
  updated_at: string | null;
  finished_at: string | null;
  n_total_trials: number;
  stats: JobStats;
  trial_results: Obj[];
  job_uri: string;
}

export interface TaskSummary {
  task_name: string;
  source: string | null;
  agent_name: string | null;
  model_provider: string | null;
  model_name: string | null;
  n_trials: number;
  n_completed: number;
  n_errors: number;
  exception_types: string[];
  avg_reward: number | null;
  evals: Record<string, EvalSummary>;
  avg_duration_ms: number | null;
  avg_tokens: number | null;
  avg_input_tokens: number | null;
  avg_cached_input_tokens: number | null;
  avg_output_tokens: number | null;
  avg_cost_usd: number | null;
}

export interface TaskFilters {
  agents: FilterOption[];
  providers: FilterOption[];
  models: FilterOption[];
  tasks: FilterOption[];
  sources: FilterOption[];
  reasoning_efforts: FilterOption[];
}

export interface TrialSummary {
  name: string;
  task_name: string;
  id: string | null;
  source: string | null;
  agent_name: string;
  reported_agent_name: string | null;
  reasoning_effort: string | null;
  model_provider: string | null;
  model_name: string | null;
  reward: number | null;
  evals: Record<string, EvalSummary>;
  error_type: string | null;
  started_at: string | null;
  finished_at: string | null;
  input_tokens: number | null;
  cached_input_tokens: number | null;
  output_tokens: number | null;
  cost_usd: number | null;
}

export interface TimingInfo {
  started_at: string | null;
  finished_at: string | null;
}

export interface AgentContext {
  n_input_tokens?: number | null;
  n_cache_tokens?: number | null;
  n_output_tokens?: number | null;
  cost_usd?: number | null;
  rollout_details?: Obj[] | null;
  metadata?: Obj | null;
}

export interface ExceptionInfo {
  exception_type: string;
  exception_message: string;
  exception_traceback: string;
  occurred_at: string;
}

/** upstream's TrialResult dump (result.json), or a partial one for in-progress trials. */
export interface TrialResult {
  id: string;
  task_name: string;
  trial_name: string;
  trial_uri: string;
  task_id: Obj;
  source: string | null;
  task_checksum: string;
  config: Obj;
  agent_info: { name: string; version: string; model_info: { name: string; provider: string | null } | null };
  agent_result: AgentContext | null;
  verifier_result: { rewards: Record<string, number | boolean | string> | null } | null;
  verifier_environment_mode?: string | null;
  exception_info: ExceptionInfo | null;
  started_at: string | null;
  finished_at: string | null;
  environment_setup: TimingInfo | null;
  agent_setup: TimingInfo | null;
  agent_execution: TimingInfo | null;
  verifier: TimingInfo | null;
  step_results: StepResult[] | null;
}

export interface StepResult {
  step_name: string;
  agent_result: AgentContext | null;
  verifier_result: { rewards: Record<string, number | boolean | string> | null } | null;
  exception_info: ExceptionInfo | null;
  started_at?: string | null;
  finished_at?: string | null;
  agent_execution?: TimingInfo | null;
  verifier?: TimingInfo | null;
  [k: string]: any;
}

export interface TrialNavigation {
  previous: string | null;
  next: string | null;
  index: number;
  total: number;
  finished: boolean;
}

export interface FileInfo {
  path: string;
  name: string;
  is_dir: boolean;
  size: number | null;
}

export interface VerifierOutput {
  stdout: string | null;
  stderr: string | null;
  ctrf: string | null;
  reward: Obj | null;
  reward_details: Obj | null;
}

export interface AgentLogs {
  oracle: string | null;
  setup: string | null;
  commands: { index: number; content: string | null }[];
  summary: string | null;
  analysis: Obj | null;
}

export interface Recording {
  available: boolean;
  file_path: string | null;
  media_type: string | null;
  size: number | null;
}

export interface Artifacts {
  files: FileInfo[];
  manifest: Json;
}

export interface Interaction {
  available: boolean;
  sources: { user_trajectory: string | null; user_runtime: string | null; bridge_trajectory: string | null; target_runtime: string | null };
  user_trajectory: Trajectory | null;
  user_events: Obj[];
  user_parse_errors: { line_number: number; error: string; raw: string }[];
  bridge_trajectory: Obj | null;
  target_events: Obj[];
  target_parse_errors: { line_number: number; error: string; raw: string }[];
}

/** ATIF trajectory (agent/trajectory.json). */
export interface Trajectory {
  schema_version?: string;
  session_id?: string;
  agent?: { name?: string; version?: string; model_name?: string; extra?: Obj };
  steps: TrajectoryStep[];
  notes?: string;
  final_metrics?: Obj;
  [k: string]: any;
}

export interface TrajectoryStep {
  step_id: number;
  timestamp?: string;
  source: "system" | "user" | "agent" | string;
  model_name?: string;
  message?: string | ContentPart[];
  reasoning_content?: string;
  tool_calls?: { tool_call_id: string; function_name: string; arguments: Obj | string }[];
  observation?: { results: { source_call_id?: string; content?: string | ContentPart[]; subagent_trajectory_ref?: Obj[] }[] };
  metrics?: { prompt_tokens?: number; completion_tokens?: number; cached_tokens?: number; cost_usd?: number; [k: string]: any };
  extra?: Obj;
  [k: string]: any;
}

export type ContentPart = { type: "text"; text: string } | { type: "image"; source?: { media_type?: string; path?: string; data?: string } } | Obj;

export interface CompareData {
  tasks: { source: string | null; task_name: string; key: string }[];
  agent_models: { job_name: string; agent_name: string | null; model_provider: string | null; model_name: string | null; key: string }[];
  cells: Record<string, Record<string, CompareCell>>;
}

export interface CompareCell {
  job_name: string;
  avg_reward: number | null;
  avg_duration_ms: number | null;
  avg_cost_usd: number | null;
  avg_tokens: number | null;
  n_trials: number;
  n_completed: number;
}

export interface KwargSpec {
  key: string;
  kind: "enum" | "json" | "bool" | "int" | "float" | "string";
  default?: Json;
  description?: string;
  choices?: Json[];
  required?: boolean;
}

export interface RunOptions {
  agents: string[];
  agent_kwargs: Record<string, KwargSpec[]>;
  environments: string[];
  resource_modes: string[];
  defaults: Obj;
  jobs_dir: string;
}

export interface RunStatus {
  running: boolean;
  returncode: number | null;
  job_ready: boolean;
  log_tail: string;
}

export interface TaskDefinitionSummary {
  name: string;
  version: string | null;
  source: string | null;
  metadata: Obj;
  has_instruction: boolean;
  has_environment: boolean;
  has_tests: boolean;
  has_solution: boolean;
  has_docker_compose: boolean;
  agent_timeout_sec: number | null;
  verifier_timeout_sec: number | null;
  os: string | null;
  cpus: number | null;
  memory_mb: number | null;
  storage_mb: number | null;
  gpus: number | null;
}

export interface TaskDefinitionFilters {
  difficulties: FilterOption[];
  categories: FilterOption[];
  tags: FilterOption[];
}

export interface TaskDefinition {
  name: string;
  task_dir: string;
  config: Obj;
  instruction: string | null;
  has_instruction: boolean;
  has_environment: boolean;
  has_tests: boolean;
  has_solution: boolean;
  has_docker_compose: boolean;
}

export interface Pricing {
  model_name: string;
  input_cost_per_token: number | null;
  cache_read_input_token_cost: number | null;
  output_cost_per_token: number | null;
}
