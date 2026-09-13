// The page explanation, and the ways out of it.
//
// The focus trap and the inert background come from <dialog> itself and are not
// retested here. What is ours is which pages offer the button at all, telling a
// click on the backdrop from a click on the dialog's own padding — both of which
// arrive with the dialog as their target — and Escape, which <dialog> is supposed to
// handle and was found not to.
//
// One harness for the file rather than one per test: `main.js` wires itself at
// import time and a module is imported once per process, so a second harness would
// be a set of listeners nothing is bound to.
import assert from "node:assert/strict";
import test from "node:test";

const documentListeners = new Map();
const windowListeners = new Map();
const elementListeners = new Map();
const elements = new Map();
const classList = { add() {}, remove() {}, toggle() {} };

function make(id) {
  return {
    id,
    classList,
    hidden: false,
    textContent: "",
    innerHTML: "",
    open: false,
    showModal() {
      this.open = true;
    },
    close() {
      this.open = false;
    },
    // The dialog's box, so a click can be placed inside or outside it.
    getBoundingClientRect: () => ({ left: 100, right: 700, top: 50, bottom: 550 }),
    addEventListener(type, listener) {
      elementListeners.set(`${id}:${type}`, listener);
    },
    querySelector: () => ({ classList }),
  };
}

const doc = {
  activeElement: null,
  createElement: () => ({ textContent: "", innerHTML: "" }),
  getElementById(id) {
    if (!elements.has(id)) elements.set(id, make(id));
    return elements.get(id);
  },
  querySelector: () => null,
  querySelectorAll: () => [],
  addEventListener(type, listener) {
    documentListeners.set(type, listener);
  },
};

const BODIES = {
  "/api/health": { version: "1", home: "/metrix", database: "/metrix/db" },
  "/api/recordings/live": { live: [] },
  "/api/series": { series: [], min_runs_for_band: 5 },
  "/api/recordings": { recordings: [], total: 0, limit: 100, facets: {} },
};

Object.defineProperty(globalThis, "fetch", {
  configurable: true,
  value: async (path) => {
    const body = BODIES[String(path).split("?")[0]];
    if (!body) throw new Error(`unexpected fetch: ${path}`);
    return { ok: true, json: async () => body };
  },
});
Object.defineProperty(globalThis, "setInterval", { configurable: true, value: () => {} });
for (const [name, value] of Object.entries({
  document: doc,
  window: {
    addEventListener(type, listener) {
      windowListeners.set(type, listener);
    },
  },
  location: { hash: "#/series" },
})) {
  Object.defineProperty(globalThis, name, { configurable: true, value, writable: true });
}

await import("../../web/js/main.js");

const el = (id) => doc.getElementById(id);
const fire = (id, type, event = {}) => elementListeners.get(`${id}:${type}`)(event);
function press(key) {
  const event = { key, defaulted: false, preventDefault() { this.defaulted = true; } };
  documentListeners.get("keydown")(event);
  return event;
}

const clickAction = (action) =>
  documentListeners.get("click")({
    target: { closest: () => ({ dataset: { action } }) },
    preventDefault() {},
  });

async function goTo(hash) {
  globalThis.location.hash = hash;
  await windowListeners.get("hashchange")();
}

// A click on the backdrop and a click on the dialog's own padding both arrive with
// the dialog as the target; only the coordinates tell them apart.
const clickAt = (x, y) => ({
  currentTarget: el("help"),
  target: el("help"),
  clientX: x,
  clientY: y,
});

test("the button is offered on a page that has written an explanation", async () => {
  await goTo("#/series");
  assert.equal(el("help-open").hidden, false);
});

test("and withheld on one that has not, rather than opening an empty dialog", async () => {
  await goTo("#/config");
  assert.equal(el("help-open").hidden, true);

  fire("help-open", "click");
  assert.equal(el("help").open, false, "pressing it anyway opens nothing");
});

test("opening it fills the dialog from the page itself", async () => {
  await goTo("#/series");
  fire("help-open", "click");

  assert.equal(el("help").open, true);
  assert.match(el("help-title").textContent, /Series/);
  assert.match(el("help-body").innerHTML, /setup identity/);
});

test("clicking outside the dialog closes it", async () => {
  await goTo("#/series");
  fire("help-open", "click");
  fire("help", "click", clickAt(20, 20));
  assert.equal(el("help").open, false);
});

test("clicking the dialog's own padding does not", async () => {
  // The backdrop is not an element, so this click looks identical to the one above
  // apart from where it landed. Closing on it would shut the dialog on anyone
  // reaching for the text.
  await goTo("#/series");
  fire("help-open", "click");
  fire("help", "click", clickAt(300, 300));
  assert.equal(el("help").open, true);
});

test("Escape closes it, without relying on the browser to do it", async () => {
  // A modal <dialog> is supposed to close itself on Escape, and was found not to in
  // one embedded browser: the keydown arrived trusted, no `cancel` fired, and the
  // dialog stayed open with no keyboard way out.
  await goTo("#/series");
  fire("help-open", "click");
  press("Escape");
  assert.equal(el("help").open, false);
});

test("Escape with nothing open is left alone", async () => {
  // Otherwise this handler would swallow the key from anything else that wants it.
  await goTo("#/series");
  const event = press("Escape");
  assert.equal(event.defaulted, false);
});

test("the close button closes it", async () => {
  await goTo("#/series");
  fire("help-open", "click");
  clickAction("help-close");
  assert.equal(el("help").open, false);
});

test("leaving the page closes it, rather than explaining the page behind it", async () => {
  await goTo("#/series");
  fire("help-open", "click");
  await goTo("#/recordings");
  assert.equal(el("help").open, false);
});
