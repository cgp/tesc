// The charts page (design §15). Deliberately separate from the table: the two are
// read at different moments, and putting both on one page makes each worse.
//
// Plotting lands in A3.2. Until then this says what it will draw, so the page is
// honest about being unfinished rather than looking broken.

import { escape } from "./format.js";

export function render(state) {
  const recording = state.selectedRecording;

  const context = recording
    ? `<p class="text-secondary">Recording <code>${escape(recording.id)}</code> —
       ${recording.metrics.length} metrics across ${recording.targets.length} target(s).</p>`
    : `<p class="text-secondary">Open a recording from
       <a href="#/recordings">Recordings</a>.</p>`;

  return `<div class="card">
    <div class="card-header"><h3 class="card-title">Charts</h3></div>
    <div class="card-body">
      ${context}
      <p class="mb-0">Not drawn yet (A3.2). This page will carry no tables — exact
      figures live on Stats — and every chart will have a one-line caption saying what
      it answers, phase bands drawn behind it, and collection gaps drawn as gaps
      rather than interpolated.</p>
    </div>
  </div>`;
}
