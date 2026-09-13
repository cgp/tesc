// Wiring: hash routing, data loading, click actions, and selective subscriptions
// for the shell, health badge, error banner and active view.
//
// Views are pure functions of state (state.js) and never touch the network; api.js
// is the only module that does, and stream.js writes into state without knowing
// anything about the DOM.

import { api } from "./api.js";
import * as charts from "./charts.js";
import * as compare from "./compare.js";
import * as config from "./config.js";
import { escape } from "./format.js";
import * as profiles from "./profiles.js";
import * as recordings from "./recordings.js";
import * as series from "./series.js";
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
  series: {
    section: "Archive",
    title: "Series",
    subtitle: "Is a setup getting better or worse, run by run?",
    view: series,
  },
  compare: {
    section: "Archive",
    // No nav item of its own -- it is always reached from a selection, never from a
    // standing menu entry that would be empty on arrival. `nav` keeps the menu
    // showing where you came from rather than highlighting nothing.
    nav: "series",
    title: "Compare",
    subtitle: "Runs side by side, and where it means anything, merged.",
    view: compare,
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
  return [
    route.name,
    route.recordingId,
    route.seriesKey,
    ...activeEntry(state).view.selectState(state),
  ];
}

// Ticking a run changes nothing that has to be fetched, and rebuilding the view
// would replace the checkbox under the pointer. The view patches itself and only
// falls back to a full render when the markup it expected is not there.
subscribe((state) => [state.selectedRuns], (state) => {
  if (activeRoute(state).name !== "series") return;
  if (!series.patchSelection(state.selectedRuns ?? [])) render(state);
});

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
  if (parts[0] === "compare") {
    // The runs are in the hash so a comparison can be linked to and reopened. The
    // selection that produced it is not -- that is a choice in progress.
    return {
      name: "compare",
      runs: (parts[1] ? decodeURIComponent(parts[1]) : "").split(",").filter(Boolean),
      phase: parts[2] ? decodeURIComponent(parts[2]) : null,
    };
  }
  if (parts[0] === "series") {
    // The key carries pipes and an `=`; the hash holds it encoded and it is decoded
    // once, here, so nothing downstream has to know it was ever escaped.
    return { name: "series", seriesKey: parts[1] ? decodeURIComponent(parts[1]) : null };
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

    if (route.name === "series") {
      await loadSeries(route.seriesKey);
      return;
    }

    if (route.name === "compare") {
      await loadComparison(route.runs, route.phase);
      return;
    }

    if (route.name === "recordings" && !route.recordingId) {
      const filters = get().archive?.filters ?? {};
      const page = await api.recordings(filters);
      set({
        recordings: page.recordings,
        archive: { filters, facets: page.facets, total: page.total, limit: page.limit },
        selectedRecording: null,
      });
      return;
    }

    // Opening a specific recording, or staying with the one already open when
    // switching between Stats and Charts.
    const id = route.recordingId ?? get().selectedRecording?.id;
    if (id) {
      const recording = await api.recording(id);
      // One request for every metric, which also carries what the charts draw behind
      // the lines. It used to be a request per metric for the last value alone.
      recording.chart = await api.series(id);
      recording.latest = latestValues(recording.chart);
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

/**
 * The list of series, or one of them with every metric's trend.
 *
 * One request for the whole series rather than one per chart: the page draws a chart
 * per metric over the same set of runs, and fetching them separately would be a
 * round trip each for data that comes out of a single pass.
 */
async function loadSeries(key) {
  if (!key) {
    const page = await api.seriesList();
    set({ series: page.series, seriesFloor: page.min_runs_for_band, selectedSeries: null });
    return;
  }
  set({ selectedSeries: await api.seriesTrend(key) });
}

/**
 * A comparison, and the samples its overlay is drawn from.
 *
 * The table arrives summarised -- every percentile in this product is computed in
 * one place, server-side, and this page is not an exception. The per-run series are
 * a second set of requests because they are the picture rather than the answer, and
 * they reuse the endpoint the single-run charts already read.
 */
async function loadComparison(runs, phase) {
  if (runs.length < 2) {
    set({ comparison: null, comparisonSeries: null });
    return;
  }
  const comparison = await api.compare(runs, phase);
  set({ comparison, comparisonSeries: null });

  const fetched = await Promise.all(comparison.runs.map((run) => api.series(run.id)));
  const byRun = {};
  comparison.runs.forEach((run, i) => {
    byRun[run.id] = fetched[i];
  });
  set({ comparisonSeries: byRun });
}

/** Tick or untick a run on the series page. */
function toggleRun(id) {
  const chosen = get().selectedRuns ?? [];
  set({
    selectedRuns: chosen.includes(id) ? chosen.filter((r) => r !== id) : [...chosen, id],
  });
}

function compareSelected() {
  const chosen = get().selectedRuns ?? [];
  if (chosen.length < 2) return;
  location.hash = `#/compare/${encodeURIComponent(chosen.join(","))}`;
}

// The window a comparison is read over. In the hash beside the runs, so a link to
// "settle against settle" reopens as that rather than as the whole recording.
function compareOver(phase) {
  const route = activeRoute(get());
  const runs = encodeURIComponent((route.runs ?? []).join(","));
  location.hash = phase ? `#/compare/${runs}/${encodeURIComponent(phase)}` : `#/compare/${runs}`;
}

// The last value of every metric, read out of the series already fetched. A live
// recording gets these from the stream instead; this is only for reading back.
function latestValues(chart) {
  const latest = {};
  for (const [metric, byTarget] of Object.entries(chart.series)) {
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

  const highlight = entry.nav ?? route.name;
  for (const item of document.querySelectorAll("#nav .nav-item[data-route]")) {
    item.classList.toggle("active", item.dataset.route === highlight);
  }

  // Offered only where a page has written one. A help button that opens an empty
  // dialog teaches people that the help button is not worth pressing.
  document.getElementById("help-open").hidden = typeof entry.view.help !== "function";
  closeHelp();
}

/* ------------------------------------------------------------- page explanation */

/**
 * The long version of what a page is for.
 *
 * A dialog rather than a paragraph on the page: the short answer belongs in the
 * subtitle and the long one is read once, by someone who has just arrived, and is in
 * the way every time after that. A native `<dialog>` gives Escape, the focus trap
 * and the inert background without any of it being written here.
 */
function openHelp() {
  const entry = activeEntry(get());
  if (typeof entry.view.help !== "function") return;
  const { title, body } = entry.view.help();
  document.getElementById("help-title").textContent = title;
  document.getElementById("help-body").innerHTML = body;
  document.getElementById("help").showModal();
}

function closeHelp() {
  const dialog = document.getElementById("help");
  if (dialog.open) dialog.close();
}

function renderView(state) {
  // A live table patches its own cells rather than being rebuilt every second:
  // replacing the markup would destroy text selection and make the Stop button
  // unclickable under the cursor.
  if (activeRoute(state).name === "stats" && table.patch(state)) return;
  if (activeRoute(state).name === "charts" && charts.patch(state)) return;

  document.getElementById("view").innerHTML = activeEntry(state).view.render(state);

  // uPlot measures the element it draws into, so the charts are built after their
  // containers exist rather than returned as markup. Feeding an existing chart new
  // data costs an array; rebuilding one costs its crosshair and its zoom.
  if (activeRoute(state).name === "charts") charts.draw(state);
  if (activeRoute(state).name === "series") series.draw(state);
  if (activeRoute(state).name === "compare") compare.draw(state);
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

/**
 * Narrow the archive. Applied by the server, so the answer covers every recording
 * rather than the page that happened to be loaded.
 */
async function filterArchive(name, value) {
  const filters = { ...(get().archive?.filters ?? {}) };
  if (value) filters[name] = value;
  else delete filters[name];
  set({ archive: { ...get().archive, filters } });
  await onRouteChange();
}

async function clearArchiveFilters() {
  set({ archive: { ...get().archive, filters: {} } });
  await onRouteChange();
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
document.getElementById("help-open").addEventListener("click", openHelp);

// Escape, handled rather than assumed. A modal <dialog> is supposed to close itself
// on Escape and mostly does, but it was found not to in one embedded browser -- the
// keydown arrived trusted, no `cancel` event fired, and the dialog stayed open with
// no other way out on the keyboard. Three lines to not depend on it.
document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape") return;
  const dialog = document.getElementById("help");
  if (!dialog.open) return;
  event.preventDefault();
  dialog.close();
});

// Clicking away closes it. The backdrop is not an element, so a click on it arrives
// with the dialog itself as the target -- which is also what a click on the dialog's
// own padding does. Hit-testing the box is what tells the two apart.
document.getElementById("help").addEventListener("click", (event) => {
  const dialog = event.currentTarget;
  if (event.target !== dialog) return;
  const box = dialog.getBoundingClientRect();
  const inside =
    event.clientX >= box.left &&
    event.clientX <= box.right &&
    event.clientY >= box.top &&
    event.clientY <= box.bottom;
  if (!inside) dialog.close();
});

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
  if (action === "archive-clear") clearArchiveFilters();
  if (action === "run-toggle") toggleRun(button.dataset.recording);
  if (action === "compare-runs") compareSelected();
  if (action === "compare-clear") set({ selectedRuns: [] });
  if (action === "compare-phase") compareOver(button.dataset.phase || null);
  if (action === "help-close") closeHelp();
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
  const filter = event.target.closest('[data-change-action="archive-filter"]');
  if (filter) filterArchive(filter.dataset.filter, filter.value.trim());
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
// A canvas does not reflow. Charts are told their new width, and only while the page
// showing them is the one on screen.
window.addEventListener("resize", () => {
  const route = activeRoute(get()).name;
  if (route === "charts") charts.resize();
  if (route === "series") series.resize();
  if (route === "compare") compare.resize();
});

await pollHealth();
await rejoinLive();
await onRouteChange();
setInterval(pollHealth, 10000);
