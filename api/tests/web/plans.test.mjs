// What the Plans page draws, and what its form gives back.
//
// Two things are worth pinning here. The page must show the server's verdict rather
// than reach one -- every figure on it arrives computed, and a browser that started
// deciding what a valid mixture looks like would be a second rulebook to drift from
// the first. And `readForm` must hand back the document it was given with only the
// fields the form draws replaced: a plan carries auth, datasets and SLOs that no
// control on this page shows, and rebuilding the document from the inputs alone
// would delete every one of them on the first save.
import assert from "node:assert/strict";
import test from "node:test";

// `escape` builds an element to do the escaping; this is the whole of the DOM these
// views need. The form is read through FormData, which is stubbed over a plain map
// below -- what is under test is the merge, not the browser's form serialisation.
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

class FakeFormData {
  constructor(form) {
    this.fields = form.fields;
  }
  get(name) {
    const value = this.fields[name];
    return Array.isArray(value) ? value[0] : (value ?? null);
  }
  getAll(name) {
    const value = this.fields[name];
    return value == null ? [] : [].concat(value);
  }
}
Object.defineProperty(globalThis, "FormData", { configurable: true, value: FakeFormData });

const { render, readForm, shareFromRate, verdict } = await import("../../web/js/plans.js");

const FIGURES = {
  mode: "fixed",
  model: "open",
  rate: 100,
  duration_s: 60,
  warmup_s: 0,
  measured_s: 60,
  baseline_s: 0,
  settle_s: 0,
  percent_total: 100,
  floor: 2250,
  iterations: 6000,
  requests: 12000,
  supported: true,
  withheld: null,
  chains: [
    {
      name: "browse",
      percent: 100,
      session: "reuse",
      steps: 2,
      iterations_per_s: 100,
      requests_per_s: 200,
      requests: 12000,
      supported: true,
    },
  ],
};

const PLAN = {
  name: "shop",
  chains: [{ name: "browse", percent: 100, steps: 2 }],
  calls: ["search", "add"],
  call_files: ["calls/shop.json"],
  extras: [],
  load: { mode: "fixed", model: "open", rate: 100, duration: "60s" },
  notes: [],
  figures: FIGURES,
  problems: [],
  ready: true,
};

const DOC = {
  version: 1,
  name: "shop",
  calls: ["calls/shop.json"],
  auth: { mode: "basic", username: "u", password: "{{ secret.p }}" },
  datasets: { users: { file: "data/users.csv", mode: "round_robin" } },
  slo: [{ metric: "error_rate", max: 0.001 }],
  phases: { baseline: "0s", settle: "0s" },
  load: { mode: "fixed", model: "open", rate: 100, duration: "60s" },
  chains: [
    {
      name: "browse",
      percent: 100,
      session: "reuse",
      steps: [
        { id: "find", call: "search" },
        { id: "buy", call: "add", delay_ms: 250 },
      ],
    },
  ],
};

function listState(patch = {}) {
  return { plans: [PLAN], brokenPlans: [], plansReadAt: null, planDraft: null, ...patch };
}

function editorState(patch = {}) {
  return {
    plans: [PLAN],
    brokenPlans: [],
    profiles: [{ name: "staging" }],
    planDraft: {
      name: "shop",
      doc: DOC,
      detail: {
        calls: [
          {
            name: "search",
            description: "Full-text search",
            method: "GET",
            path: "/api/search",
            headers: {},
            query: ["q"],
            body: "none",
            uses: ["users.term"],
            extracts: ["pid"],
            assert: [{ status: 200 }],
          },
          {
            name: "add",
            description: null,
            method: "POST",
            path: "/api/cart/{{ pid }}",
            headers: {},
            query: [],
            body: "generator order-xml",
            uses: ["pid"],
            extracts: [],
            assert: [{ status: 201 }],
          },
        ],
        notes: [],
      },
      profile: "staging",
      preview: null,
      error: null,
    },
    planCheck: { ready: true, problems: [], figures: FIGURES },
    ...patch,
  };
}

function form(fields) {
  return { fields };
}

test("a chain's share is shown as the requests it actually buys", () => {
  const markup = render(listState());
  // 100% of 100/s over two steps is 200 req/s. A percentage on its own is not a
  // quantity anybody can judge, which is the reason both are on the row.
  assert.match(markup, /200 req\/s/);
  assert.match(markup, /12,000/);
});

test("the list says which plans can run without opening each one", () => {
  assert.match(render(listState()), /badge bg-green-lt">ready/);
  const failing = {
    ...PLAN,
    ready: false,
    problems: [{ severity: "error", where: "chains", message: "5 short of 100" }],
  };
  assert.match(render(listState({ plans: [failing] })), /1 error/);
});

test("a plan that will not parse is listed with its reason rather than dropped", () => {
  const markup = render(
    listState({ plans: [], brokenPlans: [{ name: "broken", error: "mix.json: invalid JSON" }] })
  );
  assert.match(markup, /broken/);
  assert.match(markup, /invalid JSON/);
});

test("the verdict is the server's words, not a count reached here", () => {
  const markup = render(
    editorState({
      planCheck: {
        ready: false,
        figures: FIGURES,
        problems: [
          { severity: "error", where: "chains", message: "the chain percentages total 95" },
          { severity: "warning", where: "calls", message: "the call 'spare' is unused" },
        ],
      },
    })
  );
  assert.match(markup, /will not run/);
  assert.match(markup, /the chain percentages total 95/);
  assert.match(markup, /the call &#39;spare&#39; is unused/);
});

test("a mixture that does not parse shows no figures rather than the last ones", () => {
  const markup = verdict({
    ready: false,
    figures: null,
    problems: [{ severity: "error", where: "mix.json", message: "load/rate: type" }],
  });
  assert.match(markup, /No figures/);
  assert.doesNotMatch(markup, /12,000/);
});

test("a chain below the sample floor says so beside the chain", () => {
  const thin = {
    ...FIGURES,
    chains: [{ ...FIGURES.chains[0], requests: 400, supported: false }],
  };
  const markup = render(editorState({ planCheck: { ready: true, problems: [], figures: thin } }));
  assert.match(markup, /below the floor/);
});

test("a step shows what its call reads and what it provides", () => {
  const markup = render(editorState());
  assert.match(markup, /reads <code>users.term<\/code>/);
  assert.match(markup, /provides <code>pid<\/code>/);
});

test("what the form does not draw is named as carried rather than left invisible", () => {
  const markup = render(editorState());
  assert.match(markup, /Carried unchanged/);
  assert.match(markup, /<code>auth<\/code>/);
  assert.match(markup, /<code>datasets<\/code>/);
});

test("calls are shown but not editable", () => {
  const markup = render(editorState());
  assert.match(markup, /Read-only/);
  // No input carries a call's own fields: the only controls near a call are the
  // step's id and which call it picks.
  assert.doesNotMatch(markup, /name="calls\./);
});

test("reading the form back keeps every field the form does not draw", () => {
  const next = readForm(
    form({
      "load.mode": "fixed",
      "load.model": "open",
      "load.rate": "150",
      "load.duration": "90s",
      "phases.baseline": "0s",
      "phases.settle": "0s",
      "chains.0.name": "browse",
      "chains.0.percent": "100",
      "chains.0.session": "reuse",
      "chains.0.steps.0.id": "find",
      "chains.0.steps.0.call": "search",
      "chains.0.steps.1.id": "buy",
      "chains.0.steps.1.call": "add",
    }),
    DOC
  );
  assert.deepEqual(next.auth, DOC.auth);
  assert.deepEqual(next.datasets, DOC.datasets);
  assert.deepEqual(next.slo, DOC.slo);
  assert.equal(next.version, 1);
  // And a step keeps what its row does not show, too.
  assert.equal(next.chains[0].steps[1].delay_ms, 250);
  assert.equal(next.load.rate, 150);
  assert.equal(next.load.duration, "90s");
});

test("a blank warmup is absent rather than empty", () => {
  const next = readForm(
    form({
      "load.mode": "fixed",
      "load.duration": "60s",
      "load.warmup": "",
      "chains.0.name": "browse",
      "chains.0.percent": "100",
      "chains.0.steps.0.id": "find",
      "chains.0.steps.0.call": "search",
      "chains.0.steps.1.id": "buy",
      "chains.0.steps.1.call": "add",
    }),
    { ...DOC, load: { ...DOC.load, warmup: "10s" } }
  );
  // The server reads a missing key as "use the default" and an empty one as a value.
  assert.ok(!("warmup" in next.load));
});

test("a pool size is written only for a chain that pools sessions", () => {
  const fields = {
    "load.mode": "fixed",
    "load.duration": "60s",
    "chains.0.name": "browse",
    "chains.0.percent": "100",
    "chains.0.session": "pool",
    "chains.0.pool_size": "50",
    "chains.0.steps.0.id": "find",
    "chains.0.steps.0.call": "search",
    "chains.0.steps.1.id": "buy",
    "chains.0.steps.1.call": "add",
  };
  assert.equal(readForm(form(fields), DOC).chains[0].pool_size, 50);

  const reused = readForm(
    form({ ...fields, "chains.0.session": "reuse" }),
    { ...DOC, chains: [{ ...DOC.chains[0], session: "pool", pool_size: 50 }] }
  );
  // Left behind, it would be a field the engine ignores looking authoritative.
  assert.ok(!("pool_size" in reused.chains[0]));
});

test("a polling step writes one selector, not the one it used to have", () => {
  const previous = {
    ...DOC,
    chains: [
      {
        ...DOC.chains[0],
        steps: [
          {
            id: "find",
            call: "search",
            repeat_until: { json: "$.status", equals: "done", max_attempts: 5, interval_ms: 200 },
          },
        ],
      },
    ],
  };
  const next = readForm(
    form({
      "load.mode": "fixed",
      "load.duration": "60s",
      "chains.0.name": "browse",
      "chains.0.percent": "100",
      "chains.0.steps.0.id": "find",
      "chains.0.steps.0.call": "search",
      "chains.0.steps.0.repeat": "on",
      "chains.0.steps.0.repeat.selector": "header",
      "chains.0.steps.0.repeat.expression": "X-Status",
      "chains.0.steps.0.repeat.equals": "done",
      "chains.0.steps.0.repeat.max_attempts": "8",
      "chains.0.steps.0.repeat.interval_ms": "500",
    }),
    previous
  );
  const repeat = next.chains[0].steps[0].repeat_until;
  assert.deepEqual(repeat, {
    header: "X-Status",
    equals: "done",
    max_attempts: 8,
    interval_ms: 500,
  });
});

test("unticking the poll box removes the block rather than emptying it", () => {
  const previous = {
    ...DOC,
    chains: [
      {
        ...DOC.chains[0],
        steps: [
          {
            id: "find",
            call: "search",
            repeat_until: { json: "$.status", equals: "done", max_attempts: 5, interval_ms: 200 },
          },
        ],
      },
    ],
  };
  const next = readForm(
    form({
      "load.mode": "fixed",
      "load.duration": "60s",
      "chains.0.name": "browse",
      "chains.0.percent": "100",
      "chains.0.steps.0.id": "find",
      "chains.0.steps.0.call": "search",
    }),
    previous
  );
  assert.ok(!("repeat_until" in next.chains[0].steps[0]));
});

test("a rate typed instead of a share becomes the share the document stores", () => {
  assert.equal(shareFromRate(30, 150), 20);
  // No rate to divide by is no answer, not a division by zero.
  assert.equal(shareFromRate(30, 0), null);
});

test("a rate is not written into a mixture whose shape carries it", () => {
  const next = readForm(
    form({
      "load.mode": "breakpoint",
      "load.duration": "60s",
      "chains.0.name": "browse",
      "chains.0.percent": "100",
      "chains.0.steps.0.id": "find",
      "chains.0.steps.0.call": "search",
      "chains.0.steps.1.id": "buy",
      "chains.0.steps.1.call": "add",
    }),
    { ...DOC, load: { mode: "breakpoint", duration: "60s", breakpoint: { max_rate: 500 } } }
  );
  assert.ok(!("rate" in next.load));
  assert.deepEqual(next.load.breakpoint, { max_rate: 500 });
});

test("the bundle says when the form has moved on from the stored plan", () => {
  const clean = render(editorState());
  assert.doesNotMatch(clean, /not saved/);

  const edited = editorState();
  edited.planDraft.saved = DOC;
  edited.planDraft.doc = { ...DOC, load: { ...DOC.load, rate: 250 } };
  // A bundle is assembled from the plan on disk. An export that quietly omits what
  // is on screen is worse than one that refuses.
  assert.match(render(edited), /changes that are not saved/);
});
