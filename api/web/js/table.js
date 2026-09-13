// The stats table (design §14). Exact numbers, read off and quoted; no charts here.
//
// One view for a running recording and a finished one (§14.3). They differ only in
// where the numbers come from — the live stream, or `/summary` — and both arrive
// already summarised, because the sample-count rule lives in `stats/` and the UI
// must not be able to bypass it. Nothing here computes a percentile.
//
// A live table is built once and then *patched in place*. Replacing the markup every
// second would destroy text selection, and a button rebuilt under the cursor cannot
// reliably be clicked — both of which make a live view something people wait out
// rather than watch.

import { duration, escape, metricValue, targetLabel } from "./format.js";
import { empty, icon } from "./ui.js";

//: The pooled row group: every box's readings in one distribution. First, because it
//: is the run total — the question "what is this environment doing" before "which box".
const ENVIRONMENT = "*";

//: Default visible, in reading order. §14.2 puts median and p95 either side of the
//: standard deviation deliberately: std dev is the fastest way to see that a metric
//: is unstable, and the worst thing to set a threshold on, so the percentiles that
//: describe the experience sit next to it.
const COLUMNS = [
  { key: "metric", label: "Metric", align: "name" },
  { key: "n", label: "Count", hint: "Samples behind every figure in this row" },
  { key: "min", label: "Min" },
  { key: "p50", label: "Median" },
  { key: "p95", label: "p95", hint: "Withheld below 20 samples" },
  { key: "max", label: "Max" },
  { key: "stddev", label: "Std dev", hint: "High beside a low median means unstable" },
];

export function selectState(state) {
  // `tableSort` belongs here even while live: reordering is a person acting on the
  // table, and a click that changes nothing because the live path skipped the
  // subscription is worse than no sorting at all.
  return state.live?.summaries
    ? [state.live, state.tableSort]
    : [state.live, state.selectedRecording, state.tableSort];
}

export function render(state) {
  const live = state.live;
  const recording = state.selectedRecording;
  const sort = state.tableSort ?? null;

  if (live && live.summaries) return card(liveModel(live), sort);
  if (!recording) {
    return empty({
      icon: "chart-line",
      title: "Nothing to read yet",
      body: `Open a recording from <a href="#/recordings">Recordings</a>, or start one
        from <a href="#/config">Config</a>.`,
    });
  }
  if (!recording.metrics.length) {
    return empty({
      icon: "alert-triangle",
      title: "This recording holds no samples",
      body: `It was started, but nothing was ever collected. The notes on the
        <a href="#/recordings/${encodeURIComponent(recording.id)}">recording</a> say why.`,
    });
  }
  return card(recordedModel(recording), sort);
}

/* ------------------------------------------------------------------- the model */

// One shape, whichever end it came from. The view below knows nothing about live.
function liveModel(live) {
  return {
    id: live.recordingId,
    live: true,
    connection: live.connection,
    phase: live.phase,
    elapsedMs: live.elapsedMs,
    summaries: live.summaries ?? {},
    spans: live.spans ?? {},
    latest: live.latest ?? {},
    gaps: live.gaps ?? [],
  };
}

function recordedModel(recording) {
  return {
    id: recording.id,
    live: false,
    phase: (recording.phases ?? []).map((p) => p.phase).find(Boolean),
    elapsedMs: recording.duration_ms,
    summaries: recording.summary?.targets ?? {},
    spans: recording.summary?.spans ?? {},
    latest: recording.latest ?? {},
    gaps: recording.gaps ?? [],
  };
}

/**
 * Row groups, pooled first and then a box at a time.
 *
 * Grouping is what keeps this readable as targets multiply, the same way §14.1 groups
 * steps under their chain. Sorting happens *within* a group: sorting across them
 * would dissolve the grouping into a flat list and lose which box a row belongs to.
 */
function groups(model, sort) {
  const names = Object.keys(model.summaries).sort((a, b) => {
    if (a === ENVIRONMENT) return -1;
    if (b === ENVIRONMENT) return 1;
    return a.localeCompare(b);
  });

  return names.map((target) => {
    const rows = Object.values(model.summaries[target] ?? {});
    return { target, span: model.spans[target], rows: ordered(rows, sort) };
  });
}

function ordered(rows, sort) {
  const sorted = [...rows];
  if (!sort) return sorted.sort((a, b) => a.metric.localeCompare(b.metric));
  sorted.sort((a, b) => {
    const [x, y] = [a[sort.key], b[sort.key]];
    // A withheld figure sorts last whichever way the column is pointing: it is
    // absent, not small, and floating it to the top would read as a minimum.
    if (x == null && y == null) return a.metric.localeCompare(b.metric);
    if (x == null) return 1;
    if (y == null) return -1;
    const order = typeof x === "string" ? x.localeCompare(y) : x - y;
    return sort.direction === "desc" ? -order : order;
  });
  return sorted;
}

/* -------------------------------------------------------------------- the view */

function card(model, sort) {
  const body = groups(model, sort)
    .map((group) => groupMarkup(group, model))
    .join("");

  const sortKey = sort ? `${sort.key}:${sort.direction}` : "";
  return `<div class="card" data-sort="${escape(sortKey)}"
       ${model.live ? `data-live-table="${escape(model.id)}"` : ""}>
    <div class="card-header">
      <div>
        <h3 class="card-title">${escape(model.id)}</h3>
        <div class="card-subtitle" data-live-meta>${escape(metaText(model))}</div>
      </div>
      <div class="card-actions d-flex align-items-center gap-2">
        ${model.live ? connectionStatus(model.connection) : ""}
        <button class="btn btn-sm" data-action="table-copy"
                title="Copy the table as TSV, in the order shown">
          ${icon("clipboard")} Copy TSV
        </button>
        <button class="btn btn-sm" data-action="table-csv"
                title="Download the table as CSV, in the order shown">
          ${icon("download")} CSV
        </button>
        ${
          model.live
            ? `<button class="btn btn-sm btn-outline-danger" data-action="stop"
                       data-recording="${escape(model.id)}">
                 ${icon("player-stop")} Stop
               </button>`
            : `<a class="btn btn-sm" href="#/recordings/${encodeURIComponent(model.id)}">
                 Recording details
               </a>`
        }
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table metrix-stats">
        <thead><tr>${COLUMNS.map((c) => headerCell(c, sort)).join("")}</tr></thead>
        ${body}
      </table>
    </div>
    <div data-live-gaps data-count="${model.gaps.length}">${gapNote(model.gaps)}</div>
  </div>`;
}

function headerCell(column, sort) {
  const active = sort?.key === column.key;
  const arrow = active ? (sort.direction === "desc" ? " ↓" : " ↑") : "";
  const width = column.key === "metric" ? "width:28%" : "";
  return `<th class="${column.key === "metric" ? "" : "num"} metrix-sortable"
              style="${width}" data-action="table-sort" data-column="${column.key}"
              title="${escape(column.hint ?? "Sort by this column")}"
              ${active ? 'aria-sort="' + (sort.direction === "desc" ? "descending" : "ascending") + '"' : ""}
          >${escape(column.label)}${arrow}</th>`;
}

function groupMarkup(group, model) {
  const label =
    group.target === ENVIRONMENT
      ? `<strong>every box</strong>
         <span class="text-secondary">— all readings pooled</span>`
      : `<strong title="${escape(group.target)}">${escape(targetLabel(group.target))}</strong>`;

  const gaps = model.gaps.filter((g) => g.target_id === group.target).length;
  const span = `<span class="text-secondary" data-span>${escape(spanText(group.span))}</span>`;
  const gapMark = gaps
    ? `<span class="badge bg-orange-lt ms-2" title="Intervals never collected">
         ${gaps} gap${gaps === 1 ? "" : "s"}
       </span>`
    : "";

  const rows = group.rows
    .map(
      (row) => `<tr data-metric="${escape(row.metric)}" data-target="${escape(group.target)}">
        <td class="name">${escape(row.metric)}</td>
        ${COLUMNS.slice(1)
          .map(
            (c) =>
              `<td class="num" data-column="${c.key}">${cell(row, c.key)}</td>`
          )
          .join("")}
      </tr>`
    )
    .join("");

  return `<tbody data-group="${escape(group.target)}">
    <tr class="metrix-group"><td colspan="${COLUMNS.length}">${label} ${span}${gapMark}</td></tr>
    ${rows}
  </tbody>`;
}

/**
 * One figure.
 *
 * A withheld percentile is drawn as an em dash with its count on hover, never as a
 * blank and never as a zero: the difference between "we did not measure enough" and
 * "it was nothing" is the whole reason the API withholds it.
 */
function cell(row, key) {
  const value = row[key];
  if (key === "n") return String(value);
  if (value == null) {
    return `<span class="text-secondary" title="${row.n} samples is too few">—</span>`;
  }
  return escape(metricValue(row.metric, value));
}

// Kept out of the markup so the live patcher can rewrite it: a header that stops
// advancing while the numbers beside it move reads as a table that has half frozen.
function spanText(span) {
  if (!span) return "";
  return `${duration(span.first_ms)}–${duration(span.last_ms)} · ${span.n} samples`;
}

function metaText(model) {
  const parts = [duration(model.elapsedMs)];
  if (model.phase) parts.push(`${model.phase} phase`);
  const pooled = model.summaries[ENVIRONMENT] ?? {};
  const samples = Object.values(pooled)[0]?.n;
  if (samples != null) parts.push(`${samples} samples`);
  return parts.join(" · ");
}

function connectionStatus(connection) {
  const label = { live: "live", reconnecting: "reconnecting…", ended: "ended" }[connection];
  const tone = { live: "green", reconnecting: "orange", ended: "secondary" }[connection];
  // Only a connection actually delivering data gets the pulse.
  const pulse = connection === "live" ? " status-dot-animated" : "";
  return `<span class="status status-${tone ?? "secondary"}"
                data-live-status="${escape(connection ?? "")}"
          ><span class="status-dot${pulse}"></span>${escape(label ?? "—")}</span>`;
}

function gapNote(gaps) {
  if (!gaps.length) return "";
  return `<div class="alert alert-warning m-3" role="alert">
    <div class="d-flex">
      <div class="me-3">${icon("alert-triangle")}</div>
      <div>${gaps.length} collection gap(s) so far. Drawn as gaps, never
        interpolated.</div>
    </div>
  </div>`;
}

/* ------------------------------------------------------------- live patching */

/**
 * Update an existing live table without rebuilding it.
 * Returns false when the shape changed and a full render is needed.
 */
export function patch(state) {
  const live = state.live;
  if (!live || !live.summaries) return false;

  const card = document.querySelector(`[data-live-table="${cssEscape(live.recordingId)}"]`);
  if (!card) return false;

  // Patching writes values into the rows where they already are; it cannot move a
  // row. A changed sort is a full render, which is fine -- it happens on a click
  // rather than once a second, so nothing is being rebuilt under the cursor.
  const sort = state.tableSort ? `${state.tableSort.key}:${state.tableSort.direction}` : "";
  if (card.dataset.sort !== sort) return false;

  const model = liveModel(live);
  for (const group of Object.keys(model.summaries)) {
    const body = card.querySelector(`tbody[data-group="${cssEscape(group)}"]`);
    // A new target or metric changes the row set; rebuild rather than guess.
    if (!body) return false;
    const rows = body.querySelectorAll("tr[data-metric]");
    if (rows.length !== Object.keys(model.summaries[group]).length) return false;

    for (const row of rows) {
      const summary = model.summaries[group][row.dataset.metric];
      if (!summary) return false;
      for (const td of row.querySelectorAll("td[data-column]")) {
        const text = plain(summary, td.dataset.column);
        if (td.textContent.trim() !== text) td.textContent = text;
      }
    }

    const span = body.querySelector("[data-span]");
    const text = spanText(model.spans[group]);
    if (span && span.textContent !== text) span.textContent = text;
  }

  const meta = card.querySelector("[data-live-meta]");
  if (meta) meta.textContent = metaText(model);

  // Replace the status only when it actually changed: the pulsing dot is a CSS
  // animation with a delay, and a node rebuilt every second never reaches it.
  const status = card.querySelector("[data-live-status]");
  if (status && status.dataset.liveStatus !== live.connection) {
    status.outerHTML = connectionStatus(live.connection);
  }

  const gaps = card.querySelector("[data-live-gaps]");
  if (gaps) {
    const count = String((live.gaps ?? []).length);
    if (gaps.dataset.count !== count) {
      gaps.dataset.count = count;
      gaps.innerHTML = gapNote(live.gaps ?? []);
    }
  }

  return true;
}

// The same figure as `cell`, as text: patching sets textContent, so the markup a
// withheld value carries would arrive escaped and visible.
function plain(row, key) {
  const value = row[key];
  if (key === "n") return String(value);
  return value == null ? "—" : metricValue(row.metric, value);
}

function cssEscape(value) {
  return String(value).replace(/["\\]/g, "\\$&");
}

/* ------------------------------------------------------------------- export */

/**
 * The table as text, in the order shown (§14.3).
 *
 * Built from the same model the view renders, so what is copied is what is on
 * screen — including the sort, and including a withheld figure as an empty cell
 * rather than as a zero somebody would then average.
 */
export function toDelimited(state, separator) {
  const live = state.live;
  const model = live && live.summaries ? liveModel(live) : recordedModel(state.selectedRecording);
  const lines = [["target", ...COLUMNS.map((c) => c.key)].join(separator)];

  for (const group of groups(model, state.tableSort ?? null)) {
    for (const row of group.rows) {
      lines.push(
        [
          group.target,
          ...COLUMNS.map((c) => {
            const value = row[c.key];
            return value == null ? "" : String(value);
          }),
        ]
          .map((field) => quoteFor(separator, field))
          .join(separator)
      );
    }
  }
  return lines.join("\n");
}

// Raw numbers, not formatted ones: these are pasted into a spreadsheet, and "7.5 GB"
// is a string there. Quoting only where the separator makes it necessary.
function quoteFor(separator, field) {
  if (separator !== "," || !/[",\n]/.test(field)) return field;
  return `"${field.replaceAll('"', '""')}"`;
}
