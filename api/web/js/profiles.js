// Profiles: the environments a plan can be pointed at, and the editor for them.
//
// The editor submits a whole profile document rather than a patch, so the server
// runs exactly the validation a hand-written file goes through -- one write path,
// and a form that cannot save something the file loader would reject.
//
// The editor selects only its draft. Typing stays in the live form until an
// intentional editor action reads it back before rebuilding or saving the draft.

import { escape, targetLabel } from "./format.js";
import { empty, icon } from "./ui.js";

const ADDRESSING = ["load_balancer", "direct"];
const TRANSPORTS = ["none", "ssh", "scrape"];
const GROUPS = ["cpu", "memory", "disk", "net", "process"];

/** A profile with nothing filled in, and one endpoint: a profile needs at least one. */
export function blankDocument() {
  return {
    name: "",
    description: "",
    addressing: "load_balancer",
    observe: { interval: "1s", collect: [...GROUPS] },
    endpoints: [blankEndpoint()],
  };
}

export function blankEndpoint() {
  return { id: "", address: "", host_header: "", collect: { transport: "none" } };
}

export function selectState(state) {
  return state.profileDraft
    ? [state.profileDraft]
    : [state.profileDraft, state.profiles, state.brokenProfiles, state.profilesReadAt, state.resolving];
}

export function render(state) {
  if (state.profileDraft) return editor(state.profileDraft);

  const broken = state.brokenProfiles.length
    ? `<div class="alert alert-warning" role="alert">
         <div class="d-flex">
           <div class="me-3">${icon("alert-triangle")}</div>
           <div>
             <h4 class="alert-title">Profiles that will not parse</h4>
             <div class="text-secondary">Listed rather than dropped: an environment
               silently missing from this page is worse than a visible error. Fix the
               file by hand — the editor cannot open what it cannot read.</div>
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
        body: `A profile names the machines in one environment. Plans reference it by
          name, so the same mixture runs against staging or production with no edit.`,
        action: `<div class="btn-list justify-content-center">
                   <button class="btn btn-primary" data-action="profile-new">
                     ${icon("plus")} New profile
                   </button>
                   ${reloadButton(state.profilesReadAt)}
                 </div>`,
      })
    );
  }

  return `<div class="metrix-stack">
    ${broken}
    <div class="metrix-toolbar">
      ${LEGEND}
      <div class="btn-list">
        ${reloadButton(state.profilesReadAt)}
        <button class="btn btn-primary" data-action="profile-new">
          ${icon("plus")} New profile
        </button>
      </div>
    </div>
    ${state.profiles.map((p) => profileCard(p, state.resolving)).join("")}
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

/**
 * Re-read the profile directory.
 *
 * Profiles are files, and the file is the primary form -- someone editing one in a
 * text editor or pulling a change from version control has no reason to reload the
 * whole page to see it. The list is deliberately not polled: replacing what a person
 * is reading, unasked, would be worse than a button, and nothing here changes on its
 * own.
 *
 * The timestamp is the point of the label. Without it, a reload that found no change
 * looks exactly like a button that does nothing.
 */
function reloadButton(readAt) {
  const when = readAt
    ? `<span class="text-secondary ms-2">read ${escape(
        new Date(readAt).toLocaleTimeString()
      )}</span>`
    : "";
  return `<button class="btn" data-action="profile-reload"
                  title="Re-read the profile files from disk">
    ${icon("refresh")} Reload ${when}
  </button>`;
}

/**
 * A profile whose endpoints came from discovery, rather than from the file.
 *
 * Three things a person needs here that a table of addresses does not give them:
 * what was asked for, when the answer was last confirmed, and how far the walk got.
 * The last one matters because partial resolution is the normal case -- an inventory
 * that stopped at the target group is a good answer to a shorter question, and it
 * should not look like a broken one.
 */
function discovery(profile, resolving) {
  const found = profile.inventory;
  const source = `<code>${escape(profile.discover.source)}</code>`;

  if (!found) {
    return `<div class="card-body border-bottom">
      <div class="d-flex align-items-center gap-3">
        <div class="text-secondary">
          Endpoints are discovered from ${source}. Nothing has been resolved yet —
          this profile does not know what it points at until it walks.
        </div>
        ${resolveButton(profile.name, "Resolve now", resolving)}
      </div>
    </div>`;
  }

  const notes = found.notes.length
    ? `<ul class="mb-0 mt-2 text-secondary">${found.notes
        .map((n) => `<li><span class="badge bg-secondary-lt">${escape(n.hop)}</span>
                     ${escape(n.message)}</li>`)
        .join("")}</ul>`
    : "";

  return `<div class="card-body border-bottom">
    <div class="d-flex align-items-start gap-3">
      <div class="flex-fill">
        <div>Discovered from ${source} —
          reached <strong>${escape(found.reached)}</strong>,
          confirmed ${escape(when(found.confirmed_at))}${
            found.discovered_at !== found.confirmed_at
              ? `, unchanged since ${escape(when(found.discovered_at))}`
              : ""
          }.
        </div>
        ${notes}
      </div>
      ${resolveButton(profile.name, "Resolve", resolving)}
    </div>
  </div>`;
}

function resolveButton(name, label, resolving) {
  if (resolving === name) {
    return `<button class="btn btn-sm" disabled>
      <span class="spinner-border spinner-border-sm me-2" role="status"></span>Walking…
    </button>`;
  }
  return `<button class="btn btn-sm" data-action="profile-resolve"
                  data-profile="${escape(name)}"
                  title="Walk discovery again and store what it finds">
    ${icon("refresh")} ${escape(label)}
  </button>`;
}

/** A stored timestamp as a local time. The date only when it was not today. */
function when(stamp) {
  const at = new Date(stamp);
  if (Number.isNaN(at.getTime())) return stamp;
  const today = at.toDateString() === new Date().toDateString();
  return today ? at.toLocaleTimeString() : at.toLocaleString();
}

function profileCard(profile, resolving) {
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
        <td class="name" title="${escape(endpoint.id)}">${escape(targetLabel(endpoint.id))}</td>
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

  const name = escape(profile.name);
  const source = profile.discover ? " · discovered" : "";
  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">${name}</h3>
        <div class="card-subtitle">${escape(profile.addressing)} addressing ·
          ${profile.observed.length} of ${profile.endpoints.length} observed${source}</div>
      </div>
      <div class="card-actions d-flex align-items-center gap-2">
        ${
          profile.observed.length
            ? `<button class="btn btn-primary btn-sm" data-action="observe"
                       data-profile="${name}">
                 ${icon("player-play")} Start observing
               </button>`
            : `<span class="text-secondary">nothing to collect</span>`
        }
        ${
          profile.discover
            ? // The form edits endpoint rows, and a discovered profile has none of
              // its own. Until it grows a discovery section, offering it would open
              // an editor that could only save the profile by emptying it.
              `<span class="text-secondary" title="Edit the file to change what is discovered">
                 file only
               </span>`
            : `<button class="btn btn-sm" data-action="profile-edit" data-profile="${name}">
                 ${icon("pencil")} Edit
               </button>`
        }
        <button class="btn btn-sm btn-outline-danger" data-action="profile-delete"
                data-profile="${name}">
          ${icon("trash")} Delete
        </button>
      </div>
    </div>
    ${description}
    ${profile.discover ? discovery(profile, resolving) : ""}
    ${profile.endpoints.length === 0 ? "" : `<div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th style="width:14%">Endpoint</th>
          <th style="width:17%">Load target</th>
          <th style="width:19%">Host header</th>
          <th style="width:50%">Observed via</th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>`}
  </div>`;
}

/* ----------------------------------------------------------------- the editor */

function editor(draft) {
  const doc = draft.doc;
  const creating = draft.mode === "create";

  const error = draft.error
    ? `<div class="alert alert-danger" role="alert">
         <div class="d-flex">
           <div class="me-3">${icon("alert-triangle")}</div>
           <div>
             <h4 class="alert-title">Not saved</h4>
             <div>${escape(draft.error)}</div>
           </div>
         </div>
       </div>`
    : "";

  return `<form class="metrix-stack" data-profile-form novalidate>
    ${error}
    <div class="card">
      <div class="card-header">
        <div>
          <h3 class="card-title">${creating ? "New profile" : escape(doc.name)}</h3>
          <div class="card-subtitle">${
            creating
              ? "One environment: what to send traffic to, and what to watch."
              : "The name is fixed — it is part of a recording's series identity."
          }</div>
        </div>
        <div class="card-actions d-flex align-items-center gap-2">
          <button type="button" class="btn btn-sm" data-action="profile-cancel">
            Cancel
          </button>
          <button type="button" class="btn btn-sm btn-primary" data-action="profile-save">
            ${icon("device-floppy")} ${creating ? "Create profile" : "Save changes"}
          </button>
        </div>
      </div>
      <div class="card-body">
        <div class="metrix-fields">
          ${text("name", "Name", doc.name, {
            required: true,
            readonly: !creating,
            hint: "Lowercase letters, digits and hyphens. Used as the filename.",
          })}
          ${select("addressing", "Addressing", ADDRESSING, doc.addressing, {
            hint: "Part of series identity; the two measure different network paths.",
          })}
          ${text("observe.interval", "Sample interval", doc.observe?.interval ?? "1s", {
            hint: 'A duration with units, like "1s" or "5s".',
          })}
          ${text("description", "Description", doc.description, {
            wide: true,
            hint: "Optional. Shown on this page and nowhere else.",
          })}
        </div>
        <div class="mt-3">
          <div class="form-label">Collect</div>
          <div class="d-flex flex-wrap gap-3">
            ${GROUPS.map((group) => checkbox(group, doc.observe?.collect ?? [])).join("")}
          </div>
        </div>
      </div>
    </div>

    <div class="metrix-toolbar">
      <p class="metrix-note text-secondary mb-0">
        One entry per machine. <strong>Load target</strong> is where requests go;
        <strong>observed via</strong> is a separate connection for host statistics,
        and the host defaults to the load target's host when left blank.
      </p>
      <button type="button" class="btn" data-action="endpoint-add">
        ${icon("plus")} Add endpoint
      </button>
    </div>

    ${doc.endpoints.map(endpointCard).join("")}
  </form>`;
}

function endpointCard(endpoint, index, all) {
  const collect = endpoint.collect ?? { transport: "none" };
  // A profile needs at least one endpoint, so the last one cannot be removed.
  // Offering a button that only produces a validation error is worse than not
  // offering it.
  const removable = all.length > 1;
  const p = (field) => `endpoints.${index}.${field}`;

  // Only the fields the chosen transport actually uses. A port box beside
  // "not collected" invites someone to fill it in and wonder why nothing happens.
  const transportFields =
    collect.transport === "none"
      ? ""
      : `${text(p("collect.host"), "Collect from host", collect.host, {
          hint: "Blank means the load target's host.",
        })}
         ${text(p("collect.port"), "Port", collect.port, {
           hint: collect.transport === "ssh" ? "Blank means 22." : "Blank means 9100.",
         })}
         ${
           collect.transport === "ssh"
             ? text(p("collect.user"), "SSH user", collect.user, {
                 hint: "Blank means whatever your SSH config resolves.",
               })
             : text(p("collect.path"), "Metrics path", collect.path ?? "/metrics", {
                 hint: "Blank means /metrics.",
               })
         }`;

  return `<div class="card">
    <div class="card-header">
      <h3 class="card-title">Endpoint ${index + 1}</h3>
      <div class="card-actions">
        ${
          removable
            ? `<button type="button" class="btn btn-sm btn-outline-danger"
                       data-action="endpoint-remove" data-index="${index}">
                 ${icon("trash")} Remove
               </button>`
            : `<span class="text-secondary">a profile needs at least one</span>`
        }
      </div>
    </div>
    <div class="card-body">
      <div class="metrix-fields">
        ${text(p("id"), "Endpoint id", endpoint.id, {
          required: true,
          hint: "How this machine is named in every chart and table.",
        })}
        ${text(p("address"), "Load target", endpoint.address, {
          required: true,
          hint: "host:port — where requests are sent.",
        })}
        ${text(p("host_header"), "Host header", endpoint.host_header, {
          hint: "Required when addressing a container directly.",
        })}
        ${select(p("collect.transport"), "Observed via", TRANSPORTS, collect.transport, {
          reload: true,
          hint: "none means this endpoint takes load but reports no host statistics.",
        })}
        ${transportFields}
      </div>
    </div>
  </div>`;
}

/* ------------------------------------------------------------------ controls */

function text(name, label, value, { required, readonly, hint, wide } = {}) {
  return `<div class="metrix-field${wide ? " metrix-field-wide" : ""}">
    <label class="form-label" for="f-${escape(name)}">
      ${escape(label)}${required ? ' <span class="text-danger">*</span>' : ""}
    </label>
    <input class="form-control" id="f-${escape(name)}" name="${escape(name)}"
           value="${escape(value ?? "")}"${readonly ? " readonly" : ""}>
    ${hint ? `<div class="form-hint">${escape(hint)}</div>` : ""}
  </div>`;
}

function select(name, label, options, value, { hint, reload } = {}) {
  const items = options
    .map(
      (option) =>
        `<option value="${escape(option)}"${option === value ? " selected" : ""}>
           ${escape(option)}
         </option>`
    )
    .join("");
  // `reload` marks a control whose value changes which other fields exist, so the
  // form is read back and re-rendered on change. It is a change handler, not a click
  // one: preventDefault on a click would stop the dropdown from opening at all.
  return `<div class="metrix-field">
    <label class="form-label" for="f-${escape(name)}">${escape(label)}</label>
    <select class="form-select" id="f-${escape(name)}" name="${escape(name)}"
            ${reload ? 'data-change-action="draft-reload"' : ""}>${items}</select>
    ${hint ? `<div class="form-hint">${escape(hint)}</div>` : ""}
  </div>`;
}

function checkbox(group, selected) {
  return `<label class="form-check form-check-inline">
    <input class="form-check-input" type="checkbox" name="observe.collect"
           value="${escape(group)}"${selected.includes(group) ? " checked" : ""}>
    <span class="form-check-label">${escape(group)}</span>
  </label>`;
}

/* ------------------------------------------------------------- form to document */

/**
 * Read the live form back into a profile document.
 *
 * Called before every change that re-renders, so nothing typed is lost when a row
 * is added or a transport switched. Empty strings are dropped rather than sent:
 * the server treats a missing key as "use the default" and an empty one as a value,
 * and a blank port is the former.
 */
export function readForm(form, previous) {
  const data = new FormData(form);
  const value = (name) => (data.get(name) ?? "").toString().trim();

  const doc = {
    name: value("name"),
    addressing: value("addressing"),
    endpoints: [],
  };
  if (value("description")) doc.description = value("description");

  const observe = {};
  if (value("observe.interval")) observe.interval = value("observe.interval");
  const collect = data.getAll("observe.collect").map(String);
  if (collect.length) observe.collect = collect;
  if (Object.keys(observe).length) doc.observe = observe;

  for (let i = 0; i < previous.endpoints.length; i += 1) {
    const at = (field) => value(`endpoints.${i}.${field}`);
    const endpoint = { id: at("id"), address: at("address") };
    if (at("host_header")) endpoint.host_header = at("host_header");

    const transport = at("collect.transport") || "none";
    const collection = { transport };
    if (transport !== "none") {
      if (at("collect.host")) collection.host = at("collect.host");
      // A port is a number to the server; "" would be a type error, not a default.
      if (at("collect.port")) collection.port = Number(at("collect.port"));
      if (transport === "ssh" && at("collect.user")) collection.user = at("collect.user");
      if (transport === "scrape" && at("collect.path")) collection.path = at("collect.path");
    }
    endpoint.collect = collection;
    doc.endpoints.push(endpoint);
  }

  return doc;
}
