// The charts page. What is tested here is the data preparation, not the pixels:
// where the nulls go decides whether a gap is drawn as a gap, and that is the one
// drawing rule this tool cannot compromise on.
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
    querySelector() {
      return null;
    },
  },
});
// uPlot is a browser global loaded by index.html; nothing under test here needs it.
Object.defineProperty(globalThis, "uPlot", { configurable: true, value: undefined });

const { align, extent, phaseSpans, render } = await import("../../web/js/charts.js");

function chart(patch = {}) {
  return {
    metrics: ["cpu.busy"],
    series: {
      "cpu.busy": {
        "box-a": [
          [0, 10],
          [1000, 12],
          [2000, 14],
        ],
      },
    },
    phases: [],
    gaps: [],
    annotations: [],
    baseline: {},
    ...patch,
  };
}

test("a target with no sample where another has one gets a null, not a value", () => {
  const data = align(
    chart({
      series: {
        "cpu.busy": {
          "box-a": [
            [0, 10],
            [1000, 12],
          ],
          // box-b missed the second interval entirely.
          "box-b": [[0, 20]],
        },
      },
    }),
    "cpu.busy",
    ["box-a", "box-b"]
  );

  assert.deepEqual(data[0], [0, 1], "x in seconds");
  assert.deepEqual(data[1], [10, 12]);
  assert.deepEqual(data[2], [20, null], "the missing interval breaks box-b's line");
});

test("a recorded gap gets an x of its own, so a hole with no samples still breaks", () => {
  // Every target stopped at once: without an inserted point there is no timestamp
  // in the window to hang a null on, and the line would be drawn straight across.
  const data = align(
    chart({
      series: {
        "cpu.busy": {
          "box-a": [
            [0, 10],
            [30000, 14],
          ],
        },
      },
      gaps: [{ target_id: "box-a", from_ms: 5000, to_ms: 25000, reason: "timeout" }],
    }),
    "cpu.busy",
    ["box-a"]
  );

  assert.deepEqual(data[0], [0, 15, 30], "a point in the middle of the gap");
  assert.deepEqual(data[1], [10, null, 14]);
});

test("with no gaps the line is continuous", () => {
  const data = align(chart(), "cpu.busy", ["box-a"]);
  assert.deepEqual(data[1], [10, 12, 14]);
  assert.ok(!data[1].includes(null));
});

test("phases collapse to one band each, however many targets recorded them", () => {
  const spans = phaseSpans(
    chart({
      phases: [
        { target_id: "box-a", phase: "baseline", from_ms: 0, to_ms: 9000 },
        { target_id: "box-b", phase: "baseline", from_ms: 0, to_ms: 9500 },
        { target_id: "box-a", phase: "settle", from_ms: 40000, to_ms: 59000 },
      ],
    })
  );

  assert.deepEqual(
    spans.map((s) => [s.phase, s.from, s.to]),
    [
      ["baseline", 0, 9500],
      ["settle", 40000, 59000],
    ],
    "one band per phase, widest extent, in time order"
  );
});

test("a phase still running has no end and is not dropped", () => {
  const spans = phaseSpans(
    chart({ phases: [{ target_id: "box-a", phase: "measure", from_ms: 0, to_ms: null }] })
  );
  assert.equal(spans.length, 1);
  assert.equal(spans[0].from, 0);
});

test("every chart carries a caption saying what it answers", () => {
  const markup = render({
    selectedRecording: { id: "r1", chart: chart({ metrics: ["cpu.busy", "made.up"] }) },
    live: null,
  });
  assert.match(markup, /How hard the machine was working/);
  // Nothing ships without one, so an unknown metric gets a generic line rather than
  // a card with an empty subtitle.
  assert.match(markup, /made\.up over the life of the recording/);
});

test("the page says what the shading means, and whether anything is broken", () => {
  const withGap = render({
    selectedRecording: {
      id: "r1",
      chart: chart({
        gaps: [{ target_id: "box-a", from_ms: 1, to_ms: 2, reason: "x" }],
        phases: [{ target_id: "box-a", phase: "settle", from_ms: 0, to_ms: 9 }],
      }),
    },
    live: null,
  });
  assert.match(withGap, /drawn as breaks in the line, never/);
  assert.match(withGap, /metrix-swatch/);

  const clean = render({ selectedRecording: { id: "r1", chart: chart() }, live: null });
  assert.match(clean, /Every interval was collected/);
});

test("a live recording is drawn in preference to one being read back", () => {
  const markup = render({
    selectedRecording: { id: "old", chart: chart({ metrics: ["old.metric"] }) },
    live: { recordingId: "new", chart: chart({ metrics: ["new.metric"] }) },
  });
  assert.match(markup, /new\.metric/);
  assert.doesNotMatch(markup, /old\.metric/);
});

test("a recording whose series has not arrived says so rather than looking broken", () => {
  const markup = render({ selectedRecording: { id: "r1" }, live: null });
  assert.match(markup, /Loading the series/);
});

test("every chart is drawn over the recording, not over its own metric", () => {
  // Host samples start at zero; the engine's first window lands wherever the load
  // was launched. Auto-scaled, the same pixel would be a different moment on each
  // chart — and reading one against the other is the whole point of the page.
  const span = extent(
    chart({
      metrics: ["cpu.busy", "load.achieved_rate"],
      series: {
        "cpu.busy": { "box-a": [[0, 10], [11000, 12]] },
        "load.achieved_rate": { "box-a": [[2300, 60], [10100, 59]] },
      },
    }),
    ["box-a"]
  );
  assert.deepEqual(span, [0, 11], "seconds, spanning both");
});

test("a recording with one sample still has a scale to draw on", () => {
  assert.deepEqual(
    extent(chart({ series: { "cpu.busy": { "box-a": [[5000, 1]] } } }), ["box-a"]),
    [5, 6]
  );
});

test("a recording with no samples has a scale rather than an empty one", () => {
  assert.deepEqual(extent(chart({ metrics: [], series: {} }), []), [0, 1]);
});
