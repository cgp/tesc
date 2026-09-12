// The stats table (design §14). Exact numbers, read off and quoted; no charts on
// this page. A1.7 shows the last value per metric per target, and the live 1s
// update arrives with the stream in A1.8.

import { escape, metricValue } from "./format.js";

export function render(state) {
  const recording = state.selectedRecording;

  if (!recording) {
    return `<div class="card"><div class="empty-note">
      Open a recording from <a href="#/recordings">Recordings</a> to see its numbers.
    </div></div>`;
  }
  if (!recording.metrics.length) {
    return `<div class="card"><div class="empty-note">
      This recording holds no samples.
    </div></div>`;
  }

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
