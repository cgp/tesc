// Comparison mode: what the page claims about a group of runs, and what it refuses.
//
// The refusal is the part worth testing hardest. Merging across setups produces a
// distribution describing nothing, carrying a sample count that makes it look
// authoritative — so the merged column has to be gone, and the reason has to be on
// the page rather than left as an absence.
import assert from "node:assert/strict";
import test from "node:test";

Object.defineProperty(globalThis, "document", {
  configurable: true,
  value: { querySelector: () => null },
});
Object.defineProperty(globalThis, "uPlot", { configurable: true, value: undefined });

const { align, render } = await import("../../web/js/compare.js");

function summary(patch = {}) {
  return {
    metric: "cpu.busy",
    n: 30,
    min: 8,
    max: 12,
    mean: 10,
    p50: 10,
    p95: 11.8,
    iqr: 1,
    stddev: 0.9,
    supported: true,
    ...patch,
  };
}

function run(id, patch = {}) {
  return {
    id,
    started_at: "2026-09-01T00:00:00Z",
    profile: "staging",
    kind: "observation",
    addressing_mode: "load_balancer",
    series_key: "observation|staging|load_balancer|1s|api=0.0.0",
    status: "finished",
    duration_ms: 60000,
    is_baseline: false,
    annotations_by_severity: {},
    worst: null,
    targets: ["box-a"],
    ...patch,
  };
}

function comparison(patch = {}) {
  const runs = patch.runs ?? [run("run-a"), run("run-b")];
  return {
    phase: null,
    phases: [],
    reference_id: runs[0].id,
    mergeable: true,
    differences: {},
    metrics: ["cpu.busy"],
    runs,
    rows: {
      "cpu.busy": {
        per_run: Object.fromEntries(runs.map((r) => [r.id, summary()])),
        merged: summary({ n: 60 }),
        deltas: {
          [runs[1].id]: {
            metric: "cpu.busy",
            change: 0,
            change_pct: 0,
            band: 2,
            outside_band: false,
            worse: false,
            comparable: true,
          },
        },
      },
    },
    ...patch,
  };
}

const view = (c) => render({ comparison: c, comparisonSeries: null });

/* --------------------------------------------------------------------- the page */

test("with nothing selected it says where a comparison comes from", () => {
  const markup = render({ comparison: null });
  assert.match(markup, /Nothing selected to compare/);
  assert.match(markup, /#\/series/);
});

test("the earliest run is named as the reference the deltas are measured from", () => {
  const markup = view(comparison());
  assert.match(markup, /reference/);
  assert.match(markup, /what changed since/);
});

/* ----------------------------------------------------------- the merged column */

test("runs of one setup get a merged column, described as a merge not an average", () => {
  const markup = view(comparison());
  assert.match(markup, /merged/);
  assert.match(markup, /not an average/);
  assert.match(markup, /mean of several medians is a number no run ever produced/);
});

test("the merged column carries the summed count, which is the point of it", () => {
  const markup = view(comparison());
  assert.match(markup, /60/, "thirty readings from each of two runs");
});

test("runs of different setups lose the merged column entirely", () => {
  const markup = view(
    comparison({
      mergeable: false,
      differences: { profile: ["prod", "staging"] },
      rows: {
        "cpu.busy": {
          per_run: { "run-a": summary(), "run-b": summary() },
          merged: null,
          deltas: {},
        },
      },
    })
  );
  assert.doesNotMatch(markup, /<th class="num metrix-merged"/);
  assert.match(markup, /not merged/);
});

test("and the page says which part of the setup differs, not merely that one does", () => {
  const markup = view(
    comparison({
      mergeable: false,
      differences: { profile: ["prod", "staging"], "api version": ["0.1.0", "0.2.0"] },
      rows: { "cpu.busy": { per_run: {}, merged: null, deltas: {} } },
    })
  );
  assert.match(markup, /profile differs \(prod vs staging\)/);
  assert.match(markup, /api version differs \(0\.1\.0 vs 0\.2\.0\)/);
});

test("a refused merge still leaves the runs side by side", () => {
  // Reading two setups against each other deliberately is the sanctioned way to
  // compare across a setup change. It is the pooling that is wrong, not the looking.
  const markup = view(
    comparison({
      mergeable: false,
      differences: { profile: ["prod", "staging"] },
      rows: {
        "cpu.busy": {
          per_run: { "run-a": summary(), "run-b": summary({ p50: 40 }) },
          merged: null,
          deltas: {},
        },
      },
    })
  );
  assert.match(markup, /run-a/);
  assert.match(markup, /run-b/);
  assert.match(markup, /side by side is a fair thing to do/);
});

/* -------------------------------------------------------------- the sample rule */

test("a figure the count cannot support is a dash carrying that count", () => {
  const markup = view(
    comparison({
      rows: {
        "cpu.busy": {
          per_run: { "run-a": summary({ p95: null, n: 8 }), "run-b": summary() },
          merged: summary({ n: 38 }),
          deltas: {},
        },
      },
    })
  );
  assert.match(markup, /n=8/);
  assert.doesNotMatch(markup, />0\.00</, "never a zero");
});

test("a delta neither window can support is suppressed, not shown with a caveat", () => {
  // A table is exactly where a spurious percentage gets quoted without its caveat.
  const markup = view(
    comparison({
      rows: {
        "cpu.busy": {
          per_run: { "run-a": summary(), "run-b": summary() },
          merged: summary(),
          deltas: {
            "run-b": {
              metric: "cpu.busy",
              change: null,
              change_pct: null,
              band: null,
              outside_band: false,
              worse: false,
              comparable: false,
            },
          },
        },
      },
    })
  );
  assert.match(markup, /too few samples\s+to compare/);
  assert.doesNotMatch(markup, /%\)/, "no percentage to quote");
});

test("a delta inside the reference run's own spread says so", () => {
  assert.match(view(comparison()), /within the reference's own spread/);
});

/* -------------------------------------------------------------------- the window */

test("phases are offered only when the runs share more than one", () => {
  assert.doesNotMatch(view(comparison({ phases: ["measure"] })), /data-action="compare-phase"/);

  const markup = view(comparison({ phases: ["baseline", "measure", "settle"] }));
  assert.match(markup, /Whole run/);
  assert.match(markup, /environment drift/);
  assert.match(markup, /whether recovery is degrading/);
});

test("the window in force is the one marked active", () => {
  const markup = view(comparison({ phase: "settle", phases: ["baseline", "settle"] }));
  assert.match(markup, /data-action="compare-phase" data-phase="settle"[^>]*>settle/);
});

/* ------------------------------------------------------------------ the overlay */

test("each run is a column, and a run with no reading there breaks its own line", () => {
  const data = align(
    {
      "run-a": { series: { "cpu.busy": { "box-a": [[0, 10], [1000, 12]] } } },
      // run-b stopped after the first interval.
      "run-b": { series: { "cpu.busy": { "box-a": [[0, 20]] } } },
    },
    "cpu.busy",
    ["run-a", "run-b"]
  );

  assert.deepEqual(data[0], [0, 1], "seconds since each run started");
  assert.deepEqual(data[1], [10, 12]);
  assert.deepEqual(data[2], [20, null], "the shorter run stops rather than being carried");
});

test("a run's boxes are pooled into one line, because six runs of three boxes is not a chart", () => {
  const data = align(
    {
      "run-a": {
        series: { "cpu.busy": { "box-a": [[0, 10]], "box-b": [[0, 20]], "box-c": [[0, 30]] } },
      },
    },
    "cpu.busy",
    ["run-a"]
  );
  assert.equal(data.length, 2, "one x column and one run column");
  assert.deepEqual(data[1], [20], "the three boxes at that moment, together");
});

test("a run that never collected the metric is an empty line, not an error", () => {
  const data = align(
    { "run-a": { series: { "cpu.busy": { "box-a": [[0, 10]] } } }, "run-b": { series: {} } },
    "cpu.busy",
    ["run-a", "run-b"]
  );
  assert.deepEqual(data[1], [10]);
  assert.deepEqual(data[2], [null]);
});

test("runs of different lengths keep their own ends", () => {
  const data = align(
    {
      "run-a": { series: { "cpu.busy": { "box-a": [[0, 1], [1000, 2], [2000, 3]] } } },
      "run-b": { series: { "cpu.busy": { "box-a": [[0, 9]] } } },
    },
    "cpu.busy",
    ["run-a", "run-b"]
  );
  assert.deepEqual(data[0], [0, 1, 2]);
  assert.deepEqual(data[2], [9, null, null]);
});
