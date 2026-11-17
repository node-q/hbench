// Round-trip check for form.ts (not bundled): `npx tsx src/pages/run/form.check.ts` from viewer/.
import { draftErrors, emptyDraft, fromConfig, hasSource, newAgent, toConfig, versionError } from "./form";
import type { KwargSpec } from "../../lib/types";

const d = emptyDraft();
d.dataset.name = "terminal-bench";
d.dataset.version = "2.0";
d.agents[0] = { ...newAgent("aider"), model: "openai/gpt-5", kwargs: { thinking_tokens: "2000", cache_prompts: "true" }, custom: [{ k: "x", v: "[1]" }] };
const specs: Record<string, KwargSpec[]> = { aider: [{ key: "thinking_tokens", kind: "int" }, { key: "cache_prompts", kind: "bool" }] };
const c = toConfig(d, specs);
console.assert(c.agents[0].kwargs.thinking_tokens === 2000 && c.agents[0].kwargs.cache_prompts === true && c.agents[0].kwargs.x[0] === 1, "kwargs typed");
const back = fromConfig(c, specs);
console.assert(back.dataset.name === "terminal-bench" && back.agents[0].kwargs.thinking_tokens === "2000" && back.agents[0].custom[0].k === "x", "round trip");
console.assert(draftErrors(emptyDraft(), {}).includes("Name a registry dataset."), "source required");
const noVersion = emptyDraft();
noVersion.dataset.name = "terminal-bench";
console.assert(!hasSource(emptyDraft()) && hasSource(noVersion), "a named dataset is a source");
console.assert(draftErrors(noVersion, {}).length === 1 && !!versionError(noVersion), "the default registry needs a version");
noVersion.dataset.registry_url = "https://example.com/registry.json";
console.assert(draftErrors(noVersion, {}).length === 0, "a registry.json dataset may leave the version out");
// A self-hosted server: the env that points the agent at it, and back.
const sh = emptyDraft();
sh.dataset.name = "terminal-bench";
sh.dataset.version = "2.0";
sh.agents[0] = { ...newAgent("terminus-2"), model: "qwen3-coder", server: { url: "localhost:8080/v1", key: "", served_by: "tensorfold" }, env: [{ k: "FOO", v: "1" }] };
const shc = toConfig(sh, {});
const a0 = shc.agents[0];
console.assert(a0.model_name === "openai/qwen3-coder" && a0.model_served_by === "tensorfold", "server model");
console.assert(a0.env.OPENAI_BASE_URL === "http://host.docker.internal:8080/v1" && a0.env.OPENAI_API_KEY === "no-key" && a0.env.FOO === "1", "server env");
const shb = fromConfig(shc, {}).agents[0];
console.assert(shb.server?.url === "http://localhost:8080" && shb.model === "qwen3-coder" && shb.server.served_by === "tensorfold" && shb.env.length === 1, "server round trip");
sh.agents[0] = { ...sh.agents[0], name: "claude-code", server: { url: "http://gpu.lan:8000", key: "k", served_by: "" } };
const cc = toConfig(sh, {}).agents[0];
console.assert(cc.model_name === "anthropic/qwen3-coder" && cc.env.ANTHROPIC_BASE_URL === "http://gpu.lan:8000" && cc.env.ANTHROPIC_API_KEY === "k" && !("model_served_by" in cc), "claude-code server");
sh.agents[0] = { ...sh.agents[0], model: "" };
console.assert(draftErrors(sh, {}).includes("Pick the model the server serves."), "server model required");
sh.agents[0] = { ...sh.agents[0], name: "oracle" };
console.assert(toConfig(sh, {}).agents[0].model_name === null && draftErrors(sh, {}).length === 0, "oracle ignores a server");
console.log("form.ts ok");
