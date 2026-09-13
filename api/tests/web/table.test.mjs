// The stats table: what it draws, how it orders, and what it copies out.
//
// Called directly rather than through the page wiring — the view is a pure function
// of state, and `rendering.test.mjs` covers the wiring.
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
            .replaceAll(">", "&gt;");
        },
      };
    },
  },
});

const { render, toDelimited } = await import("../../web/js/table.js");

function summary(metric, patch = {}) {
  return {
    metric,
    n: 30,
    min: 1,
    max: 9,
    mean: 5,
    p50: 5,
    p95: 8,
    iqr: 2,
    stddev: 1.5,
    supported: true,
    ...patch,
  };
}

const RECORDING = {
  id: "2026-09-13T00-00-00Z_aaaa",
  metrics: ["cpu.busy", "conn.established"],
  targets: ["box-a"],
  duration_ms: 60000,
  phases: [{ phase: "measure", target_id: "box-a", from_ms: 0, to_ms: 59000 }],
  gaps: [],
  latest: {},
  summary: {
    targets: {
      "box-a": {
        "cpu.busy": summary("cpu.busy", { p50: 40 }),
        "conn.established": summary("conn.established", { p50: 120 }),
      },
      "*": {
        "cpu.busy": summary("cpu.busy", { p50: 42, n: 60 }),
        "conn.established": summary("conn.established", { p50: 121, n: 60 }),
      },
    },
    spans: {
      "box-a": { first_ms: 0, last_ms: 59000, n: 60 },
      "*": { first_ms: 0, last_ms: 59000, n: 120 },
    },
  },
};

function view(patch = {}) {
  return render({ selectedRecording: { ...RECORDING, ...patch }, live: null, ...patch.state });
}

test("rows are grouped by target, with the pooled view first", () => {
  const markup = view();
  assert.match(markup, /every box/);
  assert.ok(
    markup.indexOf("every box") < markup.indexOf("box-a"),
    "the pooled group is the run total and reads first"
  );
  assert.match(markup, /data-group="\*"/);
  assert.match(markup, /data-group="box-a"/);
});

test("every column §14.2 asks for by default is present", () => {
  const markup = view();
  for (const label of ["Metric", "Count", "Min", "Median", "p95", "Max", "Std dev"]) {
    assert.match(markup, new RegExp(`>${label.replace(".", "\\.")}`), `missing ${label}`);
  }
});

test("the group header carries the span and the sample count", () => {
  const markup = view();
  assert.match(markup, /1m 00s/, "the span of the samples");
  assert.match(markup, /120 samples/);
});

test("a target with gaps is marked in place", () => {
  const markup = view({ gaps: [{ target_id: "box-a", from_ms: 1, to_ms: 2, reason: "x" }] });
  assert.match(markup, /1 gap\s*<\/span>/);
});

test("a withheld figure is an em dash with its count, never a blank or a zero", () => {
  const markup = view({
    summary: {
      ...RECORDING.summary,
      targets: {
        "box-a": { "cpu.busy": summary("cpu.busy", { n: 4, p95: null, supported: true }) },
      },
    },
  });
  assert.match(markup, /title="4 samples is too few">—/);
  assert.doesNotMatch(markup, />0</);
});

test("sorting orders within a group and puts withheld figures last", () => {
  const state = {
    live: null,
    tableSort: { key: "p50", direction: "desc" },
    selectedRecording: {
      ...RECORDING,
      summary: {
        ...RECORDING.summary,
        targets: {
          "box-a": {
            low: summary("low", { p50: 1 }),
            high: summary("high", { p50: 99 }),
            unknown: summary("unknown", { p50: null, n: 2 }),
          },
        },
      },
    },
  };
  const rows = [...render(state).matchAll(/data-metric="([^"]+)"/g)].map((m) => m[1]);
  assert.deepEqual(rows, ["high", "low", "unknown"], "absent is not small");

  state.tableSort = { key: "p50", direction: "asc" };
  const ascending = [...render(state).matchAll(/data-metric="([^"]+)"/g)].map((m) => m[1]);
  assert.deepEqual(ascending, ["low", "high", "unknown"], "still last, pointing the other way");
});

test("the active column says which way it is pointing", () => {
  const markup = render({
    live: null,
    selectedRecording: RECORDING,
    tableSort: { key: "p95", direction: "desc" },
  });
  assert.match(markup, /aria-sort="descending"/);
  assert.match(markup, /p95 ↓/);
});

test("export carries raw numbers in the order shown, not formatted ones", () => {
  const tsv = toDelimited(
    { live: null, selectedRecording: RECORDING, tableSort: { key: "p50", direction: "desc" } },
    "\t"
  );
  const lines = tsv.split("\n");
  assert.equal(lines[0], "target\tmetric\tn\tmin\tp50\tp95\tmax\tstddev");
  // Pooled group first, and within it the higher median first.
  assert.match(lines[1], /^\*\tconn\.established\t60\t1\t121\t8\t9\t1\.5$/);
  assert.match(lines[2], /^\*\tcpu\.busy\t60\t1\t42\t8\t9\t1\.5$/);
  assert.ok(lines.some((l) => l.startsWith("box-a\t")));
});

test("a withheld figure exports as an empty cell, not a zero", () => {
  const csv = toDelimited(
    {
      live: null,
      selectedRecording: {
        ...RECORDING,
        summary: {
          ...RECORDING.summary,
          targets: { "box-a": { "cpu.busy": summary("cpu.busy", { n: 4, p95: null }) } },
        },
      },
    },
    ","
  );
  assert.match(csv, /^box-a,cpu\.busy,4,1,5,,9,1\.5$/m, "the p95 column is empty");
});

test("the live table is the same view, fed from the stream", () => {
  const markup = render({
    live: {
      recordingId: "live-1",
      connection: "live",
      phase: "measure",
      elapsedMs: 12000,
      summaries: { "*": { "cpu.busy": summary("cpu.busy", { p50: 7 }) } },
      spans: { "*": { first_ms: 0, last_ms: 12000, n: 12 } },
      latest: {},
      gaps: [],
    },
    selectedRecording: null,
  });
  assert.match(markup, /data-live-table="live-1"/);
  assert.match(markup, /measure phase/);
  assert.match(markup, /Median/, "the same columns as a finished recording");
  assert.match(markup, /Stop/);
});

test("a live table re-renders when the sort changes, since patching cannot reorder", async () => {
  const { patch } = await import("../../web/js/table.js");
  const live = {
    recordingId: "live-1",
    connection: "live",
    elapsedMs: 1000,
    summaries: { "*": { "cpu.busy": summary("cpu.busy") } },
    spans: {},
    gaps: [],
  };
  // No card in this DOM stub, so patch bails early either way; what is asserted is
  // that the sort is part of what the view subscribes to.
  const { selectState } = await import("../../web/js/table.js");
  const sorted = { live, tableSort: { key: "p95", direction: "desc" } };
  const unsorted = { live, tableSort: null };
  assert.notDeepEqual(selectState(sorted), selectState(unsorted));
  assert.equal(patch({ live: null }), false);
});
