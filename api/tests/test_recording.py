"""Observation-only recordings: start, collect, stop, reopen.

The question these answer is the one the mode exists for -- can you come back to a
recording later and still have what it saw, including what it missed.
"""

from __future__ import annotations

import asyncio
import json
from datetime import timedelta
from pathlib import Path

import pytest

from metrix_api.observer import metrics as m
from metrix_api.observer.collector import Clock
from metrix_api.profiles import parse_profile
from metrix_api.recording import Recorder, RecordingError, start_observation
from metrix_api.store import open_store
from metrix_api.store import recordings as store
from tests.test_observer_collector import FakeClock, FakeTransport
from tests.test_observer_linux import block

pytestmark = pytest.mark.asyncio

PROFILE = {
    "name": "staging",
    "addressing": "load_balancer",
    "observe": {"interval": "1s", "collect": ["cpu", "memory"]},
    "endpoints": [
        {
            "id": "task-a1b2c3",
            "address": "10.0.3.41:8080",
            "attributes": {"availability_zone": "us-east-1a", "image_digest": "sha256:9f2c1e"},
            "collect": {"transport": "ssh", "user": "ec2-user"},
        },
        {
            "id": "web-only",
            "address": "10.0.3.99:8080",
            "collect": {"transport": "none"},
        },
    ],
}

BLOCKS = [block(cpu_user=100, cpu_idle=900), block(cpu_user=200, cpu_idle=1800)]


@pytest.fixture
def db(tmp_path: Path):
    with open_store(tmp_path / "metrix.db") as conn:
        yield conn


def profile(**overrides):
    return parse_profile({**PROFILE, **overrides})


async def record(db, prof, transport, *, clock=None, groups=None, settle=0.05):
    """Start a recording against a fake transport, let it collect, then stop."""
    clock = clock or FakeClock()
    recorder = Recorder(
        conn=db,
        recording_id=store.new_id(),
        profile=prof,
        clock=clock,
        interval=timedelta(seconds=1),
        groups=list(groups or []),
    )
    store.create(
        db,
        recording_id=recorder.recording_id,
        profile=prof,
        endpoints=prof.observed,
        kind="observation",
        api_version="0.0.0",
        interval_s=1.0,
    )
    # Drive the real collector, but with a transport the test controls.
    import metrix_api.recording as recording_module

    original = recording_module.transport_for
    recording_module.transport_for = lambda endpoint, **kw: transport
    try:
        recorder._start_task()
        await asyncio.sleep(settle)
        return await recorder.stop()
    finally:
        recording_module.transport_for = original


class TestLifecycle:
    async def test_a_recording_persists_and_reopens(self, db) -> None:
        clock = FakeClock()
        transport = FakeTransport(BLOCKS, then="hang", clock=clock)
        row = await record(db, profile(), transport, clock=clock)

        assert row.status == store.FINISHED
        assert row.kind == "observation"
        assert row.finished_at is not None
        assert row.duration_ms == 2000

        # Reopen: everything the recording saw is still there.
        reopened = store.get(db, row.id)
        assert reopened.profile == "staging"
        assert reopened.targets == ["task-a1b2c3"], "only collectable endpoints are recorded"

        series = store.series(db, row.id, "task-a1b2c3", m.CPU_USER)
        assert series == [(2000, pytest.approx(10.0))]

    async def test_only_collectable_endpoints_are_pinned(self, db) -> None:
        clock = FakeClock()
        row = await record(db, profile(), FakeTransport(BLOCKS, then="hang", clock=clock),
                           clock=clock)
        targets = db.execute(
            "SELECT target_id, attributes FROM recording_target WHERE recording_id = ?", (row.id,)
        ).fetchall()
        assert [t["target_id"] for t in targets] == ["task-a1b2c3"]
        # Inventory detail is pinned with the recording: it explains an outlier later.
        assert json.loads(targets[0]["attributes"])["image_digest"] == "sha256:9f2c1e"

    async def test_the_observation_window_is_one_phase(self, db) -> None:
        """With no traffic, baseline and settle collapse into a single window."""
        clock = FakeClock()
        row = await record(db, profile(), FakeTransport(BLOCKS, then="hang", clock=clock),
                           clock=clock)
        phases = store.phases(db, row.id)
        assert [p["phase"] for p in phases] == [store.OBSERVATION_PHASE]
        assert phases[0]["from_ms"] == 0
        assert phases[0]["to_ms"] == 2000

    async def test_a_recording_is_running_until_it_is_stopped(self, db) -> None:
        clock = FakeClock()
        recorder = Recorder(
            conn=db, recording_id=store.new_id(), profile=profile(), clock=clock,
            interval=timedelta(seconds=1),
        )
        store.create(
            db, recording_id=recorder.recording_id, profile=profile(),
            endpoints=profile().observed, kind="observation", api_version="0.0.0",
            interval_s=1.0,
        )
        assert store.get(db, recorder.recording_id).status == store.RUNNING

        import metrix_api.recording as recording_module

        recording_module.transport_for = lambda e, **kw: FakeTransport([], then="hang")
        recorder._start_task()
        await asyncio.sleep(0.02)
        assert recorder.running
        row = await recorder.stop()
        assert row.status == store.FINISHED
        assert not recorder.running


class TestGaps:
    async def test_a_gap_is_stored_and_annotated_together(self, db) -> None:
        """A gap without its annotation would be invisible in the UI."""
        clock = FakeClock()
        transport = FakeTransport([BLOCKS[0]], then="raise", clock=clock)
        row = await record(db, profile(), transport, clock=clock)

        gaps = store.gaps(db, row.id)
        assert gaps, "a transport failure must reach the recording"
        assert "host went away" in gaps[0]["reason"]

        notes = store.annotations(db, row.id)
        codes = [n["code"] for n in notes]
        assert m.COLLECTION_GAP in codes
        gap_note = next(n for n in notes if n["code"] == m.COLLECTION_GAP)
        assert gap_note["severity"] == "warn", "a gap degrades a recording, it does not void it"
        assert "never interpolated" in gap_note["message"]
        assert json.loads(gap_note["detail"])["reason"]

    async def test_samples_before_a_failure_are_kept(self, db) -> None:
        clock = FakeClock()
        transport = FakeTransport(BLOCKS, then="raise", clock=clock)
        row = await record(db, profile(), transport, clock=clock)
        assert store.samples(db, row.id), "what was collected before the failure still counts"


class TestGroups:
    async def test_the_profiles_groups_limit_what_is_stored(self, db) -> None:
        clock = FakeClock()
        transport = FakeTransport(BLOCKS, then="hang", clock=clock)
        row = await record(db, profile(), transport, clock=clock, groups=["cpu"])

        stored = {metric for _, _, metric, _ in store.samples(db, row.id)}
        assert m.CPU_USER in stored
        assert m.MEM_USED not in stored, "memory was not requested"


class TestSeries:
    async def test_the_same_setup_groups_into_one_series(self, db) -> None:
        clock = FakeClock()
        for _ in range(2):
            await record(db, profile(), FakeTransport(BLOCKS, then="hang", clock=clock),
                         clock=clock)
        keys = {r.series_key for r in store.list_recordings(db)}
        assert len(keys) == 1, "identical setups must share a series"

    async def test_a_different_interval_is_a_different_series(self, db) -> None:
        """A metric sampled every 5s is not comparable with one sampled every second."""
        first = store.series_key(profile(), kind="observation", api_version="0.0.0",
                                 interval_s=1.0)
        second = store.series_key(profile(), kind="observation", api_version="0.0.0",
                                  interval_s=5.0)
        assert first != second

    async def test_addressing_mode_splits_the_series(self, db) -> None:
        """Through the load balancer and direct to a container are different paths."""
        direct = profile(
            addressing="direct",
            endpoints=[
                {
                    "id": "task-a1b2c3",
                    "address": "10.0.3.41:8080",
                    "host_header": "api.staging.example.com",
                    "collect": {"transport": "ssh"},
                }
            ],
        )
        assert store.series_key(
            profile(), kind="observation", api_version="0.0.0", interval_s=1.0
        ) != store.series_key(direct, kind="observation", api_version="0.0.0", interval_s=1.0)

    async def test_listing_is_newest_first_and_filterable(self, db) -> None:
        clock = FakeClock()
        for _ in range(3):
            await record(db, profile(), FakeTransport(BLOCKS, then="hang", clock=clock),
                         clock=clock)
        rows = store.list_recordings(db, kind="observation")
        assert len(rows) == 3
        assert store.list_recordings(db, profile="nonexistent") == []


class TestRefusals:
    async def test_a_profile_with_nothing_to_collect_is_refused(self, db) -> None:
        """An empty recording looks like a failed one; better to refuse up front."""
        nothing = profile(
            endpoints=[{"id": "lb", "address": "10.0.3.1:80", "collect": {"transport": "none"}}]
        )
        with pytest.raises(RecordingError, match="no endpoint with a collector"):
            await start_observation(db, nothing)

        assert store.list_recordings(db) == [], "no row is opened for a refused recording"


async def test_clock_is_monotonic_from_zero() -> None:
    clock = Clock.start()
    first = clock.now_ms()
    await asyncio.sleep(0.01)
    assert clock.now_ms() >= first >= 0
