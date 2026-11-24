// Check of the run link's handling in the page (not bundled into the app): from viewer/,
// `npx rolldown src/desktop/run-link.check.ts --platform node --format esm --file /tmp/run-link.check.mjs && node /tmp/run-link.check.mjs`.
import { accept, notInCatalogue, offer, openedNotice, pickFromLink, resolve, settle } from "./run-link";
import { emptyDraft, newAgent } from "../pages/run/form";
import type { Catalog, CatalogDataset } from "../lib/types";

const failed: string[] = [];
const check = (ok: boolean, what: string) => ok || failed.push(what);

// Nothing here may reach the network: a run link never fetches, and never starts a run (POST /api/run).
let fetches = 0;
globalThis.fetch = (() => {
  fetches++;
  throw new Error("fetch called");
}) as typeof fetch;

const ds = (name: string, versions: string[], domain = "coding"): CatalogDataset => ({ name, version: versions[versions.length - 1], versions, domain, details: null });
const catalog: Catalog = {
  registry: "oci://example",
  domains: [{ id: "coding", title: "Coding", description: "", n_datasets: 2 }],
  datasets: [ds("terminal-bench", ["1.0", "2.0"]), ds("swebench-verified", ["1.0.0"])],
};

// What the app pushes: only a fresh "open" or "error" is taken, once.
for (const v of [undefined, null, 3, "x", {}, { state: "none", nonce: "" }, { state: "none", nonce: "n0" }, { state: "open", nonce: "n1" }, { state: "open", dataset: "x" }, { state: "mystery", nonce: "n2", dataset: "x" }])
  check(accept(v) === null, `${JSON.stringify(v)} is not taken`);
const open = accept({ state: "open", nonce: "a1", dataset: "terminal-bench", version: "1.0" });
check(JSON.stringify(open) === JSON.stringify({ nonce: "a1", dataset: "terminal-bench", version: "1.0" }), "an open link is taken");
check(accept({ state: "open", nonce: "a1", dataset: "terminal-bench" }) === null, "the same link pushed again (a page load) is not taken twice");
const bad = accept({ state: "error", nonce: "e1", message: "This link is not valid: the version isn't valid." });
check(!!bad && "error" in bad && bad.error === "This link is not valid: the version isn't valid.", "an invalid link's message is kept");
const bare = accept({ state: "error", nonce: "e2", message: "  " });
check(!!bare && "error" in bare && bare.error.length > 0, "an error with no message still says something");
const noVersion = accept({ state: "open", nonce: "a2", dataset: "terminal-bench", version: null });
check(!!noVersion && "dataset" in noVersion && noVersion.version === null, "no version means none");

// Looking the name up.
const r1 = resolve(catalog, "terminal-bench", null);
check("entry" in r1 && r1.entry.name === "terminal-bench" && r1.version === "2.0", "no version picks the newest");
const r2 = resolve(catalog, "terminal-bench", "1.0");
check("entry" in r2 && r2.version === "1.0", "a listed version is kept");
const r3 = resolve(catalog, "nope", null);
check("error" in r3 && r3.error === notInCatalogue("nope") && r3.error === "nope isn't in the catalogue. Update the app or open the Marketplace.", "an unknown name");
const r4 = resolve(catalog, "terminal-bench", "9.9");
check("error" in r4 && r4.error.startsWith("terminal-bench 9.9 isn't in the catalogue"), "an unknown version");
check("error" in resolve(catalog, "Terminal-Bench", null), "names match exactly");

// Picking it: the test set and version change, and nothing else does.
const before = emptyDraft();
before.agents[0] = { ...newAgent("claude-code"), model: "anthropic/claude-x" };
before.job.job_name = "mine";
before.source = "path";
const snapshot = JSON.stringify(before);
if ("entry" in r2) {
  const after = pickFromLink(before, r2.entry, r2.version, undefined);
  check(JSON.stringify(before) === snapshot, "the draft passed in is not changed");
  check(after.source === "dataset" && after.dataset.name === "terminal-bench" && after.dataset.version === "1.0", "the link's test set and version are picked");
  check(JSON.stringify(after.agents) === JSON.stringify(before.agents), "the agents are unchanged");
  check(after.job.job_name === "mine" && JSON.stringify(after.env) === JSON.stringify(before.env), "the job's settings are unchanged");
  check(openedNotice(r2.entry, r2.version) === "Opened from the Marketplace: terminal-bench 1.0", "the notice names the set");
  // The same set again keeps its own filters, with the link's version.
  after.dataset.task_names = "hello-*";
  const again = pickFromLink(after, r2.entry, "2.0", r2.entry);
  check(again.dataset.task_names === "hello-*" && again.dataset.version === "2.0", "picking the set already picked keeps its filters");
  // Another set drops the old set's filters.
  if ("entry" in r1) {
    const other = resolve(catalog, "swebench-verified", null);
    if ("entry" in other) check(pickFromLink(again, other.entry, other.version, r2.entry).dataset.task_names === "", "another set starts without the old filters");
  }
}

offer({ nonce: "x", dataset: "terminal-bench", version: null });
settle();
check(fetches === 0, "nothing was fetched and no run was started");
if (failed.length) throw new Error(`run-link.check failed: ${failed.join("; ")}`);
console.log("run-link.check: ok");
