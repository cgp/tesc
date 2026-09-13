// The archive. Observation-only recordings and load runs are the same object with
// different sections populated, so one list shows both.

import {
  bytes,
  duration,
  escape,
  metricChange,
  metricValue,
  targetLabel,
  timestamp,
} from "./format.js";
import { empty, field, icon } from "./ui.js";

export function selectState(state) {
  return state.selectedRecording
    ? [state.selectedRecording]
    : [state.selectedRecording, state.recordings, state.archive];
}

export function render(state) {
  if (state.selectedRecording) return detail(state.selectedRecording);

  const archive = state.archive ?? {};
  const filtered = Object.values(archive.filters ?? {}).some(Boolean);

  if (!state.recordings.length) {
    // Two different empty states. "Nothing here yet" and "nothing matches what you
    // asked for" need different words and different ways out, and showing the first
    // to someone who has just typed a filter reads as data loss.
    return filtered
      ? filterBar(state) +
          empty({
            icon: "archive",
            title: "Nothing matches",
            body: `No recording in the archive of ${archive.total ?? 0} fits those
              filters.`,
            action: `<button class="btn" data-action="archive-clear">
                       ${icon("refresh")} Clear the filters
                     </button>`,
          })
      : empty({
          icon: "archive",
          title: "No recordings yet",
          body: `An observation-only recording needs no engine — it is the whole
            product until one exists.`,
          action: `<a href="#/profiles" class="btn btn-primary">
                     ${icon("player-play")} Start observing
                   </a>`,
        });
  }

  const rows = state.recordings
    .map(
      (r) => `<tr>
        <td class="name">
          <a href="#/recordings/${encodeURIComponent(r.id)}">${escape(r.id)}</a>
          ${
            r.is_baseline
              ? `<span class="badge bg-blue-lt ms-2"
                       title="Everything in this series is compared against this">
                   baseline
                 </span>`
              : ""
          }
        </td>
        <td class="text-secondary">${escape(r.kind)}</td>
        <td>${statusBadge(r.status)}</td>
        <td>${noteBadge(r)}</td>
        <td>${escape(r.profile ?? "—")}</td>
        <td class="num">${duration(r.duration_ms)}</td>
        <td class="num">${r.targets.length}</td>
        <td class="text-secondary">${timestamp(r.started_at)}</td>
      </tr>`
    )
    .join("");

  return `<div class="metrix-stack">
    ${filterBar(state)}
    <div class="card">
      <div class="card-header">
        <h3 class="card-title">Recordings
          <span class="card-subtitle">${showing(state)}</span>
        </h3>
      </div>
      <div class="table-responsive">
        <table class="table card-table table-vcenter metrix-table">
          <thead><tr>
            <th style="width:22%">Recording</th>
            <th style="width:10%">Kind</th>
            <th style="width:9%">Status</th>
            <th style="width:12%">Notes</th>
            <th style="width:13%">Profile</th>
            <th class="num" style="width:8%">Length</th>
            <th class="num" style="width:7%">Targets</th>
            <th style="width:19%">Started</th>
          </tr></thead>
          <tbody>${rows}</tbody>
        </table>
      </div>
    </div>
  </div>`;
}

/** What a row says about itself before anyone opens it. */
function noteBadge(recording) {
  const counts = recording.annotations_by_severity ?? {};
  if (!recording.worst) return `<span class="text-secondary">clean</span>`;
  const tone = { invalid: "red", warn: "orange", info: "blue" }[recording.worst];
  const total = Object.values(counts).reduce((a, b) => a + b, 0);
  const title =
    recording.worst === "invalid"
      ? "Carries an invalid note: its numbers cannot be trusted, and it cannot become a baseline"
      : "Carries notes worth reading before quoting anything from it";
  return `<span class="badge bg-${tone}-lt" title="${escape(title)}"
    >${escape(recording.worst)}${total > 1 ? ` ×${total}` : ""}</span>`;
}

function showing(state) {
  const archive = state.archive ?? {};
  const total = archive.total ?? state.recordings.length;
  const shown = state.recordings.length;
  // Whenever a filter is set the count is stated as a match, even when everything
  // matched: "2 captured" under an active filter reads as though nothing were
  // filtered, and the next thing that gets doubted is the filter.
  if (Object.values(archive.filters ?? {}).some(Boolean)) {
    return `${shown} of ${total} match`;
  }
  // Otherwise the only reason to see fewer than all of them is the page cap.
  return shown === total ? `${total} captured` : `${shown} of ${total} shown`;
}

/**
 * The filters, applied by the server.
 *
 * Server-side because the list is capped: filtering the rows the page happens to
 * hold would answer "nothing matches" for a recording that exists further down, and
 * a filter that lies is worse than no filter at all.
 */
function filterBar(state) {
  const archive = state.archive ?? {};
  const filters = archive.filters ?? {};
  const facets = archive.facets ?? {};
  const active = Object.values(filters).some(Boolean);

  const choose = (name, label, options) => `
    <div>
      <label class="form-label">${escape(label)}</label>
      <select class="form-select" data-change-action="archive-filter" data-filter="${name}">
        <option value=""${filters[name] ? "" : " selected"}>Any</option>
        ${(options ?? [])
          .map(
            (value) =>
              `<option value="${escape(value)}"${
                filters[name] === value ? " selected" : ""
              }>${escape(value)}</option>`
          )
          .join("")}
      </select>
    </div>`;

  return `<div class="card">
    <div class="card-body">
      <div class="metrix-filters">
        <div>
          <label class="form-label">Search</label>
          <input class="form-control" type="search" placeholder="id, profile or note"
                 data-change-action="archive-filter" data-filter="q"
                 value="${escape(filters.q ?? "")}">
        </div>
        ${choose("kind", "Kind", facets.kind)}
        ${choose("profile", "Profile", facets.profile)}
        ${choose("status", "Status", facets.status)}
        ${choose("severity", "Notes", ["invalid", "warn", "info"])}
        <div>
          <label class="form-label">Baseline</label>
          <select class="form-select" data-change-action="archive-filter"
                  data-filter="baseline">
            <option value=""${filters.baseline ? "" : " selected"}>Any</option>
            <option value="true"${
              filters.baseline === "true" ? " selected" : ""
            }>Baselines only</option>
            <option value="false"${
              filters.baseline === "false" ? " selected" : ""
            }>Everything else</option>
          </select>
        </div>
        <div class="metrix-filter-actions">
          <button class="btn" data-action="archive-clear"${active ? "" : " disabled"}>
            ${icon("refresh")} Clear
          </button>
        </div>
      </div>
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

/**
 * How this recording sits against the one its series is measured from.
 *
 * Only what moved. A table of every metric that behaved exactly as it always does is
 * a table nobody reads, and the finding it buries is the one thing here worth
 * seeing. "Nothing moved" is itself an answer, and is stated rather than left as an
 * empty card.
 *
 * Every figure carries the sample count behind it, which is not decoration: a median
 * over eleven samples and one over six hundred are different claims, and the API
 * withholds the numbers it cannot support (stats/summary.py).
 */
function comparisonCard(recording) {
  const comparison = recording.comparison;
  if (!comparison) return "";

  if (!comparison.baseline_id) {
    return `<div class="card">
      <div class="card-header"><h3 class="card-title">Against normal</h3></div>
      <div class="card-body text-secondary">
        No baseline is set for this series, so there is nothing to compare against.
        Mark a recording of this environment doing nothing in particular as the
        baseline, and later ones answer <em>is this behaving normally today?</em>
        ${recording.is_baseline ? "This recording <strong>is</strong> that baseline." : ""}
      </div>
    </div>`;
  }

  const rows = comparison.moved
    .map((d) => {
      const direction = d.worse ? "text-danger" : "text-success";
      const sign = d.change > 0 ? "+" : "";
      // `*` is the pooled view. A named box appears only where it disagrees with
      // that, which is the case worth reading: one machine out of step.
      const where =
        d.target === "*"
          ? `<span class="text-secondary">every box</span>`
          : `<span class="badge bg-yellow-lt" title="This box differs from the rest"
                   >${escape(targetLabel(d.target))}</span>`;
      return `<tr>
        <td class="name">${escape(d.metric)} ${where}</td>
        <td class="num">${figure(d.baseline)}</td>
        <td class="num">${figure(d.current)}</td>
        <td class="num ${direction}">${metricChange(d.metric, d.change)}
          <span class="text-secondary">(${sign}${d.change_pct.toFixed(1)}% of normal)</span></td>
        <td class="num text-secondary">±${metricValue(d.metric, d.band)}</td>
      </tr>`;
    })
    .join("");

  const missing = [
    comparison.only_now.length
      ? `${comparison.only_now.length} target(s) here are not in the baseline`
      : "",
    comparison.only_baseline.length
      ? `${comparison.only_baseline.length} in the baseline are gone`
      : "",
  ].filter(Boolean);

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">Against normal</h3>
        <div class="card-subtitle">
          Compared with <a href="#/recordings/${encodeURIComponent(
            comparison.baseline_id
          )}">${escape(comparison.baseline_id)}</a>, the baseline for this series.
          A metric is listed only when it moved further than the baseline's own
          spread — a band measured, not chosen.
          ${missing.length ? escape(missing.join("; ")) + "." : ""}
        </div>
      </div>
    </div>
    ${
      rows
        ? `<div class="table-responsive">
             <table class="table card-table table-vcenter metrix-table">
               <thead><tr>
                 <th style="width:34%">Metric</th>
                 <th class="num" style="width:17%">Normally</th>
                 <th class="num" style="width:17%">This time</th>
                 <th class="num" style="width:20%">Change</th>
                 <th class="num" style="width:12%">Band</th>
               </tr></thead>
               <tbody>${rows}</tbody>
             </table>
           </div>`
        : `<div class="card-body text-secondary">Nothing moved beyond its band. This
             environment is behaving the way it normally does.</div>`
    }
  </div>`;
}

// The median, and the count it rests on. Never one without the other.
function figure(summary) {
  if (summary.p50 == null) {
    return `<span class="text-secondary" title="${summary.n} samples is too few">
      — <small>n=${summary.n}</small></span>`;
  }
  return `${metricValue(summary.metric, summary.p50)}
    <small class="text-secondary">n=${summary.n}</small>`;
}

/**
 * What happened after the traffic stopped.
 *
 * Absent entirely for an observation-only recording: baseline and settle collapse
 * into one window there, so there is no "after" and nothing to claim. A card reading
 * "no data" on every recording anyone has taken so far would be worse than no card.
 */
function recoveryCard(recording) {
  const targets = recording.recovery?.targets ?? {};
  const rows = Object.entries(targets)
    .flatMap(([target, metrics]) =>
      Object.values(metrics).map((r) => {
        const verdict = r.returned
          ? r.recovered_ms === 0
            ? `<span class="text-success">never left the band</span>`
            : `<span class="text-success">back after ${duration(r.recovered_ms)}</span>`
          : r.leaked
            ? `<span class="text-danger">never came back</span>`
            : `<span class="text-secondary">still out, the good way</span>`;
        return `<tr>
          <td class="name" title="${escape(target)}">${escape(targetLabel(target))}</td>
          <td>${escape(r.metric)}</td>
          <td>${verdict}</td>
          <td class="num">${metricValue(r.metric, r.peak)}
            <span class="text-secondary">at ${duration(r.peak_at_ms)}</span></td>
          <td class="num">${metricValue(r.metric, r.final)}
            <span class="text-secondary">vs ${metricValue(r.metric, r.baseline)}</span></td>
          <td class="num text-secondary">n=${r.n}</td>
        </tr>`;
      })
    )
    .join("");
  if (!rows) return "";

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">After the traffic stopped</h3>
        <div class="card-subtitle">Time back to within the baseline's own spread, and
          the worst value seen during settle — which for free memory is the lowest,
          not the highest. Queues, collections and flushes often peak after the last
          request rather than under load. A metric that never
          came back, and drifted the wrong way, is the leak signal.</div>
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th style="width:14%">Target</th>
          <th style="width:20%">Metric</th>
          <th style="width:20%">Recovered</th>
          <th class="num" style="width:20%">Worst after stop</th>
          <th class="num" style="width:20%">Ended at</th>
          <th class="num" style="width:6%">Samples</th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}

function baselineButton(recording) {
  const marked = Boolean(recording.is_baseline);
  return `<button class="btn btn-sm ${marked ? "btn-primary" : ""}"
                  data-action="baseline-toggle"
                  data-recording="${escape(recording.id)}"
                  data-baseline="${marked ? "1" : "0"}"
                  title="${
                    marked
                      ? "Stop treating this as normal for its series"
                      : "Treat this as normal for its series, and compare later recordings with it"
                  }">
    ${icon("target")} ${marked ? "Baseline" : "Set as baseline"}
  </button>`;
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
        <div class="card-actions d-flex align-items-center gap-2">
        ${baselineButton(recording)}
        ${statusBadge(recording.status)}
      </div>
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
          ${field(
            "Series",
            // The identity is also the way into the history: what this run did is
            // half the question, and whether it is better or worse than the last
            // ten is the other half.
            `<a href="#/series/${encodeURIComponent(recording.series_key)}"
               ><code>${escape(recording.series_key)}</code></a>`
          )}
        </div>
      </div>
    </div>
    ${comparisonCard(recording)}
    ${recoveryCard(recording)}
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
