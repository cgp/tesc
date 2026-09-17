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
import { bytes as formatBytes, count, escape } from "./format.js";
import * as plans from "./plans.js";
import * as profiles from "./profiles.js";
import * as recordings from "./recordings.js";
import * as schemas from "./schemas.js";
import * as series from "./series.js";
import { detectSource } from "./sources.js";
import { get, set, subscribe } from "./state.js";
import * as stream from "./stream.js";
import * as sweep from "./sweep.js";
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
  schemas: {
    section: "Setup",
    title: "Schemas",
    subtitle: "Uploaded service descriptions, parsed into the calls they define.",
    view: schemas,
  },
  profiles: {
    section: "Setup",
    title: "Profiles",
    subtitle: "The environments a run can be pointed at, and what to watch on each.",
    view: profiles,
  },
  plans: {
    section: "Setup",
    title: "Plans",
    subtitle: "What a run sends: chains of calls, and each one's share of the load.",
    view: plans,
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
  sweep: {
    section: "Archive",
    // Reached from a recording, never from the menu: a sweep is a question about one
    // recording, and a standing menu entry would be empty on arrival.
    nav: "recordings",
    title: "Sweep",
    subtitle: "One plan, many boxes, one window. Which of them is the odd one out?",
    view: sweep,
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
    route.phase,
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
  if (parts[0] === "schemas") {
    return { name: "schemas", schema: parts[1] ? decodeURIComponent(parts[1]) : null };
  }
  if (parts[0] === "profiles") return { name: "profiles" };
  // The plan being edited is in the hash, so an editor can be linked to and reopened
  // where it was. What is typed into it is not -- that is work in progress.
  if (parts[0] === "plans") return { name: "plans", plan: parts[1] ?? null };
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
  if (parts[0] === "sweep") {
    // The phase is in the hash beside the recording, so a link to "the boxes during
    // settle" reopens as that rather than as the measured window.
    return {
      name: "sweep",
      recordingId: parts[1] ? decodeURIComponent(parts[1]) : null,
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

    if (route.name === "schemas") {
      await loadSchemas(route.schema);
      return;
    }

    if (route.name === "profiles") {
      await loadProfiles();
      return;
    }

    if (route.name === "plans") {
      await loadPlans();
      if (route.plan) await openPlan(route.plan);
      else set({ planDraft: null, planCheck: null });
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

    if (route.name === "sweep") {
      set({ sweep: route.recordingId ? await api.sweep(route.recordingId, route.phase) : null });
      return;
    }

    if (route.name === "recordings" && !route.recordingId) {
      const filters = get().archive?.filters ?? {};
      const page = await api.recordings(filters);
      set({
        recordings: page.recordings,
        archive: { filters, facets: page.facets, total: page.total, limit: page.limit },
        selectedRecording: null,
        // Cleared with the rows it was made over. A selection that survives a filter
        // change is a way to delete recordings that are no longer on the screen.
        selectedRecordings: [],
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

async function loadSchemas(entryId = null) {
  const listed = await api.schemas();
  const schemaDetail = entryId ? await api.schema(entryId) : null;
  set({ schemas: listed.schemas, schemaDetail, schemasReadAt: Date.now() });
}

/**
 * The plan directory, and the profiles a bundle could be assembled against.
 *
 * Both, because the plan page cannot offer a bundle without somewhere to point it:
 * a plan says what to send and a profile says where, and neither is a run on its own.
 */
async function loadPlans() {
  const [listed, profileList, schemaList] = await Promise.all([
    api.plans(), api.profiles(), api.schemas(),
  ]);
  set({
    plans: listed.plans,
    brokenPlans: listed.broken,
    plansReadAt: Date.now(),
    profiles: profileList.profiles,
    brokenProfiles: profileList.broken,
    schemas: schemaList.schemas,
  });
}

/**
 * Open one plan for editing.
 *
 * The document as written on disk, not the summary: the summary drops every field
 * the form does not draw, and saving a round trip through it would quietly delete an
 * auth block. The call details come with it because a chain is unreadable without
 * knowing what its steps do.
 */
async function openPlan(name) {
  try {
    const [doc, detail] = await Promise.all([api.planDocument(name), api.plan(name)]);
    set({
      planDraft: {
        name,
        doc,
        editorMode:
          detail.ready && plans.basicCompatible(doc, detail.call_details)
            ? "basic"
            : "advanced",
        // What is on disk, kept beside what is being typed: a bundle is assembled
        // from the stored plan, and the page has to be able to say when the two have
        // come apart.
        saved: doc,
        // The whole response, not a copy of the fields the view happens to read
        // today. Narrowing it here means every field the route grows has to be
        // remembered in two places, and the one that was forgotten is the one that
        // does not appear on screen.
        detail,
        profile: get().profiles[0]?.name ?? null,
        preview: null,
        error: null,
      },
      planCheck: { ready: detail.ready, problems: detail.problems, figures: detail.figures },
    });
  } catch (error) {
    set({ error: error.message });
  }
}

function newPlanEditor() {
  const doc = plans.blankDocument();
  set({
    planDraft: {
      mode: "create",
      name: "",
      doc,
      schemaId: null,
      schemaCalls: [],
      selectedCalls: [],
      editorMode: "advanced",
      saved: null,
      detail: { call_details: [], calls: [] },
      profile: get().profiles[0]?.name ?? null,
      preview: null,
      error: null,
    },
    planCheck: null,
  });
}

// Read typing back into the draft. Every committed field goes through here before it
// is checked, so what the server judges is what is on the screen.
function syncPlan() {
  const draft = get().planDraft;
  const form = document.querySelector("[data-plan-form]");
  if (!draft || !form) return draft;
  const doc = plans.readForm(form, draft.doc);
  const name = form.elements["plan.name"]?.value.trim() || draft.name;
  const synced = { ...draft, name, doc: { ...doc, name } };
  set({ planDraft: synced });
  return synced;
}

function editPlanDraft(change) {
  const draft = syncPlan();
  if (draft) set({ planDraft: { ...draft, ...change(draft) } });
}

async function choosePlanSchema(schemaId) {
  const draft = syncPlan();
  if (!draft) return;
  if (!schemaId) {
    set({
      planDraft: {
        ...draft,
        schemaId: null,
        schemaCalls: [],
        selectedCalls: [],
        doc: { ...draft.doc, chains: [] },
      },
      planCheck: null,
    });
    return;
  }
  try {
    const schema = await api.schema(schemaId);
    const current = get().planDraft;
    set({
      planDraft: {
        ...current,
        schemaId,
        schemaCalls: schema.calls ?? [],
        selectedCalls: [],
        doc: { ...current.doc, chains: [] },
        error: null,
      },
      planCheck: null,
    });
  } catch (error) {
    set({ planDraft: { ...get().planDraft, error: error.message } });
  }
}

function togglePlanSchemaCall(name, selected) {
  const draft = syncPlan();
  if (!draft || draft.mode !== "create") return;
  const names = new Set(draft.selectedCalls ?? []);
  if (selected) names.add(name);
  else names.delete(name);
  const selectedCalls = [...names];
  editPlanDraft((current) => ({
    selectedCalls,
    doc: {
      ...current.doc,
      chains: plans.chainsForCalls(current.doc.chains ?? [], selectedCalls),
    },
  }));
  set({ planCheck: null });
}

/**
 * Ask the server what is wrong with the draft as it stands.
 *
 * On every committed field rather than on save, because the whole point of the
 * figures is to be read while the duration and the rate are being chosen. The
 * browser computes none of this: one validator, server-side, and the same one the
 * bundle is gated on.
 */
async function checkPlan() {
  const draft = get().planDraft;
  if (!draft) return;
  if (draft.mode === "create") {
    set({ planCheck: null });
    return;
  }
  try {
    set({ planCheck: await api.validatePlan(draft.name, draft.doc) });
  } catch (error) {
    // A document the schema rejects has no figures to show. Saying so is better than
    // leaving the last ones up, which would be numbers for a document that is no
    // longer on the screen.
    set({
      planCheck: {
        ready: false,
        figures: null,
        problems: [{ severity: "error", where: "mix.json", message: error.message }],
      },
    });
  }
}

async function savePlan() {
  const draft = syncPlan();
  if (!draft) return;
  try {
    if (draft.mode === "create" && (!draft.name || !draft.schemaId)) {
      throw new Error("enter a plan name and choose a stored schema before saving");
    }
    const saved = draft.mode === "create"
      ? await api.createPlan({
          name: draft.name,
          schema_id: draft.schemaId,
          calls: draft.selectedCalls ?? [],
          mix: draft.doc,
        })
      : await api.replacePlan(draft.name, draft.doc);
    set({
      planDraft: { ...get().planDraft, saved: draft.doc, error: null },
      planCheck: { ready: saved.ready, problems: saved.problems, figures: saved.figures },
    });
    notify(
      saved.ready
        ? "Saved. This mixture assembles into a bundle."
        : "Saved with errors — it will not run until they are fixed."
    );
    await loadPlans();
  } catch (error) {
    // Back into the form with the server's message, which already names the field.
    // Nothing typed is lost.
    set({ planDraft: { ...get().planDraft, error: error.message } });
  }
}

async function reloadPlans() {
  try {
    set({ error: null });
    await loadPlans();
  } catch (error) {
    set({ error: error.message });
  }
}

/**
 * A percentage typed as a rate.
 *
 * The document stores shares, so an iterations/s has to become one before it can be
 * submitted. Written into the percentage box first and read back from the form with
 * everything else, so there is one path from the form to the document.
 */
function shareFromRate(input) {
  const form = input.form;
  const index = input.name.split(".")[1];
  const percent = form.elements[`chains.${index}.percent`];
  const rate = Number(form.elements["load.rate"]?.value);
  const share = plans.shareFromRate(Number(input.value), rate);
  if (percent && share != null) percent.value = String(share);
}

/** Show what the bundle holds, without downloading it. */
async function previewBundle() {
  const draft = syncPlan();
  if (!draft?.profile) return;
  try {
    set({ error: null });
    const preview = await api.bundle(draft.name, draft.profile);
    set({ planDraft: { ...get().planDraft, preview } });
  } catch (error) {
    set({ error: error.message, planDraft: { ...get().planDraft, preview: null } });
  }
}

/**
 * Download the bundle as a zip.
 *
 * Through fetch rather than a link, because a plan with errors in it is refused with
 * a message naming them, and a link would navigate the page to that message instead
 * of showing it beside the plan.
 */
async function downloadBundle() {
  const draft = syncPlan();
  if (!draft?.profile) return;
  try {
    set({ error: null });
    const { blob } = await api.bundleArchive(draft.name, draft.profile);
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = `${draft.name}-${draft.profile}.zip`;
    link.click();
    URL.revokeObjectURL(url);
    notify("Bundle downloaded. It unpacks to the directory the engine takes.");
  } catch (error) {
    set({ error: error.message });
  }
}

/**
 * Turn a pasted description into a plan, or into new calls for one.
 *
 * Both go through the same panel because they are the same act: read the service,
 * write the mechanical half. The difference is what is kept — a new plan gets a
 * starter mixture, a regeneration leaves the tuned one exactly where it was.
 */
async function runDescribe() {
  const panel = get().planNew;
  const form = document.querySelector("[data-describe-form]");
  if (!panel || !form) return;
  const fields = plans.readDescribe(form);
  const pending = { ...panel, ...fields, error: null };
  set({ planNew: pending });

  if (!fields.content.trim()) {
    set({ planNew: { ...pending, error: "Paste the document first." } });
    return;
  }
  try {
    if (panel.mode === "regenerate") {
      const result = await api.regenerateCalls(panel.plan, {
        source: fields.source,
        content: fields.content,
      });
      set({ planNew: null });
      notify(
        result.added.length || result.removed.length
          ? `Calls replaced: ${count(result.added.length, "new call")}, ` +
              `${result.removed.length} gone. The mixture is untouched.`
          : "Calls replaced. Nothing about the service had changed."
      );
      await onRouteChange();
      return;
    }
    const created = await api.generatePlan(fields);
    set({ planNew: null });
    notify(
      `Generated ${created.name} from ${fields.source} — ` +
        `${count(created.draft?.todos?.length ?? 0, "thing")} to decide.`
    );
    location.hash = `#/plans/${encodeURIComponent(created.name)}`;
  } catch (error) {
    // Back into the panel with the server's message and whatever was pasted. A
    // rejected twelve-thousand-line document must not have to be pasted twice.
    set({ planNew: { ...pending, error: error.message } });
  }
}

/** Stop marking a generated plan as unreviewed. */
async function acceptDraft() {
  const draft = get().planDraft;
  if (!draft) return;
  try {
    set({ error: null });
    await api.acceptDraft(draft.name);
    notify("Marked reviewed. It is a plan like any other now.");
    await onRouteChange();
  } catch (error) {
    set({ error: error.message });
  }
}

/**
 * Send this plan at the chosen profile, and watch while it runs.
 *
 * The stored plan, not the form: a run is assembled from what is on disk, which is
 * why the card says so when the two have come apart. Landing on the live table
 * rather than staying here, because the next thing worth looking at is the numbers.
 */
async function runPlan() {
  const draft = syncPlan();
  if (!draft?.profile) return;
  try {
    set({ error: null });
    const started = await api.startRecording({
      profile: draft.profile,
      plan: draft.name,
    });
    set({
      live: {
        recordingId: started.recording_id,
        connection: "live",
        latest: {},
        targets: [],
        metrics: [],
      },
    });
    stream.connect(started.recording_id);
    notify(`Running ${draft.name} against ${draft.profile}.`);
    location.hash = "#/performance/stats";
  } catch (error) {
    // The server refuses a plan that cannot run and names the reasons; it is the
    // same gate the bundle is behind, so the message is one somebody can act on.
    set({ error: error.message });
  }
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
  // The plan editor patches its own figures: every committed field re-checks the
  // mixture, and rebuilding the form each time would take the focus out from under
  // whoever is tabbing through the chains.
  if (activeRoute(state).name === "plans" && plans.patch(state)) return;

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
  set({
    route,
    ...(route.name === "schemas" ? {} : { schemaDetail: null }),
    ...(route.name === "profiles" ? {} : { profileDraft: null }),
    // Leaving the page abandons the draft, for the same reason: an editor that
    // reappears later over a plan somebody had stopped thinking about is worse than
    // one that closes.
    ...(route.name === "plans" ? {} : { planDraft: null, planCheck: null, planNew: null }),
  });
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
  set({
    profileDraft: {
      mode: "create",
      name: null,
      doc: profiles.blankDocument(),
      endpointEdit: 0,
      error: null,
    },
  });
}

async function editProfile(name) {
  try {
    // The document as written on disk, not the display summary: what is edited has
    // to be what is saved back, or a round trip would quietly drop fields the
    // summary does not carry.
    const doc = await api.profileDocument(name);
    set({ profileDraft: { mode: "edit", name, doc, endpointEdit: null, error: null } });
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

function editEndpointResolution(index, change) {
  const draft = get().profileDraft;
  if (!draft) return;
  const current = draft.endpointResolution?.[index] ?? {};
  const next = typeof change === "function" ? change(current) : change;
  set({
    profileDraft: {
      ...draft,
      endpointResolution: { ...(draft.endpointResolution ?? {}), [index]: next },
    },
  });
}

async function resolveEndpointHosts(index) {
  const draft = syncDraft();
  const endpoint = draft?.doc.endpoints[index];
  if (!endpoint) return;
  const hostname = profiles.endpointHost(endpoint.address);
  console.debug("[metrix] ALB resolve start", {
    index,
    endpoint: endpoint.id,
    address: endpoint.address,
    hostname,
    document: endpoint,
  });
  if (!hostname) {
    console.debug("[metrix] ALB resolve skipped: no hostname", { index, endpoint });
    editEndpointResolution(index, { hostname, error: "Enter the ALB hostname first." });
    return;
  }
  editEndpointResolution(index, { hostname, loading: true, hosts: [], tests: {} });
  try {
    const result = await api.resolveDiscovery({ hostname });
    console.debug("[metrix] ALB resolve response", {
      index,
      hostname,
      partial: result.partial,
      reached: result.inventory?.reached,
      resources: result.inventory?.resources,
      hosts: result.hosts,
    });
    const seen = new Set();
    const hosts = (result.hosts ?? []).filter((host) => {
      if (!host.address || seen.has(host.address)) return false;
      seen.add(host.address);
      return true;
    });
    if (hosts.length && !endpoint.collect?.host) {
      editDraft((current) => ({
        doc: {
          ...current.doc,
          endpoints: current.doc.endpoints.map((candidate, candidateIndex) =>
            candidateIndex === index
              ? { ...candidate, collect: { ...(candidate.collect ?? {}), host: hosts[0].address } }
              : candidate
          ),
        },
      }));
      console.debug("[metrix] ALB resolve selected first host for saving", {
        index,
        host: hosts[0].address,
      });
    }
    editEndpointResolution(index, { hostname, loading: false, hosts, tests: {} });
  } catch (error) {
    console.debug("[metrix] ALB resolve error", { index, hostname, error });
    editEndpointResolution(index, { hostname, loading: false, hosts: [], error: error.message });
  }
}

async function testEndpointHost(index, host) {
  const draft = syncDraft();
  const endpoint = draft?.doc.endpoints[index];
  if (!endpoint || !host) return;
  editEndpointResolution(index, (resolution) => ({
    ...resolution,
    testing: host,
    tests: resolution.tests ?? {},
  }));
  try {
    const sshUser = draft.doc.observe?.ssh_user?.trim();
    const result = await api.verifyCollector({
      ...endpoint,
      collect: {
        ...(endpoint.collect ?? {}),
        transport: "ssh",
        host,
        ...(sshUser ? { user: sshUser } : {}),
      },
    });
    editEndpointResolution(index, (resolution) => ({
      ...resolution,
      testing: null,
      tests: { ...(resolution.tests ?? {}), [host]: result },
    }));
  } catch (error) {
    editEndpointResolution(index, (resolution) => ({
      ...resolution,
      testing: null,
      tests: {
        ...(resolution.tests ?? {}),
        [host]: { ok: false, check: { detail: error.message } },
      },
    }));
  }
}

async function testEndpointFrontend(index) {
  const draft = syncDraft();
  const endpoint = draft?.doc.endpoints[index];
  if (!endpoint) return;
  editEndpointResolution(index, (resolution) => ({
    ...resolution,
    frontendTesting: true,
    frontendTest: null,
  }));
  try {
    const result = await api.verifyLoad(endpoint);
    editEndpointResolution(index, (resolution) => ({
      ...resolution,
      frontendTesting: false,
      frontendTest: result,
    }));
  } catch (error) {
    editEndpointResolution(index, (resolution) => ({
      ...resolution,
      frontendTesting: false,
      frontendTest: { ok: false, check: { detail: error.message } },
    }));
  }
}

/**
 * Drop a recording's request-level bulk, after saying exactly what that costs.
 *
 * The confirmation names a measured size rather than an estimate, and lists what
 * survives as well as what goes: a dialog that only enumerates losses reads as though
 * everything is being lost, and one that guesses at the number teaches people to stop
 * reading dialogs -- which is expensive on the one that mattered.
 */
async function purgeRecording(id) {
  try {
    set({ error: null });
    const what = await api.purgeable(id);
    if (!what.anything) {
      notify(
        "Nothing to purge: this recording holds no request-level data. Host samples " +
          "are not bulk and are never dropped."
      );
      return;
    }
    // Every line break lives inside a template literal: a plain string cannot span
    // lines, and this message is mostly lines.
    const list = (items) =>
      items
        .map(
          (item) => `
  — ${item}`
        )
        .join("");
    const message =
      `Drop the request-level data for ${id}?

This frees ${formatBytes(what.bytes)} across ${count(what.files, "file")}.

It removes:${list(what.drops)}

It keeps:${list(what.keeps)}

This cannot be undone.`;
    if (!window.confirm(message)) return;

    const result = await api.purge(id);
    notify(`Purged ${formatBytes(result.bytes)}. Every figure is unchanged.`);
    await onRouteChange();
  } catch (error) {
    set({ error: error.message });
  }
}

/**
 * Delete recordings outright — the archive is the wrong place for them.
 *
 * Different from a purge, and deliberately so. A purge keeps the answer and drops
 * the evidence; this says the run should not be in the history at all: a bad run, or
 * one nobody is interested in any more. Nothing survives it, which is why it names
 * what it is about to take.
 *
 * A baseline is called out by name. Deleting one leaves every later recording in its
 * series with nothing to be compared against, and that consequence lands somewhere
 * the person deleting is not looking.
 */
async function deleteSelectedRecordings() {
  const ids = get().selectedRecordings ?? [];
  if (!ids.length) return;
  const baselines = get()
    .recordings.filter((r) => ids.includes(r.id) && r.is_baseline)
    .map((r) => r.id);
  const warning = baselines.length
    ? `

${count(baselines.length, "of these is a baseline", "of these are baselines")}:
  ${baselines.join(`
  `)}
Its series loses what it is compared against.`
    : "";
  const message =
    `Delete ${count(ids.length, "recording")} permanently?

This removes their files, measurements, notes and archive entries — a purge keeps
the figures and drops only the request-level data; this keeps nothing.${warning}

This cannot be undone.`;
  if (!window.confirm(message)) return;
  try {
    set({ error: null });
    const result = await api.deleteSelected(ids);
    set({ selectedRecordings: [] });
    notify(`Deleted ${count(result.deleted.length, "recording")}.`);
    await onRouteChange();
  } catch (error) {
    set({ error: error.message });
  }
}

function toggleRecordingSelection(id, selected) {
  const chosen = get().selectedRecordings ?? [];
  set({
    selectedRecordings: selected
      ? chosen.includes(id) ? chosen : [...chosen, id]
      : chosen.filter((recordingId) => recordingId !== id),
  });
}

function toggleVisibleRecordingSelection(selected) {
  const visible = get().recordings.map((recording) => recording.id);
  const chosen = get().selectedRecordings ?? [];
  set({
    selectedRecordings: selected
      ? [...new Set([...chosen, ...visible])]
      : chosen.filter((id) => !visible.includes(id)),
  });
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

async function uploadSchemaFiles(files, source, form) {
  if (!files.length) {
    set({ error: "Choose a schema file to upload." });
    return;
  }
  const button = form.querySelector('button[type="submit"]');
  const zone = form.querySelector("[data-schema-drop-zone]");
  button.disabled = true;
  zone?.classList.add("is-uploading");
  try {
    const results = await Promise.allSettled(
      files.map(async (file) =>
        (() => file.text().then((content) => api.uploadSchema({
          filename: file.name,
          source: detectSource(content, source),
          content,
        })))()
      )
    );
    const uploaded = results.filter((result) => result.status === "fulfilled").map((result) => result.value);
    const failed = results
      .flatMap((result, index) =>
        result.status === "rejected" ? [`${files[index].name}: ${result.reason.message}`] : []
      );
    form.elements.file.value = "";
    if (uploaded.length) await loadSchemas();
    if (failed.length) {
      set({ error: `Some files were not uploaded: ${failed.join("; ")}` });
    } else {
      notify(`Uploaded ${uploaded.map((entry) => entry.filename).join(", ")}.`);
    }
  } catch (error) {
    set({ error: error.message });
  } finally {
    button.disabled = false;
    zone?.classList.remove("is-uploading");
  }
}

function schemaDropTypes(event) {
  return Array.from(event.dataTransfer?.types ?? []).map(String);
}

function isFileDrop(event) {
  return schemaDropTypes(event).includes("Files");
}

function schemaDropDebug(event, extra = {}) {
  const files = event.dataTransfer?.files;
  const detail = {
    event: event.type,
    types: schemaDropTypes(event),
    file_count: files?.length ?? 0,
    ...extra,
  };
  console.info("Metrix schema upload drag-and-drop", detail);
  return `${detail.event}: types=${detail.types.join(",") || "none"}; files=${detail.file_count}`;
}

function disableSchemaDrop(reason, diagnostic) {
  set({ schemaDrop: { enabled: false, reason, diagnostic } });
}

async function deleteSchema(entryId, filename) {
  if (!window.confirm(`Delete the uploaded schema "${filename}"? This cannot be undone.`)) {
    return;
  }
  try {
    await api.deleteSchema(entryId);
    if (activeRoute(get()).schema === entryId) {
      location.hash = "#/schemas";
    } else {
      await loadSchemas();
    }
    notify(`Deleted ${filename}.`);
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
  const zone = event.target?.closest?.("[data-schema-drop-zone]");
  if (zone && (event.key === "Enter" || event.key === " ")) {
    event.preventDefault();
    zone.closest("form").elements.file.click();
    return;
  }
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

// The dashed zone is an additional file-picker target for mouse and keyboard use.
// A real file input remains in the header, so drag and drop is not the only path.
document.addEventListener("click", (event) => {
  const zone = event.target.closest("[data-schema-drop-zone]");
  if (!zone) return;
  event.preventDefault();
  zone.closest("form").elements.file.click();
});

document.addEventListener("dragenter", (event) => {
  const zone = event.target.closest("[data-schema-drop-zone]");
  if (!zone || !isFileDrop(event)) return;
  event.preventDefault();
  schemaDropDebug(event);
  zone.classList.add("is-dragging");
});

document.addEventListener("dragover", (event) => {
  const zone = event.target.closest("[data-schema-drop-zone]");
  if (!zone) return;
  event.preventDefault();
  if (!isFileDrop(event)) return;
  event.dataTransfer.dropEffect = "copy";
});

document.addEventListener("dragleave", (event) => {
  const zone = event.target.closest("[data-schema-drop-zone]");
  if (!zone || zone.contains(event.relatedTarget)) return;
  zone.classList.remove("is-dragging");
});

document.addEventListener("drop", (event) => {
  const zone = event.target.closest("[data-schema-drop-zone]");
  if (!zone) return;
  event.preventDefault();
  zone.classList.remove("is-dragging");
  const diagnostic = schemaDropDebug(event);
  const files = Array.from(event.dataTransfer?.files ?? []);
  if (!files.length) {
    disableSchemaDrop(
      "The drop arrived without readable files, often because this workspace blocks file drag-and-drop.",
      diagnostic
    );
    return;
  }
  const form = zone.closest("form");
  uploadSchemaFiles(files, form.elements.source.value, form);
});

document.addEventListener("click", (event) => {
  const button = event.target.closest("[data-action]");
  if (!button) return;
  event.preventDefault();
  const { action, profile, plan, recording, schema, filename, index } = button.dataset;

  if (action === "observe") startObserving(profile);
  if (action === "stop") stopObserving(recording);
  if (action === "profile-new") newProfile();
  if (action === "profile-edit") editProfile(profile);
  if (action === "profile-delete") deleteProfile(profile);
  if (action === "profile-reload") reloadProfiles();
  if (action === "profile-resolve") resolveProfile(profile);
  if (action === "profile-verify") verifyProfile(profile);
  if (action === "schema-view") location.hash = `#/schemas/${encodeURIComponent(schema)}`;
  if (action === "schema-back") location.hash = "#/schemas";
  if (action === "schema-delete") deleteSchema(schema, filename);
  if (action === "archive-clear") clearArchiveFilters();
  if (action === "purge") purgeRecording(button.dataset.recording);
  if (action === "delete-selected") deleteSelectedRecordings();
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
  if (action === "plan-reload") reloadPlans();
  if (action === "plan-new") newPlanEditor();
  if (action === "calls-regenerate") {
    set({ planNew: { mode: "regenerate", plan: get().planDraft?.name, source: "openapi" } });
  }
  if (action === "describe-cancel") set({ planNew: null });
  if (action === "describe-run") runDescribe();
  if (action === "draft-accept") acceptDraft();
  if (action === "plan-edit") location.hash = `#/plans/${encodeURIComponent(plan)}`;
  if (action === "plan-cancel") location.hash = "#/plans";
  if (action === "plan-save") savePlan();
  if (action === "plan-run") runPlan();
  if (action === "bundle-preview") previewBundle();
  if (action === "bundle-download") downloadBundle();
  if (action === "plan-mode") {
    editPlanDraft(() => ({ editorMode: button.dataset.mode }));
  }
  if (action === "basic-row-add") {
    editPlanDraft((draft) => ({
      doc: {
        ...draft.doc,
        chains: [
          ...(draft.doc.chains ?? []),
          plans.blankBasicChain(
            draft.detail?.call_details?.[0]?.name,
            draft.doc.chains ?? []
          ),
        ],
      },
    }));
    checkPlan();
  }
  if (action === "basic-row-remove") {
    const chains = get().planDraft?.doc.chains ?? [];
    if (chains.length > 1) {
      editPlanDraft((draft) => ({
        doc: { ...draft.doc, chains: draft.doc.chains.filter((_, i) => i !== Number(index)) },
      }));
      checkPlan();
    }
  }
  if (action === "chain-add") {
    editPlanDraft((draft) => ({
      doc: {
        ...draft.doc,
        chains: [
          ...(draft.doc.chains ?? []),
          plans.blankChain(draft.detail?.calls[0]?.name),
        ],
      },
    }));
    checkPlan();
  }
  if (action === "chain-remove") {
    const at = Number(index);
    editPlanDraft((draft) => ({
      doc: { ...draft.doc, chains: draft.doc.chains.filter((_, i) => i !== at) },
    }));
    checkPlan();
  }
  if (action === "step-add") {
    editPlanDraft((draft) => ({
      doc: {
        ...draft.doc,
        chains: draft.doc.chains.map((chain, i) =>
          i === Number(index)
            ? { ...chain, steps: [...chain.steps, plans.blankStep(draft.detail?.calls[0]?.name)] }
            : chain
        ),
      },
    }));
    checkPlan();
  }
  if (action === "step-remove") {
    editPlanDraft((draft) => ({
      doc: {
        ...draft.doc,
        chains: draft.doc.chains.map((chain, i) =>
          i === Number(index)
            ? { ...chain, steps: chain.steps.filter((_, s) => s !== Number(button.dataset.step)) }
            : chain
        ),
      },
    }));
    checkPlan();
  }
  if (action === "step-move") {
    editPlanDraft((draft) => ({
      doc: {
        ...draft.doc,
        chains: draft.doc.chains.map((chain, i) =>
          i === Number(index)
            ? { ...chain, steps: moved(chain.steps, Number(button.dataset.step), button.dataset.move) }
            : chain
        ),
      },
    }));
    checkPlan();
  }
  if (action === "profile-cancel") set({ profileDraft: null });
  if (action === "profile-save") saveProfile();
  if (action === "endpoint-edit") {
    editDraft(() => ({ endpointEdit: Number(index) }));
  }
  if (action === "endpoint-done") {
    editDraft(() => ({ endpointEdit: null }));
  }
  if (action === "endpoint-resolve") resolveEndpointHosts(Number(index));
  if (action === "endpoint-test-host") {
    testEndpointHost(Number(index), button.dataset.host);
  }
  if (action === "endpoint-test-load") testEndpointFrontend(Number(index));
  if (action === "endpoint-add") {
    editDraft((draft) => ({
      doc: { ...draft.doc, endpoints: [...draft.doc.endpoints, profiles.blankEndpoint()] },
      endpointEdit: draft.doc.endpoints.length,
    }));
  }
  if (action === "endpoint-remove") {
    const at = Number(index);
    editDraft((draft) => ({
      doc: { ...draft.doc, endpoints: draft.doc.endpoints.filter((_, i) => i !== at) },
      endpointEdit:
        draft.endpointEdit === at
          ? null
          : draft.endpointEdit != null && draft.endpointEdit > at
            ? draft.endpointEdit - 1
            : draft.endpointEdit,
      endpointResolution: {},
    }));
  }
});

// Separate from clicks: a select whose value decides which other fields exist has to
// be read on change, and preventing its click would stop the dropdown opening.
/**
 * Reorder a chain's steps.
 *
 * Order is the chain: step two reads what step one extracted, so moving one is a
 * change to what the run does rather than to how it is displayed. The check that
 * follows is what says whether the variables still line up.
 */
function moved(steps, index, direction) {
  const to = direction === "up" ? index - 1 : index + 1;
  if (to < 0 || to >= steps.length) return steps;
  const next = [...steps];
  [next[index], next[to]] = [next[to], next[index]];
  return next;
}

document.addEventListener("change", (event) => {
  const schemaFile = event.target.closest("[data-schema-upload-form] input[type=file]");
  if (schemaFile?.files?.length === 1) {
    schemaFile.files[0].text().then((content) => {
      const detected = detectSource(content);
      if (detected) schemaFile.form.elements.source.value = detected;
    });
  }

  if (event.target.closest('[data-change-action="draft-reload"]')) syncDraft();

  const schema = event.target.closest('[data-plan-form] select[name="plan.schema"]');
  if (schema) {
    choosePlanSchema(schema.value);
    return;
  }
  const schemaCall = event.target.closest("[data-schema-call]");
  if (schemaCall) {
    togglePlanSchemaCall(schemaCall.dataset.schemaCall, schemaCall.checked);
    return;
  }

  // The source select changes the hint under it, and the panel holds a document
  // somebody pasted: read the whole panel back before redrawing it.
  const describing = event.target.closest("[data-describe-form] select");
  if (describing) {
    const form = describing.form;
    set({ planNew: { ...get().planNew, ...plans.readDescribe(form) } });
  }

  const field = event.target.closest("[data-plan-form] input, [data-plan-form] select");
  if (field) {
    if (field.dataset.basicType != null) {
      const call = field.form.elements[`basic.${field.dataset.index}.call`];
      const matching = Array.from(call?.options ?? []).find(
        (option) => option.dataset.requestType === field.value
      );
      if (matching) call.value = matching.value;
    }
    if (field.dataset.derives === "percent") shareFromRate(field);
    if (field.name === "bundle.profile") {
      // A different profile is a different targets.json, so whatever preview is on
      // screen is now for a bundle nobody asked about.
      editPlanDraft(() => ({ profile: field.value, preview: null }));
    } else {
      syncPlan();
    }
    checkPlan();
  }
  const filter = event.target.closest('[data-change-action="archive-filter"]');
  if (filter) filterArchive(filter.dataset.filter, filter.value.trim());
  const recording = event.target.closest('[data-change-action="recording-select"]');
  if (recording) toggleRecordingSelection(recording.dataset.recording, recording.checked);
  const all = event.target.closest('[data-change-action="recording-select-all"]');
  if (all) toggleVisibleRecordingSelection(all.checked);
});

document.addEventListener("input", (event) => {
  const textarea = event.target.closest("[data-describe-form] textarea");
  if (!textarea) return;
  const detected = detectSource(textarea.value);
  if (detected) {
    textarea.form.elements.source.value = detected;
    set({ planNew: { ...get().planNew, source: detected } });
  }
});

// A form with no submit button still submits on Enter, which would reload the page
// and lose the draft. Take it as "save".
document.addEventListener("submit", (event) => {
  if (event.target.matches("[data-schema-upload-form]")) {
    event.preventDefault();
    uploadSchemaFiles(
      Array.from(event.target.elements.file.files ?? []),
      event.target.elements.source.value,
      event.target
    );
  }
  if (event.target.matches("[data-profile-form]")) {
    event.preventDefault();
    saveProfile();
  }
  if (event.target.matches("[data-plan-form]")) {
    event.preventDefault();
    savePlan();
  }
  if (event.target.matches("[data-describe-form]")) {
    event.preventDefault();
    runDescribe();
  }
});

subscribe((state) => [activeRoute(state).name], renderShell);
subscribe((state) => [state.health], renderHealth);
subscribe((state) => [state.error, state.notice], renderError);
subscribe(selectView, render);
set({ schemaDrop: schemas.dropCapability(window) });
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
