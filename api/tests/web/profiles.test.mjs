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

const { parseSshDestination, readForm, render, sshDestination } =
  await import("../../web/js/profiles.js");

const PROFILE = {
  name: "staging",
  description: "",
  addressing: "load_balancer",
  discover: null,
  endpoints: [
    {
      id: "alb",
      addressing: "alb",
      address: "10.0.1.9:443",
      host_header: "api.example.com",
      load: true,
      transport: "none",
      collects_from: null,
      attributes: {},
    },
    {
      id: "task/3f1c5a7e9b2d4c6f8a0b1c2d3e4f5a6b",
      addressing: "fargate",
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

test("the editor puts the editable name in the header and endpoints in a table", () => {
  const markup = render(
    state({
      profileDraft: {
        mode: "edit",
        name: "staging",
        endpointEdit: 0,
        error: null,
        doc: {
          name: "staging",
          description: "Customer-facing environment",
          observe: { interval: "1s", collect: ["cpu", "memory"] },
          endpoints: [
            {
              id: "edge",
              addressing: "elb",
              address: "edge.example.com:443",
              collect: { transport: "ssh", user: "deploy" },
            },
          ],
        },
      },
    })
  );

  assert.match(markup, /class="metrix-profile-title"/);
  assert.match(markup, /name="name"[^>]*value="staging"/);
  assert.match(markup, /metrix-endpoint-table/);
  assert.match(markup, />SSH target</);
  assert.match(markup, /data-action="endpoint-add"/);
  assert.match(markup, /data-action="endpoint-done"/);
  assert.match(markup, /classic ELB must be entered explicitly/);
  assert.match(markup, /name="endpoints\.0\.collect\.ssh"/);
  assert.match(markup, /value="deploy@edge\.example\.com:22"/);
  assert.doesNotMatch(markup, /SSH host \(optional\)|SSH user \(optional\)|Port \(22\)/);
  assert.doesNotMatch(markup, /metrix-endpoint-detail/);
  assert.match(markup, /metrix-profile-settings/);
  assert.match(markup, /What is collected/);
  assert.match(markup, /SSH username/);
  assert.doesNotMatch(markup, />Name<span/);
});

test("an endpoint display row has edit and remove actions", () => {
  const markup = render(
    state({
      profileDraft: {
        mode: "edit",
        name: "staging",
        endpointEdit: null,
        error: null,
        doc: {
          name: "staging",
          endpoints: [
            { id: "a", addressing: "alb", address: "a.example.com:443", collect: { transport: "ssh" } },
            { id: "b", addressing: "ecs", address: "10.0.0.2:8080", collect: { transport: "ssh" } },
          ],
        },
      },
    })
  );
  assert.equal((markup.match(/data-action="endpoint-edit"/g) ?? []).length, 2);
  assert.equal((markup.match(/data-action="endpoint-remove"/g) ?? []).length, 2);
  assert.equal((markup.match(/data-action="endpoint-resolve"/g) ?? []).length, 1);
});

test("an ALB resolution shows read-only candidates with one selected SSH host", () => {
  const markup = render(
    state({
      profileDraft: {
        mode: "edit",
        name: "staging",
        endpointEdit: null,
        error: null,
        endpointResolution: {
          0: {
            hostname: "a.example.com",
            hosts: [
              { id: "task/a", address: "10.0.11.21", role: "task" },
              { id: "task/b", address: "10.0.12.34", role: "task" },
            ],
            tests: {
              "10.0.11.21": { ok: true, check: { detail: "ip-10-0-11-21" } },
            },
          },
        },
        doc: {
          name: "staging",
          endpoints: [
            {
              id: "a",
              addressing: "alb",
              address: "a.example.com:443",
              collect: { transport: "ssh", host: "10.0.11.21", user: "deploy" },
            },
          ],
        },
      },
    })
  );

  assert.match(markup, /Resolved SSH hosts/);
  assert.match(markup, /Read-only · choose one host/);
  assert.equal((markup.match(/type="radio"/g) ?? []).length, 2);
  assert.equal((markup.match(/name="endpoints\.0\.collect\.selectedHost"/g) ?? []).length, 2);
  assert.match(markup, /value="10\.0\.11\.21"[^>]*checked/);
  assert.equal((markup.match(/data-action="endpoint-test-host"/g) ?? []).length, 2);
  assert.match(markup, /Enable SSH collection/);
  assert.match(markup, /reachable/);
  assert.match(markup, /ip-10-0-11-21/);
});

test("the selected ALB candidate is the one collector host saved", () => {
  const previous = {
    name: "staging",
    endpoints: [
      {
        id: "edge",
        addressing: "alb",
        address: "edge.example.com:443",
        collect: { transport: "none" },
      },
    ],
  };
  const next = readForm(
    {
      fields: {
        name: "staging",
        "endpoints.0.id": "edge",
        "endpoints.0.addressing": "alb",
        "endpoints.0.address": "edge.example.com:443",
        "endpoints.0.collect.alb": "1",
        "endpoints.0.collect.enabled": "1",
        "endpoints.0.collect.selectedHost": "10.0.12.34",
        "endpoints.0.collect.user": "deploy",
      },
    },
    previous
  );

  assert.deepEqual(next.endpoints[0].collect, {
    transport: "ssh",
    host: "10.0.12.34",
    user: "deploy",
  });
});

test("the observation username is saved as a profile-wide default", () => {
  const next = readForm(
    {
      fields: {
        name: "staging",
        "observe.interval": "1s",
        "observe.ssh_user": "ubuntu",
        "endpoints.0.id": "edge",
        "endpoints.0.addressing": "ip",
        "endpoints.0.address": "10.0.0.8:8080",
        "endpoints.0.collect.transport": "ssh",
        "endpoints.0.collect.ssh": "deploy@10.0.0.8:22",
      },
    },
    {
      name: "staging",
      endpoints: [
        { id: "edge", addressing: "ip", address: "10.0.0.8:8080", collect: { transport: "ssh" } },
      ],
    }
  );
  assert.equal(next.observe.ssh_user, "ubuntu");
});

test("one SSH destination round-trips to the profile's separate fields", () => {
  const previous = {
    name: "staging",
    endpoints: [
      {
        id: "edge",
        addressing: "ip",
        address: "10.0.0.8:8080",
        host_header: "api.example.com",
        collect: { transport: "ssh" },
      },
    ],
  };
  const next = readForm(
    {
      fields: {
        name: "staging",
        "endpoints.0.id": "edge",
        "endpoints.0.addressing": "ip",
        "endpoints.0.address": "10.0.0.8:8080",
        "endpoints.0.host_header": "api.example.com",
        "endpoints.0.collect.transport": "ssh",
        "endpoints.0.collect.ssh": "deploy@bastion.example.com:2222",
      },
    },
    previous
  );
  assert.deepEqual(next.endpoints[0].collect, {
    transport: "ssh",
    user: "deploy",
    host: "bastion.example.com",
    port: 2222,
  });
});

test("the compact SSH spelling handles defaults and bracketed IPv6", () => {
  assert.equal(
    sshDestination({ transport: "ssh", user: "ops" }, "[2001:db8::7]:8080"),
    "ops@[2001:db8::7]:22"
  );
  assert.deepEqual(parseSshDestination("ops@[2001:db8::7]:2200"), {
    user: "ops",
    host: "2001:db8::7",
    port: 2200,
  });
  assert.deepEqual(parseSshDestination(""), {});
});

test("changing the endpoint address keeps implicit SSH defaults implicit", () => {
  const previous = {
    name: "staging",
    endpoints: [
      {
        id: "edge",
        addressing: "alb",
        address: "old.example.com:443",
        collect: { transport: "ssh", user: "deploy" },
      },
    ],
  };
  const next = readForm(
    {
      fields: {
        name: "staging",
        "endpoints.0.id": "edge",
        "endpoints.0.addressing": "alb",
        "endpoints.0.address": "new.example.com:443",
        "endpoints.0.collect.transport": "ssh",
        "endpoints.0.collect.ssh": "deploy@old.example.com:22",
      },
    },
    previous
  );
  assert.deepEqual(next.endpoints[0].collect, { transport: "ssh", user: "deploy" });
});
