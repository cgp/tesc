// Logs: what this process has been doing (§2.6). Discovery walks, verification checks
// with their timings, recordings starting and failing -- the evidence behind the
// one-line answers other pages show. Read-only; main.js polls only while it is open.

import { count, escape, timestamp } from "./format.js";
import { empty } from "./ui.js";

export const FILTERS = [
  ["all", "Everything"],
  ["problems", "Warnings and errors"],
];

const TONES = { debug: "secondary", info: "blue", warning: "yellow", error: "red", critical: "red" };

export function selectState(state) {
  return [state.logs, state.logFilter];
}

/** Newest first: the question on arrival is what just happened. */
export function visible(entries, filter) {
  const kept =
    filter === "problems"
      ? entries.filter((entry) => entry.level !== "info" && entry.level !== "debug")
      : entries;
  return [...kept].reverse();
}

export function render(state) {
  const log = state.logs;
  if (!log) {
    return empty({
      icon: "server",
      title: "Reading the server log",
      body: "The most recent records this process has written will appear here.",
    });
  }
  const filter = state.logFilter ?? "all";
  const rows = visible(log.entries, filter);
  const options = FILTERS.map(
    ([value, label]) =>
      `<option value="${value}"${value === filter ? " selected" : ""}>${label}</option>`
  ).join("");

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">Server log</h3>
        <div class="card-subtitle">${count(log.entries.length, "record")} held in memory;
          the oldest are dropped past ${log.capacity}, and a restart clears them</div>
      </div>
      <div class="card-actions">
        <select class="form-select form-select-sm" data-log-filter aria-label="Which records">
          ${options}
        </select>
      </div>
    </div>
    ${
      rows.length
        ? `<div class="table-responsive">
            <table class="table table-vcenter card-table metrix-log">
              <thead><tr><th>Time (UTC)</th><th>Level</th><th>Source</th><th>Message</th></tr></thead>
              <tbody>${rows.map(row).join("")}</tbody>
            </table>
          </div>`
        : `<div class="card-body text-secondary">${
            filter === "problems"
              ? "No warnings or errors since this process started."
              : "Nothing logged yet."
          }</div>`
    }
  </div>`;
}

function row(entry) {
  const tone = TONES[entry.level] ?? "secondary";
  const trace = entry.trace
    ? `<details class="mt-1"><summary class="text-secondary">traceback</summary>
        <pre class="metrix-log-trace">${escape(entry.trace)}</pre></details>`
    : "";
  return `<tr>
    <td class="text-nowrap" title="${escape(timestamp(entry.time))}">${escape(
      entry.time.slice(11, 23)
    )}</td>
    <td><span class="badge bg-${tone}-lt">${escape(entry.level)}</span></td>
    <td class="text-nowrap text-secondary">${escape(entry.source)}</td>
    <td class="metrix-log-message">${escape(entry.message)}${trace}</td>
  </tr>`;
}
