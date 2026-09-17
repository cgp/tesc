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

const ADDRESSING = ["ip", "alb", "elb", "ecs", "fargate"];
const GROUPS = ["cpu", "memory", "disk", "net", "process"];

/** A profile with nothing filled in, and one endpoint: a profile needs at least one. */
export function blankDocument() {
  return {
    name: "",
    description: "",
    observe: { interval: "1s", collect: [...GROUPS] },
    endpoints: [blankEndpoint()],
  };
}

export function blankEndpoint() {
  return {
    id: "",
    addressing: "ip",
    address: "",
    host_header: "",
    collect: { transport: "ssh" },
  };
}

export function selectState(state) {
  return state.profileDraft
    ? [state.profileDraft]
    : [
        state.profileDraft,
        state.profiles,
        state.brokenProfiles,
        state.profilesReadAt,
        state.resolving,
        state.verifying,
        state.verified,
      ];
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
    ${state.profiles.map((p) => profileCard(p, state)).join("")}
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

/**
 * What a reachability check found, per endpoint.
 *
 * Two answers per box, side by side, because they fail for different reasons and are
 * fixed by different people: a load target that will not accept a connection is a
 * routing or firewall question, and a collector that will not answer is a key, an
 * agent, or a path. A check that was never attempted is drawn as neither — an
 * endpoint with no collector is not broken.
 */
function verification(profile, state) {
  const report = state.verified[profile.name];
  if (state.verifying === profile.name) {
    return `<div class="card-body py-2 border-bottom text-secondary">
      <span class="spinner-border spinner-border-sm me-2" role="status"></span>
      Checking every endpoint — connecting to each load target and probing each
      collector, in parallel.
    </div>`;
  }
  if (!report) return "";

  const rows = profile.endpoints
    .map((endpoint) => {
      const cells = ["load", "collect"]
        .map((kind) => {
          const check = report.checks.find(
            (c) => c.endpoint === endpoint.id && c.kind === kind
          );
          return `<td>${outcome(check)}</td>`;
        })
        .join("");
      return `<tr><td class="name" title="${escape(endpoint.id)}">${escape(
        targetLabel(endpoint.id)
      )}</td>${cells}</tr>`;
    })
    .join("");

  return `<div class="card-body py-0 border-bottom">
    <div class="d-flex align-items-baseline gap-2 pt-2">
      <strong>Reachability</strong>
      <span class="badge ${report.ok ? "bg-green-lt" : "bg-red-lt"}">${escape(
        report.summary
      )}</span>
      <span class="text-secondary">checked ${escape(when(report.checked_at))} — a
        moment ago, not a standing fact</span>
    </div>
    <div class="table-responsive">
      <table class="table table-sm table-vcenter metrix-table mb-2">
        <thead><tr>
          <th style="width:20%">Endpoint</th>
          <th style="width:40%">Load target</th>
          <th style="width:40%">Collector</th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}

function verifyButton(profile, state) {
  if (!profile.endpoints.length) return "";
  if (state.verifying === profile.name) {
    return `<button class="btn btn-sm" disabled>
      <span class="spinner-border spinner-border-sm me-2" role="status"></span>Checking…
    </button>`;
  }
  return `<button class="btn btn-sm" data-action="profile-verify"
                  data-profile="${escape(profile.name)}"
                  title="Connect to every load target and probe every collector">
    ${icon("plug-connected")} Verify
  </button>`;
}

function outcome(check) {
  if (!check) return '<span class="text-secondary">—</span>';
  if (check.result === "skipped") {
    return `<span class="text-secondary">${escape(check.detail)}</span>`;
  }
  const ok = check.result === "ok";
  const took = check.ms == null ? "" : ` <span class="text-secondary">${check.ms}ms</span>`;
  return `<span class="badge ${ok ? "bg-green-lt" : "bg-red-lt"}">${
    ok ? "reachable" : "unreachable"
  }</span> ${escape(check.detail)}${took}`;
}

function profileCard(profile, state) {
  const resolving = state.resolving;
  const rows = profile.endpoints
    .map((endpoint) => {
      const observed = profile.observed.includes(endpoint.id);
      // The address the collector will actually use, not a restatement of the
      // profile: it is built from the transport itself (routes/profiles.py).
      const collection = observed
        ? `<span class="badge bg-green-lt">${escape(endpoint.transport)}</span>
           <code class="metrix-path ms-1">${escape(endpoint.collects_from ?? "—")}</code>`
        : `<span class="text-secondary">not collected</span>`;
      // An observation-only box still has an address -- that is where it *is* --
      // but nothing is sent there, and a column headed "Load target" would say
      // otherwise. The address is kept, muted, with the reason on it.
      const target = endpoint.load
        ? `<code>${escape(endpoint.address)}</code>`
        : `<span class="text-secondary" title="Traffic is not sent here">
             <code class="text-secondary">${escape(endpoint.address)}</code> · watched only
           </span>`;
      return `<tr>
        <td class="name" title="${escape(endpoint.id)}">${escape(targetLabel(endpoint.id))}</td>
      <td>${addressingBadge(endpoint.addressing ?? "ip")}</td>
      <td>${target}</td>
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
          ${profile.targets.length} sent to ·
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
        ${verifyButton(profile, state)}
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
    ${verification(profile, state)}
    ${profile.endpoints.length === 0 ? "" : `<div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th style="width:14%">Endpoint</th>
          <th style="width:12%">Addressing</th>
          <th style="width:17%">Load target</th>
          <th style="width:19%">Host header</th>
          <th style="width:38%">Observed via</th>
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

  return `<form class="metrix-stack metrix-profile-editor" data-profile-form novalidate>
    ${error}
    <div class="card">
      <div class="card-header">
        <div class="metrix-profile-title">
          <label class="visually-hidden" for="f-name">Profile name</label>
          <input class="form-control form-control-lg" id="f-name" name="name"
                 value="${escape(doc.name ?? "")}" placeholder="profile-name"
                 aria-label="Profile name" required>
          <div class="card-subtitle mt-1">${
            creating
              ? "One environment: what to send traffic to, and what to watch."
              : "Renaming starts a new series name; existing recordings keep the old one."
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
    </div>

    <div class="metrix-profile-workspace">
      <div class="card">
        <div class="card-header">
          <div>
            <h3 class="card-title">Endpoints</h3>
            <div class="card-subtitle">Traffic destination and SSH observation per row.</div>
          </div>
          <div class="card-actions">
            <button type="button" class="btn btn-sm btn-primary" data-action="endpoint-add">
              ${icon("plus")} Add endpoint
            </button>
          </div>
        </div>
        <div class="table-responsive">
          <table class="table card-table table-vcenter metrix-table metrix-endpoint-table">
            <thead><tr>
              <th style="width:14%">Endpoint</th>
              <th style="width:12%">Addressing</th>
              <th style="width:18%">Address</th>
              <th style="width:18%">Host header</th>
              <th style="width:25%">SSH target</th>
              <th style="width:13%" class="text-end">Actions</th>
            </tr></thead>
            <tbody>${doc.endpoints
              .map((endpoint, index, all) =>
                endpointRow(
                  endpoint,
                  index,
                  all,
                  draft.endpointEdit,
                  draft.endpointResolution?.[index]
                )
              )
              .join("")}</tbody>
          </table>
        </div>
      </div>

      <div class="card metrix-profile-settings">
        <div class="card-header"><h3 class="card-title">Observation</h3></div>
        <div class="card-body">
          ${text("description", "Description", doc.description, {
            hint: "Optional context for this environment.",
          })}
          <div class="mt-3">
            ${text("observe.interval", "Sample interval", doc.observe?.interval ?? "1s", {
              hint: 'A duration with units, like "1s" or "5s".',
            })}
          </div>
          <div class="mt-3">
            ${text("observe.ssh_user", "SSH username", doc.observe?.ssh_user, {
              hint: "Applied to every SSH endpoint during observation.",
            })}
          </div>
          <div class="mt-3">
            <div class="form-label">What is collected</div>
            <div class="d-flex flex-column gap-2">
              ${GROUPS.map((group) => checkbox(group, doc.observe?.collect ?? [])).join("")}
            </div>
          </div>
        </div>
      </div>
    </div>
  </form>`;
}

function endpointRow(endpoint, index, all, editing, resolution) {
  const collect = endpoint.collect ?? { transport: "none" };
  const removable = all.length > 1;
  const p = (field) => `endpoints.${index}.${field}`;
  const active = editing === index;
  const mode = endpoint.addressing ?? "ip";

  if (!active) {
    return `<tr>
      <td class="name">${escape(endpoint.id || `Endpoint ${index + 1}`)}</td>
      <td>${addressingBadge(mode)}</td>
      <td><code>${escape(endpoint.address || "—")}</code></td>
      <td>${endpoint.host_header ? `<code>${escape(endpoint.host_header)}</code>` : "—"}</td>
      <td>${mode === "alb" ? albSshTarget(collect, p) : sshTarget(collect, p, endpoint.address, false)}</td>
      <td class="text-end">
        ${endpointHiddenFields(endpoint, index)}
        ${resolveHostButton(index, mode, resolution)}
        ${testHostButton(endpoint, index, resolution)}
        <button type="button" class="btn btn-sm btn-icon" data-action="endpoint-edit"
                data-index="${index}" title="Edit endpoint" aria-label="Edit endpoint">
          ${icon("pencil")}
        </button>
        ${removeButton(index, removable)}
      </td>
    </tr>${resolutionPanel(endpoint, index, resolution)}`;
  }

  return `<tr class="metrix-endpoint-editing">
    <td>${cellInput(p("id"), endpoint.id, "Endpoint id", true)}</td>
    <td>${addressingSelect(p("addressing"), mode)}</td>
    <td>${cellInput(p("address"), endpoint.address, "host:port", true)}</td>
    <td>${cellInput(p("host_header"), endpoint.host_header, "Host header")}</td>
    <td>${mode === "alb" ? albSshTarget(collect, p) : sshTarget(collect, p, endpoint.address, true)}</td>
    <td class="text-end">
      ${resolveHostButton(index, mode, resolution)}
      ${testHostButton(endpoint, index, resolution)}
      <button type="button" class="btn btn-sm btn-icon" data-action="endpoint-done"
              title="Finish editing" aria-label="Finish editing">${icon("check")}</button>
      ${removeButton(index, removable)}
    </td>
  </tr>${resolutionPanel(endpoint, index, resolution)}`;
}

function resolveHostButton(index, mode, resolution) {
  if (mode !== "alb") return "";
  if (resolution?.loading) {
    return `<button type="button" class="btn btn-sm btn-icon" disabled
                    title="Resolving ALB hosts" aria-label="Resolving ALB hosts">
      <span class="spinner-border spinner-border-sm" role="status"></span>
    </button>`;
  }
  return `<button type="button" class="btn btn-sm btn-icon" data-action="endpoint-resolve"
                  data-index="${index}" title="Resolve ALB to SSH hosts"
                  aria-label="Resolve ALB to SSH hosts">${icon("refresh")}</button>`;
}

function testHostButton(endpoint, index, resolution) {
  const collect = endpoint.collect ?? {};
  const canTestSavedAlbHost = endpoint.addressing === "alb" && Boolean(collect.host);
  if (collect.transport !== "ssh" && !canTestSavedAlbHost) return "";
  const host = collect.host || endpointHost(endpoint.address);
  if (!host) return "";
  const test = resolution?.tests?.[host];
  const testing = resolution?.testing === host;
  const result = test
    ? `<span class="badge ${test.ok ? "bg-green-lt" : "bg-red-lt"} me-1"
             title="${escape(test.check?.detail ?? "No diagnostic returned")}">
         ${test.ok ? "ok" : "failed"}
       </span><span class="text-secondary small me-1">${escape(test.check?.detail ?? "")}</span>`
    : "";
  return `${result}<button type="button" class="btn btn-sm btn-icon" data-action="endpoint-test-host"
                  data-index="${index}" data-host="${escape(host)}"
                  title="Test SSH connection to ${escape(host)}"
                  aria-label="Test SSH connection to ${escape(host)}"
                  ${testing ? "disabled" : ""}>
    ${testing ? '<span class="spinner-border spinner-border-sm" role="status"></span>' : icon("plug-connected")}
  </button>`;
}

function albSshTarget(collect, p) {
  if (!["ssh", "none"].includes(collect.transport)) {
    return sshTarget(collect, p, "", false);
  }
  const selected = collect.host ?? "";
  const enabled = collect.transport === "ssh" && Boolean(selected);
  const destination = selected
    ? `${collect.user ? `${collect.user}@` : ""}${selected}:${collect.port ?? 22}`
    : "Resolve to choose a host";
  return `${hidden(p("collect.alb"), "1")}${hidden(p("collect.host"), selected)}
    ${hidden(p("collect.user"), collect.user)}${hidden(p("collect.port"), collect.port)}
    ${hidden(p("collect.path"), collect.path)}
    <label class="form-check mb-1">
      <input class="form-check-input" type="checkbox" name="${escape(p("collect.enabled"))}"
             value="1"${enabled ? " checked" : ""}${selected ? "" : " disabled"}
             data-change-action="draft-reload">
      <span class="form-check-label">Enable SSH collection</span>
    </label>
    <code class="${selected ? "" : "text-secondary"}">${escape(destination)}</code>`;
}

function resolutionPanel(endpoint, index, resolution) {
  if (
    endpoint.addressing !== "alb" ||
    !resolution ||
    resolution.loading ||
    resolution.hostname !== endpointHost(endpoint.address)
  ) return "";
  const p = (field) => `endpoints.${index}.${field}`;
  if (resolution.error) {
    return `<tr class="metrix-endpoint-resolution"><td colspan="6">
      <div class="alert alert-danger py-2 mb-0">${escape(resolution.error)}</div>
    </td></tr>`;
  }
  const selected = endpoint.collect?.host ?? "";
  const hosts = resolution.hosts ?? [];
  const choices = hosts.length
    ? hosts.map((host) => {
        const test = resolution.tests?.[host.address];
        const testing = resolution.testing === host.address;
        const outcome = test
          ? `<span class="badge ${test.ok ? "bg-green-lt" : "bg-red-lt"}">
               ${test.ok ? "reachable" : "unreachable"}
             </span> <span class="text-secondary">${escape(test.check.detail)}</span>`
          : "";
        return `<div class="metrix-resolved-host">
          <label class="form-check mb-0">
            <input class="form-check-input" type="radio"
                   name="${escape(p("collect.selectedHost"))}" value="${escape(host.address)}"
                   ${host.address === selected ? "checked" : ""}
                   data-change-action="draft-reload">
            <span class="form-check-label"><code>${escape(host.address)}</code></span>
          </label>
          <span class="text-secondary">${escape(host.role)}</span>
          <button type="button" class="btn btn-sm btn-icon" data-action="endpoint-test-host"
                  data-index="${index}" data-host="${escape(host.address)}"
                  title="Test SSH collection from ${escape(host.address)}"
                  aria-label="Test SSH collection from ${escape(host.address)}"
                  ${testing ? "disabled" : ""}>
            ${testing ? '<span class="spinner-border spinner-border-sm" role="status"></span>' : icon("plug-connected")}
          </button>
          <div class="metrix-resolved-host-result">${outcome}</div>
        </div>`;
      }).join("")
    : `<div class="text-secondary">The AWS chain resolved, but it did not yield an instance or task IP.</div>`;
  return `<tr class="metrix-endpoint-resolution"><td colspan="6">
    <div class="d-flex align-items-center justify-content-between mb-2">
      <strong>Resolved SSH hosts</strong>
      <span class="text-secondary">Read-only · choose one host</span>
    </div>
    <div class="metrix-resolved-hosts">${choices}</div>
  </td></tr>`;
}

function addressingSelect(name, value) {
  const labels = {
    ip: "IP (direct)",
    alb: "ALB",
    elb: "ELB (classic manual)",
    ecs: "ECS",
    fargate: "FG (Fargate)",
  };
  return `<select class="form-select form-select-sm" name="${escape(name)}"
                  aria-label="Addressing" title="${escape(addressingNote(value))}"
                  data-change-action="draft-reload">
    ${ADDRESSING.map((option) => `<option value="${option}"${option === value ? " selected" : ""}>
      ${labels[option]}
    </option>`).join("")}
  </select>`;
}

function addressingBadge(value) {
  const labels = { ip: "IP", alb: "ALB", elb: "ELB", ecs: "ECS", fargate: "FG" };
  return `<span class="badge bg-blue-lt">${escape(labels[value] ?? value)}</span>`;
}

function addressingNote(value) {
  const notes = {
    ip: "Direct address; a Host header is required. ECS and Fargate discovery can resolve direct task addresses.",
    alb: "ALB discovery is supported from a hostname.",
    elb: "ELBv2/NLB discovery is supported; classic ELB must be entered explicitly.",
    ecs: "ECS service discovery is supported.",
    fargate: "Fargate task discovery is supported through ECS.",
  };
  return notes[value] ?? "This addressing kind must be entered explicitly.";
}

function cellInput(name, value, placeholder, required = false) {
  return `<input class="form-control form-control-sm" name="${escape(name)}"
                 value="${escape(value ?? "")}" placeholder="${escape(placeholder)}"
                 aria-label="${escape(placeholder)}"${required ? " required" : ""}>`;
}

function removeButton(index, removable) {
  return removable
    ? `<button type="button" class="btn btn-sm btn-icon text-danger"
               data-action="endpoint-remove" data-index="${index}"
               title="Remove endpoint" aria-label="Remove endpoint">${icon("trash")}</button>`
    : `<button type="button" class="btn btn-sm btn-icon" disabled
               title="A profile needs one endpoint" aria-label="A profile needs one endpoint">
         ${icon("trash")}
       </button>`;
}

function sshTarget(collect, p, endpointAddress, editable) {
  if (collect.transport !== "ssh") {
    return `<span class="text-secondary">${escape(
      collect.transport === "none" ? "Not observed" : `${collect.transport} (existing)`
    )}</span>${hidden(p("collect.transport"), collect.transport)}${hidden(p("collect.host"), collect.host)}
      ${hidden(p("collect.port"), collect.port)}${hidden(p("collect.user"), collect.user)}
      ${hidden(p("collect.path"), collect.path)}`;
  }
  const destination = sshDestination(collect, endpointAddress);
  return editable
    ? `${hidden(p("collect.transport"), collect.transport)}${cellInput(
        p("collect.ssh"),
        destination,
        "user@host:port"
      )}`
    : `${hidden(p("collect.transport"), collect.transport)}${hidden(
        p("collect.ssh"),
        destination
      )}<code>${escape(destination || "—")}</code>`;
}

function endpointHiddenFields(endpoint, index) {
  const collect = endpoint.collect ?? { transport: "none" };
  const p = (field) => `endpoints.${index}.${field}`;
  return [
    hidden(p("id"), endpoint.id),
    hidden(p("addressing"), endpoint.addressing ?? "ip"),
    hidden(p("address"), endpoint.address),
    hidden(p("host_header"), endpoint.host_header),
    hidden(p("collect.transport"), collect.transport),
    collect.transport === "ssh"
      ? hidden(p("collect.ssh"), sshDestination(collect, endpoint.address))
      : [
          hidden(p("collect.host"), collect.host),
          hidden(p("collect.port"), collect.port),
          hidden(p("collect.user"), collect.user),
        ].join(""),
    hidden(p("collect.path"), collect.path),
  ].join("");
}

/** The editor's compact spelling of the three fields the profile document stores. */
export function sshDestination(collect, endpointAddress = "") {
  const rawHost = collect.host ?? endpointHost(endpointAddress);
  const host = rawHost.includes(":") && !rawHost.startsWith("[")
    ? `[${rawHost}]`
    : rawHost;
  const user = collect.user ? `${collect.user}@` : "";
  const resolvedPort = collect.port ?? (rawHost ? 22 : null);
  const port = resolvedPort != null ? `:${resolvedPort}` : "";
  return `${user}${host}${port}`;
}

export function endpointHost(address) {
  const value = String(address ?? "");
  if (value.startsWith("[")) {
    const close = value.indexOf("]");
    return close >= 0 ? value.slice(1, close) : value;
  }
  const colon = value.lastIndexOf(":");
  return colon >= 0 ? value.slice(0, colon) : value;
}

/**
 * Split user@host:port without mistaking a bracketed IPv6 address for a port.
 * Every component is optional; an omitted value keeps the collector's default.
 */
export function parseSshDestination(raw) {
  let destination = String(raw ?? "").trim();
  if (!destination) return {};

  let user;
  const at = destination.lastIndexOf("@");
  if (at >= 0) {
    user = destination.slice(0, at) || undefined;
    destination = destination.slice(at + 1);
  }

  let host = destination;
  let port;
  if (destination.startsWith("[")) {
    const close = destination.indexOf("]");
    if (close >= 0) {
      host = destination.slice(1, close);
      const suffix = destination.slice(close + 1);
      if (/^:\d+$/.test(suffix)) port = Number(suffix.slice(1));
    }
  } else {
    const colon = destination.lastIndexOf(":");
    const suffix = colon >= 0 ? destination.slice(colon + 1) : "";
    if (/^\d+$/.test(suffix)) {
      host = destination.slice(0, colon);
      port = Number(suffix);
    }
  }

  return {
    ...(user ? { user } : {}),
    ...(host ? { host } : {}),
    ...(port != null ? { port } : {}),
  };
}

function hidden(name, value) {
  return value == null || value === ""
    ? ""
    : `<input type="hidden" name="${escape(name)}" value="${escape(value)}">`;
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
    endpoints: [],
  };
  if (previous.order) doc.order = previous.order;
  if (previous.gap) doc.gap = previous.gap;
  if (value("description")) doc.description = value("description");

  const observe = {};
  if (value("observe.interval")) observe.interval = value("observe.interval");
  if (value("observe.ssh_user")) observe.ssh_user = value("observe.ssh_user");
  const collect = data.getAll("observe.collect").map(String);
  if (collect.length) observe.collect = collect;
  if (Object.keys(observe).length) doc.observe = observe;

  for (let i = 0; i < previous.endpoints.length; i += 1) {
    const at = (field) => value(`endpoints.${i}.${field}`);
    const original = previous.endpoints[i] ?? {};
    const endpoint = {
      id: at("id"),
      addressing: at("addressing") || "ip",
      address: at("address"),
    };
    if (at("host_header")) endpoint.host_header = at("host_header");
    if (original.load === false) endpoint.load = false;
    if (original.tls) endpoint.tls = original.tls;
    if (original.attributes) endpoint.attributes = original.attributes;

    const albCollection = at("collect.alb") === "1";
    const transport = albCollection
      ? (at("collect.enabled") === "1" ? "ssh" : "none")
      : at("collect.transport") || "none";
    const collection = { transport };
    if (albCollection) {
      const host = at("collect.selectedHost") || at("collect.host");
      if (host) collection.host = host;
      if (at("collect.user")) collection.user = at("collect.user");
      if (at("collect.port")) collection.port = Number(at("collect.port"));
    }
    if (transport !== "none") {
      if (transport === "ssh" && !albCollection) {
        const typed = at("collect.ssh");
        const unchanged = typed === sshDestination(original.collect ?? {}, original.address);
        if (unchanged) {
          // What the row initially displayed included resolved defaults. If only the
          // endpoint address changed, keep those defaults implicit so SSH follows it.
          for (const key of ["user", "host", "port"]) {
            if (original.collect?.[key] != null) collection[key] = original.collect[key];
          }
        } else {
          const ssh = parseSshDestination(typed);
          const defaultHost = endpointHost(endpoint.address);
          if (ssh.user) collection.user = ssh.user;
          if (ssh.host && ssh.host !== defaultHost) collection.host = ssh.host;
          if (ssh.port != null && ssh.port !== 22) collection.port = ssh.port;
        }
      } else {
        if (at("collect.host")) collection.host = at("collect.host");
        // A port is a number to the server; "" would be a type error, not a default.
        if (at("collect.port")) collection.port = Number(at("collect.port"));
        if (at("collect.path")) collection.path = at("collect.path");
      }
      if (original.collect?.key) collection.key = original.collect.key;
    }
    endpoint.collect = collection;
    doc.endpoints.push(endpoint);
  }

  return doc;
}
