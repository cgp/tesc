"""The stored source library reuses generation and writes only parsed uploads."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from metrix_api.config import load_config
from metrix_api.main import create_app


@pytest.fixture
def home(tmp_path: Path):
    return load_config(tmp_path).ensure_layout()


@pytest.fixture
def client(home):
    return TestClient(create_app(home))


def upload(client, **changes):
    body = {
        "filename": "shop.routes",
        "source": "routes",
        "content": "GET /api/health\nPOST /api/orders\n",
        **changes,
    }
    return client.post("/api/schemas", json=body)


class TestSchemaLibraryRoutes:
    def test_upload_parses_stores_and_lists_the_source(self, client, home) -> None:
        response = upload(client)

        assert response.status_code == 201
        assert response.json()["id"] == "shop-routes"
        assert response.json()["call_count"] == 2
        assert [call["method"] for call in response.json()["calls"]] == ["GET", "POST"]
        assert (home.schemas_dir / "shop-routes" / "source.txt").read_text() == (
            "GET /api/health\nPOST /api/orders\n"
        )

        listed = client.get("/api/schemas").json()["schemas"]
        assert listed == [
            {
                "id": "shop-routes",
                "filename": "shop.routes",
                "source": "routes",
                "uploaded_at": response.json()["uploaded_at"],
                "call_count": 2,
            }
        ]

    def test_detail_returns_the_original_document_and_parsed_calls(self, client) -> None:
        upload(client)

        detail = client.get("/api/schemas/shop-routes")

        assert detail.status_code == 200
        assert detail.json()["content"].startswith("GET /api/health")
        assert detail.json()["calls"][1]["path"] == "/api/orders"

    def test_invalid_source_is_not_stored(self, client, home) -> None:
        response = upload(client, content="not a route")

        assert response.status_code == 422
        assert not any(home.schemas_dir.iterdir())

    def test_an_id_collision_does_not_overwrite_the_first_upload(self, client) -> None:
        first = upload(client)
        second = upload(client, filename="shop.txt", content="GET /different\n")

        assert first.status_code == 201
        assert second.status_code == 409
        assert (
            client.get("/api/schemas/shop-routes").json()["content"].startswith("GET /api/health")
        )

    def test_filename_cannot_escape_the_schema_directory(self, client, home) -> None:
        response = upload(client, filename="../escape.routes")

        assert response.status_code == 422
        assert not (home.home / "escape.routes").exists()

    def test_delete_removes_the_entry(self, client, home) -> None:
        upload(client)

        response = client.delete("/api/schemas/shop-routes")

        assert response.status_code == 204
        assert not (home.schemas_dir / "shop-routes").exists()
        assert client.get("/api/schemas/shop-routes").status_code == 404
        assert client.delete("/api/schemas/shop-routes").status_code == 404

    def test_openapi_uses_the_same_parser_and_call_count_as_plan_generation(self, client) -> None:
        document = {
            "openapi": "3.0.3",
            "info": {"title": "Pet Catalog API", "version": "1"},
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "listPets",
                        "responses": {"200": {"description": "ok"}},
                    }
                }
            },
        }

        response = upload(
            client,
            filename="shop.json",
            source="openapi",
            content=json.dumps(document),
        )

        assert response.status_code == 201
        assert response.json()["id"] == "pet-catalog-api-openapi"
        assert response.json()["calls"] == [{"name": "listpets", "method": "GET", "path": "/pets"}]

    def test_openapi_description_names_schema_when_title_is_missing(self, client) -> None:
        document = {
            "openapi": "3.0.3",
            "info": {"description": "Inventory Service", "version": "1"},
            "paths": {
                "/items": {
                    "get": {"responses": {"200": {"description": "ok"}}}
                }
            },
        }

        response = upload(
            client,
            filename="uploaded.json",
            source="openapi",
            content=json.dumps(document),
        )

        assert response.status_code == 201
        assert response.json()["id"] == "inventory-service-openapi"

    def test_swagger_two_uses_its_local_adapter_before_being_stored(self, client) -> None:
        document = {
            "swagger": "2.0",
            "info": {"title": "Shop", "version": "1"},
            "basePath": "/v1",
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "listPets",
                        "responses": {"200": {"description": "ok"}},
                    }
                }
            },
        }

        response = upload(
            client,
            filename="shop-swagger.json",
            source="swagger",
            content=json.dumps(document),
        )

        assert response.status_code == 201
        assert response.json()["id"] == "shop-swagger"
        assert response.json()["calls"] == [
            {"name": "listpets", "method": "GET", "path": "/v1/pets"}
        ]

    def test_wadl_uses_the_same_local_adapter_before_being_stored(self, client) -> None:
        document = """<application xmlns="http://wadl.dev.java.net/2009/02">
          <resources base="https://shop.example.com/v1/"><resource path="pets">
            <method name="GET" id="listPets"><response status="200"/></method>
          </resource></resources>
        </application>"""

        response = upload(client, filename="shop.wadl", source="wadl", content=document)

        assert response.status_code == 201
        assert response.json()["id"] == "shop-wadl"
        assert response.json()["calls"] == [
            {"name": "listpets", "method": "GET", "path": "/v1/pets"}
        ]
