// Sweep: one plan, many boxes, one window — which of them is the odd one out.
//
// The one comparison in this tool whose reference is not history. A sweep holds
// everything still except the machine, so the sweep's own spread is the band: §17.4's
// measured noise floor applied across targets instead of across time.
//
// Nothing here decides anything. The ranking, the bands, the verdicts and the record
// of previous sweeps all arrive computed, and each box was judged against the others
// rather than against a set containing itself — so two rows of the same table can
// legitimately carry slightly different bands, and the bar for each row draws its own.

import { count, escape, metricValue, targetLabel, timestamp } from "./format.js";
import { empty, icon } from "./ui.js";

export function selectState(state) {
  return [state.sweep, state.selectedRecording];
}

export function render(state) {
  const sweep = state.sweep;
  if (!sweep) {
    return empty({
      icon: "target",
      title: "No sweep open",
      body: `A sweep is a recording that covers more than one box. Open one from the
        archive and read its targets against each other.`,
      action: `<a class="btn" href="#/recordings">${icon("archive")} All recordings</a>`,
    });
  }

  if (sweep.boxes.length < 2) {
    return (
      backLink(sweep) +
      empty({
        icon: "target",
        title: "One box is not a sweep",
        body: `This recording covers a single target, so there is nothing to rank it
          against. A sweep compares the boxes of one environment measured in the same
          window — the odd one out is only visible next to its siblings.`,
      })
    );
  }

  return `${backLink(sweep)}
  <div class="metrix-stack">
    ${lede(sweep)}
    ${findingsCard(sweep)}
    ${boxesCard(sweep)}
    ${sweep.metrics.map((one) => metricCard(one, sweep)).join("")}
  </div>`;
}

export function help() {
  return {
    title: "Reading a sweep",
    body: `
      <p>A sweep runs the same work against every box in an environment, one at a
      time, in one window. Everything is held still except the machine — so when one
      of them comes out differently, the machine is what is left to explain it.</p>

      <p><strong>The reference is the sweep's own spread, not its history.</strong>
      The middle of the other boxes is what normal looks like, and how far from it
      counts as a move is measured from how far apart they are — the same band the
      trend view draws over time, turned sideways.</p>

      <p><strong>Every box is judged against the others, never against a set
      containing itself.</strong> One slow container inflating the spread it is then
      compared against would hide exactly the thing this page exists to find, which is
      why two rows can carry slightly different bands and why each bar draws its own.</p>

      <p><strong>Being outside the band is not the verdict.</strong> A box is only
      called the odd one out when it is outside, its sample count supports the figure,
      and it is not already carrying a note saying its numbers cannot be trusted. Where
      a box was not judged, the row says which of those it missed rather than leaving a
      blank to be misread as a pass.</p>

      <p><strong>A small sweep is ranked but not judged.</strong> An interquartile
      range over three numbers is not a description of spread, so below the peer floor
      the ordering, the attributes and the baselines are all still shown and no
      accusation is made.</p>

      <p><strong>The baseline column is the one that saves a wasted afternoon.</strong>
      A container that was already busy before the run started looks exactly like one
      that buckled under the load, right until the two numbers are read side by side.</p>

      <p><strong>The attributes are usually the answer.</strong> An older image digest,
      a different instance type, a lone task in another availability zone — that is
      where the explanation for an outlier normally is, rather than in the
      measurement.</p>`,
  };
}

function backLink(sweep) {
  return `<div class="mb-3">
    <a href="#/recordings/${encodeURIComponent(sweep.recording_id)}" class="btn btn-sm">
      ${icon("arrow-left")} Back to the recording
    </a>
  </div>`;
}

/** What is being compared, and over which window. */
function lede(sweep) {
  const phases = sweep.phases
    .map(
      (phase) =>
        `<a class="btn btn-sm${phase === sweep.phase ? " btn-primary" : ""}"
            href="#/sweep/${encodeURIComponent(sweep.recording_id)}/${encodeURIComponent(
              phase
            )}">${escape(phase)}</a>`
    )
    .join("");

  return `<div class="metrix-toolbar">
    <p class="metrix-note text-secondary mb-0">
      ${count(sweep.boxes.length, "box", "boxes")} measured in one window, each ranked
      against the others. ${
        sweep.judged
          ? "The band is the spread of the rest of the sweep."
          : `<strong>Ranked, not judged:</strong> a spread needs ${sweep.min_peers}
             other boxes with a figure, and this sweep is smaller than that. The
             ordering and the attributes still say what they say.`
      }
    </p>
    <div class="btn-list">${phases}</div>
  </div>`;
}

/**
 * The odd ones out, worst first, with their record.
 *
 * First on the page because it is the answer, and carrying the history because the
 * follow-up question is always the same one: is that box reliably the slow one, or
 * was it unlucky once?
 */
function findingsCard(sweep) {
  if (!sweep.judged) return "";
  if (!sweep.findings.length) {
    return `<div class="card">
      <div class="card-body d-flex align-items-baseline gap-2">
        <span class="badge bg-green-lt">no outliers</span>
        <span class="text-secondary">Every box sits inside the spread of the others on
          every metric. That is a statement about this window, not about the boxes.</span>
      </div>
    </div>`;
  }

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">The odd ones out</h3>
        <div class="card-subtitle">Outside the spread of the other boxes, with enough
          samples to say so.</div>
      </div>
    </div>
    <div class="card-body">
      <ul class="metrix-findings">${sweep.findings.map(finding).join("")}</ul>
    </div>
  </div>`;
}

function finding(one) {
  const s = one.standing;
  const direction = s.worse ? "worse than" : "better than";
  const tone = s.worse ? "severity-invalid" : "severity-info";
  const history = one.history.length
    ? ` <span class="text-secondary">Seen in ${count(
        one.history.length,
        "earlier sweep"
      )}; flagged in ${one.previously_flagged}${
        one.previously_flagged === one.history.length && one.history.length > 1
          ? " — every one of them"
          : ""
      }.</span>`
    : ` <span class="text-secondary">No earlier sweep in this series carried this box,
        so there is nothing to say about whether it is always this one.</span>`;

  return `<li>
    <strong title="${escape(one.target_id)}">${escape(targetLabel(one.target_id))}</strong>
    on <code>${escape(one.metric)}</code> —
    <span class="${tone}">${escape(metricValue(one.metric, s.value))}</span>
    ${direction} the other boxes
    (${escape(metricValue(one.metric, s.centre))} ±
    ${escape(metricValue(one.metric, s.band))}, from ${count(s.peers, "peer")})
    <span class="sample-count">n=${s.n}</span>.
    ${baselineNote(one.metric, s)}${history}
  </li>`;
}

/**
 * Whether the box started the window already busy.
 *
 * The distinction §17.6 asks for by name: a container that was loaded before the test
 * began looks exactly like one that buckled under it, and only the baseline tells
 * them apart. Silent when the recording has no separate baseline phase to read.
 */
function baselineNote(metric, standing) {
  if (standing.baseline_value == null) return "";
  return `<span class="text-secondary">It was already at
    ${escape(metricValue(metric, standing.baseline_value))} before the traffic started
    <span class="sample-count">n=${standing.baseline_n}</span>.</span>`;
}

/** Every box and what discovery found about it. Usually where the answer is. */
function boxesCard(sweep) {
  const keys = [...new Set(sweep.boxes.flatMap((box) => Object.keys(box.attributes ?? {})))].sort();

  const header = keys.length
    ? keys.map((key) => `<th>${escape(key.replaceAll("_", " "))}</th>`).join("")
    : `<th>Attributes</th>`;

  const rows = sweep.boxes
    .map(
      (box) => `<tr>
        <td class="num">${box.position}</td>
        <td class="name" title="${escape(box.target_id)}">${escape(
          targetLabel(box.target_id)
        )}</td>
        <td><code>${escape(box.address ?? "—")}</code></td>
        ${
          keys.length
            ? keys
                .map(
                  (key) =>
                    `<td>${
                      box.attributes?.[key]
                        ? `<code>${escape(box.attributes[key])}</code>`
                        : '<span class="text-secondary">—</span>'
                    }</td>`
                )
                .join("")
            : `<td class="text-secondary">nothing recorded — this profile was written
                 by hand rather than discovered</td>`
        }
      </tr>`
    )
    .join("");

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">The boxes</h3>
        <div class="card-subtitle">In the order they were run. The explanation for an
          outlier is usually in one of these columns rather than in the
          measurement.</div>
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th>#</th>
          <th>Box</th>
          <th>Address</th>
          ${header}
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}

/** One metric, every box ranked, with each row's own band drawn behind its bar. */
function metricCard(one, sweep) {
  const span = extent(one.standings);

  const rows = one.standings
    .map(
      (s) => `<tr class="${s.flagged ? "metrix-outlier" : ""}">
        <td class="num">${s.rank}</td>
        <td class="name" title="${escape(s.target_id)}">${escape(targetLabel(s.target_id))}</td>
        <td class="num">${
          s.value == null
            ? '<span class="unsupported">—</span>'
            : escape(metricValue(one.metric, s.value))
        }<span class="sample-count">n=${s.n}</span></td>
        <td class="num">${
          s.baseline_value == null
            ? '<span class="text-secondary">—</span>'
            : `${escape(metricValue(one.metric, s.baseline_value))}` +
              `<span class="sample-count">n=${s.baseline_n}</span>`
        }</td>
        <td>${bar(s, span)}</td>
        <td>${verdict(s)}</td>
      </tr>`
    )
    .join("");

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title"><code>${escape(one.metric)}</code></h3>
        <div class="card-subtitle">Best first. ${
          one.worse === "up" ? "Lower is better here." : "Higher is better here."
        }${one.judged ? "" : " No band: the sweep is too small to describe its own spread."}</div>
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th style="width:4%">#</th>
          <th style="width:16%">Box</th>
          <th class="num" style="width:14%">${escape(sweep.phase)}</th>
          <th class="num" style="width:14%">baseline</th>
          <th style="width:32%">Against the others</th>
          <th style="width:20%"></th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}

/**
 * The range the bars are drawn over: the figures and the bands, not zero.
 *
 * A zero-based bar answers "how big is this", which the number beside it already
 * answers exactly. The question here is a different one -- how far is this box from
 * the others -- so the axis runs across what the sweep actually covers, and the band
 * drawn behind each bar is the reference rather than the origin. Padded a little so a
 * value sitting on the edge is still visible as a bar.
 */
function extent(standings) {
  const points = [];
  for (const s of standings) {
    if (s.value != null) points.push(s.value);
    if (s.band != null) points.push(s.centre - s.band, s.centre + s.band);
  }
  if (!points.length) return [0, 1];
  const low = Math.min(...points);
  const high = Math.max(...points);
  if (low === high) return [low - 1, high + 1];
  const margin = (high - low) * 0.08;
  return [low - margin, high + margin];
}

/**
 * One box's figure against the band measured from the others.
 *
 * The band is drawn per row rather than once per metric because it *is* per row: each
 * box is compared with the others, so the reference shifts slightly from row to row.
 * Drawing one shared band would put a bar visibly inside it and still call it out.
 */
function right(value, [low, high]) {
  return `${Math.max(0, Math.min(100, (1 - (value - low) / (high - low)) * 100))}%`;
}

function bar(standing, [low, high]) {
  const width = (value) =>
    `${Math.max(0, Math.min(100, ((value - low) / (high - low)) * 100))}%`;
  const band =
    standing.band == null
      ? ""
      : `<span class="metrix-band" style="left:${width(
          standing.centre - standing.band
        )};right:${right(standing.centre + standing.band, [low, high])}"></span>
         <span class="metrix-centre" style="left:${width(standing.centre)}"></span>`;
  const value =
    standing.value == null
      ? ""
      : `<span class="metrix-bar${standing.flagged ? " is-outlier" : ""}"
              style="width:${width(standing.value)}"></span>`;
  return `<span class="metrix-track">${band}${value}</span>`;
}

/** The verdict, or why there is not one. Never a blank. */
function verdict(standing) {
  if (standing.flagged) {
    return `<span class="badge ${standing.worse ? "bg-red-lt" : "bg-blue-lt"}">${
      standing.worse ? "odd one out" : "ahead of the rest"
    }</span>`;
  }
  if (standing.judged) {
    return `<span class="text-secondary">within the spread</span>`;
  }
  return `<span class="text-secondary">${escape(standing.unjudged_because ?? "not judged")}</span>`;
}

/** The last sweep a flagged box appeared in, for the recording page's summary line. */
export function lastSeen(finding) {
  const seen = finding.history[finding.history.length - 1];
  return seen ? timestamp(seen.at) : null;
}
