"""Baselines, deltas, recovery, and the rule that a number carries its count.

The arithmetic is tested on values chosen so the right answer is obvious by hand --
a percentile test whose expected value came out of the code it is testing proves
only that the code is consistent with itself.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from metrix_api import analysis
from metrix_api.observer.metrics import NOT_RETURNED_TO_BASELINE, Annotation, Sample
from metrix_api.profiles import parse_profile
from metrix_api.stats import (
    MIN_FOR_MEDIAN,
    MIN_FOR_P95,
    Summary,
    compare,
    quantile,
    recovery,
    summarize,
    worse_direction,
)
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


def profile():
    return parse_profile(PROFILE)


def record(db, *, values, targets=("box-a",), phases=(("measure", 0, None),), started=None):
    """A finished recording carrying exactly the samples a test needs.

    Written straight into the store rather than collected: what is under test is the
    reading, and driving a collector to produce a chosen distribution would be a
    fixture with a scheduler in it.
    """
    prof = profile()
    recording_id = store.new_id(started)
    store.create(
        db,
        recording_id=recording_id,
        profile=prof,
        endpoints=[e for e in prof.endpoints if e.id in targets],
        kind="observation",
        api_version="0.0.0",
        interval_s=1.0,
        started_at=started,
    )
    for target in targets:
        for phase, from_ms, to_ms in phases:
            store.start_phase(db, recording_id, target, phase, from_ms)
            if to_ms is not None:
                store.end_phase(db, recording_id, target, phase, to_ms)
        for t_ms, metrics in values.get(target, []):
            store.add_sample(db, recording_id, Sample(target_id=target, t_ms=t_ms, metrics=metrics))
    store.finish(db, recording_id, duration_ms=60000)
    return recording_id


def flat(metric, value, *, n=30, step=1000, start=0):
    """`n` samples of one metric at one value."""
    return [(start + i * step, {metric: value}) for i in range(n)]


# ------------------------------------------------------------------- the arithmetic


class TestSummary:
    def test_a_percentile_the_count_cannot_support_is_withheld(self) -> None:
        few = summarize("cpu.busy", [1.0, 2.0, 3.0, 4.0])
        assert few.n == 4
        assert few.p50 == 2.5, "a median of four points is fine"
        assert few.p95 is None, "a p95 of four points is the maximum wearing a hat"

        enough = summarize("cpu.busy", [float(i) for i in range(MIN_FOR_P95)])
        assert enough.p95 is not None

    def test_two_points_are_not_a_distribution(self) -> None:
        pair = summarize("cpu.busy", [10.0, 20.0])
        assert pair.n == 2
        assert (pair.minimum, pair.maximum, pair.mean) == (10.0, 20.0, 15.0)
        assert pair.p50 is None and pair.iqr is None
        assert not pair.supported
        assert MIN_FOR_MEDIAN == 3

    def test_an_empty_window_reports_no_samples_rather_than_zero(self) -> None:
        empty = summarize("cpu.busy", [])
        assert empty.n == 0
        assert empty.mean is None, "zero samples is not a mean of zero"

    def test_quantiles_interpolate_between_order_statistics(self) -> None:
        values = [1.0, 2.0, 3.0, 4.0]
        assert quantile(values, 0.0) == 1.0
        assert quantile(values, 1.0) == 4.0
        assert quantile(values, 0.5) == 2.5
        # position = 0.25 * 3 = 0.75, so three quarters of the way from 1 to 2.
        assert quantile(values, 0.25) == pytest.approx(1.75)

    def test_the_worse_direction_is_known_per_metric(self) -> None:
        """Deltas are coloured by good or bad, not by sign."""
        assert worse_direction("cpu.busy") == "up"
        assert worse_direction("mem.used_bytes") == "up"
        assert worse_direction("mem.available_bytes") == "down"
        assert worse_direction("cpu.idle") == "down"


class TestCompare:
    def test_a_move_inside_the_noise_band_is_not_a_finding(self) -> None:
        was = summarize("cpu.busy", [10.0, 12.0, 14.0, 16.0, 18.0])  # iqr 4, median 14
        now = summarize("cpu.busy", [13.0, 15.0, 17.0, 19.0, 21.0])  # median 17, +3
        delta = compare(was, now)
        assert delta.change == 3.0
        assert delta.band == pytest.approx(8.0), "2 x the baseline IQR"
        assert not delta.outside_band

    def test_a_move_beyond_it_is_flagged_with_its_direction(self) -> None:
        was = summarize("cpu.busy", [10.0, 12.0, 14.0, 16.0, 18.0])
        now = summarize("cpu.busy", [30.0] * 5)
        delta = compare(was, now)
        assert delta.outside_band
        assert delta.worse, "more CPU is the bad direction"
        assert delta.change_pct == pytest.approx(100 * 16 / 14)

    def test_a_big_move_the_good_way_is_flagged_and_is_not_worse(self) -> None:
        was = summarize("mem.available_bytes", [1.0e9, 1.2e9, 1.4e9, 1.6e9, 1.8e9])
        now = summarize("mem.available_bytes", [4.0e9] * 5)
        delta = compare(was, now)
        assert delta.outside_band
        assert not delta.worse, "more free memory is not a regression"

    def test_a_steady_metric_still_gets_a_band(self) -> None:
        """An IQR of zero would make every change significant."""
        was = summarize("conn.established", [100.0] * 10)
        assert was.iqr == 0.0
        assert compare(was, summarize("conn.established", [101.0] * 10)).outside_band is False
        assert compare(was, summarize("conn.established", [140.0] * 10)).outside_band is True

    def test_a_comparison_neither_window_supports_reports_nothing(self) -> None:
        delta = compare(summarize("cpu.busy", [1.0]), summarize("cpu.busy", [50.0] * 10))
        assert not delta.comparable
        assert delta.change is None, "a number nobody can support is not reported"
        assert not delta.outside_band


class TestRecovery:
    def base(self):
        return summarize("mem.used_bytes", [100.0, 102.0, 104.0, 106.0, 108.0])

    def test_a_metric_that_comes_back_reports_when(self) -> None:
        settle = [(0, 400.0), (1000, 300.0), (2000, 120.0), (3000, 104.0), (4000, 103.0)]
        result = recovery("mem.used_bytes", self.base(), settle)
        assert result.returned
        assert result.recovered_ms == 3000
        assert result.n == 5

    def test_a_dip_into_the_band_is_not_a_recovery(self) -> None:
        """It has to stay there; the first touch is the most flattering reading."""
        settle = [(0, 400.0), (1000, 104.0), (2000, 500.0), (3000, 600.0)]
        result = recovery("mem.used_bytes", self.base(), settle)
        assert not result.returned
        assert result.recovered_ms is None

    def test_the_peak_after_stop_is_kept(self) -> None:
        """Queues and flushes often peak during drain, not under load."""
        settle = [(0, 300.0), (1000, 900.0), (2000, 105.0)]
        result = recovery("mem.used_bytes", self.base(), settle)
        assert (result.peak, result.peak_at_ms) == (900.0, 1000)

    def test_a_metric_that_never_returns_and_drifted_up_is_the_leak_signal(self) -> None:
        settle = [(0, 400.0), (1000, 420.0), (2000, 430.0)]
        result = recovery("mem.used_bytes", self.base(), settle)
        assert not result.returned
        assert result.leaked

    def test_ending_high_the_good_way_is_not_a_leak(self) -> None:
        base = summarize("mem.available_bytes", [100.0, 102.0, 104.0, 106.0, 108.0])
        result = recovery("mem.available_bytes", base, [(0, 400.0), (1000, 420.0)])
        assert not result.returned
        assert not result.leaked, "ending with more memory free is not a leak"

    def test_nothing_to_judge_against_is_no_measurement(self) -> None:
        assert recovery("cpu.busy", summarize("cpu.busy", [1.0]), [(0, 5.0)]) is None
        assert recovery("cpu.busy", self.base(), []) is None


# --------------------------------------------------------------- against the store


class TestPhaseWindows:
    def test_each_phase_is_summarised_on_its_own(self, db) -> None:
        recording = record(
            db,
            phases=(("baseline", 0, 4000), ("measure", 5000, 9000)),
            values={
                "box-a": [
                    *flat("cpu.busy", 5.0, n=5, start=0),
                    *flat("cpu.busy", 70.0, n=5, start=5000),
                ]
            },
        )
        windows = {w.phase: w for w in analysis.phase_windows(db, recording)}
        assert windows["baseline"].metrics["cpu.busy"].p50 == 5.0
        assert windows["measure"].metrics["cpu.busy"].p50 == 70.0
        assert windows["baseline"].metrics["cpu.busy"].n == 5

    def test_the_pooled_view_describes_a_typical_box(self, db) -> None:
        recording = record(
            db,
            targets=("box-a", "box-b"),
            values={
                "box-a": flat("cpu.busy", 10.0, n=10),
                "box-b": flat("cpu.busy", 30.0, n=10),
            },
        )
        found = analysis.summaries(db, recording)
        assert found["box-a"]["cpu.busy"].p50 == 10.0
        assert found["box-b"]["cpu.busy"].p50 == 30.0
        assert found[analysis.ENVIRONMENT]["cpu.busy"].p50 == 20.0
        assert found[analysis.ENVIRONMENT]["cpu.busy"].n == 20


class TestBaselines:
    def test_marking_one_demotes_the_last(self, db) -> None:
        first = record(db, values={"box-a": flat("cpu.busy", 5.0)})
        second = record(db, values={"box-a": flat("cpu.busy", 6.0)})

        store.mark_baseline(db, first)
        assert store.baseline_for(db, store.get(db, first).series_key).id == first

        store.mark_baseline(db, second)
        assert store.get(db, first).is_baseline is False
        assert store.baseline_for(db, store.get(db, second).series_key).id == second

    def test_an_invalid_recording_is_refused(self, db) -> None:
        """The whole point of the severity: a poisoned baseline poisons everything."""
        recording = record(db, values={"box-a": flat("cpu.busy", 5.0)})
        store.add_annotation(
            db,
            recording,
            Annotation(
                code="target_unreachable",
                severity="invalid",
                from_ms=0,
                message="box-b never answered",
            ),
        )
        with pytest.raises(store.BaselineRefused, match="target_unreachable"):
            store.mark_baseline(db, recording)
        assert store.get(db, recording).is_baseline is False

        marked = store.mark_baseline(db, recording, override=True)
        assert marked.is_baseline is True

    def test_a_warning_does_not_block(self, db) -> None:
        recording = record(db, values={"box-a": flat("cpu.busy", 5.0)})
        store.add_annotation(
            db,
            recording,
            Annotation(code="collection_gap", severity="warn", from_ms=0, message="1s lost"),
        )
        assert store.mark_baseline(db, recording).is_baseline is True


class TestAgainstBaseline:
    def test_a_recording_with_no_baseline_compares_against_nothing(self, db) -> None:
        recording = record(db, values={"box-a": flat("cpu.busy", 5.0)})
        comparison = analysis.against_baseline(db, recording)
        assert comparison.baseline_id is None
        assert comparison.deltas == {}

    def test_an_environment_that_is_busier_than_normal_says_so(self, db) -> None:
        quiet = [(i * 1000, {"cpu.busy": 5.0 + i % 3}) for i in range(30)]
        busy = [(i * 1000, {"cpu.busy": 55.0 + i % 3}) for i in range(30)]
        normal = record(db, values={"box-a": quiet})
        store.mark_baseline(db, normal)
        today = record(db, values={"box-a": busy})

        comparison = analysis.against_baseline(db, today)
        assert comparison.baseline_id == normal
        delta = comparison.deltas["box-a"]["cpu.busy"]
        assert delta.change == pytest.approx(50.0)
        assert delta.outside_band and delta.worse
        # Once. The single box agrees with the pooled view, so it adds nothing.
        assert [(t, d.metric) for t, d in comparison.moved] == [(analysis.ENVIRONMENT, "cpu.busy")]

    def test_targets_that_only_exist_on_one_side_are_reported_not_dropped(self, db) -> None:
        """A discovered environment replaces its tasks on every deployment."""
        normal = record(db, targets=("box-a",), values={"box-a": flat("cpu.busy", 5.0)})
        store.mark_baseline(db, normal)
        today = record(db, targets=("box-b",), values={"box-b": flat("cpu.busy", 5.0)})

        comparison = analysis.against_baseline(db, today)
        assert comparison.only_now == ["box-b"]
        assert comparison.only_baseline == ["box-a"]
        # The pooled view still answers, which is the point of having it.
        assert analysis.ENVIRONMENT in comparison.deltas

    def test_a_baseline_is_not_compared_with_itself(self, db) -> None:
        recording = record(db, values={"box-a": flat("cpu.busy", 5.0)})
        store.mark_baseline(db, recording)
        assert analysis.against_baseline(db, recording).baseline_id is None


class TestRecoveryAgainstTheStore:
    def phased(self, db, settle_values):
        return record(
            db,
            phases=(("baseline", 0, 4000), ("measure", 5000, 9000), ("settle", 10000, 20000)),
            values={
                "box-a": [
                    *[(i * 1000, {"mem.used_bytes": 100.0 + i}) for i in range(5)],
                    *[(5000 + i * 1000, {"mem.used_bytes": 900.0}) for i in range(5)],
                    *[
                        (10000 + i * 1000, {"mem.used_bytes": v})
                        for i, v in enumerate(settle_values)
                    ],
                ]
            },
        )

    def test_an_observation_recording_has_no_settle_and_claims_nothing(self, db) -> None:
        recording = record(db, values={"box-a": flat("mem.used_bytes", 100.0)})
        assert analysis.recoveries(db, recording) == {}
        assert analysis.leaks(db, recording) == []

    def test_a_metric_that_came_back_is_not_a_leak(self, db) -> None:
        recording = self.phased(db, [800.0, 400.0, 105.0, 103.0, 102.0])
        result = analysis.recoveries(db, recording)["box-a"]["mem.used_bytes"]
        assert result.returned
        assert not result.leaked
        assert analysis.leaks(db, recording) == []

    def test_a_metric_that_did_not_is_the_leak_signal(self, db) -> None:
        recording = self.phased(db, [800.0, 780.0, 760.0, 750.0, 745.0])
        (target, result) = analysis.leaks(db, recording)[0]
        assert target == "box-a"
        assert result.metric == "mem.used_bytes"
        assert not result.returned


# ------------------------------------------------------------------------ the routes


@pytest.fixture
def api(tmp_path):
    from fastapi.testclient import TestClient

    from metrix_api.config import load_config
    from metrix_api.main import create_app
    from metrix_api.profiles import save_profile

    config = load_config(tmp_path).ensure_layout()
    save_profile(config, profile())
    return TestClient(create_app(config))


class TestRoutes:
    def seed(self, api, tmp_path, cpu_now=55.0):
        with open_store(tmp_path / "metrix.db") as conn:
            normal = record(
                conn, values={"box-a": [(i * 1000, {"cpu.busy": 5.0 + i % 3}) for i in range(30)]}
            )
            store.mark_baseline(conn, normal)
            today = record(
                conn,
                values={"box-a": [(i * 1000, {"cpu.busy": cpu_now + i % 3}) for i in range(30)]},
            )
        return normal, today

    def test_the_summary_carries_the_count_behind_every_number(self, api, tmp_path) -> None:
        _, today = self.seed(api, tmp_path)
        body = api.get(f"/api/recordings/{today}/summary").json()
        cpu = body["targets"]["box-a"]["cpu.busy"]
        assert cpu["n"] == 30
        assert cpu["p95"] is not None
        assert body["targets"]["*"]["cpu.busy"]["n"] == 30
        assert [p["phase"] for p in body["phases"]] == ["measure"]

    def test_the_comparison_names_what_moved(self, api, tmp_path) -> None:
        normal, today = self.seed(api, tmp_path)
        body = api.get(f"/api/recordings/{today}/comparison").json()
        assert body["baseline_id"] == normal
        assert body["moved"][0]["worse"] is True
        assert body["moved"][0]["change"] == pytest.approx(50.0)

    def test_marking_a_baseline_is_refused_for_an_invalid_recording(self, api, tmp_path) -> None:
        _, today = self.seed(api, tmp_path)
        with open_store(tmp_path / "metrix.db") as conn:
            store.add_annotation(
                conn,
                today,
                Annotation(
                    code="target_unreachable", severity="invalid", from_ms=0, message="dead"
                ),
            )
        refused = api.post(f"/api/recordings/{today}/baseline")
        assert refused.status_code == 409
        assert "target_unreachable" in refused.json()["detail"]

        forced = api.post(f"/api/recordings/{today}/baseline?override=true")
        assert forced.status_code == 200
        assert forced.json()["is_baseline"] is True, "the API sends a JSON boolean"

    def test_a_baseline_can_be_cleared(self, api, tmp_path) -> None:
        normal, _ = self.seed(api, tmp_path)
        assert api.delete(f"/api/recordings/{normal}/baseline").json()["is_baseline"] is False

    def test_recovery_is_empty_without_a_settle_phase(self, api, tmp_path) -> None:
        _, today = self.seed(api, tmp_path)
        assert api.get(f"/api/recordings/{today}/recovery").json()["targets"] == {}


def test_a_leak_becomes_an_annotation_when_a_recording_closes(db) -> None:
    """The store-level signal, raised where a person will see it."""
    from metrix_api.observer.collector import Clock
    from metrix_api.recording import Recorder

    recording = record(
        db,
        phases=(("baseline", 0, 4000), ("settle", 10000, 20000)),
        values={
            "box-a": [
                *[(i * 1000, {"mem.used_bytes": 100.0 + i}) for i in range(5)],
                *[(10000 + i * 1000, {"mem.used_bytes": 800.0 - i}) for i in range(5)],
            ]
        },
    )
    recorder = Recorder(
        conn=db,
        recording_id=recording,
        profile=profile(),
        clock=Clock.start(),
        interval=None,
    )
    recorder._note_leaks(20000)

    found = [a for a in store.annotations(db, recording) if a["code"] == NOT_RETURNED_TO_BASELINE]
    assert len(found) == 1
    assert found[0]["severity"] == "warn"
    assert "mem.used_bytes" in found[0]["message"]


def _summary_is_frozen() -> None:
    """Type-level reminder: a Summary is data, never a mutable accumulator."""
    assert Summary(metric="x", n=0)
