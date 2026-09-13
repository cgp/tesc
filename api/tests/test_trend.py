"""Trends across a series: the band, what is held out of it, and what it refuses.

The arithmetic is tested on runs whose medians make the right answer obvious by
hand. A band whose expected value came out of the code under test would prove only
that the code agrees with itself.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from metrix_api import analysis
from metrix_api.config import load_config
from metrix_api.main import create_app
from metrix_api.observer.metrics import Annotation, Sample
from metrix_api.profiles import parse_profile, save_profile
from metrix_api.stats import (
    MIN_BAND_FRACTION as SAMPLE_BAND_FRACTION,
)
from metrix_api.stats import MIN_RUNS_FOR_BAND, Run, band_from, summarize, trend
from metrix_api.stats.trend import MIN_BAND_FRACTION
from metrix_api.store import open_store
from metrix_api.store import recordings as store

PROFILE = {
    "name": "staging",
    "endpoints": [
        {"id": "box-a", "address": "10.0.0.1:8080", "collect": {"transport": "ssh"}},
        {"id": "box-b", "address": "10.0.0.2:8080", "collect": {"transport": "ssh"}},
    ],
}


@pytest.fixture
def db(tmp_path: Path):
    with open_store(tmp_path / "metrix.db") as conn:
        yield conn


def run(at: str, value: float | None, *, n: int = 30, invalid: bool = False) -> Run:
    """One run whose pooled median is exactly `value`."""
    values = [] if value is None else [value] * n
    return Run(
        recording_id=f"r-{at}",
        at=at,
        summary=summarize("cpu.busy", values),
        invalid=invalid,
    )


def day(i: int) -> str:
    return f"2026-09-{i:02d}T00:00:00Z"


def steady(count: int, value: float = 10.0, *, start: int = 1) -> list[Run]:
    return [run(day(start + i), value) for i in range(count)]


# --------------------------------------------------------------------- arithmetic


class TestBand:
    def test_a_band_needs_a_history_to_be_measured_from(self) -> None:
        """Below the floor there is no band, rather than a very narrow one."""
        assert band_from([10.0] * (MIN_RUNS_FOR_BAND - 1)) is None
        assert band_from([10.0] * MIN_RUNS_FOR_BAND) is not None

    def test_the_band_is_two_iqrs_of_the_history(self) -> None:
        # 1..9: median 5, q1 3, q3 7, iqr 4, so the band is 8 either side.
        centre, half = band_from([float(i) for i in range(1, 10)])
        assert centre == 5.0
        assert half == pytest.approx(8.0)

    def test_a_metric_that_never_moves_still_gets_a_floor(self) -> None:
        """An IQR of zero would make every later run a regression."""
        centre, half = band_from([50.0] * 8)
        assert centre == 50.0
        assert half == pytest.approx(2.5), "5% of the centre"

    def test_the_floor_is_wider_between_runs_than_it_is_within_one(self) -> None:
        """A sample straying from its window's median and a run straying from the
        last run's are different distances, and one floor cannot serve both."""
        assert MIN_BAND_FRACTION > SAMPLE_BAND_FRACTION


class TestTrend:
    def test_the_first_runs_have_no_band_and_so_are_never_outside_one(self) -> None:
        result = trend("cpu.busy", steady(MIN_RUNS_FOR_BAND + 1))
        early = result.points[:MIN_RUNS_FOR_BAND]
        assert all(p.band is None for p in early)
        assert not any(p.outside for p in result.points)
        assert result.points[-1].band is not None, "the last one has enough behind it"

    def test_the_band_is_measured_from_the_runs_before_the_point(self) -> None:
        """A window containing the point widens to swallow the movement it exists
        to detect."""
        history = steady(6, 10.0)
        jumped = [*history, run(day(9), 40.0)]
        result = trend("cpu.busy", jumped)

        last = result.points[-1]
        assert last.center == 10.0, "normal is what the six runs before it did"
        assert last.outside and last.worse
        # And the band did not move to accommodate it.
        assert last.band == pytest.approx(0.5), "5% of 10, since a flat history has no IQR"

    def test_a_move_the_series_scatter_covers_is_not_a_finding(self) -> None:
        noisy = [run(day(i + 1), v) for i, v in enumerate([10.0, 20.0, 14.0, 24.0, 12.0, 22.0])]
        result = trend("cpu.busy", [*noisy, run(day(9), 25.0)])
        last = result.points[-1]
        assert not last.outside, "this series swings by ten between runs as a matter of course"

    def test_direction_decides_whether_a_move_is_the_bad_news(self) -> None:
        """More free memory and more CPU are both up; only one is a problem."""
        history = [run(day(i + 1), 10.0) for i in range(6)]
        for metric, value, expected in (
            ("cpu.busy", 40.0, True),
            ("cpu.busy", 1.0, False),
            ("mem.available_bytes", 40.0, False),
            ("mem.available_bytes", 1.0, True),
        ):
            points = trend(
                metric,
                [
                    *[Run(r.recording_id, r.at, summarize(metric, [10.0] * 30)) for r in history],
                    Run("last", day(9), summarize(metric, [value] * 30)),
                ],
            ).points
            assert points[-1].outside
            assert points[-1].worse is expected, f"{metric} moving to {value}"

    def test_an_invalid_run_is_drawn_but_does_not_set_normal(self) -> None:
        """Its numbers are the ones already known not to be trusted."""
        history = steady(6, 10.0)
        with_a_bad_one = [*history, run(day(9), 500.0, invalid=True), run(day(10), 11.0)]
        result = trend("cpu.busy", with_a_bad_one)

        bad, after = result.points[-2], result.points[-1]
        assert bad.value == 500.0, "still plotted -- that it failed validity is history too"
        assert after.center == 10.0, "and it did not drag normal up behind it"
        assert after.band_runs == 6

    def test_a_run_the_sample_count_cannot_support_has_no_value(self) -> None:
        """A gap in the line, not a zero, and nothing the band learns from."""
        result = trend("cpu.busy", [*steady(6, 10.0), run(day(9), 10.0, n=2), run(day(10), 10.0)])
        thin, after = result.points[-2], result.points[-1]
        assert thin.value is None and thin.n == 2
        assert not thin.outside, "no value is not a movement"
        assert after.band_runs == 6, "and it contributed nothing to the band"

    def test_runs_are_ordered_by_time_however_they_arrive(self) -> None:
        result = trend("cpu.busy", [run(day(3), 3.0), run(day(1), 1.0), run(day(2), 2.0)])
        assert [p.value for p in result.points] == [1.0, 2.0, 3.0]

    def test_the_window_forgets_a_setup_that_drifted_long_ago(self) -> None:
        """A history of 10, then a settled stretch of 40: 40 is what normal is now.

        A window that remembered both settings would call normal the midpoint of two
        values the series has never sat at, and would carry a band wide enough to
        cover the whole shift -- which is a band that can no longer detect anything.
        """
        old = [run(day(i + 1), 10.0) for i in range(6)]
        recent = [run(day(i + 7), 40.0) for i in range(6)]
        runs = [*old, *recent, run(day(20), 40.0)]

        settled = trend("cpu.busy", runs, window=6).points[-1]
        assert settled.center == 40.0 and not settled.outside

        remembering_everything = trend("cpu.busy", runs, window=20).points[-1]
        assert remembering_everything.center == 25.0, "a value the series never sat at"
        assert remembering_everything.band == pytest.approx(60.0)
        assert not trend("cpu.busy", [*runs, run(day(21), 90.0)], window=20).points[-1].outside
        assert trend("cpu.busy", [*runs, run(day(21), 90.0)], window=6).points[-1].outside

    def test_an_empty_series_is_a_trend_with_nothing_in_it(self) -> None:
        result = trend("cpu.busy", [])
        assert result.points == [] and result.latest is None and not result.banded


# ------------------------------------------------------------------ over the store


def record(db, *, value: float, started, targets=("box-a", "box-b"), phases=None):
    """A finished recording where every box reads `value` throughout."""
    profile = parse_profile(PROFILE)
    recording_id = store.new_id(started)
    store.create(
        db,
        recording_id=recording_id,
        profile=profile,
        endpoints=[e for e in profile.endpoints if e.id in targets],
        kind="observation",
        api_version="0.0.0",
        interval_s=1.0,
        started_at=started,
    )
    for target in targets:
        for phase, from_ms, to_ms in phases or (("measure", 0, 29000),):
            store.start_phase(db, recording_id, target, phase, from_ms)
            store.end_phase(db, recording_id, target, phase, to_ms)
        for i in range(30):
            store.add_sample(
                db,
                recording_id,
                Sample(target_id=target, t_ms=i * 1000, metrics={"cpu.busy": value}),
            )
    store.finish(db, recording_id, duration_ms=30000)
    return recording_id


def at(i: int):
    from datetime import UTC, datetime

    return datetime(2026, 9, i, 12, 0, tzinfo=UTC)


class TestSeriesTrends:
    def test_runs_of_one_setup_group_themselves(self, db) -> None:
        """No manual tagging, which would be skipped exactly when it mattered."""
        for i in range(3):
            record(db, value=10.0 + i, started=at(i + 1))

        listed = store.series_list(db)
        assert len(listed) == 1
        assert listed[0].runs == 3
        assert listed[0].key.startswith("observation|staging|")
        assert listed[0].first_at < listed[0].last_at

    def test_a_changed_setup_is_a_different_series_not_a_break_in_this_one(self, db) -> None:
        record(db, value=10.0, started=at(1))
        other = parse_profile({**PROFILE, "name": "prod"})
        rid = store.new_id(at(2))
        store.create(
            db,
            recording_id=rid,
            profile=other,
            endpoints=other.endpoints,
            kind="observation",
            api_version="0.0.0",
            interval_s=1.0,
            started_at=at(2),
        )
        store.finish(db, rid, duration_ms=1000)

        assert {row.profile for row in store.series_list(db)} == {"staging", "prod"}
        assert all(row.runs == 1 for row in store.series_list(db))

    def test_a_point_is_the_pooled_median_of_every_box(self, db) -> None:
        record(db, value=10.0, started=at(1))
        key = store.series_list(db)[0].key
        point = analysis.series_trends(db, key).trends["cpu.busy"].points[0]
        assert point.value == 10.0
        assert point.n == 60, "thirty samples on each of two boxes, in one distribution"

    def test_the_series_reads_in_time_order_regardless_of_id(self, db) -> None:
        for i, value in enumerate([10.0, 12.0, 11.0]):
            record(db, value=value, started=at(i + 1))
        key = store.series_list(db)[0].key
        points = analysis.series_trends(db, key).trends["cpu.busy"].points
        assert [p.value for p in points] == [10.0, 12.0, 11.0]

    def test_the_baseline_phase_is_trended_separately(self, db) -> None:
        """Environment drift: a metric that crept up alongside its own idle is not
        an application regression."""
        record(
            db,
            value=10.0,
            started=at(1),
            phases=(("baseline", 0, 9000), ("measure", 10000, 29000)),
        )
        key = store.series_list(db)[0].key
        result = analysis.series_trends(db, key)
        assert result.has_baseline_phase
        point = result.trends["cpu.busy"].points[0]
        assert point.baseline_value == 10.0
        assert point.baseline_n == 20, "ten seconds on each of two boxes"

    def test_an_observation_only_series_has_no_drift_line_to_draw(self, db) -> None:
        record(db, value=10.0, started=at(1))
        key = store.series_list(db)[0].key
        result = analysis.series_trends(db, key)
        assert not result.has_baseline_phase
        assert result.trends["cpu.busy"].points[0].baseline_value is None

    def test_an_invalid_run_is_counted_in_the_list_so_a_short_history_shows(self, db) -> None:
        record(db, value=10.0, started=at(1))
        broken = record(db, value=99.0, started=at(2))
        store.add_annotation(
            db,
            broken,
            Annotation(
                code="target_unreachable", severity="invalid", from_ms=0, message="never answered"
            ),
        )
        row = store.series_list(db)[0]
        assert (row.runs, row.invalid_runs) == (2, 1)

        points = analysis.series_trends(db, row.key).trends["cpu.busy"].points
        assert [p.invalid for p in points] == [False, True]


class TestRoutes:
    @pytest.fixture
    def home(self, tmp_path: Path):
        config = load_config(tmp_path).ensure_layout()
        save_profile(config, parse_profile(PROFILE))
        with open_store(config.database) as conn:
            for i in range(7):
                record(conn, value=10.0, started=at(i + 1))
            record(conn, value=40.0, started=at(9))
        return config

    @pytest.fixture
    def client(self, home):
        return TestClient(create_app(home))

    def test_the_list_says_what_each_series_is_and_how_long_it_is(self, client) -> None:
        body = client.get("/api/series").json()
        assert len(body["series"]) == 1
        row = body["series"][0]
        assert (row["runs"], row["profile"], row["kind"]) == (8, "staging", "observation")
        assert body["min_runs_for_band"] == MIN_RUNS_FOR_BAND

    def test_a_trend_carries_its_band_and_its_runs(self, client) -> None:
        key = client.get("/api/series").json()["series"][0]["key"]
        body = client.get("/api/series/trend", params={"key": key}).json()

        assert body["metrics"] == ["cpu.busy"]
        assert len(body["runs"]) == 8
        points = body["trends"]["cpu.busy"]["points"]
        assert [p["outside"] for p in points] == [*[False] * 7, True]
        assert points[-1]["worse"] is True
        assert points[-1]["center"] == 10.0

    def test_a_series_nobody_recorded_is_a_404_rather_than_an_empty_chart(
        self, client
    ) -> None:
        response = client.get("/api/series/trend", params={"key": "made|up|key"})
        assert response.status_code == 404

    def test_the_key_survives_the_round_trip_with_its_pipes_intact(self, client) -> None:
        """It is readable on purpose: when a trend starts a new line, the reason
        should be visible without decoding anything."""
        key = client.get("/api/series").json()["series"][0]["key"]
        assert "|" in key and key.endswith("|api=0.0.0")
        assert client.get("/api/series/trend", params={"key": key}).json()["series"]["key"] == key
