// The archive. Observation-only recordings and load runs are the same object with
// different sections populated, so one list shows both.

import { duration, escape, timestamp } from "./format.js";
import { empty, field, icon } from "./ui.js";

export function render(state) {
  if (state.selectedRecording) return detail(state.selectedRecording);

  if (!state.recordings.length) {
    return empty({
      icon: "archive",
      title: "No recordings yet",
      body: `An observation-only recording needs no engine — it is the whole product
        until one exists.`,
      action: `<a href="#/config" class="btn btn-primary">
                 ${icon("player-play")} Start observing
               </a>`,
    });
  }

  const rows = state.recordings
    .map(
      (r) => `<tr>
        <td class="name">
          <a href="#/recordings/${encodeURIComponent(r.id)}">${escape(r.id)}</a>
        </td>
        <td class="text-secondary">${escape(r.kind)}</td>
        <td>${statusBadge(r.status)}</td>
        <td>${escape(r.profile ?? "—")}</td>
        <td class="num">${duration(r.duration_ms)}</td>
        <td class="num">${r.targets.length}</td>
        <td class="text-secondary">${timestamp(r.started_at)}</td>
      </tr>`
    )
    .join("");

  return `<div class="card">
    <div class="card-header">
      <h3 class="card-title">Recordings
        <span class="card-subtitle">${state.recordings.length} captured</span>
      </h3>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th style="width:22%">Recording</th>
          <th style="width:12%">Kind</th>
          <th style="width:10%">Status</th>
          <th style="width:14%">Profile</th>
          <th class="num" style="width:9%">Length</th>
          <th class="num" style="width:8%">Targets</th>
          <th style="width:25%">Started</th>
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
    ? `<div class="list-group list-group-flush">${recording.annotations
        .map(
          (a) => `<div class="list-group-item px-0 severity-${escape(a.severity)}">
            <strong>${escape(a.code)}</strong> — ${escape(a.message)}
          </div>`
        )
        .join("")}</div>`
    : `<p class="text-secondary mb-0">None.</p>`;

  const gaps = recording.gaps.length
    ? `<ul class="gap-note mb-0">${recording.gaps
        .map(
          (g) =>
            `<li>${escape(g.target_id)}: ${g.from_ms}–${g.to_ms}ms — ${escape(g.reason)}</li>`
        )
        .join("")}</ul>`
    : `<p class="text-secondary mb-0">None. Every interval was collected.</p>`;

  return `<div class="mb-3">
    <a href="#/recordings" class="btn btn-sm">${icon("arrow-left")} All recordings</a>
  </div>
  <div class="metrix-stack">
    <div class="card">
      <div class="card-header">
        <h3 class="card-title">${escape(recording.id)}</h3>
        <div class="card-actions">${statusBadge(recording.status)}</div>
      </div>
      <div class="card-body">
        <div class="datagrid">
          ${field("Profile", escape(recording.profile ?? "—"))}
          ${field("Addressing", escape(recording.addressing_mode))}
          ${field("Length", duration(recording.duration_ms))}
          ${field("Targets", recording.targets.map(escape).join(", ") || "—")}
          ${field("Metrics", String(recording.metrics.length))}
          ${field("Series", `<code>${escape(recording.series_key)}</code>`)}
        </div>
      </div>
    </div>
    <div class="metrix-split">
      <div class="card">
        <div class="card-header"><h3 class="card-title">Notes</h3></div>
        <div class="card-body">${annotations}</div>
      </div>
      <div class="card">
        <div class="card-header">
          <h3 class="card-title">Collection gaps
            <span class="card-subtitle">drawn as gaps, never interpolated</span>
          </h3>
        </div>
        <div class="card-body">${gaps}</div>
      </div>
    </div>
  </div>`;
}
