"""Launching a run from the browser: the gate, the wiring, and what gets pinned.

A load run is an observation with traffic attached, so almost everything here is
about the seams: what is refused *before* a recording exists, what the recording says
about what produced it, and how a run that ends on its own lets go of the live list.

The engine itself is stubbed. What it does with a bundle is covered against the real
binary by the contract check and by `test_ingest.py`; what is under test here is the
control plane's half of the handover.
"""

from __future__ import annotations

import asyncio
import json
import time
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from metrix_api import live as live_module
from metrix_api import plans
from metrix_api.config import load_config
from metrix_api.main import create_app
from metrix_api.profiles import parse_profile, save_profile
from metrix_api.runner.engine import EngineRun
from metrix_api.store import open_store
from metrix_api.store import recordings as store

#: A box with a collector on a port nothing is listening on. The collector has to be
#: real for the recording to be a real one; it does not have to answer, and a refused
#: connection fails in a millisecond where an unroutable address takes the timeout.
WATCHED = {
    "name": "staging",
    "endpoints": [
        {"id": "app-1", "address": "127.0.0.1:8298", "collect": {"transport": "scrape"}}
    ],
}

UNWATCHED = {
    "name": "mock",
    "endpoints": [
        {"id": "mock-1", "address": "127.0.0.1:8299", "collect": {"transport": "none"}}
    ],
}

MIX = {
    "version": 1,
    "name": "ping",
    "calls": ["calls/ping.json"],
    "phases": {"baseline": "0s", "settle": "0s"},
    "load": {"mode": "fixed", "model": "open", "rate": 75, "duration": "30s"},
    "chains": [
        {"name": "ping", "percent": 100, "steps": [{"id": "get", "call": "ping"}]}
    ],
}

CALLS = {"ping": {"method": "GET", "path": "/ping"}}


@pytest.fixture
def home(tmp_path: Path, monkeypatch):
    config = load_config(tmp_path).ensure_layout()
    for document in (WATCHED, UNWATCHED):
        save_profile(config, parse_profile(document))
    write_plan(config, "ping")
    # A file that exists, so the "is there an engine" gate passes. Nothing runs it:
    # every test here either stops before the subprocess or stubs `run_engine`.
    pretend = tmp_path / "metrix-engine"
    pretend.write_text("not really", encoding="utf-8")
    monkeypatch.setenv("METRIX_ENGINE", str(pretend))
    return config


def write_plan(config, name, *, mix=None):
    root = config.plans_dir / name
    (root / "calls").mkdir(parents=True, exist_ok=True)
    (root / "mix.json").write_bytes(plans.document_bytes({**MIX, "name": name, **(mix or {})}))
    (root / "calls" / "ping.json").write_bytes(plans.document_bytes(CALLS))
    return root


@pytest.fixture
def client(home):
    # Inside the context manager on purpose: a launched run creates tasks that
    # outlive the request that started it, and a TestClient used without it runs
    # every request in a fresh event loop -- which would tear the engine's
    # supervisor down between the start and the stop.
    with TestClient(create_app(home)) as client:
        yield client


@pytest.fixture
def stub_engine(monkeypatch):
    """An engine that starts, is asked to finish, and says it exited cleanly."""
    started: dict = {}

    async def fake(conn, recording_id, *, bundle, started_at, stop, targets=(), **kw):
        started.update(
            {
                "recording_id": recording_id,
                "bundle": bundle,
                "targets": targets,
                "stop": stop,
            }
        )
        await stop.wait()
        return EngineRun(exit_code=0, records=0, windows=0, diagnostic="")

    monkeypatch.setattr(live_module, "run_engine", fake)
    return started


def recordings(home) -> list[store.RecordingRow]:
    with open_store(home.database) as conn:
        return store.list_recordings(conn)


def until_finished(client, seconds: float = 10.0) -> list[str]:
    """Wait for the run to close itself.

    The app runs in its own thread, so sleeping here is what gives it time -- closing
    a recording probes the boxes one last time, and that is real work rather than a
    turn of the event loop.
    """
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        live = client.get("/api/recordings/live").json()["live"]
        if not live:
            return live
        time.sleep(0.05)
    return client.get("/api/recordings/live").json()["live"]


class TestTheGate:
    def test_a_plan_that_cannot_run_is_refused_before_anything_is_opened(
        self, home, client
    ) -> None:
        write_plan(
            home,
            "half",
            mix={"chains": [{"name": "a", "percent": 40, "steps": [{"id": "s", "call": "ping"}]}]},
        )
        response = client.post(
            "/api/recordings", json={"profile": "staging", "plan": "half"}
        )
        assert response.status_code == 422
        assert "60 short of 100" in response.json()["detail"]
        # Nothing was created. A row for a run that never started is a row somebody
        # has to explain later.
        assert recordings(home) == []

    def test_a_plan_that_does_not_exist_is_a_404_rather_than_a_refusal(
        self, home, client
    ) -> None:
        response = client.post(
            "/api/recordings", json={"profile": "staging", "plan": "nope"}
        )
        assert response.status_code == 404
        assert recordings(home) == []

    def test_no_engine_is_said_plainly_and_opens_nothing(
        self, home, client, monkeypatch
    ) -> None:
        monkeypatch.setenv("METRIX_ENGINE", str(home.home / "not-here"))
        response = client.post(
            "/api/recordings", json={"profile": "staging", "plan": "ping"}
        )
        assert response.status_code == 422
        assert "no engine binary" in response.json()["detail"]
        assert recordings(home) == []

    def test_a_profile_that_does_not_exist_is_a_404(self, home, client) -> None:
        response = client.post("/api/recordings", json={"profile": "nope", "plan": "ping"})
        assert response.status_code == 404


class TestWhatTheRecordingSays:
    def test_a_launched_run_is_a_load_recording_carrying_its_plan(
        self, home, client, stub_engine
    ) -> None:
        response = client.post(
            "/api/recordings", json={"profile": "staging", "plan": "ping"}
        )
        assert response.status_code == 201
        assert response.json()["plan"] == "ping"

        row = recordings(home)[0]
        assert row.kind == "load"
        assert row.plan_name == "ping"
        # The kind is in the series key, so an environment watched at rest and the
        # same environment under load never land in one series.
        assert row.series_key.startswith("load|staging|")

        client.post(f"/api/recordings/{row.id}/stop")

    def test_the_bundle_it_will_run_is_written_beside_the_recording(
        self, home, client, stub_engine
    ) -> None:
        started = client.post(
            "/api/recordings", json={"profile": "staging", "plan": "ping"}
        ).json()
        snapshot = home.run_dir(started["recording_id"]) / "plan.snapshot"
        assert (snapshot / "mix.json").is_file()
        assert (snapshot / "targets.json").is_file()
        # The targets came from the profile at launch, which is the only thing the
        # API adds to a stored plan.
        targets = json.loads((snapshot / "targets.json").read_text(encoding="utf-8"))
        assert [t["id"] for t in targets["list"]] == ["app-1"]

        client.post(f"/api/recordings/{started['recording_id']}/stop")

    def test_the_engine_is_told_which_boxes_the_recording_is_about(
        self, home, client, stub_engine
    ) -> None:
        started = client.post(
            "/api/recordings", json={"profile": "staging", "plan": "ping"}
        ).json()
        # Its phase timeline is written against these, not against its load target:
        # the boxes traffic goes to and the boxes statistics come from are different
        # sockets and usually different machines.
        assert stub_engine["targets"] == ("app-1",)
        client.post(f"/api/recordings/{started['recording_id']}/stop")


class TestNothingToWatch:
    def test_an_observation_of_a_profile_with_no_collector_is_still_refused(
        self, home, client
    ) -> None:
        response = client.post("/api/recordings", json={"profile": "mock"})
        assert response.status_code == 422
        assert "no endpoint with a collector" in response.json()["detail"]

    def test_a_load_run_against_one_is_allowed_and_says_what_is_missing(
        self, home, client, stub_engine
    ) -> None:
        # Measuring somebody else's service is a real thing to want, and the load
        # figures stand on their own.
        started = client.post(
            "/api/recordings", json={"profile": "mock", "plan": "ping"}
        )
        assert started.status_code == 201
        recording_id = started.json()["recording_id"]

        with open_store(home.database) as conn:
            row = store.get(conn, recording_id)
            notes = [a["code"] for a in store.annotations(conn, recording_id)]
        # Pinned to the boxes it sends to, so the recording still says what it ran
        # against rather than being a run with no targets at all.
        assert row.targets == ["mock-1"]
        assert "no_host_collection" in notes

        client.post(f"/api/recordings/{recording_id}/stop")


class TestEndingIt:
    def test_stopping_asks_the_engine_to_finish_rather_than_killing_it(
        self, home, client, stub_engine
    ) -> None:
        started = client.post(
            "/api/recordings", json={"profile": "staging", "plan": "ping"}
        ).json()
        assert not stub_engine["stop"].is_set()

        stopped = client.post(f"/api/recordings/{started['recording_id']}/stop")
        assert stopped.status_code == 200
        assert stopped.json()["status"] == "finished"
        # The event, not a signal: the engine drains what is in flight and emits the
        # record that says how the run went.
        assert stub_engine["stop"].is_set()

    def test_a_run_that_ends_on_its_own_leaves_the_live_list(
        self, home, client, monkeypatch
    ) -> None:
        async def brief(conn, recording_id, *, bundle, started_at, stop, targets=(), **kw):
            await asyncio.sleep(0)
            return EngineRun(exit_code=0, records=0, windows=0, diagnostic="")

        monkeypatch.setattr(live_module, "run_engine", brief)
        started = client.post(
            "/api/recordings", json={"profile": "staging", "plan": "ping"}
        ).json()

        # A load run ends when the traffic does. Left in the list it would look live
        # forever, and the page waiting on it would wait forever too.
        assert until_finished(client) == []
        with open_store(home.database) as conn:
            assert store.get(conn, started["recording_id"]).status == "finished"

    def test_an_engine_that_fails_keeps_what_was_collected_and_says_why(
        self, home, client, monkeypatch
    ) -> None:
        async def broken(conn, recording_id, *, bundle, started_at, stop, targets=(), **kw):
            raise RuntimeError("the binary is not really a binary")

        monkeypatch.setattr(live_module, "run_engine", broken)
        started = client.post(
            "/api/recordings", json={"profile": "staging", "plan": "ping"}
        ).json()

        until_finished(client)

        with open_store(home.database) as conn:
            notes = {
                a["code"]: a["severity"] for a in store.annotations(conn, started["recording_id"])
            }
        # The observation is real and is kept; what is lost is the load half, and
        # the recording says so rather than simply stopping.
        assert notes.get("engine_failed") == "invalid"
