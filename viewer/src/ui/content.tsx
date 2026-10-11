// Rendering of file/text content: syntax-highlighted code, sanitized markdown, collapsible JSON.
import { useMemo, useState, type ReactNode } from "react";
import hljs from "highlight.js/lib/core";
import bash from "highlight.js/lib/languages/bash";
import python from "highlight.js/lib/languages/python";
import json from "highlight.js/lib/languages/json";
import ini from "highlight.js/lib/languages/ini";
import yaml from "highlight.js/lib/languages/yaml";
import markdown from "highlight.js/lib/languages/markdown";
import dockerfile from "highlight.js/lib/languages/dockerfile";
import javascript from "highlight.js/lib/languages/javascript";
import typescript from "highlight.js/lib/languages/typescript";
import rust from "highlight.js/lib/languages/rust";
import go from "highlight.js/lib/languages/go";
import c from "highlight.js/lib/languages/c";
import diff from "highlight.js/lib/languages/diff";
import xml from "highlight.js/lib/languages/xml";
import { marked } from "marked";
import DOMPurify from "dompurify";
import { CopyButton } from "./kit";
import * as Icon from "./icons";

for (const [name, lang] of Object.entries({ bash, python, json, ini, yaml, markdown, dockerfile, javascript, typescript, rust, go, c, diff, xml })) {
  hljs.registerLanguage(name, lang);
}

const EXT: Record<string, string> = {
  sh: "bash", bash: "bash", zsh: "bash", py: "python", json: "json", jsonl: "json", toml: "ini", ini: "ini", cfg: "ini",
  yaml: "yaml", yml: "yaml", md: "markdown", dockerfile: "dockerfile", js: "javascript", mjs: "javascript", ts: "typescript",
  tsx: "typescript", rs: "rust", go: "go", c: "c", h: "c", cpp: "c", diff: "diff", patch: "diff", html: "xml", xml: "xml",
};

/** highlight.js language for a file path, or undefined (plain text). */
export function langOf(path: string): string | undefined {
  const name = path.split("/").pop()!.toLowerCase();
  if (name === "dockerfile" || name.startsWith("dockerfile.")) return "dockerfile";
  return EXT[name.split(".").pop() ?? ""];
}

const MAX_HIGHLIGHT = 200_000; // chars; beyond this highlighting freezes the tab.

export function CodeView({ code, lang, path, wrap = false, lines = true, maxHeight, actions }: { code: string; lang?: string; path?: string; wrap?: boolean; lines?: boolean; maxHeight?: string; actions?: ReactNode }) {
  const language = lang ?? (path ? langOf(path) : undefined);
  const html = useMemo(() => {
    const src = code.length > MAX_HIGHLIGHT || !language ? escapeHtml(code) : hljs.highlight(code, { language, ignoreIllegals: true }).value;
    // One span per line so CSS counters can number them; hljs spans never cross a newline in these grammars except
    // multi-line strings/comments, which we close and reopen per line.
    return lines ? splitLines(src).map((l) => `<span class="ln">${l}</span>`).join("") : src;
  }, [code, language, lines]);
  return (
    <div style={{ position: "relative" }}>
      <div style={{ position: "absolute", top: 6, right: 6, display: "flex", gap: 4, zIndex: 1 }} className="code-actions">
        {actions}
        <CopyButton text={code} label="Copy contents" />
      </div>
      <pre className={`code${wrap ? " code--wrap" : ""}${lines ? " code--lines" : ""}`} style={maxHeight ? { maxHeight } : undefined}>
        <code dangerouslySetInnerHTML={{ __html: html }} />
      </pre>
    </div>
  );
}

function escapeHtml(s: string) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/** Split highlighted HTML into lines, re-opening any spans left open at a line break. */
function splitLines(html: string): string[] {
  const out: string[] = [];
  const open: string[] = [];
  for (const raw of html.split("\n")) {
    let line = open.join("") + raw;
    for (const m of raw.matchAll(/<span[^>]*>|<\/span>/g)) {
      if (m[0] === "</span>") open.pop();
      else open.push(m[0]);
    }
    line += "</span>".repeat(open.length);
    out.push(line);
  }
  if (out.length > 1 && out[out.length - 1] === "") out.pop();
  return out;
}

/** Markdown from agent/task files: sanitized (it is untrusted text), links open in a new tab. */
export function Markdown({ text, className = "" }: { text: string; className?: string }) {
  const html = useMemo(() => {
    const raw = marked.parse(text, { async: false, gfm: true, breaks: false }) as string;
    const clean = DOMPurify.sanitize(raw, { USE_PROFILES: { html: true } });
    return clean.replace(/<a /g, '<a target="_blank" rel="noreferrer noopener" ');
  }, [text]);
  return <div className={`md ${className}`} dangerouslySetInnerHTML={{ __html: html }} />;
}

/** Collapsible JSON tree for configs and metadata. Objects deeper than `open` start collapsed. */
export function JsonTree({ value, open = 2 }: { value: unknown; open?: number }) {
  return (
    <div className="json" style={{ font: "400 var(--text-xs)/1.7 var(--font-mono)", color: "var(--color-ink-2)", overflowX: "auto" }}>
      <Node v={value} depth={0} open={open} />
    </div>
  );
}

function Node({ v, depth, open, name }: { v: unknown; depth: number; open: number; name?: string }) {
  const [expanded, setExpanded] = useState(depth < open);
  const key = name != null && <span style={{ color: "var(--color-muted)" }}>{name}: </span>;
  if (v === null || typeof v !== "object") {
    const color = typeof v === "string" ? "var(--color-good)" : v === null ? "var(--color-faint)" : "var(--color-accent)";
    return (
      <div style={{ paddingLeft: depth ? 14 : 0, whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>
        {key}
        <span style={{ color }}>{JSON.stringify(v)}</span>
      </div>
    );
  }
  const entries = Array.isArray(v) ? v.map((x, i) => [String(i), x] as const) : Object.entries(v);
  const [l, r] = Array.isArray(v) ? ["[", "]"] : ["{", "}"];
  if (!entries.length)
    return (
      <div style={{ paddingLeft: depth ? 14 : 0 }}>
        {key}
        <span className="faint">
          {l}
          {r}
        </span>
      </div>
    );
  return (
    <div style={{ paddingLeft: depth ? 14 : 0 }}>
      <button onClick={() => setExpanded(!expanded)} style={{ all: "unset", cursor: "pointer", display: "inline-flex", alignItems: "center", gap: 2 }} aria-expanded={expanded}>
        <span style={{ display: "inline-flex", transform: expanded ? "rotate(90deg)" : undefined, transition: "transform var(--dur-micro) var(--ease-out)", color: "var(--color-faint)" }}>
          <Icon.Chevron size={10} />
        </span>
        {key}
        <span className="faint">
          {l}
          {!expanded && ` ${entries.length} ${Array.isArray(v) ? "items" : "keys"} ${r}`}
        </span>
      </button>
      {expanded && (
        <>
          {entries.map(([k, x]) => (
            <Node key={k} v={x} depth={depth + 1} open={open} name={Array.isArray(v) ? undefined : k} />
          ))}
          <span className="faint">{r}</span>
        </>
      )}
    </div>
  );
}
