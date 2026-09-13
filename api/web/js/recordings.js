// The archive. Observation-only recordings and load runs are the same object with
// different sections populated, so one list shows both.

import { bytes, duration, escape, targetLabel, timestamp } from "./format.js";
import { empty, field, icon } from "./ui.js";

export function selectState(state) {
  return state.selectedRecording
    ? [state.selectedRecording]
    : [state.selectedRecording, state.recordings];
}

export function render(state) {
  if (state.selectedRecording) return detail(state.selectedRecording);

  if (!state.recordings.length) {
    return empty({
      icon: "archive",
      title: "No recordings yet",
      body: `An observation-only recording needs no engine — it is the whole product
        until one exists.`,
      action: `<a href="#/config" class="btn btn-primary">
                 ${icon("player-play")} Start observing
               </a>`,
    });
  }

  const rows = state.recordings
    .map(
      (r) => `<tr>
        <td class="name">
          <a href="#/recordings/${encodeURIComponent(r.id)}">${escape(r.id)}</a>
        </td>
        <td class="text-secondary">${escape(r.kind)}</td>
        <td>${statusBadge(r.status)}</td>
        <td>${escape(r.profile ?? "—")}</td>
        <td class="num">${duration(r.duration_ms)}</td>
        <td class="num">${r.targets.length}</td>
        <td class="text-secondary">${timestamp(r.started_at)}</td>
      </tr>`
    )
    .join("");

  return `<div class="card">
    <div class="card-header">
      <h3 class="card-title">Recordings
        <span class="card-subtitle">${state.recordings.length} captured</span>
      </h3>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th style="width:22%">Recording</th>
          <th style="width:12%">Kind</th>
          <th style="width:10%">Status</th>
          <th style="width:14%">Profile</th>
          <th class="num" style="width:9%">Length</th>
          <th class="num" style="width:8%">Targets</th>
          <th style="width:25%">Started</th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}

function statusBadge(status) {
  const tone = { finished: "green", running: "blue", aborted: "orange", failed: "red" }[status];
  return `<span class="badge bg-${tone ?? "secondary"}-lt">${escape(status)}</span>`;
}

// What each box was. Not a series -- it does not change during a run -- but it is
// what answers "what was this measured on?" once the run is a year old and the
// machine is gone.
function hostsCard(recording) {
  const identity = recording.identity ?? {};
  const targets = Object.keys(identity);
  if (!targets.length) return "";

  const columns = ["hostname", "os", "kernel", "arch", "cpus"];
  const rows = targets
    .map(
      (target) => `<tr>
        <td class="name">${escape(target)}</td>
        ${columns
          .map((key) => {
            const value = identity[target][key];
            // Absent is a real answer: a stripped container cannot always say.
            return `<td>${value ? escape(value) : '<span class="text-secondary">—</span>'}</td>`;
          })
          .join("")}
      </tr>`
    )
    .join("");

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">Hosts</h3>
        <div class="card-subtitle">Read once at the start. Not a series — it does
          not change while a run is going.</div>
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th style="width:16%">Target</th>
          <th style="width:16%">Hostname</th>
          <th style="width:26%">OS</th>
          <th style="width:26%">Kernel</th>
          <th style="width:10%">Arch</th>
          <th class="num" style="width:6%">CPUs</th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}

/**
 * What the recording ran against, pinned at the time it ran.
 *
 * Read from the pin, not resolved again: the tasks named here have very likely been
 * replaced since, and the answer to "what did this measure" must not change when the
 * environment does. The task definition and the image digest are the columns that
 * earn their place — they are what a later comparison uses to say *different build*
 * rather than *regression*.
 */
function inventoryCard(recording) {
  const found = recording.inventory;
  if (!found) return "";

  const rows = found.resources
    .filter((r) => r.role !== "container")
    .map((resource) => {
      const build = found.resources
        .filter((c) => c.role === "container" && c.parent === resource.id)
        .map(
          (c) => `<div><code>${escape(c.container)}</code>
            <span class="text-secondary">${escape(shortDigest(c.image_digest))}</span></div>`
        )
        .join("");
      return `<tr>
        <td class="name" title="${escape(resource.id)}">${escape(targetLabel(resource.id))}</td>
        <td><span class="badge bg-secondary-lt">${escape(resource.role)}</span></td>
        <td><code>${escape(endpointOf(resource))}</code></td>
        <td>${escape(resource.instance_type ?? resource.availability_zone ?? "—")}</td>
        <td>${resource.task_definition ? `<code>${escape(resource.task_definition)}</code>` : "—"}</td>
        <td>${build || '<span class="text-secondary">—</span>'}</td>
      </tr>`;
    })
    .join("");

  const notes = found.notes.length
    ? `<div class="card-body py-2 border-top text-secondary">
         ${found.notes.map((n) => `<div>${escape(n.hop)}: ${escape(n.message)}</div>`).join("")}
       </div>`
    : "";

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">What it ran against</h3>
        <div class="card-subtitle">Discovered from <code>${escape(found.source)}</code>,
          reached ${escape(found.reached)}. Pinned to this recording — it does not
          change when the environment does.</div>
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th style="width:19%">Resource</th>
          <th style="width:7%">Role</th>
          <th style="width:27%">Address</th>
          <th style="width:11%">Placement</th>
          <th style="width:14%">Task definition</th>
          <th style="width:22%">Build</th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
    ${notes}
  </div>`;
}

function endpointOf(resource) {
  if (!resource.address) return "—";
  return resource.port ? `${resource.address}:${resource.port}` : resource.address;
}

// The first twelve characters identify a build in practice, and the whole digest is
// 71 of them -- enough to push every other column off the screen.
function shortDigest(digest) {
  if (!digest) return "";
  return digest.startsWith("sha256:") ? digest.slice(0, 19) + "…" : digest.slice(0, 12) + "…";
}

// Disk before and after, and what the run consumed. Two readings rather than a
// series: the question is "did this eat space", which a delta answers.
function diskCard(recording) {
  const filesystems = recording.filesystems ?? [];
  if (!filesystems.length) return "";

  const rows = filesystems
    .map((fs) => {
      const delta =
        fs.used_delta_bytes == null
          ? `<span class="text-secondary" title="No reading at the end of the run">—</span>`
          : signedBytes(fs.used_delta_bytes);
      return `<tr>
        <td class="name">${escape(fs.target_id)}</td>
        <td><code>${escape(fs.mount)}</code></td>
        <td class="num">${bytes(fs.total_bytes)}</td>
        <td class="num">${bytes(fs.start_used_bytes)}</td>
        <td class="num">${bytes(fs.finish_used_bytes)}</td>
        <td class="num">${delta}</td>
      </tr>`;
    })
    .join("");

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">Disk</h3>
        <div class="card-subtitle">Read once before the run and once after it
          drained. A mount with no reading at the end has no delta rather than a
          delta of zero.</div>
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th style="width:16%">Target</th>
          <th style="width:28%">Mount</th>
          <th class="num" style="width:14%">Size</th>
          <th class="num" style="width:14%">Used before</th>
          <th class="num" style="width:14%">Used after</th>
          <th class="num" style="width:14%">Change</th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}

// A sign is the whole point of this column: a run that freed space and a run that
// consumed it are different findings, and "1.2 GB" alone does not say which.
function signedBytes(value) {
  if (value === 0) return `<span class="text-secondary">none</span>`;
  const tone = value > 0 ? "text-orange" : "text-green";
  return `<span class="${tone}">${value > 0 ? "+" : "−"}${bytes(Math.abs(value))}</span>`;
}

function detail(recording) {
  const annotations = recording.annotations.length
    ? `<div class="list-group list-group-flush">${recording.annotations
        .map(
          (a) => `<div class="list-group-item px-0 severity-${escape(a.severity)}">
            <strong>${escape(a.code)}</strong> — ${escape(a.message)}
          </div>`
        )
        .join("")}</div>`
    : `<p class="text-secondary mb-0">None.</p>`;

  const gaps = recording.gaps.length
    ? `<ul class="gap-note mb-0">${recording.gaps
        .map(
          (g) =>
            `<li>${escape(g.target_id)}: ${g.from_ms}–${g.to_ms}ms — ${escape(g.reason)}</li>`
        )
        .join("")}</ul>`
    : `<p class="text-secondary mb-0">None. Every interval was collected.</p>`;

  return `<div class="mb-3">
    <a href="#/recordings" class="btn btn-sm">${icon("arrow-left")} All recordings</a>
  </div>
  <div class="metrix-stack">
    <div class="card">
      <div class="card-header">
        <h3 class="card-title">${escape(recording.id)}</h3>
        <div class="card-actions">${statusBadge(recording.status)}</div>
      </div>
      <div class="card-body">
        <div class="datagrid">
          ${field("Profile", escape(recording.profile ?? "—"))}
          ${field("Addressing", escape(recording.addressing_mode))}
          ${field("Length", duration(recording.duration_ms))}
          ${field(
            "Targets",
            recording.targets
              .map((t) => `<span title="${escape(t)}">${escape(targetLabel(t))}</span>`)
              .join(", ") || "—"
          )}
          ${field("Metrics", String(recording.metrics.length))}
          ${field("Series", `<code>${escape(recording.series_key)}</code>`)}
        </div>
      </div>
    </div>
    ${inventoryCard(recording)}
    ${hostsCard(recording)}
    ${diskCard(recording)}
    <div class="metrix-split">
      <div class="card">
        <div class="card-header"><h3 class="card-title">Notes</h3></div>
        <div class="card-body">${annotations}</div>
      </div>
      <div class="card">
        <div class="card-header">
          <h3 class="card-title">Collection gaps
            <span class="card-subtitle">drawn as gaps, never interpolated</span>
          </h3>
        </div>
        <div class="card-body">${gaps}</div>
      </div>
    </div>
  </div>`;
}
