// Takes the run link the desktop app hands over (run-link.ts) and opens New run, which applies it. The app is
// told the link was taken, so the next one may come; the page can't make a run link of its own.
import { useEffect } from "react";
import { useNavigate } from "react-router";
import { native } from "./bridge";
import { accept, offer } from "./run-link";

export function RunLinkListener() {
  const navigate = useNavigate();
  useEffect(() => {
    const read = () => {
      const r = accept(window.__ACTUARIA_RUN__);
      if (!r) return;
      native("run-link", { do: "taken", nonce: r.nonce });
      offer(r);
      navigate("/run");
    };
    read();
    window.addEventListener("actuaria:run", read);
    return () => window.removeEventListener("actuaria:run", read);
  }, [navigate]);
  return null;
}
