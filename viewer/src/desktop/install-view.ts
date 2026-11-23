// What the install dialog says once the app has an answer (desktop/src-tauri/src/install.rs sends the state).
// Only an error says "Couldn't install"; a state the page doesn't expect (a confirm without its bot, or a state
// added later) says the link went wrong, and no state shows an empty body.

export type InstallState = "none" | "loading" | "confirm" | "working" | "approval" | "done" | "error";

const FALLBACK: Partial<Record<string, string>> = {
  error: "Nothing was installed. Open the link again.",
  approval: "Orca needs your approval before it installs this. Review the access it asks for below.",
  done: "The bot was handed to its host's installer. Finish there.",
};

export const LINK_WENT_WRONG = {
  title: "Something went wrong opening this link",
  text: "Nothing was installed. Open the install link again from market.actuaria.io; if it keeps happening, send us the app's log.",
};

/** The title and text of an outcome screen for `state`, with the app's `message` when it has one. */
export function outcome(state: string, message: string | null | undefined): { title: string; text: string; known: boolean } {
  const fallback = FALLBACK[state];
  if (!fallback) return { ...LINK_WENT_WRONG, known: false };
  const title = state === "error" ? "Couldn't install" : state === "approval" ? "Needs your approval" : "Handed over";
  return { title, text: message?.trim() || fallback, known: true };
}
