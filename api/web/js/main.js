// Wiring: hash routing, data loading, click actions, and selective subscriptions
// for the shell, health badge, error banner and active view.
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

function activeRoute(state) {
  return state.route ?? { name: DEFAULT_ROUTE };
}

function activeEntry(state) {
  return ROUTES[activeRoute(state).name] ?? ROUTES[DEFAULT_ROUTE];
}

function selectView(state) {
  const route = activeRoute(state);
  return [route.name, route.recordingId, ...activeEntry(state).view.selectState(state)];
}

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
      await loadProfiles();
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
      // Fetched with the recording rather than on demand: it is the first question
      // asked of a finished one, and a card that appears a second later reads as a
      // page still loading.
      recording.summary = await api.summary(id);
      recording.comparison = await api.comparison(id);
      recording.recovery = await api.recovery(id);
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

// Profiles are files on disk, so the list can go stale while the page is open. The
// only way it refreshes is this, called on arrival and by the Reload button -- the
// timestamp is stored so the button can show that it did something.
async function loadProfiles() {
  const { profiles: rows, broken } = await api.profiles();
  set({ profiles: rows, brokenProfiles: broken, profilesReadAt: Date.now() });
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

function renderShell(state) {
  const route = activeRoute(state);
  const entry = activeEntry(state);

  document.getElementById("page-pretitle").textContent = entry.section;
  document.getElementById("page-title").textContent = entry.title;
  document.getElementById("page-subtitle").textContent = entry.subtitle;
  document.title = `${entry.title} · Metrix`;

  for (const item of document.querySelectorAll("#nav .nav-item[data-route]")) {
    item.classList.toggle("active", item.dataset.route === route.name);
  }
}

function renderView(state) {
  // A live table patches its own cells rather than being rebuilt every second:
  // replacing the markup would destroy text selection and make the Stop button
  // unclickable under the cursor.
  if (activeRoute(state).name === "stats" && table.patch(state)) return;

  document.getElementById("view").innerHTML = activeEntry(state).view.render(state);
}

function renderError(state) {
  const slot = document.getElementById("error");
  if (state.error) {
    slot.innerHTML = `<div class="alert alert-danger">${escape(state.error)}</div>`;
  } else if (state.notice) {
    slot.innerHTML = `<div class="alert alert-success">${escape(state.notice)}</div>`;
  } else {
    slot.innerHTML = "";
  }
}

/** A confirmation that clears itself. It reports that something worked, which stops
 *  being news almost immediately -- unlike an error, which waits to be dealt with. */
let noticeTimer = null;
function notify(message) {
  set({ error: null, notice: message });
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => {
    if (get().notice === message) set({ notice: null });
  }, 4000);
}

async function onRouteChange() {
  const route = parseHash();
  // Leaving the page abandons the draft. Carrying it would mean the editor
  // reappearing later over a profile the person had stopped thinking about.
  set({ route, ...(route.name === "profiles" ? {} : { profileDraft: null }) });
  await load(route);
}

function renderHealth(state) {
  const badge = document.getElementById("health");
  const dot = badge.querySelector(".status-dot");
  const text = document.getElementById("health-text");
  const health = state.health;
  if (health) {
    badge.className = "status status-green";
    badge.title = health.home;
    // Only a reachable API pulses. A dot still animating while nothing answers is
    // the one thing this indicator must never do.
    dot.classList.add("status-dot-animated");
    text.textContent = `v${health.version}`;
  } else {
    badge.className = "status status-red";
    badge.title = "";
    dot.classList.remove("status-dot-animated");
    text.textContent = "API unreachable";
  }
}

async function pollHealth() {
  try {
    set({ health: await api.health() });
  } catch {
    set({ health: null });
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

// Read typing back before intentional editor changes. Background subscriptions
// never rebuild this form, so unrelated API responses preserve the live DOM.
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

async function reloadProfiles() {
  // Only reachable from the list -- the button is not drawn over an open editor,
  // where re-reading the directory would throw away whatever was being typed.
  try {
    set({ error: null });
    await loadProfiles();
  } catch (error) {
    set({ error: error.message });
  }
}

/**
 * Walk discovery again for one profile.
 *
 * Always a fresh walk, never the cache: this only runs because someone pressed the
 * button, and handing back the answer they were trying to get past would be a button
 * that appears to do nothing. It takes a few seconds against a real account, so the
 * button says so while it runs.
 */
async function resolveProfile(name) {
  try {
    set({ error: null, resolving: name });
    await api.resolveProfile(name);
    await loadProfiles();
  } catch (error) {
    set({ error: error.message });
  } finally {
    set({ resolving: null });
  }
}

/**
 * Check that every endpoint in one profile can actually be reached.
 *
 * The result is held in memory and not persisted: reachability is true of a moment,
 * and a green tick from yesterday shown as though it were current would be worse
 * than no tick at all. It disappears on reload, which is correct.
 */
async function verifyProfile(name) {
  try {
    set({ error: null, verifying: name });
    const report = await api.verifyProfile(name);
    set({ verified: { ...get().verified, [name]: report } });
  } catch (error) {
    set({ error: error.message });
  } finally {
    set({ verifying: null });
  }
}

/**
 * Mark a recording as the baseline its series is compared against, or clear it.
 *
 * A refusal is a 409 rather than an error to shrug at: the recording carries an
 * `invalid` annotation, and adopting it would quietly poison every later comparison.
 * The confirmation says what is wrong before offering the override.
 */
async function toggleBaseline(id, isBaseline) {
  try {
    set({ error: null });
    if (isBaseline) {
      await api.clearBaseline(id);
    } else {
      try {
        await api.markBaseline(id);
      } catch (refusal) {
        const message =
          `This recording cannot be a baseline:

${refusal.message}

` +
          `Everything in this series would be measured against it. Use it anyway?`;
        if (!window.confirm(message)) return;
        await api.markBaseline(id, true);
      }
    }
    await onRouteChange();
  } catch (error) {
    set({ error: error.message });
  }
}

/**
 * Order the stats table by a column. Clicking the active one reverses it.
 *
 * Numbers descend first: the reason to sort by p95 is to find the worst row, and
 * making that a second click is one click of friction on the common case. The metric
 * name ascends first, for the same reason in reverse.
 */
function sortTable(column) {
  const current = get().tableSort;
  const first = column === "metric" ? "asc" : "desc";
  const direction =
    current?.key === column ? (current.direction === "asc" ? "desc" : "asc") : first;
  set({ tableSort: { key: column, direction } });
}

/**
 * Put the table somewhere else: the clipboard as TSV, or a file as CSV.
 *
 * Both reflect the order on screen (§14.3), because the common case is pasting these
 * numbers into a ticket right after finding the row that looks wrong.
 */
async function copyTable() {
  const text = table.toDelimited(get(), "	");
  try {
    await navigator.clipboard.writeText(text);
    notify("Table copied as TSV.");
  } catch {
    // Clipboard access can be refused, and a silent no-op looks like a dead button.
    set({ error: "The browser would not allow writing to the clipboard." });
  }
}

function downloadTable() {
  const state = get();
  const id = state.live?.recordingId ?? state.selectedRecording?.id ?? "metrix";
  const blob = new Blob([table.toDelimited(state, ",")], { type: "text/csv" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = `${id}.csv`;
  link.click();
  URL.revokeObjectURL(url);
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

// One delegated listener rather than per-render bindings, since intentional view
// updates replace its markup.
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
  if (action === "profile-reload") reloadProfiles();
  if (action === "profile-resolve") resolveProfile(profile);
  if (action === "profile-verify") verifyProfile(profile);
  if (action === "table-sort") sortTable(button.dataset.column);
  if (action === "table-copy") copyTable();
  if (action === "table-csv") downloadTable();
  if (action === "baseline-toggle") {
    toggleBaseline(recording, button.dataset.baseline === "1");
  }
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

subscribe((state) => [activeRoute(state).name], renderShell);
subscribe((state) => [state.health], renderHealth);
subscribe((state) => [state.error, state.notice], renderError);
subscribe(selectView, render);
renderShell(get());
renderHealth(get());
renderError(get());
render(get());
window.addEventListener("hashchange", onRouteChange);

await pollHealth();
await rejoinLive();
await onRouteChange();
setInterval(pollHealth, 10000);
