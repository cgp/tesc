// Run series (design §17.2, §17.3). Every run of the same setup, in time order.
//
// This is the page that answers the question a single recording cannot: *is this
// getting better or worse?* A two-run diff cannot answer it either, because it has
// no idea what normal variation looks like — the series measures that from its own
// scatter and draws it as the band behind the points.
//
// Three rules the drawing here cannot compromise on:
//
//   * the band comes from the runs **before** each point, so a regression cannot
//     widen the band it is being judged against
//   * a run whose sample count could not support a median is a break in the line,
//     never a zero
//   * an `invalid` run is drawn hollow and excluded from the band — that it failed
//     validity is part of the history, but its numbers are not

import { duration, escape, metricChange, metricValue, timestamp } from "./format.js";
import { empty, field, icon } from "./ui.js";

//: uPlot is vendored and loaded by index.html before this module runs.
const uPlot = globalThis.uPlot;

//: One cursor across every trend on the page, as on Charts.
const SYNC = "metrix-trend";

const LINE = "#206bc4";
const DRIFT = "#98a2b3";
const BAND_FILL = "rgba(32, 107, 196, 0.10)";
const OUTSIDE = "#d63939";

//: What each trend answers. §15: no chart ships without a caption, and a trend
//: needs one more than a time series does — the same metric means something
//: different read across runs than read within one.
const CAPTIONS = {
  "cpu.busy": "Whether this environment is working harder for the same job than it used to.",
  "mem.used_bytes": "Memory per run. A staircase across runs is a leak the runs are too short to show.",
  "conn.established": "Connection counts run to run — whether keepalive behaviour has changed.",
  "proc.count": "Processes per run. A count that climbs release over release is not reclaiming.",
  "fd.open": "Open descriptors per run, which is where a slow leak shows up first.",
};

let charts = new Map();
let drawnFor = null;

export function selectState(state) {
  return [state.series, state.selectedSeries];
}

export function render(state) {
  if (state.selectedSeries) return detail(state.selectedSeries);
  return list(state);
}

/**
 * The long answer to "what is this page for", behind the help button.
 *
 * Written here rather than on the page because it is read once, by someone who has
 * just arrived, and is in the way every time after that. The short answer is the
 * subtitle; this is the part that explains why the band exists and what it will not
 * claim. It is our own text, never anything from the API, so there is nothing to
 * escape.
 */
export function help() {
  return {
    title: "Series and trends",
    body: `
      <p>A single recording says what happened during it. This page answers the
        question that needs more than one: <strong>is this getting better or worse
        than it was?</strong> Two runs cannot answer it either — a difference between
        two numbers means nothing until you know how much that number moves on its
        own.</p>

      <h4>How runs are grouped</h4>
      <p>Runs group themselves. Everything recorded against the same
        <strong>setup identity</strong> — kind, profile, addressing mode, collection
        interval and API version — is one series, in time order. There is nothing to
        tag, because manual tagging gets skipped exactly when it matters.</p>
      <p>Change any part of that identity and the runs belong to a
        <em>different</em> series. Nothing bridges the two: the old history is still
        there under its own identity, and the only way to compare across a setup
        change is to open both deliberately. Measuring through a changed profile or a
        changed collector and blaming the target is the most expensive mistake this
        view could make.</p>

      <h4>The band</h4>
      <p>The shaded band is the series' own noise, measured rather than chosen: the
        middle of the last ten runs, and twice how much they scattered around it.
        A point inside the band has not moved — that is what the band is for, and it
        is why "regression" can mean anything here at all.</p>
      <p>It is measured from the runs <strong>before</strong> each point, never from a
        window containing it. A window that included the point it was judging would
        widen to swallow exactly the movement it exists to detect. That is also why
        it is drawn as a step: it holds from each run until the next one re-measures
        it, and a smooth ribbon would show a band that was never in force.</p>
      <p>Below five usable runs <strong>no band is drawn at all</strong>. A band
        measured from three runs is narrow enough to flag the fourth for being a
        Tuesday, and the list of series says how many more runs each one needs.</p>

      <h4>What is left out, and why</h4>
      <ul>
        <li>A run carrying an <strong>invalid</strong> note is drawn as a hollow
          point and kept out of the band. That it failed validity is part of the
          history; its numbers are not, and the line breaks around it rather than
          being drawn through readings already known not to be trusted.</li>
        <li>A run whose sample count could not support a median breaks the line too.
          A withheld number is never drawn as a zero.</li>
        <li>The grey line is the <strong>baseline phase alone</strong> — what the
          environment was doing before the run asked it for anything. A metric that
          crept up alongside its own idle is the environment drifting, not the
          application regressing.</li>
      </ul>

      <h4>When a move is called a regression</h4>
      <p>Three things together, never one of them alone: the metric moved beyond the
        band, <strong>and</strong> its sample count supports the claim,
        <strong>and</strong> the run carries no <strong>invalid</strong> note. Any
        one on its own produces false positives at a rate that teaches people to
        ignore the flag, which costs more than never having flagged anything.</p>
      <p>A move in the <em>good</em> direction meets the same three conditions and is
        still not a regression — it is reported as a change. It is worth reading,
        because an unexplained improvement usually means the test stopped doing part
        of the work, but it is not something to fail a build on.</p>
      <p>When a metric cannot be checked the page says so and names the reason,
        rather than letting it pass quietly. <strong>Nothing moved</strong> and
        <strong>nothing could be checked</strong> are different answers, and the
        second one dressed as the first is the failure this flag exists to avoid.</p>

      <h4>For a pipeline</h4>
      <p><code>GET /api/series/verdict?key=…</code> returns the same judgement this
        page shows, for the latest run or for one named with
        <code>&amp;recording=…</code>. Its <code>status</code> is
        <code>regressed</code>, <code>changed</code>, <code>ok</code> or
        <code>unknown</code>; <strong>fail on <code>regressed</code></strong>, and
        treat <code>unknown</code> as unanswered rather than as a pass. A historical
        check like this is usually more useful than a fixed threshold, because fixed
        thresholds are guesses made before the data existed.</p>

      <h4>Reading the charts</h4>
      <p>One point per run, on a <strong>real time axis</strong> rather than a run
        count, so a fortnight when nothing was recorded shows up as the gap it was —
        often the explanation for the step everyone is staring at. All the charts
        share one crosshair. The table underneath carries the sample count behind
        every figure and links to the run it came from.</p>
    `,
  };
}

/* ------------------------------------------------------------------- the list */

function list(state) {
  const rows = state.series ?? [];
  if (!rows.length) {
    return empty({
      icon: "chart-line",
      title: "No series yet",
      body: `A series appears as soon as something has been recorded. Runs group
        themselves by setup — profile, addressing, interval and API version — so
        there is nothing to tag.`,
      action: `<a href="#/profiles" class="btn btn-primary">
                 ${icon("player-play")} Start observing
               </a>`,
    });
  }

  const floor = state.seriesFloor ?? 0;
  const body = rows
    .map((row) => {
      const usable = row.runs - row.invalid_runs;
      return `<tr>
        <td class="name">
          <a href="#/series/${encodeURIComponent(row.key)}">${escape(
            row.profile ?? row.key
          )}</a>
          <div class="text-secondary"><code>${escape(row.key)}</code></div>
        </td>
        <td class="text-secondary">${escape(row.kind)}</td>
        <td class="text-secondary">${escape(row.addressing_mode)}</td>
        <td class="num">${row.runs}</td>
        <td>${bandState(usable, floor)}</td>
        <td>${statusBadge(row.latest_status)}</td>
        <td class="text-secondary">${timestamp(row.last_at)}</td>
      </tr>`;
    })
    .join("");

  return `<div class="metrix-stack">
    <p class="metrix-note text-secondary">
      ${icon("chart-line")} Runs group themselves by setup identity — plan, profile,
      addressing mode, interval and API version (§17.2). Change any of them and the
      runs belong to a different series; the old history is still there, under its
      own identity. There is no manual tagging, which would be skipped exactly when
      it mattered.
    </p>
    <div class="card">
      <div class="card-header">
        <h3 class="card-title">Series
          <span class="card-subtitle">${rows.length} setup(s) recorded against</span>
        </h3>
      </div>
      <div class="table-responsive">
        <table class="table card-table table-vcenter metrix-table">
          <thead><tr>
            <th style="width:30%">Setup</th>
            <th style="width:11%">Kind</th>
            <th style="width:13%">Addressing</th>
            <th class="num" style="width:6%">Runs</th>
            <th style="width:11%">Trend</th>
            <th style="width:13%">Latest run</th>
            <th style="width:16%">Last run</th>
          </tr></thead>
          <tbody>${body}</tbody>
        </table>
      </div>
    </div>
  </div>`;
}

/**
 * How the most recent run of a series stands against that series' own history.
 *
 * In the list because this is what the list is scanned for. The alternative is
 * opening every series to find the one that moved, which is the same problem the
 * archive's severity column solved for recordings.
 *
 * `unknown` is drawn as its own thing rather than as a quiet pass: "nothing moved"
 * and "nothing could be checked" are different answers, and the second one dressed
 * as the first is the failure this whole flag exists to avoid.
 */
const STATUS = {
  regressed: {
    tone: "red",
    label: "regressed",
    title: "A metric moved beyond this series' own band, the bad way, on a run whose sample count supports it",
  },
  changed: {
    tone: "yellow",
    label: "changed",
    title: "A metric moved beyond the band, but in the good direction — worth reading, not a failure",
  },
  ok: {
    tone: "green",
    label: "within band",
    title: "Everything that could be checked sat inside the band its history supports",
  },
  unknown: {
    tone: "secondary",
    label: "not judged",
    title: "Nothing could be checked: too short a history, too few samples, or a run marked invalid",
  },
};

function statusBadge(status) {
  const state = STATUS[status] ?? STATUS.unknown;
  return `<span class="badge bg-${state.tone}-lt" title="${escape(state.title)}"
    >${escape(state.label)}</span>`;
}

// Said in the list rather than after opening it: a series too short to have a band
// is the common case early on, and finding that out one click at a time is worse.
function bandState(usable, floor) {
  if (usable >= floor) {
    return `<span class="badge bg-green-lt" title="Long enough to measure its own noise">
      banded</span>`;
  }
  const short = floor - usable;
  return `<span class="text-secondary" title="A band measured from this few runs would
    flag the next one for being a Tuesday">${short} more run(s)</span>`;
}

/* ----------------------------------------------------------------- one series */

function detail(series) {
  const runs = series.runs ?? [];
  const identity = series.series ?? {};

  const cards = (series.metrics ?? [])
    .map(
      (metric) => `<div class="card">
        <div class="card-header">
          <div>
            <h3 class="card-title">${escape(metric)}</h3>
            <div class="card-subtitle">${escape(caption(metric))}</div>
          </div>
        </div>
        <div class="card-body metrix-chart" data-trend="${escape(metric)}"></div>
      </div>`
    )
    .join("");

  return `<div class="mb-3">
    <a href="#/series" class="btn btn-sm">${icon("arrow-left")} All series</a>
  </div>
  <div class="metrix-stack">
    <div class="card">
      <div class="card-header">
        <h3 class="card-title">${escape(identity.profile ?? identity.key ?? "Series")}</h3>
      </div>
      <div class="card-body">
        <div class="datagrid">
          ${field("Kind", escape(identity.kind ?? "—"))}
          ${field("Addressing", escape(identity.addressing_mode ?? "—"))}
          ${field("Runs", String(identity.runs ?? runs.length))}
          ${field("First", timestamp(identity.first_at))}
          ${field("Latest", timestamp(identity.last_at))}
          ${field("Identity", `<code>${escape(identity.key ?? "")}</code>`)}
        </div>
      </div>
    </div>
    ${verdictCard(series)}
    ${legend(series)}
    ${cards || noMetrics()}
    ${runsCard(series)}
  </div>`;
}

function noMetrics() {
  return empty({
    icon: "alert-triangle",
    title: "Nothing was collected in this series",
    body: `The runs exist but hold no samples. The notes on each recording say why.`,
  });
}

function caption(metric) {
  return CAPTIONS[metric] ?? `${metric}, one point per run, across this series.`;
}

/**
 * What the drawing means, stated once above the charts.
 *
 * Including the case where there is no band at all. A short series draws points with
 * nothing behind them, and a reader who does not know that is reading the absence of
 * a band as the absence of a problem.
 */
function legend(series) {
  const banded = Object.values(series.trends ?? {}).some((t) => t.banded);
  const floor = firstTrend(series)?.min_runs_for_band ?? 0;
  const window = firstTrend(series)?.window ?? 0;
  const invalid = (series.runs ?? []).filter((r) => r.worst === "invalid").length;

  const notes = [
    banded
      ? `The shaded band is this series' own noise, measured from the ${window} runs
         before each point — not a percentage anybody chose. A point inside it has
         not moved.`
      : `No band is drawn: this series has fewer than ${floor} usable runs, and a band
         measured from that few would flag the next run for being a Tuesday.`,
    invalid
      ? `${invalid} run(s) carrying an <strong>invalid</strong> note are drawn hollow
         and kept out of the band — their numbers are the ones already known not to
         be trusted.`
      : "",
    series.has_baseline_phase
      ? `The grey line is the baseline phase alone: what the environment was doing
         before the run asked it for anything. A metric that crept up alongside its
         own idle is not an application regression.`
      : "",
  ]
    .filter(Boolean)
    .join(" ");

  return `<p class="metrix-note text-secondary">
    ${icon("chart-line")} One point per run, in time order. ${notes}
  </p>`;
}

function firstTrend(series) {
  return Object.values(series.trends ?? {})[0];
}

/**
 * The runs behind the points.
 *
 * A trend says which way a metric went; this is what says which run to open next,
 * and it is the only place the sample count behind each point is written down in
 * full. Newest first, because the run anyone is looking for is almost always the
 * most recent one.
 */
function runsCard(series) {
  const runs = [...(series.runs ?? [])].reverse();
  if (!runs.length) return "";

  const metric = headline(series);
  const points = new Map(
    (series.trends?.[metric]?.points ?? []).map((p) => [p.recording_id, p])
  );

  const rows = runs
    .map((run) => {
      const point = points.get(run.id);
      return `<tr>
        <td class="name">
          <a href="#/recordings/${encodeURIComponent(run.id)}">${escape(run.id)}</a>
          ${
            run.is_baseline
              ? `<span class="badge bg-blue-lt ms-2">baseline</span>`
              : ""
          }
        </td>
        <td class="text-secondary">${timestamp(run.started_at)}</td>
        <td class="num">${duration(run.duration_ms)}</td>
        <td class="num">${figure(metric, point)}</td>
        <td>${verdict(point)}</td>
        <td>${noteBadge(run)}</td>
      </tr>`;
    })
    .join("");

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">Runs</h3>
        <div class="card-subtitle">Newest first. The figure is
          <code>${escape(metric ?? "—")}</code>, with the sample count behind it —
          a median over eleven readings and one over six hundred are different
          claims.</div>
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th style="width:28%">Recording</th>
          <th style="width:18%">Started</th>
          <th class="num" style="width:9%">Length</th>
          <th class="num" style="width:17%">${escape(metric ?? "Value")}</th>
          <th style="width:16%">Against the band</th>
          <th style="width:12%">Notes</th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}

/**
 * Which metric the runs table shows a figure for.
 *
 * Whatever moved most recently, in preference to whichever metric happens to sort
 * first. This table is here to say which run to open next, and the reason to open
 * one is almost always the metric that left its band — a column of `conn.established`
 * is a column nobody came here for.
 */
function headline(series) {
  const metrics = series.metrics ?? [];
  const latest = (metric) => series.trends?.[metric]?.points?.at(-1);
  return (
    metrics.find((m) => latest(m)?.worse) ??
    metrics.find((m) => latest(m)?.outside) ??
    metrics[0]
  );
}

// The value, and the count it rests on. Never one without the other.
function figure(metric, point) {
  if (!point) return `<span class="text-secondary">—</span>`;
  if (point.value == null) {
    return `<span class="text-secondary" title="${point.n} samples is too few for a median">
      — <small>n=${point.n}</small></span>`;
  }
  return `${metricValue(metric, point.value)}
    <small class="text-secondary">n=${point.n}</small>`;
}

/**
 * Where this run sits relative to what came before it.
 *
 * Deliberately not the word *regression*: design §17.4 requires the move, the sample
 * count and validity together before that word is earned, and that verdict is not
 * built yet. This says only what the geometry says.
 */
function verdict(point) {
  if (!point) return `<span class="text-secondary">—</span>`;
  // Why it could not be checked, never a silent pass. "Nothing moved" and "nothing
  // could be checked" are different answers and only one of them is reassuring.
  const excuse = {
    invalid: ["held out of the band", "This run carries an invalid note"],
    unsupported: ["too few samples", "The sample count cannot support a median"],
    no_band: ["no band yet", "Not enough runs behind it to say"],
  }[point.unjudged_because];
  if (excuse) {
    return `<span class="text-secondary" title="${escape(excuse[1])}">${excuse[0]}</span>`;
  }
  if (!point.flagged) return `<span class="text-success">within band</span>`;
  // The same word the badge and the CI status use. Three vocabularies for one
  // judgement is how a page and a pipeline come to appear to disagree.
  return point.regressed
    ? `<span class="text-danger">regressed</span>`
    : `<span class="text-success">outside, the good way</span>`;
}
/**
 * The latest run, judged against the series (design §17.4).
 *
 * Three conditions together before the word *regression* is used: the metric moved
 * beyond the band its own history supports, its sample count supports the claim, and
 * the run carries no `invalid` note. Any one alone produces false positives at a rate
 * that teaches people to ignore the flag.
 *
 * The same judgement the CI endpoint serves, from the same field, so the page and a
 * pipeline cannot disagree about whether the last run regressed.
 */
function verdictCard(series) {
  const verdict = series.verdict;
  if (!verdict?.recording_id) return "";

  const state = STATUS[verdict.status] ?? STATUS.unknown;
  const rows = verdict.findings
    .map(
      (f) => `<tr>
        <td class="name">${escape(f.metric)}</td>
        <td class="num">${metricValue(f.metric, f.center)}
          <span class="text-secondary">±${metricValue(f.metric, f.band)}</span></td>
        <td class="num">${metricValue(f.metric, f.value)}
          <small class="text-secondary">n=${f.n}</small></td>
        <td class="num ${f.worse ? "text-danger" : "text-success"}">
          ${metricChange(f.metric, f.change)}
          <span class="text-secondary">(${f.change > 0 ? "+" : ""}${f.change_pct.toFixed(
            1
          )}% of normal)</span></td>
      </tr>`
    )
    .join("");

  // Why a metric could not be checked, rather than letting it pass silently.
  const reasons = {
    no_band: "not enough history yet",
    unsupported: "too few samples for a median",
    invalid: "this run carries an invalid note",
  };
  const unjudged = Object.entries(verdict.unjudged ?? {});
  const skipped = unjudged.length
    ? `<div class="card-body py-2 border-top text-secondary">
         Not checked:
         ${unjudged
           .map(
             ([metric, why]) =>
               `<code>${escape(metric)}</code> (${escape(reasons[why] ?? why)})`
           )
           .join(", ")}.
       </div>`
    : "";

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">Latest run
          <span class="ms-2">${statusBadge(verdict.status)}</span>
        </h3>
        <div class="card-subtitle">
          <a href="#/recordings/${encodeURIComponent(verdict.recording_id)}"
             >${escape(verdict.recording_id)}</a>, ${timestamp(verdict.at)}.
          ${escape(state.title)}.
          ${
            verdict.judged?.length
              ? `${verdict.judged.length} metric(s) checked and within band.`
              : ""
          }
        </div>
      </div>
    </div>
    ${
      rows
        ? `<div class="table-responsive">
             <table class="table card-table table-vcenter metrix-table">
               <thead><tr>
                 <th style="width:22%">Metric</th>
                 <th class="num" style="width:22%">Normally</th>
                 <th class="num" style="width:22%">This run</th>
                 <th class="num" style="width:34%">Move</th>
               </tr></thead>
               <tbody>${rows}</tbody>
             </table>
           </div>`
        : ""
    }
    ${skipped}
  </div>`;
}

function noteBadge(run) {
  if (!run.worst) return `<span class="text-secondary">clean</span>`;
  const tone = { invalid: "red", warn: "orange", info: "blue" }[run.worst];
  const total = Object.values(run.annotations_by_severity ?? {}).reduce((a, b) => a + b, 0);
  return `<span class="badge bg-${tone}-lt">${escape(run.worst)}${
    total > 1 ? ` ×${total}` : ""
  }</span>`;
}

/* --------------------------------------------------------------------- drawing */

/**
 * The x axis and the y columns for one metric's trend.
 *
 * Four columns rather than one, because per-point styling in uPlot means per-series:
 * a run that is inside the band, one that is outside it, and one that is invalid are
 * three different marks, and splitting them into three series draws all three with
 * nothing but stable options.
 *
 * The value column carries `null` at an invalid run, which breaks the line there —
 * correct, since a line drawn through a run whose numbers cannot be trusted is a
 * claim the data does not support. It also carries `null` where the sample count
 * could not support a median, for the same reason a gap is a gap on Charts.
 */
export function align(trend) {
  const xs = [];
  const values = [];
  const outside = [];
  const invalid = [];
  const drift = [];

  for (const point of trend.points ?? []) {
    xs.push(Date.parse(point.at) / 1000);
    const usable = point.invalid ? null : point.value;
    values.push(usable);
    outside.push(point.outside && !point.invalid ? point.value : null);
    invalid.push(point.invalid ? point.value : null);
    // Broken at an invalid run for the same reason the value line is: the note
    // is about the run, and its idle reading is no more trustworthy than the rest.
    drift.push(point.invalid ? null : point.baseline_value);
  }
  return [xs, values, outside, invalid, drift];
}

export function draw(state) {
  const series = state.selectedSeries;
  if (!uPlot || !series) return;

  for (const plot of charts.values()) plot.destroy();
  charts = new Map();
  for (const metric of series.metrics ?? []) {
    const holder = document.querySelector(`[data-trend="${cssEscape(metric)}"]`);
    if (!holder) continue;
    holder.innerHTML = "";
    charts.set(metric, build(holder, series, metric));
  }
  drawnFor = series.series?.key ?? null;
}

function build(holder, series, metric) {
  const trend = series.trends[metric];
  const data = align(trend);
  return new uPlot(
    {
      width: holder.clientWidth || 800,
      height: 220,
      cursor: { sync: { key: SYNC }, drag: { x: true, y: false } },
      legend: { live: true },
      // Real time, not run index: runs are not evenly spaced, and a chart that
      // pretended they were would hide a fortnight nobody recorded anything in.
      scales: { x: { time: true } },
      axes: [axisTheme(), { values: (_, t) => t.map((v) => metricValue(metric, v)), ...axisTheme() }],
      series: [
        { label: "run" },
        {
          label: metric,
          stroke: LINE,
          width: 1.5,
          spanGaps: false,
          points: { show: true, size: 6, fill: LINE, stroke: LINE },
          value: (_, v) => (v == null ? "—" : metricValue(metric, v)),
        },
        {
          label: "outside the band",
          stroke: "transparent",
          width: 0,
          points: { show: true, size: 9, fill: OUTSIDE, stroke: OUTSIDE },
          value: (_, v) => (v == null ? "—" : metricValue(metric, v)),
        },
        {
          // Hollow: the run happened and is part of the history, but its numbers
          // are not being claimed.
          label: "invalid",
          stroke: "transparent",
          width: 0,
          points: { show: true, size: 8, fill: "transparent", stroke: OUTSIDE, width: 1.5 },
          value: (_, v) => (v == null ? "—" : metricValue(metric, v)),
        },
        {
          label: "at rest",
          stroke: DRIFT,
          width: 1,
          dash: [4, 3],
          spanGaps: false,
          show: Boolean(series.has_baseline_phase),
          value: (_, v) => (v == null ? "—" : metricValue(metric, v)),
        },
      ],
      plugins: [bandPlugin(trend)],
    },
    data,
    holder
  );
}

/**
 * The noise band, drawn behind the points.
 *
 * A canvas plugin rather than uPlot's own band support, because the band is a
 * *step*: it is measured from the runs before each point, so it changes at each one
 * and holds its value across the gap between them. Interpolating it into a smooth
 * ribbon would draw a band that was never in force at any moment shown.
 */
function bandPlugin(trend) {
  const points = trend.points ?? [];
  return {
    hooks: {
      draw: (u) => {
        const { ctx } = u;
        ctx.save();
        ctx.beginPath();
        ctx.rect(u.bbox.left, u.bbox.top, u.bbox.width, u.bbox.height);
        ctx.clip();
        ctx.fillStyle = BAND_FILL;

        for (let i = 0; i < points.length; i += 1) {
          const point = points[i];
          if (point.band == null) continue;
          // A step holds from this run until the next one, where a fresh band is
          // measured. The last one runs to the right edge.
          const from = Date.parse(point.at) / 1000;
          const next = points[i + 1] ? Date.parse(points[i + 1].at) / 1000 : u.scales.x.max;
          const x0 = u.valToPos(from, "x", true);
          const x1 = u.valToPos(next, "x", true);
          const yTop = u.valToPos(point.center + point.band, "y", true);
          const yBottom = u.valToPos(point.center - point.band, "y", true);
          ctx.fillRect(x0, yTop, Math.max(1, x1 - x0), Math.max(1, yBottom - yTop));
        }
        ctx.restore();
      },
    },
  };
}

function axisTheme() {
  const style = getComputedStyle(document.body);
  const ink = style.getPropertyValue("--tblr-secondary")?.trim() || "#667382";
  const line = style.getPropertyValue("--tblr-border-color")?.trim() || "#e6e7e9";
  return {
    stroke: ink,
    grid: { stroke: line, width: 1 },
    ticks: { stroke: line, width: 1 },
    font: "11px system-ui, sans-serif",
  };
}

function cssEscape(value) {
  return String(value).replace(/["\\]/g, "\\$&");
}

/** Resize every trend to its container. Called on window resize. */
export function resize() {
  for (const [metric, plot] of charts) {
    const holder = document.querySelector(`[data-trend="${cssEscape(metric)}"]`);
    if (holder?.clientWidth) plot.setSize({ width: holder.clientWidth, height: 220 });
  }
}

/** Whether the charts on screen belong to the series in state. */
export function drawnSeries() {
  return drawnFor;
}
