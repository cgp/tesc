// Wiring: hash routing, data loading, and one render pass per state change.
//
// Views are pure functions of state (state.js) and never touch the network; api.js
// is the only module that does. That separation is what keeps the live stream in
// A1.8 from having to know anything about the DOM.

import { api } from "./api.js";
import * as charts from "./charts.js";
import * as config from "./config.js";
import { escape } from "./format.js";
import * as recordings from "./recordings.js";
import { get, set, subscribe } from "./state.js";
import * as table from "./table.js";

const ROUTES = {
  config: { title: "Config", subtitle: "Profiles and what is collected", view: config },
  stats: { title: "Stats", subtitle: "The numbers, exactly", view: table },
  charts: { title: "Charts", subtitle: "Shape and timing", view: charts },
  recordings: { title: "Recordings", subtitle: "Everything captured", view: recordings },
};

const DEFAULT_ROUTE = "recordings";

function parseHash() {
  const path = (location.hash || "").replace(/^#\/?/, "");
  const parts = path.split("/").filter(Boolean);

  if (parts[0] === "config") return { name: "config" };
  if (parts[0] === "performance") {
    return { name: parts[1] === "charts" ? "charts" : "stats" };
  }
  if (parts[0] === "recordings") {
    return { name: "recordings", recordingId: parts[1] ?? null };
  }
  return { name: DEFAULT_ROUTE };
}

async function load(route) {
  set({ error: null });
  try {
    if (route.name === "config") {
      const { profiles, broken } = await api.profiles();
      set({ profiles, brokenProfiles: broken });
      return;
    }

    if (route.name === "recordings" && !route.recordingId) {
      const { recordings: rows } = await api.recordings();
      set({ recordings: rows, selectedRecording: null });
      return;
    }

    // A recording id in the URL, or the one already open when switching pages.
    const id = route.recordingId ?? get().selectedRecording?.id;
    if (id) {
      const recording = await api.recording(id);
      recording.latest = await latestValues(recording);
      set({ selectedRecording: recording });
    }
  } catch (error) {
    set({ error: error.message });
  }
}

// The last value of every metric, per target. A1.8 replaces this with the live
// stream; until then one request per metric is honest and fast enough for the
// handful a recording holds.
async function latestValues(recording) {
  const latest = {};
  const results = await Promise.all(
    recording.metrics.map((metric) =>
      api.series(recording.id, metric).then((payload) => [metric, payload.series])
    )
  );
  for (const [metric, byTarget] of results) {
    latest[metric] = {};
    for (const [target, points] of Object.entries(byTarget)) {
      if (points.length) latest[metric][target] = points[points.length - 1][1];
    }
  }
  return latest;
}

function render(state) {
  const route = state.route ?? { name: DEFAULT_ROUTE };
  const entry = ROUTES[route.name] ?? ROUTES[DEFAULT_ROUTE];

  document.getElementById("page-title").textContent = entry.title;
  document.getElementById("page-subtitle").textContent = entry.subtitle;
  document.title = `${entry.title} · Metrix`;

  for (const item of document.querySelectorAll("#nav .nav-item[data-route]")) {
    item.classList.toggle("active", item.dataset.route === route.name);
  }

  document.getElementById("view").innerHTML = state.error
    ? `<div class="alert alert-danger">${escape(state.error)}</div>`
    : entry.view.render(state);
}

async function onRouteChange() {
  const route = parseHash();
  set({ route });
  await load(route);
}

async function pollHealth() {
  const dot = document.getElementById("health");
  const text = document.getElementById("health-text");
  try {
    const health = await api.health();
    set({ health });
    dot.className = "status-dot status-dot-animated bg-green";
    text.textContent = `v${health.version}`;
    text.title = health.home;
  } catch {
    dot.className = "status-dot bg-red";
    text.textContent = "API unreachable";
  }
}

subscribe(render);
window.addEventListener("hashchange", onRouteChange);

await pollHealth();
await onRouteChange();
setInterval(pollHealth, 10000);
