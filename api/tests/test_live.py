"""The live stream: fan-out, replay, and what happens when a client falls behind."""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from metrix_api.config import load_config
from metrix_api.live import BACKLOG, Event, Hub, Registry
from metrix_api.main import create_app
from metrix_api.observer.metrics import Gap, Sample
from metrix_api.profiles import parse_profile, save_profile

PROFILE = {
    "name": "staging",
    "endpoints": [
        {"id": "task-a", "address": "10.0.3.41:8080", "collect": {"transport": "ssh"}}
    ],
}

NOTHING_TO_COLLECT = {
    "name": "lb-only",
    "endpoints": [{"id": "lb", "address": "10.0.3.1:80", "collect": {"transport": "none"}}],
}


@pytest.fixture
def home(tmp_path: Path):
    config = load_config(tmp_path).ensure_layout()
    save_profile(config, parse_profile(PROFILE))
    save_profile(config, parse_profile(NOTHING_TO_COLLECT))
    return config


@pytest.fixture
def client(home):
    with TestClient(create_app(home)) as client:
        yield client


class TestEvent:
    def test_the_wire_format_carries_an_id_the_browser_will_echo(self) -> None:
        encoded = Event(id=7, kind="samples", data={"a": 1}).encode()
        assert encoded.startswith("id: 7\nevent: samples\ndata: ")
        assert encoded.endswith("\n\n")
        assert json.loads(encoded.split("data: ")[1].strip()) == {"a": 1}


class TestHub:
    def test_ids_are_monotonic(self) -> None:
        hub = Hub()
        assert [hub.publish("samples", {}).id for _ in range(3)] == [1, 2, 3]
        assert hub.last_id == 3

    def test_replay_returns_only_what_was_missed(self) -> None:
        hub = Hub()
        for n in range(5):
            hub.publish("samples", {"n": n})

        missed, too_old = hub.replay_since(3)
        assert [e.data["n"] for e in missed] == [3, 4]
        assert not too_old

    def test_falling_past_the_buffer_asks_for_a_snapshot(self) -> None:
        """A partial catch-up would silently omit the middle, which is worse."""
        hub = Hub(replay=3)
        for n in range(10):
            hub.publish("samples", {"n": n})

        missed, too_old = hub.replay_since(1)
        assert too_old
        assert missed == []

    @pytest.mark.asyncio
    async def test_a_subscriber_receives_what_is_published(self) -> None:
        hub = Hub()
        received = []

        async def reader():
            async for event in hub.subscribe():
                received.append(event)
                if len(received) == 2:
                    return

        task = asyncio.create_task(reader())
        await asyncio.sleep(0)
        hub.publish("samples", {"n": 1})
        hub.publish("samples", {"n": 2})
        await asyncio.wait_for(task, timeout=1)

        assert [e.data["n"] for e in received] == [1, 2]

    @pytest.mark.asyncio
    async def test_a_slow_client_is_resynced_rather_than_backlogged(self) -> None:
        """A view thirty seconds behind is worse than one that skipped thirty."""
        hub = Hub()
        subscription = hub.subscribe()
        # Register the queue without reading from it.
        task = asyncio.create_task(subscription.__anext__())
        await asyncio.sleep(0)

        for n in range(BACKLOG + 20):
            hub.publish("samples", {"n": n})

        first = await asyncio.wait_for(task, timeout=1)
        drained = [first]
        while True:
            try:
                drained.append(await asyncio.wait_for(subscription.__anext__(), timeout=0.05))
            except TimeoutError:
                break
        await subscription.aclose()

        assert len(drained) <= BACKLOG + 1, "the queue must not grow without bound"
        assert any(e.kind == "resync" for e in drained), "an overrun client is told to resync"

    @pytest.mark.asyncio
    async def test_closing_ends_every_subscription(self) -> None:
        hub = Hub()
        received = []

        async def reader():
            async for event in hub.subscribe():
                received.append(event)

        task = asyncio.create_task(reader())
        await asyncio.sleep(0)
        hub.publish("samples", {})
        hub.close()
        await asyncio.wait_for(task, timeout=1)
        assert len(received) == 1


class TestLifecycleRoutes:
    def test_starting_a_recording_with_nothing_to_collect_is_a_422(self, client) -> None:
        response = client.post("/api/recordings", json={"profile": "lb-only"})
        assert response.status_code == 422
        assert "no endpoint with a collector" in response.json()["detail"]

    def test_starting_against_an_unknown_profile_is_a_404(self, client) -> None:
        assert client.post("/api/recordings", json={"profile": "nope"}).status_code == 404

    def test_stopping_something_that_is_not_running_is_a_404(self, client) -> None:
        assert client.post("/api/recordings/nope/stop").status_code == 404

    def test_streaming_a_recording_that_is_not_live_is_a_404(self, client) -> None:
        assert client.get("/api/recordings/nope/stream").status_code == 404

    def test_the_live_list_is_not_read_as_a_recording_id(self, client) -> None:
        """`/recordings/live` must not be routed as `/recordings/{id}`."""
        response = client.get("/api/recordings/live")
        assert response.status_code == 200
        assert response.json() == {"live": []}


class TestRegistry:
    @pytest.mark.asyncio
    async def test_a_live_recording_streams_samples_and_stops(self, home) -> None:
        """The whole live path with a transport the test controls."""
        import metrix_api.recording as recording_module
        from tests.test_observer_collector import FakeClock, FakeTransport
        from tests.test_observer_linux import block

        clock = FakeClock()
        transport = FakeTransport(
            [block(cpu_user=100, cpu_idle=900), block(cpu_user=200, cpu_idle=1800)],
            then="hang",
            clock=clock,
        )
        original = recording_module.transport_for
        recording_module.transport_for = lambda endpoint, **kw: transport

        registry = Registry()
        try:
            # The clock must be in place before collection starts: the task captures
            # it, so assigning afterwards would leave the real one running.
            live = await registry.start(home, parse_profile(PROFILE), clock=clock)
            await asyncio.sleep(0.1)

            snapshot = live.snapshot()
            assert snapshot["counts"]["samples"] >= 1
            assert "cpu.user" in snapshot["latest"]
            assert snapshot["latest"]["cpu.user"]["task-a"] == pytest.approx(10.0)

            row = await registry.stop(live.recording_id)
            assert row.status == "finished"
            assert registry.get(live.recording_id) is None
        finally:
            recording_module.transport_for = original

    @pytest.mark.asyncio
    async def test_a_gap_reaches_subscribers_as_gap_and_annotation(self, home) -> None:
        registry = Registry()
        import metrix_api.recording as recording_module
        from tests.test_observer_collector import FakeTransport

        original = recording_module.transport_for
        recording_module.transport_for = lambda endpoint, **kw: FakeTransport([], then="hang")
        try:
            live = await registry.start(home, parse_profile(PROFILE))
            live.note_gap(Gap("task-a", 1000, 4000, "host went away"))

            kinds = [e.kind for e in live.hub._history]
            assert kinds == ["gap", "annotation"]
            assert live.hub._history[-1].data["severity"] == "warn"
            assert "never interpolated" in live.hub._history[-1].data["message"]

            await registry.stop(live.recording_id)
        finally:
            recording_module.transport_for = original

    @pytest.mark.asyncio
    async def test_samples_are_batched_into_one_event_per_tick(self, home) -> None:
        """Cadence is decoupled from the source: many samples, one push."""
        registry = Registry()
        import metrix_api.recording as recording_module
        from tests.test_observer_collector import FakeTransport

        original = recording_module.transport_for
        recording_module.transport_for = lambda endpoint, **kw: FakeTransport([], then="hang")
        try:
            live = await registry.start(home, parse_profile(PROFILE))
            for t_ms in (1000, 2000, 3000):
                live.note_sample(Sample("task-a", t_ms, {"cpu.busy": 1.0 * t_ms}))

            await asyncio.sleep(1.2)
            pushes = [e for e in live.hub._history if e.kind == "samples"]
            assert len(pushes) == 1, "three samples in one tick means one event"
            assert len(pushes[0].data["samples"]) == 3

            await registry.stop(live.recording_id)
        finally:
            recording_module.transport_for = original
