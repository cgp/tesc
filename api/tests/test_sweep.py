"""Ranking the boxes of one sweep against each other (design-api 17.6).

The thing being defended is the same discipline the trend view keeps, turned
sideways: **a box is measured against the other boxes and never against a set
containing itself**, and **being outside the band is not the verdict**. A sweep small
enough that its spread means nothing is ranked and not judged, because an accusation
the arithmetic cannot support is worse than no accusation.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from metrix_api import analysis
from metrix_api.config import load_config
from metrix_api.main import create_app
from metrix_api.observer.metrics import Annotation, Sample
from metrix_api.profiles import parse_profile
from metrix_api.stats import sweep as sweep_stats
from metrix_api.store import open_store
from metrix_api.store import recordings as store

START = datetime(2026, 9, 1, tzinfo=UTC)


def profile(count: int = 7, *, name: str = "staging"):
    return parse_profile(
        {
            "name": name,
            "endpoints": [
                {
                    "id": f"task-{i}",
                    "address": f"10.0.0.{i}:80",
                    "attributes": {
                        "instance_type": "m6i.large",
                        "image_digest": "sha256:old" if i == 3 else "sha256:new",
                    },
                    "collect": {"transport": "ssh"},
                }
                for i in range(count)
            ],
        }
    )


@pytest.fixture
def home(tmp_path: Path):
    return load_config(tmp_path).ensure_layout()


@pytest.fixture
def db(home):
    with open_store(home.database) as conn:
        yield conn


@pytest.fixture
def client(home):
    return TestClient(create_app(home))


def sweep_of(
    conn,
    *,
    at: datetime = START,
    values: dict[str, float],
    baselines: dict[str, float] | None = None,
    samples: int = 30,
    metric: str = "cpu.busy",
    prof=None,
) -> str:
    """One recording covering several boxes, each holding steady at its own level."""
    prof = prof or profile(len(values))
    recording_id = store.new_id(at)
    store.create(
        conn,
        recording_id=recording_id,
        profile=prof,
        endpoints=prof.endpoints,
        kind="observation",
        api_version="0.0.0",
        interval_s=1.0,
        started_at=at,
    )
    for box, level in values.items():
        if baselines is not None:
            store.start_phase(conn, recording_id, box, "baseline", 0)
            store.end_phase(conn, recording_id, box, "baseline", 4000)
            for t in range(5):
                store.add_sample(
                    conn,
                    recording_id,
                    Sample(target_id=box, t_ms=t * 1000, metrics={metric: baselines[box]}),
                )
        store.start_phase(conn, recording_id, box, "measure", 5000)
        store.end_phase(conn, recording_id, box, "measure", 5000 + samples * 1000)
        for t in range(samples):
            # A little jitter so an interquartile range has something to describe.
            store.add_sample(
                conn,
                recording_id,
                Sample(
                    target_id=box,
                    t_ms=5000 + t * 1000,
                    metrics={metric: level + (t % 3) * 0.4},
                ),
            )
    store.finish(conn, recording_id, duration_ms=40000)
    return recording_id


#: Six ordinary boxes and one that is twice the rest.
ORDINARY = {f"task-{i}": 40.0 + i * 0.5 for i in range(7)}
WITH_OUTLIER = {**ORDINARY, "task-3": 82.0}


class TestTheOddOneOut:
    def test_a_box_beyond_the_spread_of_the_others_is_flagged(self, db) -> None:
        result = analysis.sweep(db, sweep_of(db, values=WITH_OUTLIER))
        assert [f.target_id for f in result.findings] == ["task-3"]
        standing = result.findings[0].standing
        assert standing.worse is True
        assert standing.peers == 6
        assert standing.flagged

    def test_the_ordinary_boxes_are_not_flagged_for_being_ordinary(self, db) -> None:
        result = analysis.sweep(db, sweep_of(db, values=WITH_OUTLIER))
        flagged = {s.target_id for one in result.metrics for s in one.odd_ones_out}
        assert flagged == {"task-3"}

    def test_a_sweep_where_everything_agrees_finds_nothing(self, db) -> None:
        result = analysis.sweep(db, sweep_of(db, values=ORDINARY))
        assert result.findings == []
        assert result.judged is True

    def test_a_box_is_never_judged_against_a_set_containing_itself(self, db) -> None:
        # The point of the leave-one-out. Two boxes twice the rest would, between
        # them, widen a shared interquartile range enough to swallow both -- the
        # sideways version of measuring a trend band over a window holding its own
        # point.
        two_high = {**ORDINARY, "task-3": 82.0, "task-5": 84.0}
        result = analysis.sweep(db, sweep_of(db, values=two_high))
        assert {f.target_id for f in result.findings} == {"task-3", "task-5"}

        # And the proof that it is the exclusion doing the work: a band measured over
        # every box, the outlier included, does not flag it.
        every = [s.value for s in result.metrics[0].standings]
        centre, band = sweep_stats.band_from(every)
        assert abs(82.4 - centre) <= band, "a shared band would have hidden this"

    def test_being_outside_is_geometry_and_flagged_is_the_verdict(self, db) -> None:
        # A box with an invalid note is outside the band and is not the odd one out:
        # the verdict needs all three conditions, and the fields stay separate so a
        # page cannot read one as the other.
        recording_id = sweep_of(db, values=WITH_OUTLIER)
        store.add_annotation(
            db,
            recording_id,
            Annotation(
                code="collection_gap",
                severity="invalid",
                from_ms=0,
                message="half the window is missing",
                target_id="task-3",
            ),
        )
        result = analysis.sweep(db, recording_id)
        standing = next(
            s for s in result.metrics[0].standings if s.target_id == "task-3"
        )
        assert standing.outside is True
        assert standing.flagged is False
        assert "invalid note" in standing.unjudged_because
        assert result.findings == []

    def test_an_untrustworthy_box_does_not_set_normal_for_the_others(self, db) -> None:
        recording_id = sweep_of(db, values=WITH_OUTLIER)
        store.add_annotation(
            db,
            recording_id,
            Annotation(
                code="collection_gap",
                severity="invalid",
                from_ms=0,
                message="half the window is missing",
                target_id="task-3",
            ),
        )
        result = analysis.sweep(db, recording_id)
        ordinary = next(s for s in result.metrics[0].standings if s.target_id == "task-0")
        # Five peers, not six: the box nobody trusts is not one of them.
        assert ordinary.peers == 5

    def test_a_note_naming_no_box_covers_every_box_in_the_run(self, db) -> None:
        recording_id = sweep_of(db, values=WITH_OUTLIER)
        store.add_annotation(
            db,
            recording_id,
            Annotation(code="clock_skew", severity="invalid", from_ms=0, message="the whole run"),
        )
        result = analysis.sweep(db, recording_id)
        # "This run was invalid" is not a statement about one machine, so nothing in
        # it is left judging the rest.
        assert result.findings == []
        assert all(s.invalid for s in result.metrics[0].standings)


class TestWhatIsWithheld:
    def test_a_sweep_too_small_to_have_a_spread_is_ranked_and_not_judged(self, db) -> None:
        result = analysis.sweep(
            db, sweep_of(db, values={"task-0": 10.0, "task-1": 90.0, "task-2": 11.0})
        )
        assert result.judged is False
        assert result.findings == []
        # Still ranked: the ordering is useful at any size.
        assert [s.rank for s in result.metrics[0].standings] == [1, 2, 3]
        assert [s.target_id for s in result.metrics[0].standings] == ["task-0", "task-2", "task-1"]

    def test_a_box_that_was_not_judged_says_which_condition_it_missed(self, db) -> None:
        result = analysis.sweep(db, sweep_of(db, values={"task-0": 10.0, "task-1": 90.0}))
        reasons = [s.unjudged_because for s in result.metrics[0].standings]
        assert all("more would give the sweep a spread" in reason for reason in reasons)

    def test_a_box_with_too_few_samples_has_no_figure_and_sorts_last(self, db) -> None:
        recording_id = sweep_of(db, values=ORDINARY)
        # An eighth box that answered once, which is below the floor a median needs.
        # Added directly because the point is a box present in the sweep with almost
        # nothing behind it, which is what a machine that stopped answering looks like.
        db.execute(
            "INSERT INTO recording_target (recording_id, target_id, position, address,"
            " host_header, attributes) VALUES (?,?,?,?,?,?)",
            (recording_id, "task-7", 8, "10.0.0.7:80", None, "{}"),
        )
        store.start_phase(db, recording_id, "task-7", "measure", 5000)
        store.add_sample(
            db,
            recording_id,
            Sample(target_id="task-7", t_ms=6000, metrics={"cpu.busy": 500.0}),
        )
        result = analysis.sweep(db, recording_id)
        thin = next(s for s in result.metrics[0].standings if s.target_id == "task-7")
        assert thin.value is None
        assert thin.rank == len(result.metrics[0].standings), "a box with no figure sorts last"
        assert "too few samples" in thin.unjudged_because

    def test_a_single_box_is_not_a_sweep(self, db) -> None:
        result = analysis.sweep(db, sweep_of(db, values={"task-0": 10.0}))
        assert len(result.boxes) == 1
        assert result.judged is False


class TestReadingItRight:
    def test_the_ranking_follows_what_is_good_for_the_metric(self, db) -> None:
        # More free memory is better, so the biggest number ranks first.
        values = {f"task-{i}": 1_000.0 + i * 100 for i in range(7)}
        result = analysis.sweep(db, sweep_of(db, values=values, metric="mem.available_bytes"))
        ranked = result.metrics[0]
        assert ranked.worse == "down"
        assert ranked.standings[0].target_id == "task-6"

    def test_the_baseline_travels_with_the_measurement(self, db) -> None:
        # The distinction that otherwise sends somebody chasing a phantom: this box
        # was already busy before the traffic started.
        baselines = {box: 8.0 for box in WITH_OUTLIER}
        baselines["task-3"] = 70.0
        result = analysis.sweep(
            db, sweep_of(db, values=WITH_OUTLIER, baselines=baselines)
        )
        standing = result.findings[0].standing
        assert standing.baseline_value == 70.0
        assert standing.baseline_n == 5

    def test_a_phase_can_be_chosen_and_the_available_ones_are_listed(self, db) -> None:
        recording_id = sweep_of(
            db, values=WITH_OUTLIER, baselines={box: 8.0 for box in WITH_OUTLIER}
        )
        result = analysis.sweep(db, recording_id, phase="baseline")
        assert result.phase == "baseline"
        assert result.phases == ["baseline", "measure"]
        # Every box idles at the same level, so nothing is odd at rest.
        assert result.findings == []

    def test_the_attributes_come_with_the_boxes(self, db) -> None:
        result = analysis.sweep(db, sweep_of(db, values=WITH_OUTLIER))
        odd = next(box for box in result.boxes if box["target_id"] == "task-3")
        # The explanation is usually sitting in this row rather than in the number.
        assert odd["attributes"]["image_digest"] == "sha256:old"
        assert odd["position"] == 4


class TestAcrossRepeatedSweeps:
    def test_a_box_flagged_every_time_says_so(self, db) -> None:
        for day in range(4):
            sweep_of(db, at=START + timedelta(days=day), values=WITH_OUTLIER)
        latest = sweep_of(db, at=START + timedelta(days=9), values=WITH_OUTLIER)

        finding = analysis.sweep(db, latest).findings[0]
        assert len(finding.history) == 4
        assert finding.previously_flagged == 4
        # Oldest first, so the record reads forwards.
        assert [a.at for a in finding.history] == sorted(a.at for a in finding.history)
        assert all(a.rank == a.targets for a in finding.history), "worst every time"

    def test_a_box_unlucky_once_is_distinguishable_from_one_that_always_is(self, db) -> None:
        for day in range(4):
            sweep_of(db, at=START + timedelta(days=day), values=ORDINARY)
        latest = sweep_of(db, at=START + timedelta(days=9), values=WITH_OUTLIER)

        finding = analysis.sweep(db, latest).findings[0]
        assert len(finding.history) == 4
        assert finding.previously_flagged == 0

    def test_a_box_no_earlier_sweep_carried_has_no_record_rather_than_a_zero(
        self, db
    ) -> None:
        # The usual case for ephemeral tasks: the environment replaces them between
        # runs, so there is nothing to say about whether it is always this one.
        sweep_of(db, at=START, values=ORDINARY, prof=profile(7, name="staging"))
        renamed = {f"fresh-{i}": level for i, level in enumerate(ORDINARY.values())}
        renamed["fresh-3"] = 82.0
        later = parse_profile(
            {
                "name": "staging",
                "endpoints": [
                    {"id": box, "address": "10.0.0.1:80", "collect": {"transport": "ssh"}}
                    for box in renamed
                ],
            }
        )
        latest = sweep_of(db, at=START + timedelta(days=1), values=renamed, prof=later)
        finding = analysis.sweep(db, latest).findings[0]
        assert finding.history == []
        assert finding.previously_flagged == 0

    def test_history_is_read_over_the_same_window_as_the_sweep(self, db) -> None:
        # Reading this window's boxes against another window's would be comparing a
        # loaded box with a resting one and calling the difference a trend.
        baselines = {box: 8.0 for box in WITH_OUTLIER}
        for day in range(4):
            sweep_of(
                db, at=START + timedelta(days=day), values=WITH_OUTLIER, baselines=baselines
            )
        latest = sweep_of(db, at=START + timedelta(days=9), values=WITH_OUTLIER,
                          baselines=baselines)
        result = analysis.sweep(db, latest, phase="baseline")
        assert result.findings == [], "nothing is odd at rest"


class TestTheRoute:
    def test_every_figure_carries_its_sample_count(self, db, client) -> None:
        recording_id = sweep_of(db, values=WITH_OUTLIER)
        body = client.get(f"/api/recordings/{recording_id}/sweep").json()
        assert body["phase"] == "measure"
        assert body["judged"] is True
        for standing in body["metrics"][0]["standings"]:
            assert standing["n"] > 0 or standing["value"] is None

    def test_the_verdict_and_the_geometry_are_separate_fields(self, db, client) -> None:
        recording_id = sweep_of(db, values=WITH_OUTLIER)
        body = client.get(f"/api/recordings/{recording_id}/sweep").json()
        odd = next(s for s in body["metrics"][0]["standings"] if s["target_id"] == "task-3")
        assert odd["outside"] is True
        assert odd["flagged"] is True
        assert odd["unjudged_because"] is None

    def test_a_recording_that_does_not_exist_is_a_404(self, client) -> None:
        assert client.get("/api/recordings/nope/sweep").status_code == 404

    def test_the_phase_can_be_asked_for(self, db, client) -> None:
        recording_id = sweep_of(
            db, values=WITH_OUTLIER, baselines={box: 8.0 for box in WITH_OUTLIER}
        )
        body = client.get(f"/api/recordings/{recording_id}/sweep?phase=baseline").json()
        assert body["phase"] == "baseline"
        assert body["findings"] == []
