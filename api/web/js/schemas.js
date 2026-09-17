// Stored source schemas: upload once, inspect what the plan parser found, and delete.

import { count, escape } from "./format.js";
import { icon } from "./ui.js";

export const SOURCES = [
  ["openapi", "OpenAPI 3 (JSON or YAML)", "https://spec.openapis.org/oas/"],
  ["swagger", "Swagger 2.0 (JSON or YAML)", "https://swagger.io/specification/v2/"],
  ["wadl", "WADL", "https://www.w3.org/submissions/wadl/"],
  ["wsdl", "WSDL 1.1", "https://www.w3.org/TR/2001/NOTE-wsdl-20010315"],
  ["har", "HAR capture", "https://github.com/ahmadnassri/har-spec/blob/master/versions/1.2.md"],
  ["access_log", "Access log", "https://httpd.apache.org/docs/2.4/logs.html"],
  ["routes", "Route list", "https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Methods"],
];

const LABELS = Object.fromEntries(SOURCES.map(([value, label]) => [value, label]));

export function selectState(state) {
  return [state.schemas, state.schemaDetail, state.schemasReadAt, state.schemaDrop];
}

/** The picker is universal; the large drop target is only useful when these exist. */
export function dropCapability(environment = globalThis) {
  const missing = ["DragEvent", "DataTransfer", "File"].filter(
    (name) => typeof environment[name] !== "function"
  );
  if (!missing.length) return { enabled: true, diagnostic: "DragEvent, DataTransfer and File available." };
  return {
    enabled: false,
    reason: `This browser or workspace does not expose ${missing.join(", ")}.`,
    diagnostic: `Missing: ${missing.join(", ")}.`,
  };
}

export function render(state) {
  if (state.schemaDetail) return detail(state.schemaDetail);

  const dropTarget = state.schemaDrop?.enabled === false
    ? `<div class="metrix-schema-drop-unavailable" data-schema-drop-unavailable>
        <strong>File dropping is unavailable here</strong>
        <span class="text-secondary">${escape(state.schemaDrop.reason)} Choose files with the picker above.</span>
        <details class="metrix-schema-drop-diagnostics">
          <summary>Upload diagnostics</summary>
          <code>${escape(state.schemaDrop.diagnostic)}</code>
        </details>
      </div>`
    : `<div class="metrix-schema-drop-zone" data-schema-drop-zone tabindex="0" role="button"
             aria-label="Drop schema files here, or choose files">
          <strong>Drop files here to upload</strong>
          <span class="text-secondary">They are parsed immediately as the selected source type.</span>
          <span class="text-secondary">Or click here to choose files.</span>
          <details class="metrix-schema-drop-diagnostics">
            <summary>Upload diagnostics</summary>
            <code>${escape(state.schemaDrop?.diagnostic ?? "Awaiting a file drag.")}</code>
          </details>
        </div>`;

  const rows = state.schemas
    .map(
      (entry) => `<tr>
        <td><code>${escape(entry.id)}</code></td>
        <td>${escape(entry.filename)}</td>
        <td>${escape(LABELS[entry.source] ?? entry.source)}</td>
        <td>${escape(count(entry.call_count, "call"))}</td>
        <td class="text-end text-nowrap">
          <button type="button" class="btn btn-icon btn-sm" data-action="schema-view"
                  data-schema="${escape(entry.id)}" title="View ${escape(entry.filename)}"
                  aria-label="View ${escape(entry.filename)}">${icon("eye")}</button>
          <button type="button" class="btn btn-icon btn-sm text-danger"
                  data-action="schema-delete" data-schema="${escape(entry.id)}"
                  data-filename="${escape(entry.filename)}"
                  title="Delete ${escape(entry.filename)}"
                  aria-label="Delete ${escape(entry.filename)}">${icon("trash")}</button>
        </td>
      </tr>`
    )
    .join("");

  const table = state.schemas.length
    ? `<div class="card">
        <div class="table-responsive">
          <table class="table card-table table-vcenter metrix-table">
            <thead><tr>
              <th style="width:24%">ID</th>
              <th style="width:28%">Filename</th>
              <th style="width:28%">Type</th>
              <th style="width:10%">Calls</th>
              <th style="width:10%" class="text-end">Actions</th>
            </tr></thead>
            <tbody>${rows}</tbody>
          </table>
        </div>
      </div>`
    : `<div class="card"><div class="empty">
        <p class="empty-title">No schemas uploaded</p>
        <p class="empty-subtitle text-secondary">Drop a file in the upload target above, or choose one.</p>
      </div></div>`;

  return `<div class="metrix-stack">
    <form class="card" data-schema-upload-form>
      <div class="card-header">
        <div>
          <h3 class="card-title">Schemas</h3>
          <div class="card-subtitle">Stored service descriptions and the calls they define.</div>
        </div>
        <div class="card-actions d-flex align-items-center gap-2">
          <label class="visually-hidden" for="schema-source">Source type</label>
          <select class="form-select form-select-sm" id="schema-source" name="source">
            ${SOURCES.map(
              ([value, label]) => `<option value="${value}">${escape(label)}</option>`
            ).join("")}
          </select>
          <label class="visually-hidden" for="schema-file">Schema file</label>
          <input class="form-control form-control-sm" id="schema-file" name="file"
                 type="file" multiple required>
          <button type="submit" class="btn btn-sm btn-primary">
            ${icon("file-upload")} Upload
          </button>
        </div>
      </div>
      <div class="card-body pt-0">
        ${dropTarget}
        <div class="metrix-schema-references text-secondary">
          <span>Accepted types:</span>
          ${SOURCES.map(
            ([, label, reference]) =>
              `<a href="${reference}"${
                reference.startsWith("http") ? ' target="_blank" rel="noreferrer"' : ""
              }>${escape(label)}</a>`
          ).join(" · ")}
        </div>
      </div>
    </form>
    ${table}
  </div>`;
}

function detail(entry) {
  const calls = entry.calls
    .map(
      (call) => `<tr>
        <td><code>${escape(call.name)}</code></td>
        <td><span class="badge bg-blue-lt">${escape(call.method)}</span></td>
        <td><code>${escape(call.path)}</code></td>
        <td class="text-secondary">${escape(call.description ?? "—")}</td>
      </tr>`
    )
    .join("");

  return `<div class="metrix-stack">
    <div class="metrix-toolbar">
      <button type="button" class="btn" data-action="schema-back">
        ${icon("arrow-left")} Schemas
      </button>
      <button type="button" class="btn text-danger" data-action="schema-delete"
              data-schema="${escape(entry.id)}" data-filename="${escape(entry.filename)}">
        ${icon("trash")} Delete
      </button>
    </div>
    <div class="card">
      <div class="card-header">
        <div>
          <h3 class="card-title">${escape(entry.filename)}</h3>
          <div class="card-subtitle"><code>${escape(entry.id)}</code> ·
            ${escape(LABELS[entry.source] ?? entry.source)} ·
            ${escape(count(entry.call_count, "call"))}</div>
        </div>
      </div>
      <div class="table-responsive">
        <table class="table card-table table-vcenter metrix-table">
          <thead><tr><th>Call</th><th>Method</th><th>Path</th><th>Description</th></tr></thead>
          <tbody>${calls}</tbody>
        </table>
      </div>
    </div>
    <div class="card">
      <div class="card-header"><h3 class="card-title">Uploaded source</h3></div>
      <div class="card-body"><pre class="metrix-schema-source"><code>${escape(
        entry.content ?? ""
      )}</code></pre></div>
    </div>
  </div>`;
}

export function help() {
  return {
    title: "Schemas",
    body: `<p>Upload an OpenAPI 3, Swagger 2.0, WADL, WSDL 1.1, HAR, access-log or route-list file. The
      server immediately passes it through the same parser used to generate calls for
      a plan. Files that cannot produce calls are rejected and are not stored.</p>
      <p class="mb-0">The id is the filename stem plus its selected type. Uploading
      the same id twice is refused rather than replacing the earlier source.</p>`,
  };
}
