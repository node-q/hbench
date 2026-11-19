// Signing in to Actuaria from the desktop app (hbench-studio docs/design/sign-in-products.md 7.2). The page never
// holds a token: it asks the app to sign in or out by navigating to `/__actuaria/login`, `/logout` or `/account`;
// the app runs the bundled `hbench login --browser` (or `hbench logout`) and reports progress as
// `window.__ACTUARIA_SIGNIN__` with an `actuaria:signin` event (desktop/src-tauri/src/signin.rs). Who is signed in
// comes from the local viewer's `/api/account`, which asks the market with hbench's own sign-in.
import { useEffect, useMemo, useRef, useState } from "react";
import * as Icon from "../ui/icons";
import { useApi } from "../lib/api";
import { useToast } from "../ui/overlay";
import { useCommands, type Command } from "../shell";
import { desktopInfo, native } from "./bridge";

export interface SigninView {
  /** `waiting`: a sign-in is open in the browser. */
  state: "idle" | "waiting";
  /** What last finished, and how. */
  last?: "login" | "logout" | null;
  ok?: boolean | null;
  /** The address to open by hand when no browser opened. Not a secret: the code and tokens never reach the page. */
  url?: string | null;
  error?: string | null;
  /** Bumped by the app at each change. */
  seq: number;
}

interface AccountInfo {
  signed_in: boolean;
  account?: Record<string, any> | null;
  error?: string;
}

declare global {
  interface Window {
    __ACTUARIA_SIGNIN__?: SigninView;
  }
}

const IDLE: SigninView = { state: "idle", seq: 0 };

function useSignin(): SigninView {
  const [v, setV] = useState<SigninView>(() => window.__ACTUARIA_SIGNIN__ ?? IDLE);
  useEffect(() => {
    const on = () => setV(window.__ACTUARIA_SIGNIN__ ?? IDLE);
    window.addEventListener("actuaria:signin", on);
    return () => window.removeEventListener("actuaria:signin", on);
  }, []);
  return v;
}

/** A name to show for the account `whoami` returned. */
function nameOf(a: AccountInfo | undefined): string {
  const x = a?.account;
  const person = x?.person ?? x;
  return person?.name || person?.email || person?.username || "Signed in";
}

export function useAccount() {
  const signin = useSignin();
  const toast = useToast();
  // Only the app's viewer is asked: the account route needs the launch key the app hands its window.
  const account = useApi<AccountInfo>(desktopInfo ? "/api/account" : null);
  const seen = useRef(signin.seq);
  const reload = account.reload;

  useEffect(() => {
    if (signin.seq === seen.current) return;
    seen.current = signin.seq;
    if (signin.state === "waiting" && signin.url) {
      const url = signin.url;
      toast(
        {
          text: "No browser opened. Open the sign-in address in your browser to continue.",
          action: { label: "Copy address", run: () => void navigator.clipboard?.writeText(url) },
        },
        300_000,
      );
    }
    if (signin.state !== "idle") return;
    reload();
    if (signin.last === "login") {
      toast(signin.ok ? { text: "Signed in to Actuaria." } : { text: `Sign-in was not completed${signin.error ? `: ${signin.error}` : "."}`, kind: "bad" }, 8000);
    } else if (signin.last === "logout") {
      toast(
        signin.ok
          ? { text: "Signed out of Actuaria Studio Essentials. Your browser may still be signed in to Actuaria; sign out there too on a shared computer." }
          : { text: `Signed out on this computer, but: ${signin.error ?? "the sign-out was not confirmed."}`, kind: "bad" },
        10000,
      );
    }
  }, [signin, toast, reload]);

  const signedIn = !!account.data?.signed_in;
  const cmds: Command[] = useMemo(
    () =>
      !desktopInfo
        ? []
        : signedIn
          ? [
              { id: "a:account", label: "Manage your Actuaria account", group: "Account", run: () => native("account") },
              { id: "a:logout", label: "Sign out of Actuaria", group: "Account", run: () => native("logout") },
            ]
          : [{ id: "a:login", label: "Sign in to Actuaria", group: "Account", run: () => native("login") }],
    [signedIn],
  );
  useCommands(cmds);
  return { signin, account };
}

/** The status bar's account item: sign in, the sign-in in progress, or who is signed in. */
export function AccountStatus() {
  const { signin, account } = useAccount();
  if (!desktopInfo) return null;
  if (signin.state === "waiting") {
    return (
      <button className="dk-status__item" title="Finish signing in in your browser. Click to start again." onClick={() => native("login")}>
        <span className="dk-spin" aria-hidden="true" /> Signing in…
      </button>
    );
  }
  if (account.data?.signed_in) {
    const name = nameOf(account.data);
    return (
      <button className="dk-status__item" title={account.data.error ? `Signed in. ${account.data.error}` : `${name}\nManage your Actuaria account`} onClick={() => native("account")}>
        <Icon.Shield size={12} />
        <span className="trunc">{name}</span>
      </button>
    );
  }
  return (
    <button className="dk-status__item" title="Sign in to Actuaria in your browser" onClick={() => native("login")}>
      <Icon.Lock size={12} /> Sign in
    </button>
  );
}
