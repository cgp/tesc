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
  const p = {
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
  // Derived server-side and sent on the wire (stats/trend.py). Computed here the
  // same way rather than pinned per test, so a fixture cannot describe a point the
  // API would never send -- flagged without a band, say.
  const supported = p.value != null;
  return {
    flagged: p.outside && supported && !p.invalid,
    regressed: p.outside && supported && !p.invalid && p.worse,
    judged: p.band != null && supported && !p.invalid,
    unjudged_because: p.invalid
      ? "invalid"
      : !supported
        ? "unsupported"
        : p.band == null
          ? "no_band"
          : null,
    ...p,
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

test("a flagged run is called a regression only when all three conditions hold", () => {
  // §17.4: the move, a sample count that supports it, and no invalid note.
  const moved = { center: 10, band: 2, value: 40, outside: true, worse: true };
  assert.match(
    render({ selectedSeries: series([point(moved)]) }),
    /regressed/,
    "all three hold"
  );

  for (const [missing, label] of [
    [{ invalid: true }, /held out of the band/],
    [{ value: null, n: 2 }, /too few samples/],
    [{ band: null, center: null }, /no band yet/],
  ]) {
    const markup = render({ selectedSeries: series([point({ ...moved, ...missing })]) });
    assert.match(markup, label);
    assert.doesNotMatch(markup, /regressed/, `one condition short: ${JSON.stringify(missing)}`);
  }
});

test("a flag in the good direction is not a regression", () => {
  const markup = render({
    selectedSeries: series([
      point({ center: 10, band: 2, value: 1, outside: true, worse: false }),
    ]),
  });
  assert.match(markup, /outside, the good way/);
  assert.doesNotMatch(markup, /regressed/);
});

test("a point with no band behind it says so rather than reading as within one", () => {
  const markup = render({ selectedSeries: series([point()]) });
  assert.match(markup, /no band yet/);
  assert.doesNotMatch(markup, /within band/, "unchecked is not a pass");
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

/* ------------------------------------------------------------------ the verdict */

function verdict(patch = {}) {
  return {
    series_key: "k",
    recording_id: "2026-09-09_zz",
    at: "2026-09-09T00:00:00Z",
    status: "ok",
    findings: [],
    judged: ["cpu.busy"],
    unjudged: {},
    ...patch,
  };
}

const withVerdict = (v) => render({ selectedSeries: series([point()], { verdict: v }) });

test("a regression names the metric, what normal was, and the size of the move", () => {
  const markup = withVerdict(
    verdict({
      status: "regressed",
      judged: [],
      findings: [
        {
          metric: "cpu.busy",
          value: 40,
          center: 10,
          band: 2,
          change: 30,
          change_pct: 300,
          n: 60,
          worse: true,
        },
      ],
    })
  );
  assert.match(markup, /regressed/);
  assert.match(markup, /cpu\.busy/);
  assert.match(markup, /\+300\.0%/);
  assert.match(markup, /n=60/, "the count behind the claim travels with it");
  assert.match(markup, /#\/recordings\/2026-09-09_zz/);
});

test("a move the good way is reported as a change, not a failure", () => {
  const markup = withVerdict(verdict({ status: "changed" }));
  assert.match(markup, /changed/);
  assert.doesNotMatch(markup, /regressed/);
});

test("a clean run says how many metrics were actually checked", () => {
  // "Nothing moved" is only reassuring if something was looked at.
  const markup = withVerdict(verdict({ judged: ["cpu.busy", "fd.open"] }));
  assert.match(markup, /within band/);
  assert.match(markup, /2 metric\(s\) checked/);
});

test("a metric that could not be checked is named, with the reason", () => {
  const markup = withVerdict(
    verdict({ status: "unknown", judged: [], unjudged: { "cpu.busy": "no_band" } })
  );
  assert.match(markup, /not judged/);
  assert.match(markup, /Not checked:/);
  assert.match(markup, /not enough history yet/);
});

test("an unchecked run is never dressed as a passing one", () => {
  // The failure this whole flag exists to avoid: silence reported as success.
  const markup = withVerdict(
    verdict({ status: "unknown", judged: [], unjudged: { "cpu.busy": "invalid" } })
  );
  assert.match(markup, /this run carries an invalid note/);
  assert.doesNotMatch(markup, /within band/);
});

test("a series with no runs to judge shows no verdict card at all", () => {
  const markup = withVerdict(verdict({ recording_id: null, status: "unknown" }));
  assert.doesNotMatch(markup, /Latest run/);
});

test("the list says how each series' latest run stands", () => {
  // So the archive can be scanned for the one that moved.
  const markup = render(
    listState([row({ latest_status: "regressed" }), row({ key: "b", latest_status: "ok" })])
  );
  assert.match(markup, /regressed/);
  assert.match(markup, /within band/);
});

test("a series that could not be judged says so rather than showing a quiet pass", () => {
  const markup = render(listState([row({ latest_status: "unknown" })]));
  assert.match(markup, /not judged/);
  assert.doesNotMatch(markup, /within band/);
});

/* ------------------------------------------------------------ picking runs */

const picked = (runs, chosen) =>
  render({ selectedSeries: series(runs), selectedRuns: chosen });

test("with nothing ticked the page says what ticking is for", () => {
  const markup = picked([point()], []);
  assert.match(markup, /Tick two or\s+more to compare/);
  assert.doesNotMatch(markup, /data-action="compare-runs"/);
});

test("one run is not a comparison, so the button is offered but refuses", () => {
  const markup = picked([point()], ["r1"]);
  assert.match(markup, /data-action="compare-runs"[^>]*disabled/);
  assert.match(markup, /Compare 1/);
});

test("two or more and the button goes", () => {
  const markup = picked([point(), point({ recording_id: "r2" })], ["r1", "r2"]);
  assert.match(markup, /Compare 2/);
  assert.doesNotMatch(markup, /data-action="compare-runs"[^>]*disabled/);
});

test("too many says so before the request rather than after it is refused", () => {
  // Past six an overlay has more lines than there are colours anyone can tell apart.
  const markup = picked([point()], ["a", "b", "c", "d", "e", "f", "g"]);
  assert.match(markup, /7 is too many \(max 6\)/);
  assert.match(markup, /data-action="compare-runs"[^>]*disabled/);
});

test("a ticked run is marked in its own row, not only in the count", () => {
  const markup = picked([point({ recording_id: "r1" })], ["r1"]);
  assert.match(markup, /<tr class="metrix-picked">/);
  assert.match(markup, /type="checkbox" checked/);
});
