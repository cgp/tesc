// The recordings archive: what a row says before anyone opens it, and what the page
// says about what it is not showing.
import assert from "node:assert/strict";
import test from "node:test";

Object.defineProperty(globalThis, "document", {
  configurable: true,
  value: {
    createElement() {
      return {
        textContent: "",
        get innerHTML() {
          return this.textContent
            .replaceAll("&", "&amp;")
            .replaceAll("<", "&lt;")
            .replaceAll(">", "&gt;");
        },
      };
    },
  },
});

const { render } = await import("../../web/js/recordings.js");

function recording(patch = {}) {
  return {
    id: "2026-09-13T00-00-00Z_aaaa",
    kind: "observation",
    status: "finished",
    profile: "staging",
    addressing_mode: "load_balancer",
    series_key: "observation|staging|load_balancer|1s|api=0.0.0",
    duration_ms: 60000,
    started_at: "2026-09-13T00:00:00Z",
    is_baseline: false,
    targets: ["box-a"],
    annotations_by_severity: {},
    worst: null,
    ...patch,
  };
}

/** The detail page for one recording, with only the fields a test cares about. */
function detail(patch = {}) {
  return render({
    selectedRecording: {
      ...recording(patch),
      metrics: [],
      phases: [],
      annotations: [],
      gaps: [],
      targets: ["box-a"],
      summary: null,
      comparison: null,
      recovery: null,
      inventory: null,
      hosts: {},
      disks: {},
    },
    recordings: [],
    archive: { filters: {}, facets: {}, total: 0 },
  });
}

function view(recordings, archive = {}) {
  return render({
    selectedRecording: null,
    recordings,
    archive: { filters: {}, facets: {}, total: recordings.length, ...archive },
  });
}

test("a clean recording says so, rather than saying nothing", () => {
  const markup = view([recording()]);
  assert.match(markup, />clean</);
});

test("an invalid recording is marked, and says why it matters", () => {
  const markup = view([
    recording({ worst: "invalid", annotations_by_severity: { invalid: 1 } }),
  ]);
  assert.match(markup, /bg-red-lt/);
  assert.match(markup, /cannot become a baseline/);
});

test("several notes are counted rather than listed", () => {
  const markup = view([
    recording({ worst: "warn", annotations_by_severity: { warn: 3, info: 1 } }),
  ]);
  // Three warnings, not four notes. A load run carries a routine info annotation
  // per interval, and counting those in an orange badge asks somebody to act on a
  // hundred observations that are working exactly as intended.
  assert.match(markup, /warn\s*×3/);
  assert.match(markup, /3 of 4 notes in all/);
});

test("a run whose only notes are routine is not dressed up as a problem", () => {
  const markup = view([
    recording({ worst: "info", annotations_by_severity: { info: 128 } }),
  ]);
  assert.match(markup, /bg-blue-lt/);
  assert.match(markup, /info\s*×128/);
  // Nothing to qualify: every note it carries is the severity on the badge.
  assert.doesNotMatch(markup, /in all/);
});

test("a truncated list says how much of the archive it is showing", () => {
  const markup = view([recording()], { total: 40 });
  assert.match(markup, /1 of 40 shown/);
});

test("a filtered list says the rows matched rather than that they are all there is", () => {
  const markup = view([recording()], { total: 40, filters: { severity: "invalid" } });
  assert.match(markup, /1 of 40 match/);
});

test("a filter that matches everything still says it is a filter", () => {
  // "2 captured" under an active filter reads as though nothing were filtered, and
  // the next thing that gets doubted is the filter.
  const markup = view([recording(), recording({ id: "b" })], {
    total: 2,
    filters: { q: "staging" },
  });
  assert.match(markup, /2 of 2 match/);
  assert.doesNotMatch(markup, /2 captured/);
});

test("filters come back selected, so the page shows what it is doing", () => {
  const markup = view([recording()], {
    total: 3,
    filters: { kind: "observation", baseline: "true" },
    facets: { kind: ["observation", "load"], profile: ["staging"], status: ["finished"] },
  });
  assert.match(markup, /<option value="observation" selected>/);
  assert.match(markup, /<option value="true" selected>/);
  // The choices stay offered even while one is in use.
  assert.match(markup, /<option value="load">/);
});

test("an empty archive and an empty filter result are different states", () => {
  const fresh = view([], { total: 0 });
  assert.match(fresh, /No recordings yet/);
  assert.doesNotMatch(fresh, /archive-filter/, "no filters to offer over nothing");

  const narrowed = view([], { total: 12, filters: { q: "nope" } });
  assert.match(narrowed, /Nothing matches/);
  assert.match(narrowed, /archive of 12/);
  assert.match(narrowed, /archive-clear/);
  assert.match(narrowed, /archive-filter/, "the filters stay, so they can be undone");
});

test("Clear is offered only when there is something to clear", () => {
  assert.match(view([recording()]), /data-action="archive-clear" disabled/);
  assert.doesNotMatch(
    view([recording()], { filters: { kind: "observation" } }),
    /data-action="archive-clear" disabled/
  );
});

test("the archive offers selection checkboxes and disables deletion until selected", () => {
  const markup = view([recording()]);
  assert.match(markup, /data-change-action="recording-select"/);
  assert.match(markup, /data-action="delete-selected"[\s\S]*disabled/);
  assert.match(markup, /data-change-action="recording-select-all"/);
});

test("selected recordings enable the archive delete action", () => {
  const row = recording();
  const markup = render({
    selectedRecording: null,
    recordings: [row],
    selectedRecordings: [row.id],
    archive: { filters: {}, facets: {}, total: 1 },
  });
  assert.doesNotMatch(markup, /data-action="delete-selected"[\s\S]*disabled/);
});

test("a search term is escaped back into the box, not interpreted", () => {
  const markup = view([recording()], { filters: { q: '"><script>' } });
  assert.match(markup, /&quot;&gt;&lt;script&gt;/);
  assert.doesNotMatch(markup, /<script>/);
});

/* ------------------------------------------------------- exporting and purging */

function opened(patch = {}) {
  return render({
    selectedRecording: {
      ...recording(),
      metrics: ["cpu.busy"],
      annotations: [],
      gaps: [],
      identity: {},
      filesystems: [],
      inventory: null,
      ...patch,
    },
    recordings: [],
    archive: { filters: {}, facets: {}, total: 1 },
  });
}

test("all three exports are offered, and the report opens rather than downloads", () => {
  // The first thing anyone does with a report is look at it.
  const markup = opened();
  assert.match(markup, /report\.html" target="_blank"/);
  assert.match(markup, /export\.csv\?kind=summary" download/);
  assert.match(markup, /export\.csv\?kind=series" download/);
  assert.match(markup, /export\.json" download/);
});

test("the summary export says it carries the sample count", () => {
  assert.match(opened(), /sample count beside every figure/);
});

test("a recording that still holds its request data offers the purge", () => {
  const markup = opened();
  assert.match(markup, /data-action="purge"/);
  assert.match(markup, /Every figure, note and trend stays/);
  assert.match(markup, />kept</);
});

test("one already purged says so instead of offering it again", () => {
  const markup = opened({ purged_at: "2026-09-13T10:00:00Z" });
  assert.doesNotMatch(markup, /data-action="purge"/);
  assert.match(markup, /purged/);
  assert.match(markup, /Every figure on this page is unaffected/);
});

test("the delete button counts what it will take once something is picked", () => {
  const row = recording();
  const markup = render({
    selectedRecording: null,
    recordings: [row, recording({ id: "b" })],
    selectedRecordings: [row.id, "b"],
    archive: { filters: {}, facets: {}, total: 2 },
  });
  assert.match(markup, /Delete 2/);
  // Said on the button, not only in the dialog: the difference between the two
  // destructive actions should be legible before either is pressed.
  assert.match(markup, /A purge keeps the figures; this does not/);
});

test("a launched run says what sent the traffic, hash and all", () => {
  const markup = detail({
    plan_name: "checkout",
    plan_hash: "sha256:2991b369",
    engine_exit_code: 0,
  });
  assert.match(markup, /#\/plans\/checkout/);
  // The hash, not only the name: two runs of "checkout" are the same plan only if
  // the bytes the engine read were the same.
  assert.match(markup, /sha256:2991b369/);
  assert.match(markup, /engine exit 0/);
});

test("an observation carries no plan row at all", () => {
  assert.doesNotMatch(detail({}), />Plan</);
});
