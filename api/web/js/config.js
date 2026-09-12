// Config: profiles and what will be collected from them.
// Editing the mix and targets lands in A4; this inspects.

import { escape } from "./format.js";
import { empty, field, icon } from "./ui.js";

export function render(state) {
  if (state.error) return `<div class="alert alert-danger">${escape(state.error)}</div>`;

  const broken = state.brokenProfiles.length
    ? `<div class="alert alert-warning" role="alert">
         <div class="d-flex">
           <div class="me-3">${icon("alert-triangle")}</div>
           <div>
             <h4 class="alert-title">Profiles that will not parse</h4>
             <div class="text-secondary">Listed rather than dropped: an environment
               silently missing from this page is worse than a visible error.</div>
             <ul class="mb-0 mt-2">${state.brokenProfiles
               .map((p) => `<li><code>${escape(p.name)}</code> — ${escape(p.error)}</li>`)
               .join("")}</ul>
           </div>
         </div>
       </div>`
    : "";

  if (!state.profiles.length) {
    return (
      broken +
      empty({
        icon: "server",
        title: "No profiles yet",
        body: `A profile names the boxes to watch. Add one to
          <code>$METRIX_HOME/profiles/&lt;name&gt;.json</code> — there is a worked
          example at <code>examples/profiles/local.json</code>.`,
      })
    );
  }

  return `<div class="metrix-stack">
    ${storageCard(state.health)}
    ${broken}
    ${state.profiles.map(profileCard).join("")}
  </div>`;
}

// Where this process is reading and writing. Shown rather than buried in a tooltip:
// the root is resolved from four different places (§2 of the implementation plan),
// two of which depend on state outside the process, so "which directory is this?"
// is a question the page should answer without anyone having to guess.
function storageCard(health) {
  const rows = health
    ? `${field("METRIX_HOME", `<code class="metrix-path">${escape(health.home)}</code>`)}
       ${field("Resolved from", escape(health.home_source))}
       ${field("Database", `<code class="metrix-path">${escape(health.database)}</code>`)}`
    : field("METRIX_HOME", `<span class="text-secondary">API unreachable</span>`);

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">Storage</h3>
        <div class="card-subtitle">Everything this process keeps on disk lives
          under one root</div>
      </div>
    </div>
    <div class="card-body"><div class="datagrid">${rows}</div></div>
  </div>`;
}


function profileCard(profile) {
  const rows = profile.endpoints
    .map((endpoint) => {
      const observed = profile.observed.includes(endpoint.id);
      const collection = observed
        ? `<span class="badge bg-green-lt">${escape(endpoint.transport)}</span>`
        : `<span class="text-secondary">not collected</span>`;
      return `<tr>
        <td class="name">${escape(endpoint.id)}</td>
        <td><code>${escape(endpoint.address)}</code></td>
        <td>${endpoint.host_header ? `<code>${escape(endpoint.host_header)}</code>` : "—"}</td>
        <td>${collection}</td>
      </tr>`;
    })
    .join("");

  const description = profile.description
    ? `<div class="card-body py-2 text-secondary border-bottom">
         ${escape(profile.description)}
       </div>`
    : "";

  const action = profile.observed.length
    ? `<button class="btn btn-primary btn-sm" data-action="observe"
               data-profile="${escape(profile.name)}">
         ${icon("player-play")} Start observing
       </button>`
    : `<span class="text-secondary">nothing to collect</span>`;

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">${escape(profile.name)}</h3>
        <div class="card-subtitle">${escape(profile.addressing)} addressing ·
          ${profile.observed.length} of ${profile.endpoints.length} observed</div>
      </div>
      <div class="card-actions">${action}</div>
    </div>
    ${description}
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th style="width:22%">Endpoint</th>
          <th style="width:26%">Address</th>
          <th style="width:28%">Host header</th>
          <th style="width:24%">Collection</th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}
