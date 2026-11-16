// App updates. The desktop app checks for a newer release, keeps the state and sends it here as
// `window.__ACTUARIA_UPDATE__` with an `actuaria:update` event (see desktop/src-tauri/src/update.rs). The page shows
// a prompt; once dismissed, the status bar keeps an update action. Choices go back as `/__actuaria/update?do=..`.
// In the browser preview, `?desktop&update=<version>` fakes an update so the flow can be tried without the app.
import { useEffect, useState } from "react";
import * as Icon from "../ui/icons";
import { APP_NAME, desktopInfo, native } from "./bridge";

export interface UpdateView {
  state: "none" | "available" | "downloading" | "ready" | "error";
  current: string;
  version?: string | null;
  progress?: number | null;
  error?: string | null;
  prompt: boolean;
  installRestarts: boolean;
}

type Choice = "install" | "dismiss" | "show" | "restart" | "notes";

declare global {
  interface Window {
    __ACTUARIA_UPDATE__?: UpdateView;
  }
}

const NONE: UpdateView = { state: "none", current: desktopInfo?.version ?? "", prompt: false, installRestarts: false };

// ---- Browser preview: the app's state machine, faked.
const demoVersion = !desktopInfo ? new URLSearchParams(location.search).get("update") : null;
if (demoVersion) sessionStorage.setItem("hb:update-demo", demoVersion);
const demo = !desktopInfo ? sessionStorage.getItem("hb:update-demo") : null;
let fake: UpdateView = demo ? { state: "available", current: "0.3.16", version: demo, prompt: true, installRestarts: false } : NONE;
function setFake(v: Partial<UpdateView>) {
  fake = { ...fake, ...v };
  window.__ACTUARIA_UPDATE__ = fake;
  window.dispatchEvent(new Event("actuaria:update"));
}
function fakeChoice(c: Choice) {
  if (c === "dismiss") setFake({ prompt: false, ...(fake.state === "error" ? { state: "available", error: null } : {}) });
  else if (c === "show") setFake({ prompt: fake.state !== "none" });
  else if (c === "restart") setFake({ ...NONE, current: fake.version ?? "" });
  else if (c === "notes") console.info("[desktop preview] release notes for", fake.version);
  else if (c === "install") {
    let p = 0;
    setFake({ state: "downloading", progress: 0 });
    const t = setInterval(() => {
      p += 0.025;
      if (p < 1) return setFake({ progress: p });
      clearInterval(t);
      setFake({ state: "ready", progress: null, prompt: true });
    }, 75);
  }
}
if (demo) window.__ACTUARIA_UPDATE__ = fake;

export function choose(c: Choice) {
  if (desktopInfo) native("update", { do: c });
  else if (demo) fakeChoice(c);
}

export function useUpdate(): UpdateView {
  const [view, setView] = useState<UpdateView>(() => window.__ACTUARIA_UPDATE__ ?? NONE);
  useEffect(() => {
    const read = () => setView(window.__ACTUARIA_UPDATE__ ?? NONE);
    read();
    window.addEventListener("actuaria:update", read);
    return () => window.removeEventListener("actuaria:update", read);
  }, []);
  return view;
}

const pct = (p?: number | null) => (p == null ? "" : `${Math.round(p * 100)}%`);
const runsNote = (n: number) => `${n === 1 ? "A run is" : `${n} runs are`} in progress: restarting stops ${n === 1 ? "it" : "them"}.`;

/** The prompt, bottom right above the status bar. */
export function UpdatePrompt({ u, nRunning }: { u: UpdateView; nRunning: number }) {
  if (!u.prompt || u.state === "none") return null;
  const v = u.version ?? "";
  let icon = <Icon.Download size={16} />;
  let title: string;
  let body: React.ReactNode;
  let actions: React.ReactNode = null;
  if (u.state === "available") {
    title = `${APP_NAME} ${v} is available`;
    body = (
      <>
        You have {u.current}.{" "}
        <button className="dk-update__link" onClick={() => choose("notes")}>
          What's new <Icon.External size={11} />
        </button>
        {u.installRestarts && nRunning > 0 && <span className="dk-update__warn">{runsNote(nRunning)}</span>}
      </>
    );
    actions = (
      <>
        <button className="btn btn--sm btn--ghost" onClick={() => choose("dismiss")}>
          Later
        </button>
        <button className="btn btn--sm btn--primary" onClick={() => choose("install")} autoFocus>
          {u.installRestarts ? "Update and restart" : "Update now"}
        </button>
      </>
    );
  } else if (u.state === "downloading") {
    icon = <span className="dk-spin" aria-hidden="true" />;
    title = `Downloading ${APP_NAME} ${v}…`;
    body = (
      <div className="dk-update__bar" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={u.progress == null ? undefined : Math.round(u.progress * 100)}>
        <span style={{ width: u.progress == null ? "100%" : pct(u.progress) }} data-indeterminate={u.progress == null} />
      </div>
    );
  } else if (u.state === "ready") {
    icon = <Icon.Check size={16} />;
    title = `${APP_NAME} ${v} is ready`;
    body = (
      <>
        Restart to finish updating.
        {nRunning > 0 && <span className="dk-update__warn">{runsNote(nRunning)}</span>}
      </>
    );
    actions = (
      <>
        <button className="btn btn--sm btn--ghost" onClick={() => choose("dismiss")}>
          Later
        </button>
        <button className="btn btn--sm btn--primary" onClick={() => choose("restart")} autoFocus>
          Restart now
        </button>
      </>
    );
  } else {
    icon = <Icon.Alert size={16} />;
    title = `Couldn't update ${APP_NAME}`;
    body = <span className="dk-update__error">{u.error}</span>;
    actions = (
      <>
        <button className="btn btn--sm btn--ghost" onClick={() => choose("dismiss")}>
          Dismiss
        </button>
        <button className="btn btn--sm btn--primary" onClick={() => choose("install")}>
          Try again
        </button>
      </>
    );
  }
  return (
    <section className="dk-update" data-state={u.state} role="region" aria-label="Update" onKeyDown={(e) => e.key === "Escape" && choose("dismiss")}>
      <span className="dk-update__icon">{icon}</span>
      <div className="dk-update__main">
        <div className="dk-update__head">
          <strong aria-live="polite">{title}</strong>
          {u.state === "downloading" && <span className="dk-update__pct mono">{pct(u.progress)}</span>}
        </div>
        <div className="dk-update__body">{body}</div>
        {actions && <div className="dk-update__actions">{actions}</div>}
      </div>
      <button className="dk-icon dk-update__close" aria-label={u.state === "downloading" ? "Hide" : "Close"} title={u.state === "downloading" ? "Hide (keeps downloading)" : "Close"} onClick={() => choose("dismiss")}>
        <Icon.Close size={12} />
      </button>
    </section>
  );
}

/** The status bar's update action: what is left once the prompt is dismissed. */
export function UpdateStatus({ u, nRunning }: { u: UpdateView; nRunning: number }) {
  const v = u.version ?? "";
  if (u.state === "none") return null;
  if (u.state === "available")
    return (
      <button className="dk-status__item dk-status__item--update" title={`Download and install ${APP_NAME} ${v}`} onClick={() => choose(u.installRestarts && nRunning > 0 ? "show" : "install")}>
        <Icon.Download size={12} /> Update to {v}
      </button>
    );
  if (u.state === "downloading")
    return (
      <button className="dk-status__item" title={`Downloading ${APP_NAME} ${v}`} onClick={() => choose("show")}>
        <span className="dk-spin" aria-hidden="true" /> Updating {pct(u.progress)}
      </button>
    );
  if (u.state === "ready")
    return (
      <button className="dk-status__item dk-status__item--update" title={`Restart to finish updating to ${v}`} onClick={() => choose(nRunning > 0 ? "show" : "restart")}>
        <Icon.Refresh size={12} /> Restart to update
      </button>
    );
  return (
    <button className="dk-status__item dk-status__item--bad" title={u.error ?? undefined} onClick={() => choose("show")}>
      <Icon.Alert size={12} /> Update failed
    </button>
  );
}
