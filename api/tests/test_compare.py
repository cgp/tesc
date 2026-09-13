"""Comparing runs, and the one arithmetic rule that makes an aggregate mean anything.

Distributions merge; percentiles do not. Most of this file exists to hold that line
in place, because the wrong version — average the five p95s — is easier to write, is
never obviously wrong on a chart, and produces a number with no interpretation at all.
"""

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from metrix_api import analysis
from metrix_api.config import load_config
from metrix_api.main import create_app
from metrix_api.observer.metrics import Sample
from metrix_api.profiles import parse_profile, save_profile
from metrix_api.stats import MIN_FOR_P95, merge, quantile, summarize
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


def at(day: int) -> datetime:
    return datetime(2026, 9, day, 12, 0, tzinfo=UTC)


def record(
    db,
    *,
    values,
    started,
    profile=None,
    targets=("box-a",),
    phases=(("measure", 0, None),),
    api_version="0.0.0",
):
    """A finished recording carrying exactly the readings a test needs."""
    prof = parse_profile(profile or PROFILE)
    recording_id = store.new_id(started)
    store.create(
        db,
        recording_id=recording_id,
        profile=prof,
        endpoints=[e for e in prof.endpoints if e.id in targets],
        kind="observation",
        api_version=api_version,
        interval_s=1.0,
        started_at=started,
    )
    for target in targets:
        for phase, from_ms, to_ms in phases:
            store.start_phase(db, recording_id, target, phase, from_ms)
            if to_ms is not None:
                store.end_phase(db, recording_id, target, phase, to_ms)
        for t_ms, metrics in values:
            store.add_sample(db, recording_id, Sample(target_id=target, t_ms=t_ms, metrics=metrics))
    store.finish(db, recording_id, duration_ms=60000)
    return recording_id


def readings(metric, series, *, start=0, step=1000):
    return [(start + i * step, {metric: v}) for i, v in enumerate(series)]


def flat(metric, value, *, n=30, start=0, step=1000):
    return readings(metric, [value] * n, start=start, step=step)


# ------------------------------------------------------------------- the arithmetic


class TestMerge:
    def test_merging_is_not_averaging_the_percentiles(self) -> None:
        """The one thing this must never quietly become.

        Two windows that are each internally tight but sit far apart: the average of
        their medians lands between them, where neither window ever was. The merged
        median is a real order statistic of the readings that actually happened.
        """
        low = [10.0] * 20
        high = [100.0] * 4

        merged = merge("cpu.busy", [low, high])
        averaged = (summarize("cpu.busy", low).p50 + summarize("cpu.busy", high).p50) / 2

        assert merged.n == 24, "the count is the readings, not the windows"
        assert merged.p50 == 10.0, "the median of the 24 readings that exist"
        assert averaged == 55.0, "a value neither window ever produced"
        assert merged.p50 != averaged

    def test_merging_reaches_a_percentile_no_single_window_could_support(self) -> None:
        """The cheapest route past a short window (design-engine 12.2)."""
        short = [[float(i) for i in range(8)] for _ in range(3)]
        assert all(summarize("cpu.busy", w).p95 is None for w in short), "each too short"

        merged = merge("cpu.busy", short)
        assert merged.n == 24 >= MIN_FOR_P95
        assert merged.p95 is not None, "24 real readings support one"

    def test_the_merged_p95_is_the_p95_of_everything(self) -> None:
        windows = [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0]]
        everything = [v for w in windows for v in w]
        assert merge("cpu.busy", windows).p95 is None, "nine readings is still too few"
        assert merge("cpu.busy", windows).p50 == quantile(everything, 0.5)

    def test_merging_one_window_changes_nothing(self) -> None:
        one = [float(i) for i in range(30)]
        assert merge("cpu.busy", [one]) == summarize("cpu.busy", one)

    def test_merging_nothing_is_a_count_of_zero_rather_than_an_error(self) -> None:
        assert merge("cpu.busy", []).n == 0
        assert merge("cpu.busy", [[], []]).p50 is None


# ------------------------------------------------------------------ over the store


class TestCompareRuns:
    def test_each_run_keeps_its_own_column_and_its_own_count(self, db) -> None:
        first = record(db, values=flat("cpu.busy", 10.0), started=at(1))
        second = record(db, values=flat("cpu.busy", 20.0, n=10), started=at(2))

        result = analysis.compare_runs(db, [first, second])
        assert result.metrics == ["cpu.busy"]
        assert result.per_run["cpu.busy"][first].p50 == 10.0
        assert result.per_run["cpu.busy"][second].p50 == 20.0
        assert result.per_run["cpu.busy"][second].n == 10

    def test_the_reference_is_the_oldest_run_however_they_are_named(self, db) -> None:
        """A comparison reads as "what changed since", and since is the earlier one."""
        later = record(db, values=flat("cpu.busy", 20.0), started=at(9))
        earlier = record(db, values=flat("cpu.busy", 10.0), started=at(1))

        result = analysis.compare_runs(db, [later, earlier])
        assert result.reference_id == earlier
        assert [r.id for r in result.runs] == [earlier, later]
        assert result.deltas["cpu.busy"][later].change == 10.0

    def test_runs_of_one_setup_merge_into_a_better_version_of_it(self, db) -> None:
        ids = [record(db, values=flat("cpu.busy", 10.0, n=8), started=at(i + 1)) for i in range(3)]
        result = analysis.compare_runs(db, ids)

        assert result.mergeable
        assert result.merged["cpu.busy"].n == 24
        assert result.merged["cpu.busy"].p95 is not None, "no single run could support one"

    def test_runs_of_different_setups_are_shown_but_never_pooled(self, db) -> None:
        """Their pooled distribution describes nothing that exists, with a sample
        count that would make it look authoritative."""
        staging = record(db, values=flat("cpu.busy", 10.0), started=at(1))
        prod = record(
            db,
            values=flat("cpu.busy", 90.0),
            started=at(2),
            profile={**PROFILE, "name": "prod"},
        )

        result = analysis.compare_runs(db, [staging, prod])
        assert not result.mergeable
        assert result.merged == {}
        assert result.per_run["cpu.busy"].keys() == {staging, prod}, "side by side still stands"
        assert result.deltas["cpu.busy"][prod].change == 80.0

    def test_the_difference_is_named_rather_than_just_refused(self, db) -> None:
        staging = record(db, values=flat("cpu.busy", 10.0), started=at(1))
        newer = record(db, values=flat("cpu.busy", 10.0), started=at(2), api_version="0.2.0")

        result = analysis.compare_runs(db, [staging, newer])
        assert result.differences == {"api version": ["0.0.0", "0.2.0"]}
        assert "profile" not in result.differences, "only what actually differs"

    def test_one_run_on_its_own_has_nothing_to_merge_with(self, db) -> None:
        only = record(db, values=flat("cpu.busy", 10.0), started=at(1))
        result = analysis.compare_runs(db, [only])
        assert not result.mergeable and result.merged == {}
        assert result.deltas["cpu.busy"] == {}

    def test_a_phase_restricts_every_run_to_that_window_of_its_own_timeline(self, db) -> None:
        """Baseline against baseline is environment drift; measure against measure is
        the actual question (design-api 17.5)."""
        phases = (("baseline", 0, 9000), ("measure", 10000, 39000))
        ids = [
            record(
                db,
                values=flat("cpu.busy", 5.0, n=10) + flat("cpu.busy", 50.0, n=30, start=10000),
                started=at(i + 1),
                phases=phases,
            )
            for i in range(2)
        ]

        resting = analysis.compare_runs(db, ids, phase="baseline")
        assert resting.merged["cpu.busy"].p50 == 5.0
        assert resting.merged["cpu.busy"].n == 20

        working = analysis.compare_runs(db, ids, phase="measure")
        assert working.merged["cpu.busy"].p50 == 50.0

    def test_only_phases_every_run_recorded_are_offered(self, db) -> None:
        """A phase half the group lacks makes columns empty for no stated reason."""
        phased = record(
            db,
            values=flat("cpu.busy", 5.0),
            started=at(1),
            phases=(("baseline", 0, 9000), ("measure", 10000, 39000)),
        )
        plain = record(db, values=flat("cpu.busy", 5.0), started=at(2))

        assert analysis.shared_phases(db, [phased, plain]) == ["measure"]
        assert analysis.shared_phases(db, [phased, phased]) == ["baseline", "measure"]

    def test_a_metric_one_run_never_collected_is_absent_rather_than_zero(self, db) -> None:
        both = record(
            db,
            values=readings("cpu.busy", [10.0] * 30) + readings("fd.open", [4.0] * 30),
            started=at(1),
        )
        one = record(db, values=flat("cpu.busy", 10.0), started=at(2))

        result = analysis.compare_runs(db, [both, one])
        assert result.per_run["fd.open"].keys() == {both}
        assert one not in result.deltas["fd.open"]

    def test_a_run_that_does_not_exist_is_skipped_rather_than_fatal(self, db) -> None:
        real = record(db, values=flat("cpu.busy", 10.0), started=at(1))
        result = analysis.compare_runs(db, [real, "not-a-run"])
        assert [r.id for r in result.runs] == [real]


# ------------------------------------------------------------------------ the route


class TestRoute:
    @pytest.fixture
    def home(self, tmp_path: Path):
        config = load_config(tmp_path).ensure_layout()
        save_profile(config, parse_profile(PROFILE))
        return config

    @pytest.fixture
    def client(self, home):
        return TestClient(create_app(home))

    @pytest.fixture
    def runs(self, home):
        with open_store(home.database) as conn:
            return [
                record(conn, values=flat("cpu.busy", 10.0 + i, n=8), started=at(i + 1))
                for i in range(3)
            ]

    def test_a_group_of_one_setup_carries_its_merged_column(self, client, runs) -> None:
        body = client.get("/api/compare", params={"run": runs}).json()

        assert body["mergeable"] is True
        assert body["differences"] == {}
        row = body["rows"]["cpu.busy"]
        assert set(row["per_run"]) == set(runs)
        assert row["merged"]["n"] == 24
        assert row["merged"]["p95"] is not None

    def test_every_figure_carries_the_count_behind_it(self, client, runs) -> None:
        """design-api 14.4: a table is where a spurious percentile gets quoted."""
        row = client.get("/api/compare", params={"run": runs}).json()["rows"]["cpu.busy"]
        assert all("n" in s for s in row["per_run"].values())
        assert all("supported" in s for s in row["per_run"].values())

    def test_a_mixed_group_is_served_without_a_merged_column(self, client, home, runs) -> None:
        with open_store(home.database) as conn:
            other = record(
                conn,
                values=flat("cpu.busy", 90.0),
                started=at(9),
                profile={**PROFILE, "name": "prod"},
            )

        body = client.get("/api/compare", params={"run": [runs[0], other]}).json()
        assert body["mergeable"] is False
        assert body["rows"]["cpu.busy"]["merged"] is None
        assert body["differences"] == {"profile": ["prod", "staging"]}
        assert len(body["rows"]["cpu.busy"]["per_run"]) == 2, "still side by side"

    def test_fewer_than_two_runs_is_not_a_comparison(self, client, runs) -> None:
        assert client.get("/api/compare", params={"run": runs[:1]}).status_code == 400
        assert client.get("/api/compare").status_code == 400

    def test_more_runs_than_the_overlay_can_carry_are_refused_with_the_reason(
        self, client, home
    ) -> None:
        with open_store(home.database) as conn:
            many = [
                record(conn, values=flat("cpu.busy", 10.0), started=at(i + 1))
                for i in range(analysis.MAX_COMPARED + 1)
            ]
        response = client.get("/api/compare", params={"run": many})
        assert response.status_code == 400
        assert "fewer runs" in response.json()["detail"]

    def test_a_run_that_does_not_exist_is_a_404_rather_than_a_short_table(
        self, client, runs
    ) -> None:
        response = client.get("/api/compare", params={"run": [runs[0], "not-a-run"]})
        assert response.status_code == 404

    def test_the_phases_on_offer_are_the_shared_ones(self, client, home) -> None:
        with open_store(home.database) as conn:
            ids = [
                record(
                    conn,
                    values=flat("cpu.busy", 5.0),
                    started=at(i + 1),
                    phases=(("baseline", 0, 9000), ("measure", 10000, 39000)),
                )
                for i in range(2)
            ]
        body = client.get("/api/compare", params={"run": ids}).json()
        assert body["phases"] == ["baseline", "measure"]
        assert body["phase"] is None, "the whole recording unless one is asked for"

        narrowed = client.get("/api/compare", params={"run": ids, "phase": "baseline"}).json()
        assert narrowed["phase"] == "baseline"
