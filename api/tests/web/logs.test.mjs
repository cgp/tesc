// The Logs page is a pure view of the records the server returned.
import assert from "node:assert/strict";
import test from "node:test";

Object.defineProperty(globalThis, "document", {
  configurable: true,
  value: {
    createElement() {
      return {
        textContent: "",
        get innerHTML() {
          return this.textContent
            .replaceAll("&", "&amp;")
            .replaceAll("<", "&lt;")
            .replaceAll(">", "&gt;")
            .replaceAll('"', "&quot;");
        },
      };
    },
  },
});

const { render, visible } = await import("../../web/js/logs.js");

const ENTRIES = [
  { seq: 1, time: "2026-09-24T10:00:00.001Z", level: "info", source: "reachability",
    message: "front end check ok endpoint=a status=404 connect=1.2ms", trace: null },
  { seq: 2, time: "2026-09-24T10:00:01.002Z", level: "error", source: "reachability",
    message: "collector probe failed endpoint=<a>", trace: "Traceback\nOSError: refused" },
];

function state(patch = {}) {
  return { logs: { entries: ENTRIES, latest: 2, capacity: 2000 }, logFilter: "all", ...patch };
}

test("the newest record is first", () => {
  assert.deepEqual(visible(ENTRIES, "all").map((e) => e.seq), [2, 1]);
});

test("the problems filter keeps warnings and errors only", () => {
  assert.deepEqual(visible(ENTRIES, "problems").map((e) => e.seq), [2]);
  const html = render(state({ logFilter: "problems" }));
  assert.doesNotMatch(html, /front end check ok/);
});

test("messages and traces are escaped, and times are shown to the millisecond", () => {
  const html = render(state());
  assert.match(html, /endpoint=&lt;a&gt;/);
  assert.match(html, /OSError: refused/);
  assert.match(html, /10:00:01\.002/);
  assert.match(html, /2 records held in memory/);
});

test("an empty filter says so rather than showing an empty table", () => {
  const html = render(state({ logs: { entries: [ENTRIES[0]], latest: 1, capacity: 2000 },
    logFilter: "problems" }));
  assert.match(html, /No warnings or errors/);
  assert.doesNotMatch(html, /<table/);
});

test("before the first read the page says what it is waiting for", () => {
  assert.match(render(state({ logs: null })), /Reading the server log/);
});
