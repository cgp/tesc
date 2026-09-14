// What the sweep view draws. The page must show the server's verdict rather than
// reach one, and it must never leave a blank where a verdict is missing: an empty
// cell in a ranked table reads as a pass, which is the one thing this page must not
// say about a box nobody measured.
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

const { render, help } = await import("../../web/js/sweep.js");

function standing(patch = {}) {
  return {
    target_id: "task-0",
    rank: 1,
    value: 40.0,
    n: 30,
    invalid: false,
    baseline_value: null,
    baseline_n: 0,
    centre: 41.0,
    band: 4.0,
    peers: 6,
    outside: false,
    worse: false,
    flagged: false,
    judged: true,
    unjudged_because: null,
    ...patch,
  };
}

const ODD = standing({
  target_id: "task-3",
  rank: 7,
  value: 82.0,
  outside: true,
  worse: true,
  flagged: true,
});

function sweep(patch = {}) {
  return {
    recording_id: "2026-09-01T00-00-00Z_abcd",
    phase: "measure",
    phases: ["baseline", "measure"],
    min_peers: 5,
    judged: true,
    boxes: [
      {
        target_id: "task-0",
        position: 1,
        address: "10.0.0.0:80",
        attributes: { instance_type: "m6i.large", image_digest: "sha256:new" },
      },
      {
        target_id: "task-3",
        position: 4,
        address: "10.0.0.3:80",
        attributes: { instance_type: "m6i.large", image_digest: "sha256:old" },
      },
    ],
    metrics: [
      {
        metric: "cpu.busy",
        worse: "up",
        judged: true,
        standings: [standing(), ODD],
      },
    ],
    findings: [
      {
        metric: "cpu.busy",
        target_id: "task-3",
        standing: ODD,
        previously_flagged: 4,
        history: [
          { recording_id: "a", at: "2026-08-28T00:00:00Z", value: 81, n: 30, rank: 7, targets: 7, flagged: true },
          { recording_id: "b", at: "2026-08-29T00:00:00Z", value: 83, n: 30, rank: 7, targets: 7, flagged: true },
          { recording_id: "c", at: "2026-08-30T00:00:00Z", value: 82, n: 30, rank: 7, targets: 7, flagged: true },
          { recording_id: "d", at: "2026-08-31T00:00:00Z", value: 80, n: 30, rank: 7, targets: 7, flagged: true },
        ],
      },
    ],
    ...patch,
  };
}

function state(patch = {}) {
  return { sweep: sweep(), selectedRecording: null, ...patch };
}

test("the answer comes before the tables it came out of", () => {
  const markup = render(state());
  assert.ok(
    markup.indexOf("The odd ones out") < markup.indexOf("The boxes"),
    "the finding should be above the evidence"
  );
  assert.match(markup, /task-3/);
  assert.match(markup, /worse than the other boxes/);
});

test("a box flagged in every earlier sweep is not described as unlucky", () => {
  const markup = render(state());
  assert.match(markup, /flagged in 4 — every one of them/);
});

test("a box no earlier sweep carried says that, rather than showing nothing", () => {
  const one = sweep();
  one.findings[0].history = [];
  one.findings[0].previously_flagged = 0;
  const markup = render(state({ sweep: one }));
  assert.match(markup, /No earlier sweep in this series carried this box/);
});

test("a baseline is quoted where there is one, because it changes the reading", () => {
  const one = sweep();
  one.findings[0].standing = { ...ODD, baseline_value: 70, baseline_n: 5 };
  assert.match(render(state({ sweep: one })), /already at[\s\S]*before the traffic started/);

  // And silent where the recording has no separate baseline phase: an invented
  // reassurance is worse than none.
  assert.doesNotMatch(render(state()), /before the traffic started/);
});

test("a sweep too small to have a spread is ranked and says so", () => {
  const small = sweep({
    judged: false,
    findings: [],
    metrics: [
      {
        metric: "cpu.busy",
        worse: "up",
        judged: false,
        standings: [
          standing({
            centre: null,
            band: null,
            peers: 1,
            judged: false,
            unjudged_because: "1 other box(es) with a figure; 4 more would give the sweep a spread",
          }),
        ],
      },
    ],
  });
  const markup = render(state({ sweep: small }));
  assert.match(markup, /Ranked, not judged/);
  assert.match(markup, /a spread needs 5\s+other boxes with a figure/);
  // No accusation anywhere on the page.
  assert.doesNotMatch(markup, /odd one out/);
});

test("a row without a verdict says which condition it missed", () => {
  const thin = sweep();
  thin.metrics[0].standings = [
    standing({
      value: null,
      n: 1,
      judged: false,
      centre: null,
      band: null,
      unjudged_because: "too few samples on this box to have a median",
    }),
  ];
  const markup = render(state({ sweep: thin }));
  assert.match(markup, /too few samples on this box/);
  // The figure itself is withheld rather than drawn as a zero.
  assert.match(markup, /class="unsupported"/);
});

test("each row draws the band it was actually judged against", () => {
  // Not one band per metric: every box is compared with the others, so the
  // reference shifts from row to row and a shared band would shade a bar that was
  // still called out.
  const shifted = sweep();
  shifted.metrics[0].standings = [
    standing({ centre: 41, band: 4 }),
    standing({ target_id: "task-1", rank: 2, centre: 45, band: 9 }),
  ];
  const markup = render(state({ sweep: shifted }));
  assert.equal(markup.match(/metrix-band/g).length, 2);
  assert.equal(markup.match(/metrix-centre/g).length, 2);
});

test("which direction is good is stated rather than assumed", () => {
  const memory = sweep();
  memory.metrics[0] = { metric: "mem.available_bytes", worse: "down", judged: true, standings: [standing()] };
  assert.match(render(state({ sweep: memory })), /Higher is better here/);
  assert.match(render(state()), /Lower is better here/);
});

test("one box is not a sweep, and the page says why rather than drawing an empty table", () => {
  const single = sweep({ boxes: [sweep().boxes[0]] });
  const markup = render(state({ sweep: single }));
  assert.match(markup, /One box is not a sweep/);
  assert.doesNotMatch(markup, /The odd ones out/);
});

test("a sweep with nothing odd in it says that, rather than showing an empty list", () => {
  const clean = sweep({ findings: [] });
  const markup = render(state({ sweep: clean }));
  assert.match(markup, /no outliers/);
  // And says what that claim covers: this window, not the boxes in general.
  assert.match(markup, /statement about this window/);
});

test("the attributes are shown as columns, because that is where the answer usually is", () => {
  const markup = render(state());
  assert.match(markup, /instance type/);
  assert.match(markup, /image digest/);
  assert.match(markup, /sha256:old/);
});

test("every phase in the recording can be read, and the open one is marked", () => {
  const markup = render(state());
  assert.match(markup, /#\/sweep\/[^/]+\/baseline/);
  assert.match(markup, /btn-primary"\s*\n?\s*href="#\/sweep\/[^"]*measure"/);
});

test("the help explains the leave-one-out rather than leaving it as a surprise", () => {
  const body = help().body;
  assert.match(body, /never against a set\s+containing itself/);
  assert.match(body, /outside the band is not the verdict/i);
});
