// Trial-page helpers: URL builders scoped to one trial (+ optional step).
import { enc, p, qs } from "../../lib/api";

export interface TrialRef {
  job: string;
  trial: string;
  step?: string;
}

/** `/api/jobs/<job>/trials/<trial>/<what>?step=…` */
export function api(t: TrialRef, what?: string, extra: Record<string, string> = {}): string {
  const base = p("jobs", t.job, "trials", t.trial);
  return `${base}${what ? `/${what}` : ""}${qs({ step: t.step, ...extra })}`;
}

/** Raw file under the trial (or step) root; path segments are encoded individually. */
export function fileUrl(t: TrialRef, path: string): string {
  return `${p("jobs", t.job, "trials", t.trial)}/files/${path.split("/").map(enc).join("/")}${qs({ step: t.step })}`;
}

export const MEDIA: Record<string, "image" | "audio" | "video"> = {
  png: "image", jpg: "image", jpeg: "image", gif: "image", webp: "image", svg: "image",
  wav: "audio", mp3: "audio", aac: "audio", oga: "audio", ogg: "audio", flac: "audio", weba: "audio", aiff: "audio", aif: "audio",
  mp4: "video", webm: "video", mov: "video",
};

export function mediaOf(path: string) {
  return MEDIA[path.split(".").pop()?.toLowerCase() ?? ""];
}

/** Text of an ATIF message / observation content (string or content parts). */
export function textOf(v: unknown): string {
  if (v == null) return "";
  if (typeof v === "string") return v;
  if (Array.isArray(v)) return v.map((x) => (x && typeof x === "object" && "text" in x ? String((x as any).text) : "")).filter(Boolean).join("\n");
  return JSON.stringify(v, null, 2);
}
