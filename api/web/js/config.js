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
    ${LEGEND}
    ${state.profiles.map(profileCard).join("")}
  </div>`;
}

// The one thing about a profile that is not guessable, stated where the columns it
// describes are read. An endpoint carries two addresses for two different purposes,
// and reading either as the other is the mistake this prevents.
const LEGEND = `<p class="metrix-note text-secondary">
  <strong>Load target</strong> is the socket requests are sent to.
  <strong>Observed via</strong> is a separate connection for host statistics — an SSH
  login, or an exporter to scrape. Nothing is inferred from the load target: the tool
  does not probe that port or attach to whatever process is listening on it, and the
  statistics it collects are for the whole machine, not for one process.
</p>`;

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
      // The address the collector will actually use, not a restatement of the
      // profile: it is built from the transport itself (routes/profiles.py).
      const collection = observed
        ? `<span class="badge bg-green-lt">${escape(endpoint.transport)}</span>
           <code class="metrix-path ms-1">${escape(endpoint.collects_from ?? "—")}</code>`
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
          <th style="width:14%">Endpoint</th>
          <th style="width:17%">Load target</th>
          <th style="width:19%">Host header</th>
          <th style="width:50%">Observed via</th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}
