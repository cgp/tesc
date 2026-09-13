// What the Profiles page draws for a verified, a discovered, and a watched-only
// endpoint. The view is a pure function of state, so this calls it directly rather
// than through the page wiring -- `rendering.test.mjs` covers the wiring, and what
// is at stake here is the markup one state produces.
import assert from "node:assert/strict";
import test from "node:test";

// `escape` builds an element to do the escaping. This is the whole of the DOM the
// view module needs, and stubbing it beats pulling in a DOM implementation.
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

const { render } = await import("../../web/js/profiles.js");

const PROFILE = {
  name: "staging",
  description: "",
  addressing: "load_balancer",
  discover: null,
  endpoints: [
    {
      id: "alb",
      address: "10.0.1.9:443",
      host_header: "api.example.com",
      load: true,
      transport: "none",
      collects_from: null,
      attributes: {},
    },
    {
      id: "task/3f1c5a7e9b2d4c6f8a0b1c2d3e4f5a6b",
      address: "10.0.11.21:8080",
      host_header: "api.example.com",
      load: false,
      transport: "scrape",
      collects_from: "http://10.0.11.21:9100/metrics",
      attributes: {},
    },
  ],
  observed: ["task/3f1c5a7e9b2d4c6f8a0b1c2d3e4f5a6b"],
  targets: ["alb"],
  inventory: null,
};

function state(patch = {}) {
  return {
    profiles: [PROFILE],
    brokenProfiles: [],
    profilesReadAt: null,
    profileDraft: null,
    resolving: null,
    verifying: null,
    verified: {},
    error: null,
    ...patch,
  };
}

test("a box that takes no traffic is not drawn as a load target", () => {
  const markup = render(state());
  assert.match(markup, /watched only/);
  // The address is still shown -- that is where the box is -- but the count in the
  // subtitle is what says how many places traffic actually goes.
  assert.match(markup, /1 sent to/);
  assert.match(markup, /1 of 2 observed/);
});

test("a long discovered id is shortened on screen and kept in the title", () => {
  const markup = render(state());
  assert.match(markup, /task\/3f1c5a7e…/);
  assert.match(markup, /title="task\/3f1c5a7e9b2d4c6f8a0b1c2d3e4f5a6b"/);
});

test("a check in progress says so and offers no second click", () => {
  const markup = render(state({ verifying: "staging" }));
  assert.match(markup, /Checking…/);
  assert.doesNotMatch(markup, /data-action="profile-verify"/);
});

test("a report shows both answers for each endpoint", () => {
  const markup = render(
    state({
      verified: {
        staging: {
          ok: false,
          summary: "1 of 3 unreachable",
          checked_at: "2026-09-12T23:40:00Z",
          checks: [
            {
              endpoint: "alb",
              kind: "load",
              result: "ok",
              address: "10.0.1.9:443",
              detail: "TLS handshake completed",
              ms: 24.5,
            },
            {
              endpoint: "alb",
              kind: "collect",
              result: "skipped",
              address: "—",
              detail: "no collector configured",
              ms: null,
            },
            {
              endpoint: "task/3f1c5a7e9b2d4c6f8a0b1c2d3e4f5a6b",
              kind: "load",
              result: "skipped",
              address: "10.0.11.21:8080",
              detail: "observed only",
              ms: null,
            },
            {
              endpoint: "task/3f1c5a7e9b2d4c6f8a0b1c2d3e4f5a6b",
              kind: "collect",
              result: "failed",
              address: "http://10.0.11.21:9100/metrics",
              detail: "connection refused",
              ms: 3.1,
            },
          ],
        },
      },
    })
  );

  assert.match(markup, /1 of 3 unreachable/);
  assert.match(markup, /TLS handshake completed/);
  assert.match(markup, /24\.5ms/);
  assert.match(markup, /connection refused/);
  // A check that was never attempted is neither a pass nor a failure.
  assert.doesNotMatch(markup, /badge[^>]*>reachable<\/span> observed only/);
});

test("a failing detail from the API is escaped, not interpreted", () => {
  const markup = render(
    state({
      verified: {
        staging: {
          ok: false,
          summary: "1 of 1 unreachable",
          checked_at: "2026-09-12T23:40:00Z",
          checks: [
            {
              endpoint: "alb",
              kind: "load",
              result: "failed",
              address: "10.0.1.9:443",
              detail: "<script>alert(1)</script>",
              ms: null,
            },
          ],
        },
      },
    })
  );
  assert.match(markup, /&lt;script&gt;/);
  assert.doesNotMatch(markup, /<script>/);
});
