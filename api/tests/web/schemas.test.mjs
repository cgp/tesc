// The Schemas page is a pure view: source choices, stored rows and detail evidence.
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
            .replaceAll(">", "&gt;")
            .replaceAll('"', "&quot;");
        },
      };
    },
  },
});

const { render } = await import("../../web/js/schemas.js");

function state(patch = {}) {
  return { schemas: [], schemaDetail: null, schemasReadAt: null, ...patch };
}

test("the upload target offers every plan source parser", () => {
  const markup = render(state());

  for (const source of ["openapi", "swagger", "wsdl", "har", "access_log", "routes"]) {
    assert.match(markup, new RegExp(`value="${source}"`));
  }
  assert.match(markup, /type="file"/);
  assert.match(markup, /multiple/);
  assert.match(markup, /data-schema-upload-form/);
  assert.match(markup, /data-schema-drop-zone/);
  assert.match(markup, /Drop files here to upload/);
  assert.match(markup, /Accepted types:/);
  assert.match(markup, /spec\.openapis\.org/);
  assert.doesNotMatch(markup, /empty-icon/);
});

test("the table shows id, filename, type, call count and row actions", () => {
  const markup = render(
    state({
      schemas: [
        {
          id: "shop-openapi",
          filename: "shop.yaml",
          source: "openapi",
          call_count: 3,
        },
      ],
    })
  );

  assert.match(markup, /shop-openapi/);
  assert.match(markup, /shop\.yaml/);
  assert.match(markup, /OpenAPI 3 \(JSON or YAML\)/);
  assert.match(markup, /3 calls/);
  assert.match(markup, /data-action="schema-view"/);
  assert.match(markup, /data-action="schema-delete"/);
});

test("detail lists parsed calls and displays escaped original source", () => {
  const markup = render(
    state({
      schemaDetail: {
        id: "shop-routes",
        filename: "shop.routes",
        source: "routes",
        call_count: 1,
        content: "GET /pets?<unsafe>",
        calls: [
          { name: "listpets", method: "GET", path: "/pets", description: "List pets" },
        ],
      },
    })
  );

  assert.match(markup, /listpets/);
  assert.match(markup, /List pets/);
  assert.match(markup, /GET \/pets\?&lt;unsafe&gt;/);
  assert.doesNotMatch(markup, /GET \/pets\?<unsafe>/);
  assert.match(markup, /data-action="schema-back"/);
});
