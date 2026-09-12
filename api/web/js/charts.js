// The charts page (design §15). Deliberately separate from the table: the two are
// read at different moments, and putting both on one page makes each worse.
//
// Plotting lands in A3.2. Until then this says what it will draw, so the page is
// honest about being unfinished rather than looking broken.

import { escape } from "./format.js";
import { icon } from "./ui.js";

export function render(state) {
  const recording = state.selectedRecording;

  const context = recording
    ? `Recording <code>${escape(recording.id)}</code> — ${recording.metrics.length}
       metrics across ${recording.targets.length} target(s), ready to plot.`
    : `Open a recording from <a href="#/recordings">Recordings</a> to choose what to
       plot.`;

  return `<div class="card">
    <div class="empty">
      <div class="empty-icon">${icon("chart-line")}</div>
      <p class="empty-title">Nothing drawn yet</p>
      <p class="empty-subtitle text-secondary">${context}</p>
    </div>
    <div class="card-footer text-secondary">
      Charts land in A3.2. This page will carry no tables — exact figures live on
      <a href="#/performance/stats">Stats</a> — and every chart will have a one-line
      caption saying what it answers, phase bands drawn behind it, and collection gaps
      drawn as gaps rather than interpolated.
    </div>
  </div>`;
}
