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


class TestHostFacts:
    """What the box was, and what the run did to its disks."""

    def _probed(self, home, recording_id):
        from metrix_api.observer.facts import Filesystem, HostFacts
        from metrix_api.store.db import connect, migrate

        conn = connect(home.database)
        migrate(conn)
        store.save_facts(
            conn, recording_id, "task-a",
            HostFacts(
                identity={"hostname": "web-1", "os": "Ubuntu 24.04", "cpus": "8"},
                filesystems=[Filesystem("/", 1000, 400), Filesystem("/data", 5000, 1000)],
            ),
            at="start",
        )
        store.save_facts(
            conn, recording_id,  "task-a",
            HostFacts(
                identity={"hostname": "web-1"},
                filesystems=[Filesystem("/", 1000, 450)],
            ),
            at="finish",
        )
        conn.close()

    def test_identity_and_the_disk_delta_reach_the_recording(self, client, home, recording) -> None:
        self._probed(home, recording)
        body = client.get(f"/api/recordings/{recording}").json()

        assert body["identity"]["task-a"]["os"] == "Ubuntu 24.04"
        by_mount = {f["mount"]: f for f in body["filesystems"]}
        assert by_mount["/"]["used_delta_bytes"] == 50, "450 - 400"

    def test_a_mount_with_no_finish_reading_has_no_delta(self, client, home, recording) -> None:
        """The second probe failing is not the same as nothing being written, and a
        delta of zero would claim it was."""
        self._probed(home, recording)
        by_mount = {
            f["mount"]: f for f in client.get(f"/api/recordings/{recording}").json()["filesystems"]
        }
        assert by_mount["/data"]["start_used_bytes"] == 1000
        assert by_mount["/data"]["finish_used_bytes"] is None
        assert by_mount["/data"]["used_delta_bytes"] is None

    def test_a_second_probe_does_not_thin_out_the_identity(self, client, home, recording) -> None:
        """The finish probe reported only a hostname -- a box under load can answer
        less. The fuller first answer is the one worth keeping."""
        self._probed(home, recording)
        identity = client.get(f"/api/recordings/{recording}").json()["identity"]["task-a"]
        assert identity["cpus"] == "8"

    def test_a_recording_with_no_probe_reports_nothing_rather_than_zero(
        self, client, recording
    ) -> None:
        body = client.get(f"/api/recordings/{recording}").json()
        assert body["identity"] == {}
        assert body["filesystems"] == []


class TestHealth:
    def test_reports_version_and_home(self, client, home) -> None:
        body = client.get("/api/health").json()
        assert body["status"] == "ok"
        assert body["home"] == str(home.home)
        assert body["database"] == str(home.database)

    def test_says_where_the_home_came_from(self, client) -> None:
        """The Config page prints this verbatim, so it has to be a sentence rather
        than an internal key."""
        body = client.get("/api/health").json()
        assert body["home_source"] == "passed in directly"


class TestProfiles:
    def test_listing_says_where_stats_are_collected_from(self, client) -> None:
        """`address` is the load target and says nothing about collection. The page
        prints this string, and it comes from the transport rather than being
        re-formatted for display, so it cannot drift from what is really used."""
        endpoint = client.get("/api/profiles").json()["profiles"][0]["endpoints"][0]
        assert endpoint["address"] == "10.0.3.41:8080"
        assert endpoint["collects_from"] == "ec2-user@10.0.3.41:22"
        # The transport is named once, in its own field; the page renders the badge.
        assert endpoint["transport"] == "ssh"

    def test_an_unobserved_endpoint_collects_from_nowhere(self, client, home) -> None:
        from metrix_api.profiles import parse_profile, save_profile

        save_profile(
            home,
            parse_profile(
                {
                    "name": "edge",
                    "endpoints": [
                        {"id": "lb", "address": "10.0.1.9:443", "collect": {"transport": "none"}}
                    ],
                }
            ),
        )
        body = client.get("/api/profiles/edge").json()
        assert body["endpoints"][0]["collects_from"] is None
        assert body["observed"] == []

    def test_the_document_round_trips_through_the_editor(self, client) -> None:
        """What the editor loads has to be what it can save back, or a round trip
        silently drops whatever the display summary does not carry."""
        document = client.get("/api/profiles/staging/document").json()
        assert client.put("/api/profiles/staging", json=document).status_code == 200
        assert client.get("/api/profiles/staging/document").json() == document

    def test_creating_a_profile(self, client) -> None:
        document = {
            "name": "new-env",
            "addressing": "direct",
            "observe": {"interval": "1s", "collect": ["cpu"]},
            "endpoints": [
                {
                    "id": "box",
                    "address": "10.0.9.1:8080",
                    "host_header": "new.example.com",
                    "collect": {"transport": "ssh", "user": "deploy"},
                }
            ],
        }
        created = client.post("/api/profiles", json=document)
        assert created.status_code == 201
        assert created.json()["endpoints"][0]["collects_from"] == "deploy@10.0.9.1:22"
        assert "new-env" in [p["name"] for p in client.get("/api/profiles").json()["profiles"]]

    def test_creating_over_an_existing_profile_is_a_409(self, client) -> None:
        """Saving a new profile must never quietly replace one already there."""
        body = client.post("/api/profiles", json={"name": "staging", "endpoints": [
            {"id": "x", "address": "1.2.3.4:80"}
        ]})
        assert body.status_code == 409
        assert "already exists" in body.json()["detail"]
        # The original is untouched.
        assert len(client.get("/api/profiles/staging").json()["endpoints"]) > 1

    def test_an_invalid_document_is_a_422_naming_the_field(self, client) -> None:
        """The form prints this beside the fields that are still filled in, so it
        carries the field and the reason and not the path of a file that does not
        exist."""
        body = client.post("/api/profiles", json={"name": "Bad Name", "endpoints": []})
        assert body.status_code == 422
        detail = body.json()["detail"]
        assert "name" in detail
        assert not detail.startswith("<"), f"source prefix leaked into the UI: {detail}"

    def test_a_profile_with_no_endpoints_is_refused(self, client) -> None:
        body = client.post("/api/profiles", json={"name": "empty", "endpoints": []})
        assert body.status_code == 422
        assert "endpoints" in body.json()["detail"]

    def test_replacing_a_profile_that_is_not_there_is_a_404(self, client) -> None:
        document = {"name": "ghost", "endpoints": [{"id": "x", "address": "1.2.3.4:80"}]}
        assert client.put("/api/profiles/ghost", json=document).status_code == 404

    def test_a_profile_cannot_be_renamed_by_editing_it(self, client) -> None:
        """The name is part of a recording's series identity: a rename through the
        editor would split one environment's history in two with no sign of it."""
        document = client.get("/api/profiles/staging/document").json()
        document["name"] = "staging-renamed"
        body = client.put("/api/profiles/staging", json=document)
        assert body.status_code == 422
        assert "does not match" in body.json()["detail"]

    def test_deleting_a_profile(self, client) -> None:
        assert client.delete("/api/profiles/staging").status_code == 204
        assert client.get("/api/profiles/staging").status_code == 404
        assert client.get("/api/profiles").json()["profiles"] == []

    def test_deleting_something_that_is_not_there_is_a_404(self, client) -> None:
        assert client.delete("/api/profiles/ghost").status_code == 404

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
        """Keyed by metric then target: the charts page draws one chart per metric."""
        body = client.get(f"/api/recordings/{recording}/series?metric=cpu.busy").json()
        assert body["series"]["cpu.busy"]["task-a"] == [[1000, 11.0], [2000, 12.5]]

    def test_every_metric_comes_back_when_none_is_named(self, client, recording) -> None:
        body = client.get(f"/api/recordings/{recording}/series").json()
        assert "cpu.busy" in body["metrics"]
        assert set(body["series"]) == set(body["metrics"])

    def test_series_carries_what_a_chart_draws_behind_the_lines(self, client, recording) -> None:
        body = client.get(f"/api/recordings/{recording}/series").json()
        # Phases to shade, gaps to leave as holes, annotations to mark.
        assert [p["phase"] for p in body["phases"]] == ["measure"]
        assert body["gaps"] and body["gaps"][0]["reason"]
        assert body["annotations"]
        assert body["baseline"] == {}, "no baseline is set for this series"

    def test_series_can_be_narrowed_to_one_target(self, client, recording) -> None:
        body = client.get(
            f"/api/recordings/{recording}/series?metric=cpu.busy&target=task-a"
        ).json()
        assert list(body["series"]["cpu.busy"]) == ["task-a"]

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
