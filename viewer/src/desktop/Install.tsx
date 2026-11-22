// The one-click install link's confirm screen. The desktop app parses `actuaria://install/actuaria/<bot>`, looks
// the bot up (in the catalogue) and sends the state here as `window.__ACTUARIA_INSTALL__` with an
// `actuaria:install` event (desktop/src-tauri/src/install.rs). Choices go back as `/__actuaria/install?do=..`.
// Everything from the catalogue is shown as plain text (React escapes it), capped again here.
// In the browser preview, `?desktop&install=1` shows a sample so the screen can be tried without the app.
import { useEffect, useState } from "react";
import * as Icon from "../ui/icons";
import { APP_NAME, desktopInfo, native } from "./bridge";

type Target = "claude" | "orca";

export interface InstallView {
  state: "none" | "loading" | "confirm" | "working" | "approval" | "done" | "error";
  nonce: string;
  bot?: { id: string; name: string; version: string; description: string; source: string } | null;
  targets: Target[];
  message?: string | null;
  output?: string | null;
  getHost?: Target | null;
}

declare global {
  interface Window {
    __ACTUARIA_INSTALL__?: InstallView;
  }
}

const NONE: InstallView = { state: "none", nonce: "", targets: [] };
const HOST: Record<Target, string> = { claude: "Claude Desktop", orca: "Orca" };
const AFTER: Record<Target, string> = {
  claude: "Claude Desktop then shows its own install dialog. Nothing is approved for you.",
  orca: "Orca then asks you to approve the plugin's access. Nothing is approved for you.",
};
const cap = (s: string | null | undefined, n: number) => (s ?? "").slice(0, n);

const demo = !desktopInfo && new URLSearchParams(location.search).has("install");
if (demo)
  window.__ACTUARIA_INSTALL__ = {
    state: "confirm",
    nonce: "demo",
    bot: { id: "prompt-slim", name: "Prompt Slim", version: "0.1.0", description: "Slims long prompts, logs and pasted docs to save LLM tokens. Deterministic and offline: no keys, no network.", source: "market.actuaria.io" },
    targets: ["claude", "orca"],
  };

function choose(what: "confirm" | "cancel" | "close" | "get-host", params: Record<string, string> = {}) {
  if (desktopInfo) native("install", { do: what, ...params });
  else if (demo) {
    window.__ACTUARIA_INSTALL__ = what === "confirm" ? { ...NONE, state: "done", message: "Handed over (preview)." } : NONE;
    window.dispatchEvent(new Event("actuaria:install"));
  }
}

function useInstall(): InstallView {
  const [view, setView] = useState<InstallView>(() => window.__ACTUARIA_INSTALL__ ?? NONE);
  useEffect(() => {
    const read = () => setView(window.__ACTUARIA_INSTALL__ ?? NONE);
    read();
    window.addEventListener("actuaria:install", read);
    return () => window.removeEventListener("actuaria:install", read);
  }, []);
  return view;
}

export function InstallDialog() {
  const v = useInstall();
  const [target, setTarget] = useState<Target | null>(null);
  useEffect(() => setTarget(v.targets.length === 1 ? v.targets[0] : null), [v.nonce, v.targets]);
  if (v.state === "none") return null;
  const closable = v.state !== "working";
  const close = () => closable && choose(v.state === "confirm" || v.state === "loading" ? "cancel" : "close");
  const bot = v.bot;
  let title = "Install a bot";
  let body: React.ReactNode = null;
  let actions: React.ReactNode = null;

  if (v.state === "loading") {
    body = (
      <p className="dk-install__muted">
        <span className="dk-spin" aria-hidden="true" /> Checking the catalogue…
      </p>
    );
    actions = <button className="btn btn--sm btn--ghost" onClick={close}>Cancel</button>;
  } else if (v.state === "confirm" && bot) {
    title = `Install ${cap(bot.name, 60)}?`;
    body = (
      <>
        <p className="dk-install__desc">{cap(bot.description, 400)}</p>
        <dl className="dk-install__facts">
          <dt>Version</dt>
          <dd className="mono">{cap(bot.version, 32)}</dd>
          <dt>Source</dt>
          <dd className="mono">{cap(bot.source, 40)}</dd>
          <dt>Publisher</dt>
          <dd>
            Actuaria <span className="dk-install__badge">First-party preview</span>
          </dd>
          <dt>Install into</dt>
          <dd>
            {v.targets.length === 1 ? (
              HOST[v.targets[0]]
            ) : (
              <span className="dk-install__targets" role="radiogroup" aria-label="Host">
                {v.targets.map((t) => (
                  <label key={t}>
                    <input type="radio" name="dk-install-target" checked={target === t} onChange={() => setTarget(t)} /> {HOST[t]}
                  </label>
                ))}
              </span>
            )}
          </dd>
        </dl>
        {target && <p className="dk-install__muted">{AFTER[target]}</p>}
      </>
    );
    actions = (
      <>
        <button className="btn btn--sm btn--ghost" onClick={close}>Cancel</button>
        <button className="btn btn--sm btn--primary" disabled={!target} onClick={() => target && choose("confirm", { target, nonce: v.nonce })}>
          Install
        </button>
      </>
    );
  } else if (v.state === "working") {
    title = bot ? `Installing ${cap(bot.name, 60)}` : "Installing";
    body = (
      <p className="dk-install__muted">
        <span className="dk-spin" aria-hidden="true" /> {cap(v.message, 300)}
      </p>
    );
  } else {
    title = v.state === "approval" ? "Needs your approval" : v.state === "done" ? "Handed over" : "Couldn't install";
    body = (
      <>
        <p>{cap(v.message, 500)}</p>
        {v.output && <pre className="dk-install__output mono">{cap(v.output, 4000)}</pre>}
      </>
    );
    actions = (
      <>
        {v.getHost === "claude" && (
          <button className="btn btn--sm btn--ghost" onClick={() => choose("get-host")}>
            Install Claude Desktop <Icon.External size={11} />
          </button>
        )}
        <button className="btn btn--sm btn--primary" onClick={close} autoFocus>
          Close
        </button>
      </>
    );
  }

  return (
    <div className="dk-install__scrim" onKeyDown={(e) => e.key === "Escape" && close()}>
      <section className="dk-install" data-state={v.state} role="dialog" aria-modal="true" aria-labelledby="dk-install-title">
        <header className="dk-install__head">
          <span className="dk-update__icon">{v.state === "error" ? <Icon.Alert size={16} /> : <Icon.Shield size={16} />}</span>
          <strong id="dk-install-title">{title}</strong>
          {closable && (
            <button className="dk-icon" aria-label="Close" title="Close" onClick={close}>
              <Icon.Close size={12} />
            </button>
          )}
        </header>
        <div className="dk-install__body">{body}</div>
        {actions && <div className="dk-update__actions">{actions}</div>}
        <p className="dk-install__foot">{APP_NAME} checks the download's SHA-256 against the catalogue before handing it over.</p>
      </section>
    </div>
  );
}
