// Run series: what the page claims about a history, and what it refuses to claim.
//
// The drawing is tested through `align`, not through pixels. Which column a run's
// value lands in decides whether it is drawn as a normal point, a flagged one, or a
// hollow one — and whether the line is broken there at all.
import assert from "node:assert/strict";
import test from "node:test";

Object.defineProperty(globalThis, "document", {
  configurable: true,
  value: { querySelector: () => null },
});
Object.defineProperty(globalThis, "uPlot", { configurable: true, value: undefined });

const { align, render } = await import("../../web/js/series.js");

function point(patch = {}) {
  return {
    recording_id: "r1",
    at: "2026-09-01T00:00:00Z",
    value: 10,
    n: 60,
    invalid: false,
    status: "finished",
    baseline_value: null,
    baseline_n: 0,
    center: null,
    band: null,
    band_runs: 0,
    outside: false,
    worse: false,
    ...patch,
  };
}

function trend(points, patch = {}) {
  return {
    metric: "cpu.busy",
    window: 10,
    min_runs_for_band: 5,
    banded: points.some((p) => p.band != null),
    points,
    ...patch,
  };
}

function series(points, patch = {}) {
  return {
    series: {
      key: "observation|staging|load_balancer|1s|api=0.0.0",
      kind: "observation",
      profile: "staging",
      addressing_mode: "load_balancer",
      runs: points.length,
      first_at: "2026-09-01T00:00:00Z",
      last_at: "2026-09-09T00:00:00Z",
      baseline_id: null,
      invalid_runs: points.filter((p) => p.invalid).length,
    },
    has_baseline_phase: false,
    metrics: ["cpu.busy"],
    trends: { "cpu.busy": trend(points) },
    runs: points.map((p) => ({
      id: p.recording_id,
      started_at: p.at,
      status: "finished",
      duration_ms: 30000,
      is_baseline: false,
      annotations_by_severity: p.invalid ? { invalid: 1 } : {},
      worst: p.invalid ? "invalid" : null,
      targets: ["box-a"],
      note: null,
    })),
    ...patch,
  };
}

const listState = (rows, floor = 5) => ({ series: rows, selectedSeries: null, seriesFloor: floor });

function row(patch = {}) {
  return {
    key: "observation|staging|load_balancer|1s|api=0.0.0",
    kind: "observation",
    profile: "staging",
    addressing_mode: "load_balancer",
    runs: 3,
    first_at: "2026-09-01T00:00:00Z",
    last_at: "2026-09-09T00:00:00Z",
    baseline_id: null,
    invalid_runs: 0,
    ...patch,
  };
}

/* ------------------------------------------------------------------- the list */

test("an empty archive says series appear on their own, not that one must be made", () => {
  const markup = render(listState([]));
  assert.match(markup, /No series yet/);
  assert.match(markup, /Runs group\s+themselves by setup/);
});

test("the list says which series are long enough to have a band, before opening one", () => {
  // Finding that out one click at a time is the thing this column exists to stop.
  const markup = render(listState([row({ runs: 3 }), row({ key: "b", runs: 9 })]));
  assert.match(markup, /2 more run\(s\)/);
  assert.match(markup, /banded/);
});

test("runs held out of the band do not count towards having one", () => {
  // Six runs looks long enough until two of them are invalid.
  const markup = render(listState([row({ runs: 6, invalid_runs: 2 })]));
  assert.match(markup, /1 more run\(s\)/);
});

test("the setup identity is shown, because it is what says why a history split", () => {
  const markup = render(listState([row()]));
  assert.match(markup, /observation\|staging\|load_balancer\|1s\|api=0\.0\.0/);
});

/* ----------------------------------------------------------------- one series */

test("a short series says no band is drawn, rather than drawing nothing and leaving it", () => {
  const markup = render({ selectedSeries: series([point(), point()]) });
  assert.match(markup, /No band is drawn/);
  assert.match(markup, /fewer than 5 usable runs/);
});

test("a banded series says the band is measured, not chosen", () => {
  const markup = render({
    selectedSeries: series([point({ center: 10, band: 2, band_runs: 6 })]),
  });
  assert.match(markup, /measured from the 10 runs\s+before each point/);
  assert.match(markup, /not a percentage anybody chose/);
});

test("invalid runs are declared, so a hollow point is not a mystery", () => {
  const markup = render({
    selectedSeries: series([point(), point({ recording_id: "r2", invalid: true })]),
  });
  assert.match(markup, /1 run\(s\) carrying an <strong>invalid<\/strong> note/);
  assert.match(markup, /kept out of the band/);
});

test("the drift line is explained only when there is one", () => {
  const without = render({ selectedSeries: series([point()]) });
  assert.doesNotMatch(without, /grey line/);

  const withDrift = render({
    selectedSeries: series([point({ baseline_value: 4 })], { has_baseline_phase: true }),
  });
  assert.match(withDrift, /before the run asked it for anything/);
});

test("every figure carries the count behind it", () => {
  const markup = render({ selectedSeries: series([point({ value: 10, n: 60 })]) });
  assert.match(markup, /n=60/);
});

test("a run the count could not support shows no value rather than a zero", () => {
  const markup = render({ selectedSeries: series([point({ value: null, n: 2 })]) });
  assert.match(markup, /n=2/);
  assert.doesNotMatch(markup, />0\.00</);
});

test("a point outside the band is not called a regression", () => {
  // §17.4 wants the move, the sample count and validity together before that word
  // is earned, and that verdict is not built yet.
  const markup = render({
    selectedSeries: series([point({ center: 10, band: 2, value: 40, outside: true, worse: true })]),
  });
  assert.match(markup, /outside, the bad way/);
  assert.doesNotMatch(markup, /regression/i);
});

test("a point with no band behind it says so rather than reading as within one", () => {
  const markup = render({ selectedSeries: series([point()]) });
  assert.match(markup, /no band yet/);
});

test("an invalid run's verdict says it was not measured, not that it was fine", () => {
  const markup = render({
    selectedSeries: series([point({ invalid: true, center: 10, band: 2 })]),
  });
  assert.match(markup, /not measured/);
});

test("a series that recorded nothing says so rather than drawing empty cards", () => {
  const markup = render({
    selectedSeries: series([], { metrics: [], trends: {} }),
  });
  assert.match(markup, /Nothing was collected in this series/);
});

test("each run links back to the recording it came from", () => {
  const markup = render({ selectedSeries: series([point({ recording_id: "2026-09-01_ab" })]) });
  assert.match(markup, /#\/recordings\/2026-09-01_ab/);
});

/* --------------------------------------------------------------------- drawing */

test("a run outside the band is drawn twice: once on the line, once as a flag", () => {
  const [xs, values, outside, invalid] = align(
    trend([
      point({ at: "2026-09-01T00:00:00Z", value: 10 }),
      point({ at: "2026-09-02T00:00:00Z", value: 40, outside: true }),
    ])
  );
  assert.equal(xs.length, 2);
  assert.deepEqual(values, [10, 40], "the line still passes through it");
  assert.deepEqual(outside, [null, 40], "and a second mark sits on top");
  assert.deepEqual(invalid, [null, null]);
});

test("an invalid run breaks the line and is drawn hollow instead", () => {
  // A line through a run whose numbers cannot be trusted is a claim the data does
  // not support; a missing point would hide that the run happened at all.
  const [, values, outside, invalid] = align(
    trend([
      point({ at: "2026-09-01T00:00:00Z", value: 10 }),
      point({ at: "2026-09-02T00:00:00Z", value: 90, invalid: true, outside: true }),
      point({ at: "2026-09-03T00:00:00Z", value: 11 }),
    ])
  );
  assert.deepEqual(values, [10, null, 11]);
  assert.deepEqual(invalid, [null, 90, null]);
  assert.deepEqual(outside, [null, null, null], "not flagged: it was never measured");
});

test("a run with no supported median is a break in the line, not a zero", () => {
  const [, values] = align(
    trend([point({ value: 10 }), point({ at: "2026-09-02T00:00:00Z", value: null })])
  );
  assert.deepEqual(values, [10, null]);
});

test("the x axis is real time, so an unrecorded fortnight is visible as one", () => {
  const [xs] = align(
    trend([
      point({ at: "2026-09-01T00:00:00Z" }),
      point({ at: "2026-09-02T00:00:00Z" }),
      point({ at: "2026-09-16T00:00:00Z" }),
    ])
  );
  assert.equal(xs[1] - xs[0], 86400);
  assert.equal(xs[2] - xs[1], 86400 * 14, "the gap is drawn at its real width");
});

test("the drift column carries the baseline phase, and null where there was none", () => {
  const [, , , , drift] = align(
    trend([point({ baseline_value: 4 }), point({ at: "2026-09-02T00:00:00Z" })])
  );
  assert.deepEqual(drift, [4, null]);
});

test("the drift line breaks at an invalid run too", () => {
  // The note is about the run: its idle reading is no more trustworthy than the rest
  // of it, so a line drawn straight through would claim something the data does not.
  const [, , , , drift] = align(
    trend([
      point({ baseline_value: 4 }),
      point({ at: "2026-09-02T00:00:00Z", baseline_value: 30, invalid: true }),
      point({ at: "2026-09-03T00:00:00Z", baseline_value: 4.2 }),
    ])
  );
  assert.deepEqual(drift, [4, null, 4.2]);
});

test("the runs table shows the metric that moved, not the one that sorts first", () => {
  // The reason to open a run is almost always the metric that left its band.
  const moved = point({ center: 10, band: 2, value: 40, outside: true, worse: true });
  const markup = render({
    selectedSeries: series([point()], {
      metrics: ["conn.established", "cpu.busy"],
      trends: {
        "conn.established": trend([point()]),
        "cpu.busy": trend([moved]),
      },
    }),
  });
  assert.match(markup, /<th class="num" style="width:17%">cpu\.busy<\/th>/);
});
