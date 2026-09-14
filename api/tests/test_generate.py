"""Generating a draft plan: what a description can decide, and what it cannot.

The property under test throughout is **determinism**. There is no model in the
generator, so the same document must give the same plan down to the bytes — that is
what makes a diff between two generated plans mean the service changed. Everything
else here is about the line between the mechanical half and the judgment half (§8.1):
what a schema genuinely says, and what the todo list has to admit it does not.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from metrix_api import generate, plans
from metrix_api.config import load_config
from metrix_api.main import create_app

OPENAPI = """
openapi: 3.0.3
info: {title: Shop, version: "1"}
paths:
  /pets/{petId}:
    get:
      operationId: getPet
      summary: Read one pet
      parameters:
        - {name: petId, in: path, required: true, schema: {type: string}}
      responses: {"200": {description: ok}, "404": {description: gone}}
  /orders/{orderId}:
    get:
      operationId: getOrder
      parameters:
        - name: orderId
          in: path
          required: true
          schema: {type: string, example: "A-1000"}
      responses: {"200": {description: ok}}
  /pets:
    post:
      operationId: createPet
      requestBody:
        content:
          application/json:
            schema: {$ref: "#/components/schemas/Pet"}
      responses: {"201": {description: made}, "202": {description: queued}}
components:
  schemas:
    Pet:
      type: object
      properties:
        name: {type: string}
        born: {type: string, format: date}
        tags: {type: array, items: {type: string}}
"""

HAR = json.dumps(
    {
        "log": {
            "entries": [
                {
                    "request": {"method": "GET", "url": "https://x/api/products?q=a"},
                    "response": {"status": 200},
                },
                {
                    "request": {"method": "GET", "url": "https://x/api/products?q=b"},
                    "response": {"status": 200},
                },
                {
                    "request": {"method": "GET", "url": "https://x/api/orders/12345"},
                    "response": {"status": 200},
                },
                {
                    "request": {
                        "method": "POST",
                        "url": "https://x/api/session",
                        "headers": [{"name": "Authorization", "value": "Bearer hunter2"}],
                        "postData": {"text": '{"password":"hunter2"}'},
                    },
                    "response": {"status": 401},
                },
            ]
        }
    }
)

_WHEN = "[10/Oct/2026:13:55:36 +0000]"
ACCESS_LOG = "\n".join(
    f'10.0.0.{host} - - {_WHEN} "{request}" {status} 26'
    for host, request, status in [
        (1, "GET /api/products?q=shoes HTTP/1.1", 200),
        (2, "GET /api/products HTTP/1.1", 200),
        (3, "GET /api/products HTTP/1.1", 500),
        (4, "GET /api/orders/99 HTTP/1.1", 200),
    ]
)

WSDL = """<?xml version="1.0"?>
<definitions xmlns="http://schemas.xmlsoap.org/wsdl/"
             xmlns:soap="http://schemas.xmlsoap.org/wsdl/soap/"
             xmlns:xsd="http://www.w3.org/2001/XMLSchema"
             xmlns:tns="http://example.com/shop"
             targetNamespace="http://example.com/shop">
  <types>
    <xsd:schema targetNamespace="http://example.com/shop">
      <xsd:element name="PlaceOrder">
        <xsd:complexType><xsd:sequence>
          <xsd:element name="customer" type="xsd:string"/>
          <xsd:element name="quantity" type="xsd:int"/>
          <xsd:element name="priority" type="tns:Priority"/>
        </xsd:sequence></xsd:complexType>
      </xsd:element>
      <xsd:element name="PlaceOrderResponse" type="xsd:string"/>
      <xsd:simpleType name="Priority">
        <xsd:restriction base="xsd:string">
          <xsd:enumeration value="normal"/><xsd:enumeration value="express"/>
        </xsd:restriction>
      </xsd:simpleType>
    </xsd:schema>
  </types>
  <message name="In"><part name="p" element="tns:PlaceOrder"/></message>
  <message name="Out"><part name="p" element="tns:PlaceOrderResponse"/></message>
  <portType name="ShopPort">
    <operation name="PlaceOrder">
      <documentation>Submit an order</documentation>
      <input message="tns:In"/><output message="tns:Out"/>
    </operation>
  </portType>
  <binding name="ShopBinding" type="tns:ShopPort">
    <soap:binding style="document" transport="http://schemas.xmlsoap.org/soap/http"/>
    <operation name="PlaceOrder"><soap:operation soapAction="urn:PlaceOrder"/></operation>
  </binding>
  <service name="Shop"><port name="p" binding="tns:ShopBinding">
    <soap:address location="http://shop.example.com/soap/v1"/>
  </port></service>
</definitions>
"""


@pytest.fixture
def home(tmp_path: Path):
    return load_config(tmp_path).ensure_layout()


@pytest.fixture
def client(home):
    return TestClient(create_app(home))


def call(draft, name):
    return draft.calls[name]


def todos(draft):
    return " | ".join(todo.message for todo in draft.todos)


class TestDeterminism:
    def test_the_same_document_gives_the_same_plan_down_to_the_bytes(self) -> None:
        # The property the whole feature rests on. Without it a regenerated plan
        # diffs against itself, and a diff stops meaning the service changed.
        first = generate.generate("openapi", OPENAPI, name="shop")
        second = generate.generate("openapi", OPENAPI, name="shop")
        assert plans.document_bytes(first.mix) == plans.document_bytes(second.mix)
        assert plans.document_bytes(first.calls) == plans.document_bytes(second.calls)

    def test_an_enum_is_read_in_a_stable_order_rather_than_as_written(self) -> None:
        # Nothing guarantees the order an enum was serialised in survives a round
        # trip through whatever produced the document.
        draft = generate.generate("wsdl", WSDL, name="shop")
        assert "<tns:priority>express</tns:priority>" in call(draft, "placeorder")["body"]


class TestEveryGeneratedPlanRuns:
    @pytest.mark.parametrize(
        ("kind", "content"),
        [
            ("openapi", OPENAPI),
            ("wsdl", WSDL),
            ("har", HAR),
            ("access_log", ACCESS_LOG),
            ("routes", "GET /api/health\nPOST /api/orders\n"),
        ],
    )
    def test_it_loads_through_the_real_loader_and_is_ready(self, home, kind, content) -> None:
        # Generated against the frozen schemas rather than against a second opinion
        # about them: a skeleton that will not load is not a starting point.
        draft = generate.generate(kind, content, name="drafted")
        generate.write(plans.plan_path(home, "drafted"), draft)
        plan = plans.load_plan(home, "drafted")
        problems = plans.check(plan)
        assert plans.ready(problems), [p.message for p in problems if p.severity == "error"]
        assert abs(sum(c["percent"] for c in plan.chains) - 100) < plans.PERCENT_EPSILON

    def test_shares_total_exactly_a_hundred_however_they_divide(self) -> None:
        # Three ways and seven ways both have to land on 100, or the plan the tool
        # just produced is one its own validator refuses.
        for count in (1, 3, 7, 11, 13):
            routes = "\n".join(f"GET /api/r{index}" for index in range(count))
            draft = generate.generate("routes", routes, name="wide")
            total = sum(chain["percent"] for chain in draft.mix["chains"])
            assert abs(total - 100) < plans.PERCENT_EPSILON, count

    def test_nothing_generated_is_ever_more_than_one_step(self) -> None:
        # No chain inference, in any source (§8.3). A wrong chain is worse than none:
        # it runs cleanly while testing a flow the service does not have.
        for kind, content in (("openapi", OPENAPI), ("har", HAR), ("wsdl", WSDL)):
            draft = generate.generate(kind, content, name="drafted")
            assert {len(c["steps"]) for c in draft.mix["chains"]} == {1}, kind
            assert "never infers a sequence" in todos(draft)


class TestOpenAPI:
    def test_an_example_is_used_and_a_type_is_only_a_shape(self) -> None:
        draft = generate.generate("openapi", OPENAPI, name="shop")
        # orderId carries an example, so the path is concrete.
        assert call(draft, "getorder")["path"] == "/orders/A-1000"
        # petId does not, so it stays a template rather than becoming a made-up id.
        assert call(draft, "getpet")["path"] == "/pets/{{ petId }}"
        assert "{{ petId }} has no source" in todos(draft)

    def test_a_body_is_built_from_the_schema_it_refs(self) -> None:
        draft = generate.generate("openapi", OPENAPI, name="shop")
        body = json.loads(call(draft, "createpet")["body"])
        assert body == {"born": "2026-01-01", "name": "string", "tags": ["string"]}
        assert call(draft, "createpet")["headers"]["Content-Type"] == "application/json"
        assert "the right shape and mean nothing" in todos(draft)

    def test_assertions_take_the_declared_success_codes_and_not_the_failures(self) -> None:
        draft = generate.generate("openapi", OPENAPI, name="shop")
        assert call(draft, "createpet")["assert"] == [{"status_in": [201, 202]}]
        # 404 is declared on getPet and is a documented outcome, not an expected one.
        assert call(draft, "getpet")["assert"] == [{"status": 200}]

    def test_the_summary_becomes_the_description_the_call_list_reads(self) -> None:
        draft = generate.generate("openapi", OPENAPI, name="shop")
        assert call(draft, "getpet")["description"] == "Read one pet"

    def test_swagger_two_is_refused_rather_than_half_read(self) -> None:
        with pytest.raises(generate.GenerationError, match="Swagger 2.0"):
            generate.generate("openapi", json.dumps({"swagger": "2.0", "paths": {}}), name="old")

    def test_a_remote_ref_is_left_alone_rather_than_fetched(self) -> None:
        document = {
            "openapi": "3.0.0",
            "paths": {
                "/x": {
                    "post": {
                        "operationId": "x",
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": {"$ref": "https://elsewhere.example/schema.json"}
                                }
                            }
                        },
                        "responses": {"200": {"description": "ok"}},
                    }
                }
            },
        }
        # No network, and no exception: a generator that resolved remote refs would
        # be a control plane fetching whatever address a document names.
        draft = generate.generate("openapi", json.dumps(document), name="remote")
        assert call(draft, "x").get("body") is None

    def test_a_document_declaring_security_points_at_the_mix_not_at_a_header(self) -> None:
        document = {
            "openapi": "3.0.0",
            "components": {"securitySchemes": {"bearer": {"type": "http", "scheme": "bearer"}}},
            "paths": {
                "/x": {"get": {"operationId": "x", "responses": {"200": {"description": "k"}}}}
            },
        }
        draft = generate.generate("openapi", json.dumps(document), name="secured")
        assert "authentication is set once in the mix" in todos(draft)
        assert "Authorization" not in call(draft, "x").get("headers", {})


class TestTraffic:
    def test_weights_come_from_what_was_actually_asked_for(self) -> None:
        draft = generate.generate("har", HAR, name="observed")
        shares = {c["name"]: c["percent"] for c in draft.mix["chains"]}
        # Two of four entries were the product search.
        assert shares["get-api-products"] == 50.0
        assert draft.observed_weights is True
        assert "even split" not in todos(draft)

    def test_a_capture_contributes_no_bodies_and_no_headers(self) -> None:
        draft = generate.generate("har", HAR, name="observed")
        serialised = json.dumps(draft.calls)
        # The capture held a bearer token and a password. Neither reaches a plan,
        # which is a file that ends up in a repository.
        assert "hunter2" not in serialised
        assert "Authorization" not in serialised
        assert "had a body; none was copied" in todos(draft)

    def test_identifier_segments_collapse_so_one_operation_is_one_call(self) -> None:
        draft = generate.generate("har", HAR, name="observed")
        assert "/api/orders/{{ id1 }}" in [c["path"] for c in draft.calls.values()]
        assert "read as identifiers" in todos(draft)

    def test_the_assertion_takes_the_status_most_often_seen_and_says_so(self) -> None:
        draft = generate.generate("access_log", ACCESS_LOG, name="logged")
        # /api/products was 200 twice and 500 once.
        assert call(draft, "get-api-products")["assert"] == [{"status": 200}]
        assert "more than one status" in todos(draft)

    def test_a_log_nothing_can_be_read_from_says_what_it_expected(self) -> None:
        with pytest.raises(generate.GenerationError, match="request line"):
            generate.generate("access_log", "nothing like a log line\n", name="bad")

    def test_a_route_list_names_the_line_it_could_not_read(self) -> None:
        with pytest.raises(generate.GenerationError, match="line 2"):
            generate.generate("routes", "GET /ok\nnonsense\n", name="bad")

    def test_comments_and_blank_lines_are_allowed_in_a_route_list(self) -> None:
        draft = generate.generate("routes", "# the api\n\nGET /ok\n", name="listed")
        assert list(draft.calls) == ["get-ok"]


class TestWsdl:
    def test_the_envelope_carries_every_declared_field_of_the_input(self) -> None:
        body = call(generate.generate("wsdl", WSDL, name="soap"), "placeorder")["body"]
        assert "<tns:customer>string</tns:customer>" in body
        assert "<tns:quantity>0</tns:quantity>" in body
        assert body.startswith('<?xml version="1.0" encoding="utf-8"?>')

    def test_soap_eleven_sends_an_action_header_and_a_text_xml_content_type(self) -> None:
        headers = call(generate.generate("wsdl", WSDL, name="soap"), "placeorder")["headers"]
        assert headers["SOAPAction"] == '"urn:PlaceOrder"'
        assert headers["Content-Type"] == "text/xml; charset=utf-8"

    def test_a_fault_is_a_two_hundred_so_the_response_element_is_asserted_too(self) -> None:
        asserts = call(generate.generate("wsdl", WSDL, name="soap"), "placeorder")["assert"]
        assert {"xpath": "//*[local-name()='PlaceOrderResponse']", "exists": True} in asserts

    def test_the_host_is_left_to_the_profile_and_only_the_path_is_kept(self) -> None:
        draft = generate.generate("wsdl", WSDL, name="soap")
        assert call(draft, "placeorder")["path"] == "/soap/v1"
        assert "comes from the profile" in todos(draft)

    def test_a_document_declaring_entities_is_declined_rather_than_parsed(self) -> None:
        # Nothing legitimate here needs a DOCTYPE, and a document that defines its
        # own entities can expand into gigabytes inside the parser.
        bomb = '<?xml version="1.0"?><!DOCTYPE x [<!ENTITY a "aaaa">]><definitions/>'
        with pytest.raises(generate.GenerationError, match="DOCTYPE"):
            generate.generate("wsdl", bomb, name="bomb")

    def test_xml_that_is_not_well_formed_says_so(self) -> None:
        with pytest.raises(generate.GenerationError, match="well-formed"):
            generate.generate("wsdl", "<definitions>", name="bad")


class TestTheDraftMarker:
    def test_a_generated_plan_is_marked_and_warned_about_but_still_runs(self, home) -> None:
        draft = generate.generate("openapi", OPENAPI, name="drafted")
        generate.write(plans.plan_path(home, "drafted"), draft)
        plan = plans.load_plan(home, "drafted")
        assert plan.draft["source"] == "openapi"
        problems = plans.check(plan)
        assert any("not yet reviewed" in p.message for p in problems)
        # A warning, never an error: blocking it would only teach people to delete
        # the marker.
        assert plans.ready(problems)

    def test_accepting_it_removes_the_marker_and_the_warning(self, home) -> None:
        draft = generate.generate("openapi", OPENAPI, name="drafted")
        generate.write(plans.plan_path(home, "drafted"), draft)
        assert plans.accept_draft(home, "drafted") is True
        plan = plans.load_plan(home, "drafted")
        assert plan.draft is None
        assert not any("not yet reviewed" in p.message for p in plans.check(plan))
        assert plans.accept_draft(home, "drafted") is False

    def test_a_marker_that_will_not_parse_does_not_take_the_plan_with_it(self, home) -> None:
        draft = generate.generate("openapi", OPENAPI, name="drafted")
        root = plans.plan_path(home, "drafted")
        generate.write(root, draft)
        (root / plans.DRAFT).write_text("{not json", encoding="utf-8")
        plan = plans.load_plan(home, "drafted")
        assert plan.draft is None
        assert any("todo list is missing" in note for note in plan.notes)


class TestTheRoutes:
    def test_generating_saves_a_plan_the_editor_can_open(self, client, home) -> None:
        response = client.post(
            "/api/plans/generate",
            json={"name": "shop", "source": "openapi", "content": OPENAPI},
        )
        assert response.status_code == 201
        body = response.json()
        assert body["draft"]["source"] == "openapi"
        assert body["ready"] is True
        assert client.get("/api/plans/shop/document").status_code == 200

    def test_a_preview_writes_nothing(self, client, home) -> None:
        response = client.post(
            "/api/plans/generate",
            json={"name": "shop", "source": "openapi", "content": OPENAPI, "save": False},
        )
        assert response.status_code == 201
        assert response.json()["draft"] is True
        assert not plans.plan_path(home, "shop").exists()

    def test_generating_over_an_existing_plan_is_refused(self, client) -> None:
        payload = {"name": "shop", "source": "openapi", "content": OPENAPI}
        assert client.post("/api/plans/generate", json=payload).status_code == 201
        second = client.post("/api/plans/generate", json=payload)
        assert second.status_code == 409
        assert "regenerate its calls" in second.json()["detail"]

    def test_regenerating_replaces_the_calls_and_leaves_the_mixture_alone(
        self, client, home
    ) -> None:
        client.post(
            "/api/plans/generate",
            json={"name": "shop", "source": "openapi", "content": OPENAPI},
        )
        # A tuned mixture: the weights somebody decided on.
        tuned = client.get("/api/plans/shop/document").json()
        tuned["chains"][0]["percent"] = 80
        tuned["chains"][1]["percent"] = 10
        tuned["chains"][2]["percent"] = 10
        tuned["load"]["rate"] = 300
        assert client.put("/api/plans/shop", json=tuned).status_code == 200

        # The same service, one operation later. Built from the document rather than
        # by appending text, so the fixture cannot drift out of shape.
        import yaml

        parsed = yaml.safe_load(OPENAPI)
        parsed["paths"]["/pets/{petId}/toys"] = {
            "get": {"operationId": "listToys", "responses": {"200": {"description": "ok"}}}
        }
        grown = json.dumps(parsed)
        response = client.post(
            "/api/plans/shop/regenerate", json={"source": "openapi", "content": grown}
        )
        assert response.status_code == 200
        assert response.json()["added"] == ["listtoys"]
        # The judgment half is untouched: that is what the document split is for.
        after = client.get("/api/plans/shop/document").json()
        assert [c["percent"] for c in after["chains"]] == [80, 10, 10]
        assert after["load"]["rate"] == 300

    def test_a_regeneration_that_would_orphan_a_step_is_refused(self, client) -> None:
        client.post(
            "/api/plans/generate",
            json={"name": "shop", "source": "openapi", "content": OPENAPI},
        )
        response = client.post(
            "/api/plans/shop/regenerate",
            json={"source": "routes", "content": "GET /api/health"},
        )
        assert response.status_code == 409
        assert "does not define every call" in response.json()["detail"]
        # And nothing was written: the plan still loads and still runs.
        assert client.get("/api/plans/shop").json()["ready"] is True

    def test_accepting_a_draft_clears_it(self, client) -> None:
        client.post(
            "/api/plans/generate",
            json={"name": "shop", "source": "openapi", "content": OPENAPI},
        )
        assert client.delete("/api/plans/shop/draft").json()["draft"] is None
        assert client.delete("/api/plans/shop/draft").status_code == 404

    def test_a_source_nobody_implements_names_the_ones_that_exist(self, client) -> None:
        response = client.post(
            "/api/plans/generate",
            json={"name": "shop", "source": "postman", "content": "{}"},
        )
        assert response.status_code == 422
        assert "openapi" in response.json()["detail"]

    def test_a_name_that_is_not_a_directory_name_is_refused_before_anything_is_read(
        self, client, home
    ) -> None:
        response = client.post(
            "/api/plans/generate",
            json={"name": "../escape", "source": "routes", "content": "GET /x"},
        )
        assert response.status_code == 422
        assert not (home.plans_dir / ".." / "escape").exists()
