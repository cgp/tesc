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

const { dropCapability, render } = await import("../../web/js/schemas.js");
const { detectSource } = await import("../../web/js/sources.js");

function state(patch = {}) {
  return { schemas: [], schemaDetail: null, schemasReadAt: null, ...patch };
}

test("WADL is detected from its top stylesheet marker or initial application namespace", () => {
  assert.equal(
    detectSource('<?xml version="1.0"?><?xml-stylesheet type="text/wadl"?>\n<application/>'),
    "wadl"
  );
  assert.equal(
    detectSource('<application xmlns="http://wadl.dev.java.net/2009/02">'),
    "wadl"
  );
  assert.equal(detectSource('<application xmlns="urn:other">', "openapi"), "openapi");
  assert.equal(detectSource("<!-- wadl -->\n<application>", "xml"), "xml");
});

test("the upload target offers every plan source parser", () => {
  const markup = render(state());

  for (const source of ["openapi", "swagger", "wadl", "wsdl", "har", "access_log", "routes"]) {
    assert.match(markup, new RegExp(`value="${source}"`));
  }
  assert.match(markup, /type="file"/);
  assert.match(markup, /multiple/);
  assert.match(markup, /data-schema-upload-form/);
  assert.match(markup, /data-schema-drop-zone/);
  assert.match(markup, /Drop files here to upload/);
  assert.match(markup, /Accepted types:/);
  assert.match(markup, /Upload diagnostics/);
  assert.match(markup, /spec\.openapis\.org/);
  assert.match(markup, /www\.w3\.org\/submissions\/wadl/);
  assert.doesNotMatch(markup, /empty-icon/);
});

test("a browser without file-drop primitives gets an explanation, not a dead target", () => {
  const support = dropCapability({ DragEvent: undefined, DataTransfer: undefined, File: undefined });
  const markup = render(state({ schemaDrop: support }));

  assert.equal(support.enabled, false);
  assert.match(markup, /File dropping is unavailable here/);
  assert.match(markup, /Choose files with the picker above/);
  assert.match(markup, /Missing: DragEvent, DataTransfer, File/);
  assert.doesNotMatch(markup, /data-schema-drop-zone/);
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
