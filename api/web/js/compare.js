// Comparison mode (design §14.4) and multi-run overlay (§17.5).
//
// The stats table with every selected run's values alongside, a delta column against
// the earliest of them, and — where it means anything — a merged column holding all
// of them as one distribution.
//
// The rule the merged column rests on is that **distributions merge and percentiles
// do not**. The mean of five p95s is not the p95 of the five windows together and
// describes nothing; pooling the readings and summarising once is the only
// arithmetic that answers a question about the whole. That merge happens server-side
// in `stats/`, like every other figure — this page never computes a percentile.
//
// Merging is refused across runs of different setups, and the page says which part of
// the identity differs. Side by side is still legitimate there; a pooled distribution
// of two different things is not, however impressive its sample count looks.

import { duration, escape, metricChange, metricValue, timestamp } from "./format.js";
import { empty, icon } from "./ui.js";

//: uPlot is vendored and loaded by index.html before this module runs.
const uPlot = globalThis.uPlot;

const SYNC = "metrix-compare";

//: One colour per run. Six, which is also the server's cap on a comparison: past
//: that an overlay stops being readable and the answer is fewer runs, not more hues.
const RUN_COLORS = ["#206bc4", "#d63939", "#2fb344", "#f76707", "#ae3ec9", "#0ca678"];

// The merged distribution has no line of its own: it is not a series in time, so it
// appears as a column and never on the overlay.
let charts = new Map();
let drawnFor = null;

export function selectState(state) {
  return [state.comparison, state.comparisonSeries];
}

export function render(state) {
  const comparison = state.comparison;
  if (!comparison) {
    return empty({
      icon: "chart-line",
      title: "Nothing selected to compare",
      body: `Pick runs from a <a href="#/series">series</a> and compare them. Two or
        more, up to six — past that an overlay stops being readable.`,
    });
  }

  return `<div class="mb-3">
    <a href="#/series" class="btn btn-sm">${icon("arrow-left")} All series</a>
  </div>
  <div class="metrix-stack">
    ${runsCard(comparison)}
    ${mergeNote(comparison)}
    ${phaseBar(comparison)}
    ${comparison.metrics.length ? tables(comparison) : nothingCollected()}
    ${overlays(state)}
  </div>`;
}

function nothingCollected() {
  return empty({
    icon: "alert-triangle",
    title: "These runs hold no samples in common",
    body: `Nothing was collected, or the phase asked for is not one these runs share.`,
  });
}

/** Which runs are in front, and what each one is. */
function runsCard(comparison) {
  const rows = comparison.runs
    .map(
      (run, i) => `<tr>
        <td class="name">
          <span class="metrix-swatch" style="background:${color(i)}"></span>
          <a href="#/recordings/${encodeURIComponent(run.id)}">${escape(run.id)}</a>
          ${run.id === comparison.reference_id
            ? `<span class="badge bg-secondary-lt ms-2"
                     title="Deltas are measured from this run — the earliest of them">
                 reference
               </span>`
            : ""}
          ${run.is_baseline ? `<span class="badge bg-blue-lt ms-2">baseline</span>` : ""}
        </td>
        <td class="text-secondary">${escape(run.profile ?? "—")}</td>
        <td class="text-secondary">${escape(run.addressing_mode)}</td>
        <td class="num">${duration(run.duration_ms)}</td>
        <td>${noteBadge(run)}</td>
        <td class="text-secondary">${timestamp(run.started_at)}</td>
      </tr>`
    )
    .join("");

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">Comparing ${comparison.runs.length} runs</h3>
        <div class="card-subtitle">Oldest first. Deltas are measured from the
          earliest, because a comparison reads as <em>what changed since</em>.</div>
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th style="width:30%">Run</th>
          <th style="width:14%">Profile</th>
          <th style="width:14%">Addressing</th>
          <th class="num" style="width:9%">Length</th>
          <th style="width:12%">Notes</th>
          <th style="width:21%">Started</th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}

function noteBadge(run) {
  if (!run.worst) return `<span class="text-secondary">clean</span>`;
  const tone = { invalid: "red", warn: "orange", info: "blue" }[run.worst];
  return `<span class="badge bg-${tone}-lt">${escape(run.worst)}</span>`;
}

/**
 * Whether these runs pool into one answer, and if not, why not.
 *
 * Stated above the table rather than left to the empty column, because a missing
 * column reads as a bug and a refusal with a reason reads as an answer.
 */
function mergeNote(comparison) {
  if (comparison.mergeable) {
    return `<p class="metrix-note text-secondary">
      ${icon("chart-line")} These runs share a setup, so they also merge: the
      <strong>merged</strong> column is every reading from all of them in one
      distribution, summarised once. That is a merge, not an average of the columns
      beside it — the mean of several medians is a number no run ever produced. It is
      also the cheapest way past a short window: runs that are each too short to
      support a p95 can merge into a set that is not.
    </p>`;
  }

  const differs = Object.entries(comparison.differences ?? {});
  if (!differs.length) return "";
  return `<div class="alert alert-warning">
    <strong>These runs measure different setups, so they are not merged.</strong>
    ${differs
      .map(([what, values]) => `${escape(what)} differs (${values.map(escape).join(" vs ")})`)
      .join("; ")}.
    Reading them side by side is a fair thing to do — it is the deliberate way to
    compare across a setup change. Pooling them is not: the combined distribution
    describes nothing that exists, while carrying a sample count that would make it
    look authoritative.
  </div>`;
}

/** Baseline against baseline is environment drift; measure against measure is the
 *  actual question; settle against settle says whether recovery is degrading. */
function phaseBar(comparison) {
  const phases = comparison.phases ?? [];
  if (phases.length < 2) return "";
  const button = (value, label, title) =>
    `<button class="btn ${comparison.phase === value ? "active" : ""}"
             data-action="compare-phase" data-phase="${escape(value ?? "")}"
             title="${escape(title)}">${escape(label)}</button>`;

  return `<div class="card">
    <div class="card-body d-flex align-items-center gap-3 flex-wrap">
      <span class="text-secondary">Window</span>
      <div class="btn-group">
        ${button(null, "Whole run", "Every sample in each recording")}
        ${phases
          .map((p) =>
            button(p, p, {
              baseline: "Before anything was asked of it — this is environment drift",
              measure: "Under load — the actual question",
              settle: "After the traffic stopped — whether recovery is degrading",
            }[p] ?? `The ${p} phase of each run`)
          )
          .join("")}
      </div>
    </div>
  </div>`;
}

/* ---------------------------------------------------------------------- tables */

function tables(comparison) {
  const runs = comparison.runs;
  const columns = [
    { key: "n", label: "Samples", read: (s) => (s ? String(s.n) : "—") },
    { key: "p50", label: "Median" },
    { key: "p95", label: "p95" },
    { key: "min", label: "Min" },
    { key: "max", label: "Max" },
    { key: "stddev", label: "Std dev" },
  ];

  return comparison.metrics
    .map((metric) => {
      const row = comparison.rows[metric];
      const body = columns
        .map(
          (column) => `<tr>
            <td class="name">${escape(column.label)}</td>
            ${runs
              .map((run) => `<td class="num">${cell(metric, row.per_run[run.id], column)}</td>`)
              .join("")}
            ${comparison.mergeable
              ? `<td class="num metrix-merged">${cell(metric, row.merged, column)}</td>`
              : ""}
          </tr>`
        )
        .join("");

      return `<div class="card">
        <div class="card-header">
          <div>
            <h3 class="card-title">${escape(metric)}</h3>
            <div class="card-subtitle">${deltaLine(metric, comparison)}</div>
          </div>
        </div>
        <div class="table-responsive">
          <table class="table card-table table-vcenter metrix-table">
            <thead><tr>
              <th style="width:14%"></th>
              ${runs
                .map(
                  (run, i) => `<th class="num">
                    <span class="metrix-swatch" style="background:${color(i)}"></span>
                    ${escape(shortId(run.id))}
                  </th>`
                )
                .join("")}
              ${comparison.mergeable
                ? `<th class="num metrix-merged"
                       title="Every reading from all of these runs, as one distribution">
                     merged
                   </th>`
                : ""}
            </tr></thead>
            <tbody>${body}</tbody>
          </table>
        </div>
      </div>`;
    })
    .join("");
}

/**
 * One figure, and never a bare one.
 *
 * A value the sample count could not support comes back `null` from the API and is
 * drawn as a dash carrying that count — not as a blank, which reads as "not
 * collected", and not as a zero, which reads as a measurement.
 */
function cell(metric, summary, column) {
  if (!summary) return `<span class="text-secondary">—</span>`;
  if (column.read) return column.read(summary);
  const value = summary[column.key];
  if (value == null) {
    return `<span class="text-secondary unsupported"
                  title="${summary.n} samples cannot support this">— <small>n=${
                    summary.n
                  }</small></span>`;
  }
  return metricValue(metric, value);
}

/**
 * What moved, against the reference run.
 *
 * Suppressed wherever either window's count cannot support the claim (§14.4): a
 * table is exactly where a spurious percentage gets quoted without its caveat, and
 * the caveat never travels with the quote.
 */
function deltaLine(metric, comparison) {
  const deltas = comparison.rows[metric].deltas ?? {};
  const parts = comparison.runs
    .filter((run) => run.id !== comparison.reference_id)
    .map((run) => {
      const delta = deltas[run.id];
      if (!delta) return "";
      if (!delta.comparable) {
        return `<span class="text-secondary">${escape(shortId(run.id))}: too few samples
          to compare</span>`;
      }
      const tone = delta.outside_band
        ? delta.worse
          ? "text-danger"
          : "text-success"
        : "text-secondary";
      return `<span class="${tone}">${escape(shortId(run.id))}:
        ${metricChange(metric, delta.change)}
        (${delta.change > 0 ? "+" : ""}${delta.change_pct.toFixed(1)}%)${
          delta.outside_band ? "" : " — within the reference's own spread"
        }</span>`;
    })
    .filter(Boolean)
    .join(" · ");

  return parts || "Against the earliest run.";
}

// Enough of an id to tell six runs apart in a column heading; the full one is a
// timestamp and a token, and six of those is a table nobody can read.
function shortId(id) {
  return String(id).replace(/T\d\d-\d\d-\d\dZ_/, " ");
}

function color(i) {
  return RUN_COLORS[i % RUN_COLORS.length];
}

/* --------------------------------------------------------------------- overlays */

/**
 * Every run's line for one metric, on one axis.
 *
 * The x axis is time *since each run started*, not wall clock: that is what makes
 * two runs of the same plan lie on top of each other. Each run's own clock is
 * already relative to its start, so they align with no work — and comparing them on
 * absolute time would draw two runs a week apart as two distant specks.
 */
function overlays(state) {
  const comparison = state.comparison;
  if (!state.comparisonSeries || !comparison.metrics.length) return "";
  return comparison.metrics
    .map(
      (metric) => `<div class="card">
        <div class="card-header">
          <div>
            <h3 class="card-title">${escape(metric)} over each run</h3>
            <div class="card-subtitle">One line per run, on seconds since that run
              started — which is what makes two runs of the same plan lie on top of
              each other.</div>
          </div>
        </div>
        <div class="card-body metrix-chart" data-overlay="${escape(metric)}"></div>
      </div>`
    )
    .join("");
}

/**
 * The x axis and one y column per run, with `null` where a run has no reading.
 *
 * Same rule as the single-run charts: a missing reading is a break in that run's
 * line, never a value carried across from its neighbour. Runs are different lengths,
 * so the shorter ones simply stop — which is the correct picture and the reason the
 * columns are padded with nulls rather than truncated to the shortest run.
 */
export function align(series, metric, runIds) {
  const times = new Set();
  const byRun = new Map();

  for (const id of runIds) {
    const points = pooled(series[id], metric);
    byRun.set(id, points);
    for (const [t] of points) times.add(t);
  }

  const xs = [...times].sort((a, b) => a - b);
  const index = new Map(xs.map((t, i) => [t, i]));
  const columns = runIds.map((id) => {
    const column = new Array(xs.length).fill(null);
    for (const [t, value] of byRun.get(id)) column[index.get(t)] = value;
    return column;
  });
  return [xs.map((t) => t / 1000), ...columns];
}

/**
 * One line per run, not one per box: every target's reading at a moment, averaged.
 *
 * A comparison of six runs on three boxes each is eighteen lines, which is not a
 * chart. The per-box view is on Charts, one run at a time; here the run is the unit,
 * so the boxes are pooled — the same pooled view the table's figures come from.
 */
function pooled(chart, metric) {
  const byTarget = chart?.series?.[metric];
  if (!byTarget) return [];
  const sums = new Map();
  for (const points of Object.values(byTarget)) {
    for (const [t, value] of points) {
      const at = sums.get(t) ?? { total: 0, n: 0 };
      at.total += value;
      at.n += 1;
      sums.set(t, at);
    }
  }
  return [...sums.entries()]
    .map(([t, { total, n }]) => [t, total / n])
    .sort((a, b) => a[0] - b[0]);
}

export function draw(state) {
  const comparison = state.comparison;
  if (!uPlot || !comparison || !state.comparisonSeries) return;

  for (const plot of charts.values()) plot.destroy();
  charts = new Map();
  const runIds = comparison.runs.map((r) => r.id);
  for (const metric of comparison.metrics) {
    const holder = document.querySelector(`[data-overlay="${cssEscape(metric)}"]`);
    if (!holder) continue;
    holder.innerHTML = "";
    charts.set(metric, build(holder, state, metric, runIds));
  }
  drawnFor = runIds.join(",");
}

function build(holder, state, metric, runIds) {
  const data = align(state.comparisonSeries, metric, runIds);
  return new uPlot(
    {
      width: holder.clientWidth || 800,
      height: 200,
      cursor: { sync: { key: SYNC }, drag: { x: true, y: false } },
      legend: { live: true },
      scales: { x: { time: false } },
      axes: [
        {
          label: "seconds since each run started",
          values: (_, ticks) => ticks.map((t) => `${t}s`),
          ...axisTheme(),
        },
        { values: (_, ticks) => ticks.map((v) => metricValue(metric, v)), ...axisTheme() },
      ],
      series: [
        { label: "t" },
        ...runIds.map((id, i) => ({
          label: shortId(id),
          stroke: color(i),
          width: 1.5,
          spanGaps: false,
          value: (_, v) => (v == null ? "—" : metricValue(metric, v)),
        })),
      ],
    },
    data,
    holder
  );
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

export function resize() {
  for (const [metric, plot] of charts) {
    const holder = document.querySelector(`[data-overlay="${cssEscape(metric)}"]`);
    if (holder?.clientWidth) plot.setSize({ width: holder.clientWidth, height: 200 });
  }
}

/** The long answer, behind the help button. */
export function help() {
  return {
    title: "Comparing runs",
    body: `
      <p>Every selected run's figures side by side, a delta against the earliest of
        them, and one line per run drawn on a shared axis. Two runs up to six — past
        six an overlay has more lines than there are colours anyone can tell apart,
        and the answer to that is fewer runs, not more hues.</p>

      <h4>The merged column</h4>
      <p>Where the runs share a setup, the <strong>merged</strong> column is every
        reading from all of them in one distribution, summarised once. It is a
        <em>merge</em>, not an average of the columns beside it: the mean of several
        medians is a number no run ever produced, and the mean of several p95s has no
        interpretation at all. Pooling the readings and describing the pooled set is
        the only arithmetic that answers a question about the whole.</p>
      <p>That is also the cheapest way past a short window. Runs that are each too
        short to support a p95 can merge into a set that is, and the count travelling
        with that figure is a real count of real readings rather than a borrowed
        one.</p>

      <h4>When merging is refused</h4>
      <p>Runs of <em>different</em> setups are never pooled. They measure different
        things, so their combined distribution describes nothing that exists — while
        carrying a sample count that would make it look authoritative. The page says
        which part of the identity differs rather than simply dropping the column.</p>
      <p>The side-by-side columns and the overlay stay, because reading two setups
        against each other deliberately is the sanctioned way to compare across a
        setup change. It is the pooling that is wrong, not the looking.</p>

      <h4>Windows</h4>
      <p>Comparing one phase across runs asks a different question each time:
        <strong>baseline against baseline</strong> is whether the environment itself
        is drifting, <strong>measure against measure</strong> is the actual question,
        and <strong>settle against settle</strong> is whether recovery is degrading.
        Only phases every selected run recorded are offered — a phase half the group
        lacks would make columns that are empty for no stated reason.</p>

      <h4>Reading the figures</h4>
      <p>Every figure carries the count behind it, and one the count cannot support is
        drawn as a dash with that count rather than as a blank or a zero. Deltas are
        suppressed entirely where either side is unsupported: a table is exactly where
        a spurious percentage gets quoted, and the caveat never travels with the
        quote. A delta inside the reference run's own spread says so.</p>
      <p>The overlay draws seconds since each run started, not wall clock — that is
        what makes two runs of the same plan lie on top of each other. One line per
        run, with each run's boxes pooled; the per-box view is on Charts, one run at a
        time.</p>
    `,
  };
}
