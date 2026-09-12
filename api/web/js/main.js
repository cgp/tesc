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
import * as profiles from "./profiles.js";
import * as recordings from "./recordings.js";
import { get, set, subscribe } from "./state.js";
import * as stream from "./stream.js";
import * as table from "./table.js";

// `section` is the small-caps line above the title: it says which part of the menu
// you are standing in, which the title alone does not once a page is bookmarked.
const ROUTES = {
  config: {
    section: "Setup",
    title: "Config",
    subtitle: "What this process is, and where it keeps what it records.",
    view: config,
  },
  profiles: {
    section: "Setup",
    title: "Profiles",
    subtitle: "The environments a run can be pointed at, and what to watch on each.",
    view: profiles,
  },
  stats: {
    section: "Performance",
    title: "Stats",
    subtitle: "The numbers, exactly, with the sample count behind each one.",
    view: table,
  },
  charts: {
    section: "Performance",
    title: "Charts",
    subtitle: "Shape and timing. Gaps are drawn as gaps, never interpolated.",
    view: charts,
  },
  recordings: {
    section: "Archive",
    title: "Recordings",
    subtitle: "Everything captured, whether or not a load run was attached.",
    view: recordings,
  },
};

const DEFAULT_ROUTE = "recordings";

function parseHash() {
  const path = (location.hash || "").replace(/^#\/?/, "");
  const parts = path.split("/").filter(Boolean);

  if (parts[0] === "config") return { name: "config" };
  if (parts[0] === "profiles") return { name: "profiles" };
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
    if (route.name === "config") return;

    if (route.name === "profiles") {
      const { profiles: rows, broken } = await api.profiles();
      set({ profiles: rows, brokenProfiles: broken });
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

  document.getElementById("page-pretitle").textContent = entry.section;
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
  // Leaving the page abandons the draft. Carrying it would mean the editor
  // reappearing later over a profile the person had stopped thinking about.
  set({ route, ...(route.name === "profiles" ? {} : { profileDraft: null }) });
  await load(route);
}

async function pollHealth() {
  const badge = document.getElementById("health");
  const dot = badge.querySelector(".status-dot");
  const text = document.getElementById("health-text");
  try {
    const health = await api.health();
    set({ health });
    badge.className = "status status-green";
    badge.title = health.home;
    // Only a reachable API pulses. A dot still animating while nothing answers is
    // the one thing this indicator must never do.
    dot.classList.add("status-dot-animated");
    text.textContent = `v${health.version}`;
  } catch {
    badge.className = "status status-red";
    badge.title = "";
    dot.classList.remove("status-dot-animated");
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

/* ------------------------------------------------------------ profile editing */

// The draft is the source of truth, not the DOM: every state change replaces the
// markup, so anything typed has to be read back before a change that re-renders.
// Every mutating action goes through here first.
function syncDraft() {
  const draft = get().profileDraft;
  const form = document.querySelector("[data-profile-form]");
  if (!draft || !form) return draft;
  const synced = { ...draft, doc: profiles.readForm(form, draft.doc) };
  set({ profileDraft: synced });
  return synced;
}

function editDraft(change) {
  const draft = syncDraft();
  if (draft) set({ profileDraft: { ...draft, ...change(draft) } });
}

function newProfile() {
  set({ profileDraft: { mode: "create", name: null, doc: profiles.blankDocument(), error: null } });
}

async function editProfile(name) {
  try {
    // The document as written on disk, not the display summary: what is edited has
    // to be what is saved back, or a round trip would quietly drop fields the
    // summary does not carry.
    const doc = await api.profileDocument(name);
    set({ profileDraft: { mode: "edit", name, doc, error: null } });
  } catch (error) {
    set({ error: error.message });
  }
}

async function saveProfile() {
  const draft = syncDraft();
  if (!draft) return;
  try {
    if (draft.mode === "create") await api.createProfile(draft.doc);
    else await api.replaceProfile(draft.name, draft.doc);
    set({ profileDraft: null });
    await onRouteChange();
  } catch (error) {
    // Back into the form with the message the server gave, which already names the
    // field and the reason. Nothing typed is lost.
    set({ profileDraft: { ...get().profileDraft, error: error.message } });
  }
}

async function deleteProfile(name) {
  const message =
    `Delete the profile "${name}"?

` +
    "Recordings made against it are kept — deleting the profile does not make the " +
    "measurements untrue. This cannot be undone.";
  if (!window.confirm(message)) return;
  try {
    await api.deleteProfile(name);
    set({ profileDraft: null });
    await onRouteChange();
  } catch (error) {
    set({ error: error.message });
  }
}

/* ------------------------------------------------------------------ listeners */

// One delegated listener rather than per-render bindings, since the view is replaced
// wholesale on every state change.
document.addEventListener("click", (event) => {
  const button = event.target.closest("[data-action]");
  if (!button) return;
  event.preventDefault();
  const { action, profile, recording, index } = button.dataset;

  if (action === "observe") startObserving(profile);
  if (action === "stop") stopObserving(recording);
  if (action === "profile-new") newProfile();
  if (action === "profile-edit") editProfile(profile);
  if (action === "profile-delete") deleteProfile(profile);
  if (action === "profile-cancel") set({ profileDraft: null });
  if (action === "profile-save") saveProfile();
  if (action === "endpoint-add") {
    editDraft((draft) => ({
      doc: { ...draft.doc, endpoints: [...draft.doc.endpoints, profiles.blankEndpoint()] },
    }));
  }
  if (action === "endpoint-remove") {
    const at = Number(index);
    editDraft((draft) => ({
      doc: { ...draft.doc, endpoints: draft.doc.endpoints.filter((_, i) => i !== at) },
    }));
  }
});

// Separate from clicks: a select whose value decides which other fields exist has to
// be read on change, and preventing its click would stop the dropdown opening.
document.addEventListener("change", (event) => {
  if (event.target.closest('[data-change-action="draft-reload"]')) syncDraft();
});

// A form with no submit button still submits on Enter, which would reload the page
// and lose the draft. Take it as "save".
document.addEventListener("submit", (event) => {
  if (!event.target.matches("[data-profile-form]")) return;
  event.preventDefault();
  saveProfile();
});

subscribe(render);
window.addEventListener("hashchange", onRouteChange);

await pollHealth();
await rejoinLive();
await onRouteChange();
setInterval(pollHealth, 10000);
