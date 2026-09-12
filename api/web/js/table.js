// The stats table (design §14). Exact numbers, read off and quoted; no charts here.
//
// A live table is built once and then *patched in place* (§14.3). Replacing the
// markup every second would destroy text selection, and a button rebuilt under the
// cursor cannot reliably be clicked -- both of which make a live view something
// people wait out rather than watch.

import { duration, escape, metricValue } from "./format.js";

export function render(state) {
  const live = state.live;
  const recording = state.selectedRecording;

  if (live && live.latest) return liveTable(live);
  if (!recording) {
    return `<div class="card"><div class="empty-note">
      Open a recording from <a href="#/recordings">Recordings</a>, or start one from
      <a href="#/config">Config</a>.
    </div></div>`;
  }
  if (!recording.metrics.length) {
    return `<div class="card"><div class="empty-note">
      This recording holds no samples.
    </div></div>`;
  }
  return staticTable(recording);
}

/**
 * Update an existing live table without rebuilding it.
 * Returns false when the shape changed and a full render is needed.
 */
export function patch(state) {
  const live = state.live;
  if (!live || !live.latest) return false;

  const card = document.querySelector(`[data-live-table="${cssEscape(live.recordingId)}"]`);
  if (!card) return false;

  // A new metric or target means new rows and columns; rebuild rather than guess.
  const rows = card.querySelectorAll("tbody tr[data-metric]");
  if (rows.length !== (live.metrics ?? []).length) return false;

  for (const row of rows) {
    const metric = row.dataset.metric;
    for (const cell of row.querySelectorAll("td[data-target]")) {
      const value = (live.latest[metric] ?? {})[cell.dataset.target];
      const text = value == null ? "—" : metricValue(metric, value);
      if (cell.textContent !== text) cell.textContent = text;
    }
  }

  const meta = card.querySelector("[data-live-meta]");
  if (meta) meta.textContent = metaText(live);

  const badge = card.querySelector("[data-live-badge]");
  if (badge) badge.outerHTML = connectionBadge(live.connection);

  const gaps = card.querySelector("[data-live-gaps]");
  if (gaps) gaps.innerHTML = gapNote(live);

  return true;
}

function cssEscape(value) {
  return String(value).replace(/["\\]/g, "\\$&");
}

function metaText(live) {
  return `${duration(live.elapsedMs)} · ${live.counts?.samples ?? 0} samples`;
}

function connectionBadge(connection) {
  const label = { live: "live", reconnecting: "reconnecting…", ended: "ended" }[connection];
  const tone = { live: "green", reconnecting: "orange", ended: "secondary" }[connection];
  return `<span class="badge bg-${tone ?? "secondary"}-lt" data-live-badge>${escape(
    label ?? "—"
  )}</span>`;
}

function gapNote(live) {
  if (!(live.gaps ?? []).length) return "";
  return `<div class="alert alert-warning mt-2 mb-0">
    ${live.gaps.length} collection gap(s). Drawn as gaps, never interpolated.
  </div>`;
}

function liveTable(live) {
  const targets = live.targets ?? [];
  const header = targets.map((t) => `<th class="num">${escape(t)}</th>`).join("");

  const rows = (live.metrics ?? [])
    .map((metric) => {
      const cells = targets
        .map((target) => {
          const value = (live.latest[metric] ?? {})[target];
          return `<td class="num" data-target="${escape(target)}">${
            value == null ? "—" : metricValue(metric, value)
          }</td>`;
        })
        .join("");
      return `<tr data-metric="${escape(metric)}">
        <td class="name">${escape(metric)}</td>${cells}
      </tr>`;
    })
    .join("");

  return `<div class="card" data-live-table="${escape(live.recordingId)}">
    <div class="card-header">
      <h3 class="card-title">${escape(live.recordingId)}</h3>
      <div class="card-actions">
        ${connectionBadge(live.connection)}
        <span class="text-secondary ms-2" data-live-meta>${metaText(live)}</span>
        <button class="btn btn-sm btn-outline-danger ms-2" data-action="stop"
                data-recording="${escape(live.recordingId)}">Stop</button>
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table metrix-table">
        <thead><tr><th style="width:30%">Metric</th>${header}</tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
    <div data-live-gaps>${gapNote(live)}</div>
  </div>`;
}

function staticTable(recording) {
  const targets = recording.targets;
  const header = targets.map((t) => `<th class="num">${escape(t)}</th>`).join("");
  const latest = recording.latest ?? {};

  const rows = recording.metrics
    .map((metric) => {
      const cells = targets
        .map((target) => {
          const value = (latest[metric] ?? {})[target];
          return `<td class="num">${value == null ? "—" : metricValue(metric, value)}</td>`;
        })
        .join("");
      return `<tr><td class="name">${escape(metric)}</td>${cells}</tr>`;
    })
    .join("");

  return `<div class="card">
    <div class="card-header">
      <h3 class="card-title">${escape(recording.id)}</h3>
      <div class="card-actions text-secondary">last value per target</div>
    </div>
    <div class="table-responsive">
      <table class="table card-table metrix-table">
        <thead><tr><th style="width:30%">Metric</th>${header}</tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}
