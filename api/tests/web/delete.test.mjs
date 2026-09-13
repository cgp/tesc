// Deleting recordings: what the confirmation says, and what it cannot reach.
//
// Delete is not purge. A purge keeps every figure and drops the request-level
// evidence; this keeps nothing. So the two things worth holding in place are that
// the dialog says which of the two is happening, and that a selection can never
// outlive the rows it was made over — otherwise a filter change turns "delete the
// three I picked" into "delete three I can no longer see".
//
// One harness for the file: `main.js` wires itself at import time and a module is
// imported once per process, so a second harness would bind to nothing.
import assert from "node:assert/strict";
import test from "node:test";

const documentListeners = new Map();
const windowListeners = new Map();
const elements = new Map();
const classList = { add() {}, remove() {}, toggle() {} };

const doc = {
  activeElement: null,
  createElement: () => ({ textContent: "", innerHTML: "" }),
  getElementById(id) {
    if (!elements.has(id)) {
      elements.set(id, {
        id,
        classList,
        hidden: false,
        textContent: "",
        innerHTML: "",
        open: false,
        showModal() {},
        close() {},
        addEventListener() {},
        querySelector: () => ({ classList }),
      });
    }
    return elements.get(id);
  },
  querySelector: () => null,
  querySelectorAll: () => [],
  addEventListener(type, listener) {
    documentListeners.set(type, listener);
  },
};

// What the archive endpoint hands back, swapped per test.
let page = { recordings: [], total: 0, limit: 100, facets: {} };
const deleted = [];

Object.defineProperty(globalThis, "fetch", {
  configurable: true,
  value: async (path, options) => {
    const route = String(path).split("?")[0];
    if (route === "/api/health") {
      return { ok: true, json: async () => ({ version: "1", home: "/m", database: "/m/db" }) };
    }
    if (route === "/api/recordings/live") return { ok: true, json: async () => ({ live: [] }) };
    if (route === "/api/recordings/delete") {
      const body = JSON.parse(options.body);
      deleted.push(body.recording_ids);
      return { ok: true, json: async () => ({ deleted: body.recording_ids }) };
    }
    if (route === "/api/recordings") return { ok: true, json: async () => page };
    throw new Error(`unexpected fetch: ${path}`);
  },
});
Object.defineProperty(globalThis, "setInterval", { configurable: true, value: () => {} });

let asked = null;
let answer = true;
for (const [name, value] of Object.entries({
  document: doc,
  window: {
    addEventListener(type, listener) {
      windowListeners.set(type, listener);
    },
    confirm(message) {
      asked = message;
      return answer;
    },
  },
  location: { hash: "#/recordings" },
})) {
  Object.defineProperty(globalThis, name, { configurable: true, value, writable: true });
}

await import("../../web/js/main.js");
const { get, set } = await import("../../web/js/state.js");

const clickAction = (action) =>
  documentListeners.get("click")({
    target: { closest: () => ({ dataset: { action } }) },
    preventDefault() {},
  });

async function reload() {
  await windowListeners.get("hashchange")();
}

function recording(id, patch = {}) {
  return { id, is_baseline: false, profile: "staging", targets: [], ...patch };
}

async function showing(rows) {
  page = { recordings: rows, total: rows.length, limit: 100, facets: {} };
  await reload();
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

/* ------------------------------------------------------------ the confirmation */

test("the dialog says this is not a purge, because the two lose different things", async () => {
  await showing([recording("r1"), recording("r2")]);
  set({ selectedRecordings: ["r1", "r2"] });
  answer = false;
  clickAction("delete-selected");
  await settle();

  assert.match(asked, /Delete 2 recordings permanently\?/);
  assert.match(asked, /a purge keeps\s+the figures/);
  assert.match(asked, /this keeps nothing/);
  assert.deepEqual(deleted, [], "declining deletes nothing");
});

test("deleting a baseline says whose history loses its reference", async () => {
  // The consequence lands in a series the person deleting is not looking at.
  await showing([recording("r1", { is_baseline: true }), recording("r2")]);
  set({ selectedRecordings: ["r1", "r2"] });
  answer = false;
  clickAction("delete-selected");
  await settle();

  assert.match(asked, /1 of these is a baseline/);
  assert.match(asked, /r1/);
  assert.match(asked, /loses what it is compared against/);
});

test("with no baseline among them the warning is absent rather than empty", async () => {
  await showing([recording("r1"), recording("r2")]);
  set({ selectedRecordings: ["r1"] });
  answer = false;
  clickAction("delete-selected");
  await settle();

  assert.doesNotMatch(asked, /baseline/);
  assert.match(asked, /Delete 1 recording permanently\?/, "singular, not '1 recordings'");
});

/* ---------------------------------------------------------------- the selection */

test("confirming sends exactly the selected ids and clears the selection", async () => {
  await showing([recording("r1"), recording("r2"), recording("r3")]);
  set({ selectedRecordings: ["r1", "r3"] });
  answer = true;
  deleted.length = 0;
  clickAction("delete-selected");
  await settle();

  assert.deepEqual(deleted, [["r1", "r3"]]);
  assert.deepEqual(get().selectedRecordings, []);
});

test("a selection cannot outlive the rows it was made over", async () => {
  // Otherwise narrowing the filter turns "delete the two I picked" into "delete two
  // that are no longer on the screen".
  await showing([recording("r1"), recording("r2")]);
  set({ selectedRecordings: ["r1", "r2"] });

  await showing([recording("r9")]);
  assert.deepEqual(get().selectedRecordings, [], "cleared with the list it described");

  deleted.length = 0;
  clickAction("delete-selected");
  await settle();
  assert.deepEqual(deleted, [], "and nothing is deletable until something is picked again");
});

test("selecting nothing asks nothing and sends nothing", async () => {
  await showing([recording("r1")]);
  asked = null;
  deleted.length = 0;
  clickAction("delete-selected");
  await settle();

  assert.equal(asked, null);
  assert.deepEqual(deleted, []);
});
