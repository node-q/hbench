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
console.log("form.ts ok");
