// Check of the catalog's details store (not bundled into the app): from viewer/,
// `npx rolldown src/pages/run/details.check.ts --platform node --format esm --file /tmp/details.check.mjs && node /tmp/details.check.mjs`.
import { BATCH, IN_FLIGHT, detailsStore, type DetailsAnswer } from "./details";
import type { CatalogDataset } from "../../lib/types";

const ds = (name: string, details: CatalogDataset["details"] = null): CatalogDataset => ({ name, version: "1.0", versions: ["1.0"], domain: "law-compliance", details });
const info = { description: "d", n_tasks: 3, license: "MIT", upstream_url: null, creator: null };
const failed: string[] = [];
const fail = (what: string) => void failed.push(what);
const check = (ok: boolean, what: string) => ok || fail(what);
const settle = () => new Promise((r) => setTimeout(r, 0));

const calls: { names: string[]; resolve: (a: DetailsAnswer) => void; reject: (e: Error) => void }[] = [];
const store = detailsStore((names) => new Promise((resolve, reject) => calls.push({ names, resolve, reject })));
let renders = 0;
store.subscribe(() => renders++);

// What the index carried is ready at once, and isn't asked for.
const given = ds("given", info);
check(store.state(given).status === "ready", "index details are ready");
const many = Array.from({ length: BATCH * 3 }, (_, i) => ds(`set-${i}`));
store.load([given, ...many]);
check(many.every((d) => store.state(d).status === "loading"), "each card shows loading");
check(calls.length === IN_FLIGHT && calls.every((c) => c.names.length === BATCH), `${IN_FLIGHT} requests of ${BATCH} at a time`);
check(!calls.some((c) => c.names.includes("given")), "a dataset the index described is not fetched");

// Loading again asks for nothing new.
store.load(many);
check(calls.length === IN_FLIGHT, "no request twice");

// The first batch answers, all but one; the one gets its own error.
const first = calls[0];
const details = Object.fromEntries(first.names.slice(1).map((n) => [n, info]));
first.resolve({ details, errors: { [first.names[0]]: "manifest unknown" } });
await settle();
const s0 = store.state(many[0]);
check(s0.status === "error" && s0.message === "manifest unknown", "a failed card says why");
check(store.state(many[1]).status === "ready", "the rest of the batch is ready");
check(calls.length === IN_FLIGHT + 1, "the next batch starts when one ends");

// A whole request failing marks each of its cards, never leaves them loading.
calls[1].reject(new Error("Opening 6 dataset(s) took more than 40s."));
await settle();
check(store.state(many[BATCH]).status === "error", "a failed request fails its cards");
calls[2].resolve({ details: {}, errors: {} });
await settle();
const silent = store.state(many[BATCH * 2]);
check(silent.status === "error" && silent.message.length > 0, "an answer without the dataset is an error with a message");
check(many.every((d) => store.state(d).status !== "loading"), "nothing is left loading");

// Retry asks for that one card again.
store.retry(many[0]);
check(store.state(many[0]).status === "loading" && calls.at(-1)!.names.join() === many[0].name, "retry fetches the one card");
calls.at(-1)!.resolve({ details: { [many[0].name]: info }, errors: {} });
await settle();
check(store.state(many[0]).status === "ready", "retry fills the card");
check(renders > 0, "changes are announced");
if (failed.length) throw new Error(`details.check failed: ${failed.join("; ")}`);
console.log("details.check: ok");
