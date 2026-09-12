// Wiring: hash routing, data loading, click actions, and one render pass per state
// change.
//
// Views are pure functions of state (state.js) and never touch the network; api.js
// is the only module that does, and stream.js writes into state without knowing
// anything about the DOM.

import { api } from "./api.js";
import * as charts from "./charts.js";
import * as config from "./config.js";
import { escape } from "./format.js";
import * as recordings from "./recordings.js";
import { get, set, subscribe } from "./state.js";
import * as stream from "./stream.js";
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

    // Opening a specific recording, or staying with the one already open when
    // switching between Stats and Charts.
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

// The last value of every metric for a finished recording. A live one gets these
// from the stream instead; this is only for reading history back.
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

async function startObserving(profileName) {
  try {
    const { recording_id: id } = await api.startRecording({ profile: profileName });
    set({ live: { recordingId: id, connection: "live", latest: {}, targets: [], metrics: [] } });
    stream.connect(id);
    location.hash = "#/performance/stats";
  } catch (error) {
    set({ error: error.message });
  }
}

async function stopObserving(recordingId) {
  try {
    await api.stopRecording(recordingId);
    stream.disconnect();
    const recording = await api.recording(recordingId);
    recording.latest = await latestValues(recording);
    set({ live: null, selectedRecording: recording });
  } catch (error) {
    set({ error: error.message });
  }
}

function render(state) {
  try {
    renderView(state);
  } catch (error) {
    // A bug in one view must not freeze the whole page: without this, a thrown
    // render leaves the last markup on screen and every later update dies too.
    console.error("render failed", error);
    document.getElementById("view").innerHTML =
      `<div class="alert alert-danger">The view failed to render: ${escape(
        error.message
      )}</div>`;
  }
}

function renderView(state) {
  const route = state.route ?? { name: DEFAULT_ROUTE };
  const entry = ROUTES[route.name] ?? ROUTES[DEFAULT_ROUTE];

  document.getElementById("page-title").textContent = entry.title;
  document.getElementById("page-subtitle").textContent = entry.subtitle;
  document.title = `${entry.title} · Metrix`;

  for (const item of document.querySelectorAll("#nav .nav-item[data-route]")) {
    item.classList.toggle("active", item.dataset.route === route.name);
  }

  // A live table patches its own cells rather than being rebuilt every second:
  // replacing the markup would destroy text selection and make the Stop button
  // unclickable under the cursor.
  if (!state.error && route.name === "stats" && table.patch(state)) return;

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

// Rejoin a recording that is still running -- a page reload should not orphan it.
async function rejoinLive() {
  try {
    const { live } = await api.live();
    if (live.length) {
      set({ live: { recordingId: live[0], connection: "live", latest: {}, targets: [], metrics: [] } });
      stream.connect(live[0]);
    }
  } catch {
    // Nothing live, or the API is down; pollHealth already says which.
  }
}

// One delegated listener rather than per-render bindings, since the view is replaced
// wholesale on every state change.
document.addEventListener("click", (event) => {
  const button = event.target.closest("[data-action]");
  if (!button) return;
  event.preventDefault();
  if (button.dataset.action === "observe") startObserving(button.dataset.profile);
  if (button.dataset.action === "stop") stopObserving(button.dataset.recording);
});

subscribe(render);
window.addEventListener("hashchange", onRouteChange);

await pollHealth();
await rejoinLive();
await onRouteChange();
setInterval(pollHealth, 10000);
