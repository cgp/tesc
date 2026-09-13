// Run the real page wiring with a minimal DOM surface. Replacing #view destroys
// the simulated form and focus, just as innerHTML does in a browser. This tests
// subscription behavior without a browser install or a front-end build step.
import assert from "node:assert/strict";
import test from "node:test";

test("background API updates preserve profile fields, focus and selection", async (t) => {
  const documentListeners = new Map();
  const windowListeners = new Map();
  const elements = new Map();
  let form = null;
  let markup = "";
  let replacements = 0;
  let poll;
  let healthFails = false;
  let version = "1";
  const classList = { add() {}, remove() {}, toggle() {} };
  const doc = {
    activeElement: null,
    createElement() {
      return {
        textContent: "",
        get innerHTML() {
          return this.textContent.replaceAll("&", "&amp;").replaceAll("<", "&lt;")
            .replaceAll(">", "&gt;");
        },
      };
    },
    getElementById(id) {
      // `addEventListener` and the <dialog> methods because main.js binds the help
      // dialog at import time; this test is about subscriptions, not about help.
      if (!elements.has(id)) {
        elements.set(id, {
          classList,
          addEventListener() {},
          open: false,
          showModal() {},
          close() {},
        });
      }
      return elements.get(id);
    },
    querySelector(selector) {
      return selector === "[data-profile-form]" ? form : null;
    },
    querySelectorAll() { return []; },
    addEventListener(type, listener) { documentListeners.set(type, listener); },
  };
  elements.set("health", {
    querySelector() { return { classList }; },
  });
  elements.set("view", {
    get innerHTML() { return markup; },
    set innerHTML(value) {
      markup = value;
      replacements += 1;
      doc.activeElement = null;
      form = value.includes("data-profile-form") ? { name: { value: "" } } : null;
    },
  });
  t.mock.method(globalThis, "fetch", async (path) => {
    if (path === "/api/health") {
      if (healthFails) throw new Error("connection refused");
      return { ok: true, json: async () => ({ version, home: "/metrix", database: "/metrix/db" }) };
    }
    if (path === "/api/recordings/live") return { ok: true, json: async () => ({ live: [] }) };
    if (path === "/api/profiles") return { ok: true, json: async () => ({ profiles: [], broken: [] }) };
    throw new Error(`unexpected fetch: ${path}`);
  });
  t.mock.method(globalThis, "setInterval", (callback, interval) => {
    assert.equal(interval, 10000);
    poll = callback;
  });
  // Restore all browser globals when finished, including ones Node doesn't define.
  for (const [name, value] of Object.entries({
    document: doc,
    window: { addEventListener(type, listener) { windowListeners.set(type, listener); } },
    location: { hash: "#/profiles" },
  })) {
    const previous = Object.getOwnPropertyDescriptor(globalThis, name);
    Object.defineProperty(globalThis, name, { configurable: true, value, writable: true });
    t.after(() => {
      if (previous) Object.defineProperty(globalThis, name, previous);
      else delete globalThis[name];
    });
  }

  await import("../../web/js/main.js");
  const { get, set } = await import("../../web/js/state.js");
  function action(name) {
    documentListeners.get("click")({
      target: { closest() { return { dataset: { action: name } }; } },
      preventDefault() {},
    });
  }

  action("profile-new");
  const originalForm = form;
  originalForm.name.value = "new-server";
  originalForm.name.selectionStart = 4;
  originalForm.name.selectionEnd = 10;
  doc.activeElement = originalForm.name;
  const before = replacements;

  // Exercise actual timer -> fetch -> shared state -> subscriptions wiring.
  version = "2";
  await poll();
  assert.equal(elements.get("health-text").textContent, "v2");
  healthFails = true;
  await poll();
  assert.equal(elements.get("health-text").textContent, "API unreachable");
  healthFails = false;
  await poll();
  const summaries = (n) => ({ "*": { "cpu.busy": { metric: "cpu.busy", n, p50: n, supported: true } } });
  set({
    live: {
      latest: { "cpu.busy": { host: 12 } },
      targets: ["host"],
      metrics: ["cpu.busy"],
      summaries: summaries(12),
      spans: {},
    },
  });
  set({ profiles: [], brokenProfiles: [], profilesReadAt: Date.now(), resolving: "other-server" });
  set({ error: "Failed <request>" });
  assert.match(elements.get("error").innerHTML, /Failed &lt;request&gt;/);
  set({ error: null });

  assert.equal(replacements, before, "background updates must not replace #view");
  assert.equal(form, originalForm);
  assert.equal(form.name.value, "new-server");
  assert.equal(doc.activeElement, originalForm.name);
  assert.equal(form.name.selectionStart, 4);
  assert.equal(form.name.selectionEnd, 10);

  // Intentional editor changes still render the new draft; cancelling returns
  // to the list and uses the profile state received while the editor was open.
  const draft = get().profileDraft;
  set({ profileDraft: { ...draft, doc: { ...draft.doc, name: "new-server" } } });
  assert.match(markup, /value="new-server"/);
  action("profile-cancel");
  assert.equal(form, null);
  assert.match(markup, /No profiles yet/);

  // Switching views updates dependencies: Config responds to health, and Stats
  // responds to live samples while ignoring unrelated health polling.
  location.hash = "#/config";
  await windowListeners.get("hashchange")();
  assert.match(markup, /Storage/);
  healthFails = true;
  await poll();
  assert.match(markup, /The API is not answering/);
  healthFails = false;
  await poll();
  assert.match(markup, /Storage/);
  location.hash = "#/performance/stats";
  await windowListeners.get("hashchange")();
  assert.match(markup, /cpu.busy/);
  const statsBefore = replacements;
  await poll();
  assert.equal(replacements, statsBefore);
  set({ live: { ...get().live, summaries: summaries(34) } });
  assert.ok(replacements > statsBefore, "Stats must still respond to live updates");
});
