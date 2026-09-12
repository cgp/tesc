// Config: profiles and what will be collected from them.
// Editing the mix and targets lands in A4; this inspects.

import { escape } from "./format.js";

export function render(state) {
  if (state.error) return `<div class="alert alert-danger">${escape(state.error)}</div>`;

  const broken = state.brokenProfiles.length
    ? `<div class="alert alert-warning">
         <strong>Profiles that will not parse.</strong> Listed rather than dropped:
         an environment silently missing from the list is worse than a visible error.
         <ul class="mb-0 mt-2">${state.brokenProfiles
           .map((p) => `<li><code>${escape(p.name)}</code> — ${escape(p.error)}</li>`)
           .join("")}</ul>
       </div>`
    : "";

  if (!state.profiles.length) {
    return `${broken}<div class="card"><div class="empty-note">
      No profiles yet. Add one to <code>$METRIX_HOME/profiles/&lt;name&gt;.json</code>;
      there is a worked example at <code>examples/profiles/local.json</code>.
    </div></div>`;
  }

  return broken + state.profiles.map(profileCard).join("");
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
    ? `<div class="card-body py-2 text-secondary">${escape(profile.description)}</div>`
    : "";

  return `<div class="card mb-3">
    <div class="card-header">
      <h3 class="card-title">${escape(profile.name)}</h3>
      <div class="card-actions text-secondary">
        ${escape(profile.addressing)} · ${profile.observed.length} of
        ${profile.endpoints.length} observed
      </div>
    </div>
    ${description}
    <div class="table-responsive">
      <table class="table card-table metrix-table">
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
