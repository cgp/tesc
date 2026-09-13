// The charts page (design §15). Shape and timing; no tables — exact figures are on
// Stats, and putting both on one page makes each worse.
//
// One chart per metric, every target overlaid, all of them sharing an x-axis and a
// crosshair. Three things are drawn *behind* the lines and matter as much as the
// lines do:
//
//   * phase bands, so a number is never read without knowing which phase produced it
//   * annotations, shaded where they apply, so an invalid stretch is visible
//   * gaps as gaps — a break in the line, never a segment drawn straight across
//
// That last one is the rule this page cannot compromise on. An interpolated gap is a
// picture of data that was never collected, and it is indistinguishable from a flat
// healthy stretch at exactly the moment someone is looking for the opposite.

import { escape, metricValue } from "./format.js";
import { empty, icon } from "./ui.js";

//: uPlot is vendored and loaded by index.html before this module runs.
const uPlot = globalThis.uPlot;

//: One cursor across every chart on the page (§15). uPlot syncs by key.
const SYNC = "metrix";

//: Enough distinguishable colours for the boxes in one environment; past that the
//: overlay stops being readable and the answer is fewer targets, not more hues.
const SERIES_COLORS = ["#206bc4", "#d63939", "#2fb344", "#f76707", "#ae3ec9", "#0ca678"];

//: What each phase means, and how strongly to tint it. Baseline and settle are the
//: windows a reader compares, so they are the ones that get a visible band.
const PHASE_TINT = {
  baseline: "rgba(32, 107, 196, 0.07)",
  warmup: "rgba(247, 103, 7, 0.06)",
  measure: "rgba(0, 0, 0, 0)",
  drain: "rgba(247, 103, 7, 0.06)",
  settle: "rgba(47, 179, 68, 0.07)",
};

const SEVERITY_TINT = {
  invalid: "rgba(214, 57, 57, 0.10)",
  warn: "rgba(247, 103, 7, 0.09)",
  info: "rgba(32, 107, 196, 0.06)",
};

//: One line per metric, saying what the chart answers. §15: no chart ships without
//: one. Anything not named here falls back to a generic caption rather than none.
const CAPTIONS = {
  "cpu.busy": "How hard the machine was working, ignoring idle and steal.",
  "cpu.user": "Time in application code, as opposed to the kernel.",
  "cpu.system": "Time in the kernel — high with heavy IO or syscall traffic.",
  "cpu.iowait": "Time blocked on storage. Latency that tracks this is a disk problem.",
  "cpu.steal": "Time the hypervisor gave to someone else. Not your code's fault.",
  "mem.used_bytes": "Memory in use. A line that only ever climbs is the leak signal.",
  "mem.available_bytes": "What is left for the workload before it starts swapping.",
  "swap.used_bytes": "Swapping at all under load usually explains a latency cliff.",
  "conn.established": "Open connections — whether keepalive is doing its job.",
  "proc.count": "Processes on the box. A climbing count that never falls is a leak.",
  "thread.count": "Kernel scheduling entities: processes and threads together.",
  "fd.open": "Open file descriptors. Another thing that leaks and then fails hard.",
};

let charts = new Map();
let drawnFor = null;

export function selectState(state) {
  // The live chart object is replaced on every tick, so identity is enough to notice
  // new points -- no counter, and no deep comparison of a growing array.
  return [state.selectedRecording, state.live?.chart, state.live?.recordingId];
}

/** Whichever recording is in front: one being watched, or one being read back. */
function subject(state) {
  const live = state.live;
  if (live?.chart) return { id: live.recordingId, chart: live.chart, live: true };
  const recording = state.selectedRecording;
  return recording ? { id: recording.id, chart: recording.chart, live: false } : null;
}

export function render(state) {
  const recording = subject(state);
  if (!recording) {
    return empty({
      icon: "chart-line",
      title: "Nothing to draw yet",
      body: `Open a recording from <a href="#/recordings">Recordings</a>, or start one
        from <a href="#/profiles">Profiles</a>.`,
    });
  }
  if (!recording.chart) {
    return empty({
      icon: "chart-line",
      title: "Loading the series…",
      body: `Reading every sample in <code>${escape(recording.id)}</code>.`,
    });
  }
  if (!recording.chart.metrics.length) {
    return empty({
      icon: "alert-triangle",
      title: "This recording holds no samples",
      body: `It was started, but nothing was ever collected. The notes on the
        <a href="#/recordings/${encodeURIComponent(recording.id)}">recording</a> say why.`,
    });
  }

  const cards = recording.chart.metrics
    .map(
      (metric) => `<div class="card">
        <div class="card-header">
          <div>
            <h3 class="card-title">${escape(metric)}</h3>
            <div class="card-subtitle">${escape(caption(metric))}</div>
          </div>
        </div>
        <div class="card-body metrix-chart" data-chart="${escape(metric)}"></div>
      </div>`
    )
    .join("");

  return `<div class="metrix-stack">
    ${legend(recording.chart)}
    ${cards}
  </div>`;
}

function caption(metric) {
  return CAPTIONS[metric] ?? `${metric} over the life of the recording.`;
}

function legend(chart) {
  const phases = [...new Set(chart.phases.map((p) => p.phase))];
  const swatches = phases
    .filter((p) => PHASE_TINT[p] && PHASE_TINT[p] !== "rgba(0, 0, 0, 0)")
    .map(
      (p) => `<span class="metrix-swatch" style="background:${PHASE_TINT[p]}"></span>
              ${escape(p)}`
    )
    .join(" ");

  const notes = [
    swatches ? `Shaded bands are phases: ${swatches}.` : "",
    chart.gaps.length
      ? `${chart.gaps.length} collection gap(s) are drawn as breaks in the line, never
         joined across.`
      : "Every interval was collected, so no line is broken.",
    Object.keys(chart.baseline).length
      ? `The dashed line is what this environment normally sits at.`
      : "",
  ]
    .filter(Boolean)
    .join(" ");

  return `<p class="metrix-note text-secondary">
    ${icon("chart-line")} Every chart shares one time axis and one crosshair — move
    the pointer over any of them and the rest follow. ${notes}
  </p>`;
}

/* --------------------------------------------------------------------- drawing */

/**
 * Draw or update the charts for whatever the state holds.
 *
 * Called after the markup is in the DOM. Existing charts are fed new data rather
 * than rebuilt: a chart replaced every second loses its crosshair and its zoom, and
 * rebuilding a canvas is far more work than handing uPlot a new array.
 */
export function draw(state) {
  const recording = subject(state);
  if (!uPlot || !recording?.chart) return;

  const chart = recording.chart;
  const targets = targetsIn(chart);

  for (const plot of charts.values()) plot.destroy();
  charts = new Map();
  for (const metric of chart.metrics) {
    const holder = document.querySelector(`[data-chart="${cssEscape(metric)}"]`);
    if (!holder) continue;
    holder.innerHTML = "";
    charts.set(metric, build(holder, chart, metric, targets));
  }
  drawnFor = recording.id;
}

/**
 * Feed the existing charts new points without touching the markup.
 *
 * Returns false when the shape changed — a different recording, a metric that was
 * not there before, or the containers replaced by a route change — and the caller
 * does a full render instead. Same bargain as the live table: a canvas rebuilt once
 * a second loses the crosshair and any zoom the reader set, which is most of what
 * makes a chart worth looking at while it fills.
 */
export function patch(state) {
  const recording = subject(state);
  if (!uPlot || !recording?.chart || drawnFor !== recording.id || !charts.size) return false;
  if (recording.chart.metrics.length !== charts.size) return false;

  const targets = targetsIn(recording.chart);
  for (const [metric, plot] of charts) {
    if (!document.querySelector(`[data-chart="${cssEscape(metric)}"]`)) return false;
    plot.setData(align(recording.chart, metric, targets));
  }
  return true;
}

function targetsIn(chart) {
  const found = new Set();
  for (const byTarget of Object.values(chart.series)) {
    for (const target of Object.keys(byTarget)) found.add(target);
  }
  return [...found].sort();
}

/**
 * The span every chart on the page is drawn over: the recording, not the metric.
 *
 * Host samples start at zero; the engine's first window lands wherever the load was
 * launched. Left to itself uPlot fits each chart to its own data, so a vertical line
 * at the same screen position would mean 1.2s on one chart and 3.4s on the next --
 * and the question this page exists to answer is whether the shape of one explains
 * the shape of the other.
 */
export function extent(chart, targets) {
  let low = Infinity;
  let high = -Infinity;
  for (const metric of chart.metrics) {
    const [xs] = align(chart, metric, targets);
    if (!xs.length) continue;
    low = Math.min(low, xs[0]);
    high = Math.max(high, xs[xs.length - 1]);
  }
  // A recording with a single sample has no span; give it one so uPlot has a scale.
  if (!Number.isFinite(low)) return [0, 1];
  return low === high ? [low, low + 1] : [low, high];
}

/**
 * The x axis, and one y array per target, with `null` wherever nothing was collected.
 *
 * uPlot breaks a line at a null and joins across a missing x, so the nulls are what
 * make a gap a gap. Two sources of them: a target with no sample at a time another
 * target has one, and a recorded gap — which needs an x of its own, because when
 * every target stopped at once there is no timestamp there to hang a null on.
 */
export function align(chart, metric, targets) {
  const byTarget = chart.series[metric] ?? {};
  const times = new Set();
  for (const points of Object.values(byTarget)) {
    for (const [t] of points) times.add(t);
  }
  // A point inside each recorded gap, so a hole with no samples either side still
  // breaks the line rather than being spanned.
  for (const gap of chart.gaps) {
    times.add(Math.round((gap.from_ms + gap.to_ms) / 2));
  }

  const xs = [...times].sort((a, b) => a - b);
  const index = new Map(xs.map((t, i) => [t, i]));
  const columns = targets.map((target) => {
    const column = new Array(xs.length).fill(null);
    for (const [t, value] of byTarget[target] ?? []) column[index.get(t)] = value;
    return column;
  });

  // Seconds, because uPlot's time axis works in them and the recording clock is
  // monotonic from zero rather than wall clock.
  return [xs.map((t) => t / 1000), ...columns];
}

function build(holder, chart, metric, targets) {
  const data = align(chart, metric, targets);
  const span = extent(chart, targets);
  return new uPlot(
    {
      width: holder.clientWidth || 800,
      height: 200,
      cursor: { sync: { key: SYNC }, drag: { x: true, y: false } },
      legend: { live: true },
      // Every chart on the page gets the recording's whole span, not its own
      // metric's. A shared crosshair is not a shared axis: with load starting two
      // seconds after the observer did, auto-scaled charts put the same moment at
      // different pixels, and reading one against the other is exactly what this
      // page is for. `u.scales.x.min` is honoured after a zoom so dragging still
      // works and still moves every chart together.
      scales: {
        x: {
          time: false,
          range: (u, min, max) => (u.select?.width ? [min, max] : span),
        },
      },
      axes: [
        {
          label: "seconds since the recording started",
          values: (_, ticks) => ticks.map((t) => `${t}s`),
          ...axisTheme(),
        },
        {
          // Units are never implied (§15): the axis is formatted by the same rule the
          // table uses, so a byte count reads as GB in both places.
          values: (_, ticks) => ticks.map((v) => metricValue(metric, v)),
          ...axisTheme(),
        },
      ],
      series: [
        { label: "t" },
        ...targets.map((target, i) => ({
          label: target,
          stroke: SERIES_COLORS[i % SERIES_COLORS.length],
          width: 1.5,
          // The whole point: uPlot joins across a null unless told not to.
          spanGaps: false,
          value: (_, v) => (v == null ? "—" : metricValue(metric, v)),
        })),
      ],
      plugins: [bands(chart, metric)],
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

/**
 * Everything drawn behind the lines: phase bands, annotation regions, and the
 * baseline reference.
 *
 * A uPlot plugin rather than markup around the canvas, because these have to line up
 * with the x scale exactly — including after a zoom, which is when a band drawn as a
 * positioned div silently stops meaning anything.
 */
function bands(chart, metric) {
  const spans = phaseSpans(chart);
  const regions = chart.annotations
    .filter((a) => a.to_ms != null && a.to_ms > a.from_ms)
    .map((a) => ({ ...a, tint: SEVERITY_TINT[a.severity] }));
  const normal = chart.baseline[metric];

  return {
    hooks: {
      draw: (u) => {
        const { ctx } = u;
        const top = u.bbox.top;
        const height = u.bbox.height;
        ctx.save();
        ctx.beginPath();
        ctx.rect(u.bbox.left, top, u.bbox.width, height);
        ctx.clip();

        for (const { from, to, tint } of [...spans, ...regionSpans(regions)]) {
          if (!tint) continue;
          const x0 = u.valToPos(from / 1000, "x", true);
          const x1 = u.valToPos(to / 1000, "x", true);
          ctx.fillStyle = tint;
          ctx.fillRect(x0, top, Math.max(1, x1 - x0), height);
        }

        // A boundary is a line even when neither side is tinted: the measure phase
        // has no band, and where it starts is exactly what a reader is looking for.
        ctx.strokeStyle = "rgba(102, 115, 130, 0.45)";
        ctx.setLineDash([3, 3]);
        ctx.lineWidth = 1;
        for (const { from } of spans) {
          if (from <= 0) continue;
          const x = u.valToPos(from / 1000, "x", true);
          ctx.beginPath();
          ctx.moveTo(x, top);
          ctx.lineTo(x, top + height);
          ctx.stroke();
        }

        if (normal != null) {
          const y = u.valToPos(normal, "y", true);
          ctx.strokeStyle = "rgba(32, 107, 196, 0.7)";
          ctx.setLineDash([6, 4]);
          ctx.beginPath();
          ctx.moveTo(u.bbox.left, y);
          ctx.lineTo(u.bbox.left + u.bbox.width, y);
          ctx.stroke();
        }

        ctx.restore();
      },
    },
  };
}

// Phases are recorded per target and are the same window on each, so one band is
// drawn per distinct phase rather than one per target stacked on top of itself.
export function phaseSpans(chart) {
  const seen = new Map();
  for (const phase of chart.phases) {
    const current = seen.get(phase.phase) ?? { from: Infinity, to: -Infinity };
    seen.set(phase.phase, {
      from: Math.min(current.from, phase.from_ms),
      to: Math.max(current.to, phase.to_ms ?? phase.from_ms),
    });
  }
  return [...seen.entries()]
    .map(([name, span]) => ({ ...span, phase: name, tint: PHASE_TINT[name] }))
    .sort((a, b) => a.from - b.from);
}

function regionSpans(regions) {
  return regions.map((a) => ({ from: a.from_ms, to: a.to_ms, tint: a.tint }));
}

function cssEscape(value) {
  return String(value).replace(/["\\]/g, "\\$&");
}

/** Resize every chart to its container. Called on window resize. */
export function resize() {
  for (const [metric, plot] of charts) {
    const holder = document.querySelector(`[data-chart="${cssEscape(metric)}"]`);
    if (holder?.clientWidth) plot.setSize({ width: holder.clientWidth, height: 200 });
  }
}
