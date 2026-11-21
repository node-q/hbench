// Links from dataset or agent content never reach the desktop app's `/__actuaria/` page actions.
// Run with `npm test` (node's own runner; no DOM needed).
import { test } from "node:test";
import assert from "node:assert/strict";
import { isAppAction, safeUrl } from "../src/lib/fmt.ts";

const actions = [
  "/__actuaria/open-external?url=https://evil.example",
  "/__actuaria/logout",
  "/__actuaria",
  "/__ACTUARIA/login",
  "http://127.0.0.1:8080/__actuaria/open-folder",
  "https://localhost/__actuaria/update?do=install",
  "tauri://localhost/__actuaria/server-log",
  "/x/../__actuaria/logout",
  "/./__actuaria/logout",
  "/%2e%2e/__actuaria/logout",
  "/%5F%5Factuaria/logout",
  "/__actuaria%2Flogout",
  "\\__actuaria\\logout",
  "  /__actuaria/logout",
  "__actuaria/logout",
];

test("app actions are recognised however they are spelled", () => {
  for (const u of actions) assert.equal(isAppAction(u), true, u);
});

test("safeUrl refuses app actions and keeps ordinary links", () => {
  for (const u of actions) assert.equal(safeUrl(u), undefined, u);
  assert.equal(safeUrl("/api/jobs/x/trials/y/files/agent/a.png"), "/api/jobs/x/trials/y/files/agent/a.png");
  assert.equal(safeUrl("https://example.com/__actuaria-notes"), "https://example.com/__actuaria-notes");
  assert.equal(safeUrl("https://example.com/docs/__actuaria/x"), "https://example.com/docs/__actuaria/x");
  assert.equal(safeUrl("javascript:alert(1)"), undefined);
  assert.equal(safeUrl("data:image/png;base64,AAAA"), "data:image/png;base64,AAAA");
});
