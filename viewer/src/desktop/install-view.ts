// What the install dialog says (desktop/src-tauri/src/install.rs sends the state). The app pushes its state on
// every page load, so a state that is empty, missing or unknown means no dialog. Only an error says
// "Couldn't install"; a confirm without its bot says the link went wrong; no screen has an empty body.

export type InstallState = "none" | "loading" | "confirm" | "working" | "approval" | "done" | "error";

const SHOWN: readonly string[] = ["loading", "confirm", "working", "approval", "done", "error"];

/** Whether `state` opens the dialog: "none", "" and states this page doesn't know don't. */
export const shows = (state: unknown): state is Exclude<InstallState, "none"> => typeof state === "string" && SHOWN.includes(state);

const FALLBACK: Partial<Record<string, string>> = {
  error: "Nothing was installed. Open the link again.",
  approval: "Orca needs your approval before it installs this. Review the access it asks for below.",
  done: "The bot was handed to its host's installer. Finish there.",
};

export const LINK_WENT_WRONG = {
  title: "Something went wrong opening this link",
  text: "Nothing was installed. Open the install link again from market.actuaria.io; if it keeps happening, send us the app's log.",
};

/**
 * The title and text of the screen after the confirm step (error, approval, done), with the app's `message`
 * when it has one. A confirm without its bot gets [`LINK_WENT_WRONG`].
 */
export function outcome(state: string, message: string | null | undefined): { title: string; text: string; known: boolean } {
  const fallback = FALLBACK[state];
  if (!fallback) return { ...LINK_WENT_WRONG, known: false };
  const title = state === "error" ? "Couldn't install" : state === "approval" ? "Needs your approval" : "Handed over";
  return { title, text: message?.trim() || fallback, known: true };
}
