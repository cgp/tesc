"""The API the front end reads, and that the page is actually served."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from metrix_api.config import load_config
from metrix_api.main import create_app
from metrix_api.observer.metrics import Gap, Sample
from metrix_api.profiles import parse_profile, save_profile
from metrix_api.store import open_store
from metrix_api.store import recordings as store

PROFILE = {
    "name": "staging",
    "description": "two tasks behind the load balancer",
    "endpoints": [
        {
            "id": "task-a",
            "address": "10.0.3.41:8080",
            "host_header": "api.staging.example.com",
            "collect": {"transport": "ssh", "user": "ec2-user"},
        },
        {"id": "lb-only", "address": "10.0.3.1:80", "collect": {"transport": "none"}},
    ],
}


@pytest.fixture
def home(tmp_path: Path):
    config = load_config(tmp_path).ensure_layout()
    save_profile(config, parse_profile(PROFILE))
    return config


@pytest.fixture
def client(home):
    return TestClient(create_app(home))


@pytest.fixture
def recording(home):
    """A finished recording with samples, a gap, and its annotation."""
    profile = parse_profile(PROFILE)
    with open_store(home.database) as conn:
        recording_id = store.new_id()
        store.create(
            conn,
            recording_id=recording_id,
            profile=profile,
            endpoints=profile.observed,
            kind="observation",
            api_version="0.0.0",
            interval_s=1.0,
        )
        store.start_phase(conn, recording_id, "task-a", store.OBSERVATION_PHASE, 0)
        store.add_sample(conn, recording_id, Sample("task-a", 1000, {"cpu.busy": 11.0}))
        store.add_sample(conn, recording_id, Sample("task-a", 2000, {"cpu.busy": 12.5}))
        store.add_gap(conn, recording_id, Gap("task-a", 2000, 5000, "host went away"))
        store.end_phase(conn, recording_id, "task-a", store.OBSERVATION_PHASE, 5000)
        store.finish(conn, recording_id, duration_ms=5000)
    return recording_id


class TestHealth:
    def test_reports_version_and_home(self, client, home) -> None:
        body = client.get("/api/health").json()
        assert body["status"] == "ok"
        assert body["home"] == str(home.home)


class TestProfiles:
    def test_listing_says_which_endpoints_are_observed(self, client) -> None:
        body = client.get("/api/profiles").json()
        assert [p["name"] for p in body["profiles"]] == ["staging"]
        profile = body["profiles"][0]
        assert profile["observed"] == ["task-a"], "the lb has no collector"
        assert len(profile["endpoints"]) == 2, "but every endpoint is still listed"

    def test_an_unparseable_profile_is_reported_not_dropped(self, client, home) -> None:
        """An environment silently missing from the list is worse than a visible error."""
        (home.profiles_dir / "broken.json").write_text(
            json.dumps({"name": "broken", "endpoints": []}), encoding="utf-8"
        )
        body = client.get("/api/profiles").json()
        assert [p["name"] for p in body["profiles"]] == ["staging"]
        assert [b["name"] for b in body["broken"]] == ["broken"]
        assert "endpoints" in body["broken"][0]["error"]

    def test_a_missing_profile_is_a_404(self, client) -> None:
        assert client.get("/api/profiles/nope").status_code == 404


class TestRecordings:
    def test_listing(self, client, recording) -> None:
        body = client.get("/api/recordings").json()
        assert [r["id"] for r in body["recordings"]] == [recording]
        assert body["recordings"][0]["targets"] == ["task-a"]

    def test_filtering(self, client, recording) -> None:
        assert client.get("/api/recordings?kind=load").json()["recordings"] == []
        assert client.get("/api/recordings?profile=staging").json()["recordings"]

    def test_detail_carries_what_a_chart_needs_to_be_honest(self, client, recording) -> None:
        body = client.get(f"/api/recordings/{recording}").json()

        assert body["status"] == "finished"
        assert body["metrics"] == ["cpu.busy"]
        assert body["phases"][0]["phase"] == store.OBSERVATION_PHASE

        # Gaps travel with the recording so holes are drawn rather than interpolated.
        assert body["gaps"][0]["reason"] == "host went away"
        codes = [a["code"] for a in body["annotations"]]
        assert "collection_gap" in codes
        assert body["annotations"][0]["detail"]["reason"] == "host went away"

    def test_series_is_shaped_for_a_chart(self, client, recording) -> None:
        body = client.get(f"/api/recordings/{recording}/series?metric=cpu.busy").json()
        assert body["series"]["task-a"] == [[1000, 11.0], [2000, 12.5]]

    def test_series_can_be_narrowed_to_one_target(self, client, recording) -> None:
        body = client.get(
            f"/api/recordings/{recording}/series?metric=cpu.busy&target=task-a"
        ).json()
        assert list(body["series"]) == ["task-a"]

    def test_a_missing_recording_is_a_404(self, client) -> None:
        assert client.get("/api/recordings/nope").status_code == 404


class TestStaticPage:
    def test_the_page_is_served_at_the_root(self, client) -> None:
        response = client.get("/")
        assert response.status_code == 200
        assert "Metrix" in response.text
        assert "/js/main.js" in response.text

    def test_modules_and_styles_are_served(self, client) -> None:
        for path in (
            "/js/main.js",
            "/js/api.js",
            "/js/state.js",
            "/js/config.js",
            "/js/recordings.js",
            "/js/table.js",
            "/js/charts.js",
            "/js/format.js",
            "/css/app.css",
        ):
            assert client.get(path).status_code == 200, path

    def test_api_routes_win_over_the_static_mount(self, client) -> None:
        """The mount is at /, so an ordering mistake would shadow the whole API."""
        assert client.get("/api/health").json()["status"] == "ok"
