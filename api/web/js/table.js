// The stats table (design §14). Exact numbers, read off and quoted; no charts here.
//
// A live table is built once and then *patched in place* (§14.3). Replacing the
// markup every second would destroy text selection, and a button rebuilt under the
// cursor cannot reliably be clicked -- both of which make a live view something
// people wait out rather than watch.

import { duration, escape, metricValue, targetLabel } from "./format.js";
import { empty, icon } from "./ui.js";

export function selectState(state) {
  return state.live?.latest ? [state.live] : [state.live, state.selectedRecording];
}

export function render(state) {
  const live = state.live;
  const recording = state.selectedRecording;

  if (live && live.latest) return liveTable(live);
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
      gaps.innerHTML = gapNote(live);
    }
  }

  return true;
}

function cssEscape(value) {
  return String(value).replace(/["\\]/g, "\\$&");
}

function metaText(live) {
  return `${duration(live.elapsedMs)} · ${live.counts?.samples ?? 0} samples`;
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

function gapNote(live) {
  if (!(live.gaps ?? []).length) return "";
  return `<div class="alert alert-warning m-3" role="alert">
    <div class="d-flex">
      <div class="me-3">${icon("alert-triangle")}</div>
      <div>${live.gaps.length} collection gap(s) so far. Drawn as gaps, never
        interpolated.</div>
    </div>
  </div>`;
}

function liveTable(live) {
  const targets = live.targets ?? [];
  const header = targets
    .map((t) => `<th class="num" title="${escape(t)}">${escape(targetLabel(t))}</th>`)
    .join("");

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
      <div>
        <h3 class="card-title">${escape(live.recordingId)}</h3>
        <div class="card-subtitle" data-live-meta>${metaText(live)}</div>
      </div>
      <div class="card-actions d-flex align-items-center gap-2">
        ${connectionStatus(live.connection)}
        <button class="btn btn-sm btn-outline-danger" data-action="stop"
                data-recording="${escape(live.recordingId)}">
          ${icon("player-stop")} Stop
        </button>
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr><th style="width:30%">Metric</th>${header}</tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
    <div data-live-gaps data-count="${(live.gaps ?? []).length}">${gapNote(live)}</div>
  </div>`;
}

function staticTable(recording) {
  const targets = recording.targets;
  const header = targets
    .map((t) => `<th class="num" title="${escape(t)}">${escape(targetLabel(t))}</th>`)
    .join("");
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
      <div>
        <h3 class="card-title">${escape(recording.id)}</h3>
        <div class="card-subtitle">last value per target · ${recording.metrics.length}
          metrics</div>
      </div>
      <div class="card-actions">
        <a class="btn btn-sm" href="#/recordings/${encodeURIComponent(recording.id)}">
          Recording details
        </a>
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr><th style="width:30%">Metric</th>${header}</tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}
