// Check of the install dialog's outcome text (not bundled into the app): from viewer/,
// `npx rolldown src/desktop/install-view.check.ts --platform node --format esm --file /tmp/install-view.check.mjs && node /tmp/install-view.check.mjs`.
import { LINK_WENT_WRONG, outcome } from "./install-view";

const failed: string[] = [];
const check = (ok: boolean, what: string) => ok || failed.push(what);

for (const state of ["confirm", "loading-later", "", "mystery"]) {
  const o = outcome(state, null);
  check(o.title === LINK_WENT_WRONG.title && o.text === LINK_WENT_WRONG.text, `${JSON.stringify(state)} says the link went wrong`);
}
for (const message of [null, undefined, "", "   "]) {
  const o = outcome("error", message);
  check(o.title === "Couldn't install" && o.text.length > 0, `an error with message ${JSON.stringify(message)} still has a body`);
  for (const state of ["done", "approval"]) check(outcome(state, message).text.length > 0, `${state} with no message still has a body`);
}
check(outcome("error", "The download failed: timed out").text === "The download failed: timed out", "the app's message is shown");
for (const state of ["confirm", "done", "approval", "mystery", "working"]) {
  check(outcome(state, "x").title !== "Couldn't install", `only an error says Couldn't install (${state})`);
}
if (failed.length) throw new Error(`install-view.check failed: ${failed.join("; ")}`);
console.log("install-view.check: ok");
