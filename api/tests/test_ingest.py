"""Reading the engine's stream, and putting it on the recording's clock.

The one thing this has to get right is the clock. The engine counts from its own
start; the observer has been collecting since before the engine was spawned. A load
figure and a host figure carrying the same `t_ms` must describe the same moment, or
every chart that overlays them is lying about cause and effect — which is the whole
reason anyone puts them on one axis.
"""

from __future__ import annotations

import asyncio
import json
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from metrix_api.config import load_config
from metrix_api.main import create_app
from metrix_api.observer.metrics import Sample
from metrix_api.profiles import parse_profile, save_profile
from metrix_api.runner.engine import EngineError, engine_binary, run_engine
from metrix_api.runner.ingest import Ingest
from metrix_api.store import open_store
from metrix_api.store import recordings as store

REPO = Path(__file__).resolve().parents[2]
FIXTURE = REPO / "examples" / "fixtures" / "summary.ndjson"

PROFILE = {
    "name": "staging",
    "endpoints": [
        {"id": "task-a1b2c3", "address": "10.0.0.1:8080", "collect": {"transport": "ssh"}},
    ],
}

STARTED = datetime(2026, 9, 12, 14, 3, 0, tzinfo=UTC)


@pytest.fixture
def db(tmp_path: Path):
    with open_store(tmp_path / "metrix.db") as conn:
        yield conn


@pytest.fixture
def recording(db):
    profile = parse_profile(PROFILE)
    recording_id = store.new_id(STARTED)
    store.create(
        db,
        recording_id=recording_id,
        profile=profile,
        endpoints=profile.endpoints,
        kind="load",
        api_version="0.0.0",
        interval_s=1.0,
        started_at=STARTED,
    )
    return recording_id


def ingest_for(db, recording_id, *, started=STARTED) -> Ingest:
    return Ingest(conn=db, recording_id=recording_id, started_at=started)


def started_record(at: str = "2026-09-12T14:03:11Z") -> dict:
    return {"type": "run_started", "events_version": 1, "run_id": "r", "t_ms": 0,
            "started_at": at, "engine_version": "0.0.0", "plan_hash": "sha256:x",
            "plan_name": "p", "seed": 1, "targets": ["task-a1b2c3"],
            "histogram_encoding": "hdr-v2-base64"}


def summary_record(t_ms: int, **patch) -> dict:
    return {"type": "summary", "t_ms": t_ms, "target_id": "task-a1b2c3", "phase": "measure",
            "window_ms": 250, "target_rate": 150.0, "achieved_rate": 149.2,
            "in_flight": 37, "queue_depth": 0, "drift_ms": 0.4,
            "bytes_sent": 1, "bytes_received": 2,
            "connections_opened": 3, "connections_reused": 4,
            "chains": {"search": {"iterations_started": 8, "iterations_completed": 8,
                                  "iterations_aborted": 0,
                                  "duration": {"count": 8, "min_us": 1, "max_us": 9,
                                               "mean_us": 4.0, "hdr": "AAAA"},
                                  "steps": {"query": {"attempted": 8, "completed": 8,
                                                      "failed": 0, "statuses": {"200": 8},
                                                      "total": {"count": 8, "min_us": 1,
                                                                "max_us": 9, "mean_us": 4.0,
                                                                "hdr": "BBBB"},
                                                      "ttfb": {"count": 8, "min_us": 1,
                                                               "max_us": 5, "mean_us": 2.0,
                                                               "hdr": "CCCC"}}}}},
            **patch}


# ----------------------------------------------------------------------- the clock


class TestClockAlignment:
    def test_the_engine_clock_is_shifted_onto_the_recordings(self, db, recording) -> None:
        """The engine started eleven seconds after the observer did, so its zero is
        eleven seconds into the recording."""
        ingest = ingest_for(db, recording)
        ingest.record(started_record("2026-09-12T14:03:11Z"))
        assert ingest.offset_ms == 11_000

        ingest.record(summary_record(40_250))
        row = store.load_windows(db, recording)[0]
        assert row["engine_t_ms"] == 40_250, "as the engine reported it"
        assert row["t_ms"] == 51_250, "on the recording's clock"

    def test_a_load_figure_and_a_host_figure_line_up(self, db, recording) -> None:
        """The point of the whole exercise: one moment, two sources, one t_ms."""
        # The observer samples at 51.25s into the recording.
        store.add_sample(
            db, recording, Sample(target_id="task-a1b2c3", t_ms=51_250, metrics={"cpu.busy": 80.0})
        )
        ingest = ingest_for(db, recording)
        ingest.record(started_record("2026-09-12T14:03:11Z"))
        ingest.record(summary_record(40_250))

        host = dict(store.series(db, recording, "task-a1b2c3", "cpu.busy"))
        load = store.load_scalars(db, recording)["load.achieved_rate"]["task-a1b2c3"]
        assert 51_250 in host
        assert load[0][0] == 51_250, "the same instant carries the same stamp"

    def test_sub_second_offsets_are_kept(self, db, recording) -> None:
        ingest = ingest_for(db, recording)
        ingest.record(started_record("2026-09-12T14:03:11.464Z"))
        assert ingest.offset_ms == 11_464

    def test_alignment_uses_the_engines_own_start_not_when_we_read_it(
        self, db, recording
    ) -> None:
        """Reading is delayed by pipe buffering and by whatever the reader was busy
        with. Folding that into the alignment would put the API's latency inside a
        measurement."""
        ingest = ingest_for(db, recording)
        # Applied long after the fact; the offset must not depend on now.
        ingest.record(started_record("2026-09-12T14:03:11Z"))
        assert ingest.offset_ms == 11_000

    def test_an_engine_that_claims_to_predate_the_recording_is_clamped(
        self, db, recording
    ) -> None:
        """A clock that moved is worth a note, not a lost run -- and a negative
        offset would put load samples before the recording began."""
        ingest = ingest_for(db, recording)
        ingest.record(started_record("2026-09-12T14:02:00Z"))
        assert ingest.offset_ms == 0

    def test_records_before_run_started_are_counted_not_guessed_at(
        self, db, recording
    ) -> None:
        ingest = ingest_for(db, recording)
        ingest.record(summary_record(1000))
        assert ingest.unplaced == 1
        assert store.load_windows(db, recording) == []


# ---------------------------------------------------------------------- the shapes


class TestShapes:
    def test_a_chain_and_its_steps_are_stored_apart(self, db, recording) -> None:
        """A chain's duration is not the sum of its step medians, so the end-to-end
        row is not one of the steps."""
        ingest = ingest_for(db, recording)
        ingest.record(started_record())
        ingest.record(summary_record(1000))

        rows = {(r["chain"], r["step"], r["kind"]): r for r in store.load_rows(db, recording)}
        assert all(r["step"] != "" for r in store.load_rows(db, recording)), (
            "the storage sentinel does not leave the store"
        )
        assert ("search", None, "duration") in rows
        assert ("search", "query", "total") in rows
        assert ("search", "query", "ttfb") in rows

    def test_every_distribution_keeps_its_count_and_its_histogram(
        self, db, recording
    ) -> None:
        ingest = ingest_for(db, recording)
        ingest.record(started_record())
        ingest.record(summary_record(1000))

        row = store.load_rows(db, recording, kind="total")[0]
        assert row["count"] == 8
        assert row["hdr"] == "BBBB", "kept so windows can be merged, not summarised away"
        assert json.loads(row["statuses"]) == {"200": 8}

    def test_totals_add_the_counters_and_take_the_real_extremes(
        self, db, recording
    ) -> None:
        ingest = ingest_for(db, recording)
        ingest.record(started_record())
        ingest.record(summary_record(1000))
        ingest.record(summary_record(1250, chains={
            "search": {"iterations_started": 2, "iterations_completed": 2,
                       "iterations_aborted": 0,
                       "duration": {"count": 2, "min_us": 100, "max_us": 200, "mean_us": 150.0,
                                    "hdr": "DDDD"}, "steps": {}}}))

        chain = next(
            r for r in store.load_totals(db, recording) if r["step"] is None
        )
        assert chain["count"] == 10, "8 + 2"
        assert (chain["min_us"], chain["max_us"]) == (1, 200)

    def test_no_percentile_is_derived_in_sql(self, db, recording) -> None:
        """Merging histograms is what produces one, and that lives in stats/."""
        ingest = ingest_for(db, recording)
        ingest.record(started_record())
        ingest.record(summary_record(1000))
        totals = store.load_totals(db, recording)
        assert all("p50" not in row and "p95" not in row for row in totals)
        assert store.load_histograms(
            db, recording, chain="search", step="query", kind="total"
        ) == ["BBBB"]

    def test_the_engines_notes_join_the_recordings_notes(self, db, recording) -> None:
        """One list, whatever noticed. Two would mean two places to look before
        quoting a number."""
        ingest = ingest_for(db, recording)
        ingest.record(started_record())
        ingest.record({"type": "annotation", "t_ms": 5000, "code": "events_dropped",
                       "severity": "warn", "from_ms": 5000, "to_ms": 6000,
                       "message": "output backpressure", "target_id": "task-a1b2c3"})

        note = store.annotations(db, recording)[0]
        assert note["code"] == "events_dropped"
        assert note["source"] == "engine", "which side noticed is part of the answer"
        assert note["from_ms"] == 5000 + ingest.offset_ms, "on the recording's clock"

    def test_the_exit_code_and_the_reason_land_on_the_recording(
        self, db, recording
    ) -> None:
        ingest = ingest_for(db, recording)
        ingest.record(started_record())
        ingest.record({"type": "run_finished", "t_ms": 9000, "exit_code": 130,
                       "slo": [], "stopped_because": "interrupted"})

        row = store.get(db, recording)
        assert row.engine_exit_code == 130
        assert row.stopped_because == "interrupted"


class TestSelfMetricsHonesty:
    """`self_metrics_unavailable` covers the OS resource probes and nothing else.

    The engine's own message says which: *zero in cpu_pct, rss_bytes and open_fds
    means unavailable*. Treating it as a blanket over everything the generator
    reports about itself throws away `in_flight`, `queue_depth` and `drift_ms`,
    which B1.5 genuinely measures. Both mistakes are avoided by naming the fields
    rather than by trusting the annotation to mean more than it says.
    """

    UNAVAILABLE = {
        "type": "annotation", "t_ms": 0, "code": "self_metrics_unavailable",
        "severity": "info", "from_ms": 0,
        "message": "Zero in cpu_pct, rss_bytes and open_fds means unavailable",
    }

    def test_placeholder_resource_probes_are_dropped_not_stored_as_zero(
        self, db, recording
    ) -> None:
        """A chart drawing `cpu_pct: 0` reports a generator loafing along at
        nothing, which is the most reassuring wrong answer this tool could give."""
        ingest = ingest_for(db, recording)
        ingest.record(started_record())
        ingest.record(self.UNAVAILABLE)
        ingest.record(summary_record(1000, generator={
            "cpu_pct": 0.0, "rss_bytes": 0, "open_fds": 0,
            "scheduler_lag_ms": 7.9, "events_dropped": 0,
        }))

        generator = json.loads(store.load_windows(db, recording)[0]["generator"])
        assert "cpu_pct" not in generator
        assert "rss_bytes" not in generator
        assert "open_fds" not in generator
        assert generator["scheduler_lag_ms"] == 7.9, "this one is measured"
        assert generator["events_dropped"] == 0

    def test_the_scheduler_gauges_survive_that_annotation(self, db, recording) -> None:
        """They are measured. Reading the annotation as a blanket would silently
        discard three real numbers a run is judged by."""
        ingest = ingest_for(db, recording)
        ingest.record(started_record())
        ingest.record(self.UNAVAILABLE)
        ingest.record(summary_record(1000, in_flight=37, queue_depth=2, drift_ms=0.4))

        row = store.load_windows(db, recording)[0]
        assert (row["in_flight"], row["queue_depth"], row["drift_ms"]) == (37, 2, 0.4)
        assert "load.in_flight" in store.load_scalars(db, recording)

    def test_a_measured_zero_is_kept(self, db, recording) -> None:
        """Nothing in flight is a real and interesting reading, not an absence."""
        ingest = ingest_for(db, recording)
        ingest.record(started_record())
        ingest.record(summary_record(1000, in_flight=0))
        assert store.load_windows(db, recording)[0]["in_flight"] == 0

    def test_without_the_annotation_the_generator_is_kept_whole(
        self, db, recording
    ) -> None:
        ingest = ingest_for(db, recording)
        ingest.record(started_record())
        ingest.record(summary_record(1000, generator={"cpu_pct": 12.5, "open_fds": 91}))
        generator = json.loads(store.load_windows(db, recording)[0]["generator"])
        assert generator == {"cpu_pct": 12.5, "open_fds": 91}


# --------------------------------------------------------------------- the fixture


class TestRecordedFixture:
    """F0.3: the API ingests a recorded fixture stream from the start, so that
    integration is a wiring exercise rather than a negotiation."""

    def test_the_committed_stream_ingests_whole(self, db, recording) -> None:
        ingest = ingest_for(db, recording)
        for line in FIXTURE.read_text(encoding="utf-8").splitlines():
            ingest.line(line)

        assert ingest.records == 13
        assert ingest.unplaced == 0
        assert ingest.windows == 2
        # The fixture ends on a failed SLO, which the engine reports as exit 2.
        row = store.get(db, recording)
        assert row.engine_exit_code == 2
        assert row.stopped_because == "duration reached"

    def test_it_produces_rows_a_chart_can_draw(self, db, recording) -> None:
        ingest = ingest_for(db, recording)
        for line in FIXTURE.read_text(encoding="utf-8").splitlines():
            ingest.line(line)

        scalars = store.load_scalars(db, recording)
        assert "load.achieved_rate" in scalars
        points = scalars["load.achieved_rate"]["task-a1b2c3"]
        assert len(points) == 2
        assert all(t > 0 for t, _ in points)

    def test_blank_lines_and_unknown_records_do_not_stop_it(self, db, recording) -> None:
        ingest = ingest_for(db, recording)
        ingest.line("")
        ingest.line(json.dumps(started_record()))
        ingest.line(json.dumps({"type": "something_later", "t_ms": 1}))
        ingest.line(json.dumps(summary_record(1000)))
        assert ingest.windows == 1

    def test_an_unreadable_line_names_where_and_never_what(self, db, recording) -> None:
        from metrix_api.runner.ingest import IngestError

        ingest = ingest_for(db, recording)
        with pytest.raises(IngestError) as caught:
            ingest.line('{"type": "request", "url": "https://x/?token=s3cret"')
        assert "s3cret" not in str(caught.value)


# ------------------------------------------------------------------ the supervisor


class TestSupervision:
    @pytest.fixture
    def home(self, tmp_path: Path):
        config = load_config(tmp_path).ensure_layout()
        save_profile(config, parse_profile(PROFILE))
        return config

    def bundle(self, at: Path, address: str = "127.0.0.1:1") -> Path:
        """A real bundle the engine will parse, pointed at nothing."""
        at.mkdir(parents=True, exist_ok=True)
        (at / "calls").mkdir(exist_ok=True)
        source = REPO / "examples" / "plans" / "mock-fixed"
        (at / "mix.json").write_bytes((source / "mix.json").read_bytes())
        (at / "calls" / "ping.json").write_bytes((source / "calls" / "ping.json").read_bytes())
        (at / "targets.json").write_text(
            json.dumps({"list": [{"id": "task-a1b2c3", "address": address,
                                  "http_version": "http1"}]}),
            encoding="utf-8",
        )
        return at

    def test_no_engine_installed_says_so_rather_than_failing_obscurely(
        self, db, recording, tmp_path, monkeypatch
    ) -> None:
        """The normal state for most of this tool's life, and it should read as a
        thing to go and do rather than as a crash."""
        monkeypatch.setattr(
            "metrix_api.runner.engine.engine_binary", lambda: None
        )
        with pytest.raises(EngineError, match="METRIX_ENGINE"):
            asyncio.run(
                run_engine(
                    db, recording,
                    bundle=self.bundle(tmp_path / "b"),
                    started_at=STARTED,
                    stop=asyncio.Event(),
                    binary=None,
                )
            )

    def test_a_configured_binary_that_is_not_there_says_which(
        self, db, recording, tmp_path
    ) -> None:
        """Different from none being configured, and it should not surface as a
        FileNotFoundError thrown out of the event loop."""
        with pytest.raises(EngineError, match="does not exist"):
            asyncio.run(
                run_engine(
                    db, recording,
                    bundle=self.bundle(tmp_path / "b"),
                    started_at=STARTED,
                    stop=asyncio.Event(),
                    binary=tmp_path / "nowhere" / "metrix-engine",
                )
            )

    @pytest.mark.skipif(engine_binary() is None, reason="engine not built")
    def test_a_real_engine_streams_into_the_recording(
        self, db, recording, tmp_path
    ) -> None:
        """The whole handoff: spawn it, read what it says, land it on our clock.

        Pointed at a closed port, so it reports itself and fails setup without
        needing a target. What is being checked is the wiring, not the traffic.
        """
        started = datetime.now(UTC) - timedelta(seconds=5)
        result = asyncio.run(
            run_engine(
                db, recording,
                bundle=self.bundle(tmp_path / "b"),
                started_at=started,
                stop=asyncio.Event(),
            )
        )

        assert result.records > 0, result.diagnostic
        assert result.exit_code is not None
        # The engine reported itself before it discovered the target was not there.
        assert store.get(db, recording).engine_exit_code == result.exit_code

    @pytest.mark.skipif(engine_binary() is None, reason="engine not built")
    def test_a_bundle_that_is_not_a_directory_is_refused_before_spawning(
        self, db, recording, tmp_path
    ) -> None:
        with pytest.raises(EngineError, match="not a directory"):
            asyncio.run(
                run_engine(
                    db, recording, bundle=tmp_path / "absent",
                    started_at=STARTED, stop=asyncio.Event(),
                )
            )


# ---------------------------------------------------------------------- the routes


class TestRoutes:
    @pytest.fixture
    def home(self, tmp_path: Path):
        config = load_config(tmp_path).ensure_layout()
        save_profile(config, parse_profile(PROFILE))
        return config

    @pytest.fixture
    def loaded(self, home):
        profile = parse_profile(PROFILE)
        recording_id = store.new_id(STARTED)
        with open_store(home.database) as conn:
            store.create(
                conn, recording_id=recording_id, profile=profile,
                endpoints=profile.endpoints, kind="load", api_version="0.0.0",
                interval_s=1.0, started_at=STARTED,
            )
            store.add_sample(
                conn, recording_id,
                Sample(target_id="task-a1b2c3", t_ms=51_250, metrics={"cpu.busy": 80.0}),
            )
            ingest = Ingest(conn=conn, recording_id=recording_id, started_at=STARTED)
            for line in FIXTURE.read_text(encoding="utf-8").splitlines():
                ingest.line(line)
            store.finish(conn, recording_id, duration_ms=60_000)
        return recording_id

    def test_load_and_host_series_arrive_together_on_one_axis(self, home, loaded) -> None:
        """No second endpoint and no second clock: the question this page answers is
        whether the shape of one explains the shape of the other."""
        client = TestClient(create_app(home))
        body = client.get(f"/api/recordings/{loaded}/series").json()

        assert "cpu.busy" in body["metrics"]
        assert "load.achieved_rate" in body["metrics"]
        assert body["series"]["cpu.busy"]["task-a1b2c3"][0][0] == 51_250

    def test_the_load_table_reports_counts_and_no_percentiles(self, home, loaded) -> None:
        client = TestClient(create_app(home))
        body = client.get(f"/api/recordings/{loaded}/load").json()

        assert body["ran"] is True
        assert body["windows"] == 2
        assert body["engine_exit_code"] == 2, "the fixture ends on a failed SLO"
        assert body["rows"]
        assert all("p95" not in row for row in body["rows"])
        assert any(row["step"] is None for row in body["rows"]), "the chain's own row"

    def test_a_recording_with_no_engine_says_none_ran(self, home) -> None:
        profile = parse_profile(PROFILE)
        recording_id = store.new_id(STARTED)
        with open_store(home.database) as conn:
            store.create(
                conn, recording_id=recording_id, profile=profile,
                endpoints=profile.endpoints, kind="observation", api_version="0.0.0",
                interval_s=1.0, started_at=STARTED,
            )
        body = TestClient(create_app(home)).get(f"/api/recordings/{recording_id}/load").json()
        assert body["ran"] is False
        assert body["engine_exit_code"] is None, "no engine ran is not exit 0"


class TestWhatProducedTheRun:
    """`run_started` is where a stored run gets its identity (design-engine B2.6)."""

    def test_the_plan_and_the_engine_are_pinned_to_the_recording(self, db, recording) -> None:
        ingest_for(db, recording).record(started_record())
        row = store.get(db, recording)
        # The plan hash is what a series is filed under, so a run without it cannot
        # be told apart from a run of a plan that has since been edited.
        assert row.plan_hash == "sha256:x"
        assert row.plan_name == "p"
        assert row.engine_version == "0.0.0"

    def test_a_field_the_engine_did_not_send_is_left_alone(self, db, recording) -> None:
        record = {k: v for k, v in started_record().items() if k != "machine_profile"}
        ingest_for(db, recording).record(record)
        stored = db.execute(
            "SELECT machine_profile, seed FROM recording WHERE id = ?", (recording,)
        ).fetchone()
        # Not written as null over something, and not invented: an uncalibrated run
        # has no machine profile, which is a fact rather than a gap.
        assert stored["machine_profile"] is None
        assert stored["seed"] == 1


class TestThePhaseTimeline:
    """The engine owns the phases; the recording inherits them (design-engine 10.1)."""

    def phases(self, db, recording) -> list[tuple]:
        return [
            (row["phase"], row["from_ms"], row["to_ms"])
            for row in store.phases(db, recording)
        ]

    def run_through(self, db, recording, *, targets=("task-a1b2c3",)) -> Ingest:
        ingest = Ingest(
            conn=db, recording_id=recording, started_at=STARTED, targets=tuple(targets)
        )
        ingest.record(started_record())
        for t_ms, phase in [(7, "baseline"), (1026, "warmup"), (2014, "measure"),
                            (3014, "drain"), (5025, "settle")]:
            ingest.record({"type": "phase_changed", "t_ms": t_ms, "target_id": "load-target",
                           "phase": phase})
        ingest.record({"type": "run_finished", "t_ms": 6033, "exit_code": 0})
        return ingest

    def test_each_transition_closes_the_window_before_it(self, db, recording) -> None:
        self.run_through(db, recording)
        # Shifted onto the recording's clock: the engine started 11 seconds after it.
        assert self.phases(db, recording) == [
            ("baseline", 11007, 12026),
            ("warmup", 12026, 13014),
            ("measure", 13014, 14014),
            ("drain", 14014, 16025),
            ("settle", 16025, 17033),
        ]

    def test_the_last_phase_is_closed_by_the_end_of_the_run(self, db, recording) -> None:
        self.run_through(db, recording)
        settle = [p for p in self.phases(db, recording) if p[0] == "settle"][0]
        # Left open it would read as a window that never ended, and every figure over
        # it would silently include whatever came later.
        assert settle[2] is not None

    def test_phases_are_written_against_the_boxes_being_watched(self, db, recording) -> None:
        self.run_through(db, recording)
        filed = {row["target_id"] for row in store.phases(db, recording)}
        # Not "load-target", which is where the traffic went. Host samples are filed
        # under the boxes statistics come from, and a phase filed anywhere else is a
        # window nothing can be read over.
        assert filed == {"task-a1b2c3"}

    def test_a_phase_before_the_clock_is_counted_rather_than_guessed_at(
        self, db, recording
    ) -> None:
        ingest = Ingest(
            conn=db, recording_id=recording, started_at=STARTED, targets=("task-a1b2c3",)
        )
        ingest.record({"type": "phase_changed", "t_ms": 7, "target_id": "x", "phase": "baseline"})
        assert ingest.unplaced == 1
        assert self.phases(db, recording) == []

    def test_a_recording_with_no_targets_records_no_phases(self, db, recording) -> None:
        # Nothing to file them against. Silent rather than inventing a target row.
        ingest = Ingest(conn=db, recording_id=recording, started_at=STARTED)
        ingest.record(started_record())
        ingest.record({"type": "phase_changed", "t_ms": 7, "target_id": "x", "phase": "baseline"})
        assert self.phases(db, recording) == []
