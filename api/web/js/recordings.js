// The archive. Observation-only recordings and load runs are the same object with
// different sections populated, so one list shows both.

import { duration, escape, timestamp } from "./format.js";

export function render(state) {
  if (state.selectedRecording) return detail(state.selectedRecording);

  if (!state.recordings.length) {
    return `<div class="card"><div class="empty-note">
      No recordings yet. An observation-only recording needs no engine — it is the
      whole product until one exists.
    </div></div>`;
  }

  const rows = state.recordings
    .map(
      (r) => `<tr>
        <td class="name">
          <a href="#/recordings/${encodeURIComponent(r.id)}">${escape(r.id)}</a>
        </td>
        <td>${escape(r.kind)}</td>
        <td>${statusBadge(r.status)}</td>
        <td>${escape(r.profile ?? "—")}</td>
        <td class="num">${duration(r.duration_ms)}</td>
        <td class="num">${r.targets.length}</td>
        <td>${timestamp(r.started_at)}</td>
      </tr>`
    )
    .join("");

  return `<div class="card">
    <div class="table-responsive">
      <table class="table card-table metrix-table">
        <thead><tr>
          <th style="width:24%">Recording</th>
          <th style="width:11%">Kind</th>
          <th style="width:11%">Status</th>
          <th style="width:14%">Profile</th>
          <th class="num" style="width:10%">Length</th>
          <th class="num" style="width:8%">Targets</th>
          <th style="width:22%">Started</th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}

function statusBadge(status) {
  const tone = { finished: "green", running: "blue", aborted: "orange", failed: "red" }[status];
  return `<span class="badge bg-${tone ?? "secondary"}-lt">${escape(status)}</span>`;
}

function detail(recording) {
  const annotations = recording.annotations.length
    ? recording.annotations
        .map(
          (a) => `<li class="severity-${escape(a.severity)}">
            <strong>${escape(a.code)}</strong> — ${escape(a.message)}
          </li>`
        )
        .join("")
    : `<li class="text-secondary">None.</li>`;

  const gaps = recording.gaps.length
    ? `<ul class="gap-note">${recording.gaps
        .map(
          (g) =>
            `<li>${escape(g.target_id)}: ${g.from_ms}–${g.to_ms}ms — ${escape(g.reason)}</li>`
        )
        .join("")}</ul>`
    : `<p class="text-secondary mb-0">None. Every interval was collected.</p>`;

  return `<div class="mb-3">
    <a href="#/recordings" class="btn btn-sm">&larr; All recordings</a>
  </div>
  <div class="row row-cards">
    <div class="col-6">
      <div class="card">
        <div class="card-header"><h3 class="card-title">Recording</h3></div>
        <div class="card-body">
          <dl class="row mb-0">
            <dt class="col-4">Status</dt><dd class="col-8">${escape(recording.status)}</dd>
            <dt class="col-4">Profile</dt>
            <dd class="col-8">${escape(recording.profile ?? "—")}</dd>
            <dt class="col-4">Addressing</dt>
            <dd class="col-8">${escape(recording.addressing_mode)}</dd>
            <dt class="col-4">Length</dt>
            <dd class="col-8">${duration(recording.duration_ms)}</dd>
            <dt class="col-4">Targets</dt>
            <dd class="col-8">${recording.targets.map(escape).join(", ") || "—"}</dd>
            <dt class="col-4">Metrics</dt><dd class="col-8">${recording.metrics.length}</dd>
            <dt class="col-4">Series</dt>
            <dd class="col-8"><code>${escape(recording.series_key)}</code></dd>
          </dl>
        </div>
      </div>
    </div>
    <div class="col-6">
      <div class="card">
        <div class="card-header"><h3 class="card-title">Notes</h3></div>
        <div class="card-body">
          <ul class="mb-3">${annotations}</ul>
          <h4 class="mb-1">Collection gaps</h4>
          ${gaps}
        </div>
      </div>
    </div>
  </div>`;
}
