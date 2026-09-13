// What the recording detail draws for a baseline comparison. The view is a pure
// function of state, so this calls it directly; `rendering.test.mjs` covers wiring.
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

const { render } = await import("../../web/js/recordings.js");

function summary(patch = {}) {
  return { metric: "cpu.busy", n: 30, p50: 6, p95: 9, iqr: 2, supported: true, ...patch };
}

function recording(patch = {}) {
  return {
    id: "2026-09-13T00-00-00Z_aaaa",
    kind: "observation",
    status: "finished",
    profile: "staging",
    addressing_mode: "load_balancer",
    series_key: "observation|staging|load_balancer|1s|api=0.0.0",
    duration_ms: 60000,
    is_baseline: false,
    targets: ["box-a"],
    metrics: ["cpu.busy"],
    phases: [],
    gaps: [],
    annotations: [],
    identity: {},
    filesystems: [],
    inventory: null,
    latest: {},
    comparison: null,
    ...patch,
  };
}

function view(patch) {
  return render({ selectedRecording: recording(patch), recordings: [] });
}

test("with no baseline set, the card says what a baseline is for", () => {
  const markup = view({ comparison: { baseline_id: null, moved: [], targets: {} } });
  assert.match(markup, /No baseline is set for this series/);
  assert.match(markup, /Set as baseline/);
});

test("a metric that moved is shown with the count behind every figure", () => {
  const markup = view({
    comparison: {
      baseline_id: "2026-09-01T00-00-00Z_bbbb",
      only_now: [],
      only_baseline: [],
      targets: {},
      moved: [
        {
          target: "*",
          metric: "cpu.busy",
          baseline: summary(),
          current: summary({ n: 28, p50: 56 }),
          change: 50,
          change_pct: 833.3,
          band: 4,
          outside_band: true,
          worse: true,
          comparable: true,
        },
      ],
    },
  });

  assert.match(markup, /cpu\.busy/);
  assert.match(markup, /n=30/, "the baseline's count");
  assert.match(markup, /n=28/, "this recording's count");
  assert.match(markup, /\+50\.0/);
  assert.match(markup, /text-danger/, "more CPU is the bad direction");
});

test("a move the good way is not coloured as a regression", () => {
  const markup = view({
    comparison: {
      baseline_id: "b",
      only_now: [],
      only_baseline: [],
      targets: {},
      moved: [
        {
          target: "*",
          metric: "mem.available_bytes",
          baseline: summary({ metric: "mem.available_bytes", p50: 1e9 }),
          current: summary({ metric: "mem.available_bytes", p50: 4e9 }),
          change: 3e9,
          change_pct: 300,
          band: 1e8,
          outside_band: true,
          worse: false,
          comparable: true,
        },
      ],
    },
  });
  assert.match(markup, /text-success/);
  assert.doesNotMatch(markup, /text-danger/);
});

test("nothing moved is stated rather than left as an empty table", () => {
  const markup = view({
    comparison: { baseline_id: "b", only_now: [], only_baseline: [], targets: {}, moved: [] },
  });
  assert.match(markup, /behaving the way it normally does/);
  assert.doesNotMatch(markup, /<tbody><\/tbody>/);
});

test("a figure the sample count cannot support is withheld, with its count", () => {
  const markup = view({
    comparison: {
      baseline_id: "b",
      only_now: [],
      only_baseline: [],
      targets: {},
      moved: [
        {
          target: "app-2",
          metric: "cpu.busy",
          baseline: summary({ n: 2, p50: null, supported: false }),
          current: summary(),
          change: 1,
          change_pct: 10,
          band: 1,
          outside_band: true,
          worse: true,
          comparable: false,
        },
      ],
    },
  });
  assert.match(markup, /n=2/);
  assert.match(markup, /too few/);
});

test("the recording that is the baseline says so", () => {
  // The API sends a JSON boolean here, not 0/1 -- checking `=== 1` never matched.
  const markup = view({
    is_baseline: true,
    comparison: { baseline_id: null, moved: [], targets: {} },
  });
  assert.match(markup, /is<\/strong> that baseline/);
  assert.match(markup, /data-baseline="1"/);
});
