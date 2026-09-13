// Config: what this process is, and where it keeps things.
// Profiles moved to their own page (profiles.js) once they became editable.

import { escape } from "./format.js";
import { empty, field } from "./ui.js";

export function selectState(state) {
  return [state.health];
}

export function render(state) {
  if (!state.health) {
    return empty({
      icon: "alert-triangle",
      title: "The API is not answering",
      body: `Nothing here is readable until it is. The indicator at the foot of the
        menu says the same thing, and keeps checking.`,
    });
  }
  return `<div class="metrix-stack">${storageCard(state.health)}</div>`;
}

// Where this process is reading and writing. Shown rather than buried in a tooltip:
// the root is resolved from four different places (§2 of the implementation plan),
// two of which depend on state outside the process, so "which directory is this?"
// is a question the page should answer without anyone having to guess.
function storageCard(health) {
  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">Storage</h3>
        <div class="card-subtitle">Everything this process keeps on disk lives
          under one root</div>
      </div>
    </div>
    <div class="card-body">
      <div class="datagrid">
        ${field("METRIX_HOME", `<code class="metrix-path">${escape(health.home)}</code>`)}
        ${field("Resolved from", escape(health.home_source))}
        ${field("Database", `<code class="metrix-path">${escape(health.database)}</code>`)}
        ${field("Version", escape(health.version))}
      </div>
    </div>
    <div class="card-footer text-secondary">
      Profiles are edited under <a href="#/profiles">Profiles</a> and stored as JSON
      under <code class="metrix-path">${escape(health.home)}</code>. The files and the
      editor are the same thing; either may be used.
    </div>
  </div>`;
}
