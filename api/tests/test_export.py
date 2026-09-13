"""Purging the bulk, and taking a recording somewhere else.

Two things are being held in place here. A purge must never cost a number — it drops
request-level evidence and nothing else, and a recording that has been purged still
answers every question the archive asks of it. And an export must carry the sample
count wherever the figure goes, because a spreadsheet and an emailed report are
exactly where a figure gets separated from its caveat.
"""

from __future__ import annotations

import csv
import io
import json
from datetime import UTC, datetime
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from metrix_api import analysis, export
from metrix_api import purge as purging
from metrix_api.config import load_config
from metrix_api.main import create_app
from metrix_api.observer.metrics import Annotation, Gap, Sample
from metrix_api.profiles import parse_profile, save_profile
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
def home(tmp_path: Path):
    config = load_config(tmp_path).ensure_layout()
    save_profile(config, parse_profile(PROFILE))
    return config


@pytest.fixture
def client(home):
    return TestClient(create_app(home))


def at(day: int) -> datetime:
    return datetime(2026, 9, day, 12, 0, tzinfo=UTC)


def record(conn, *, started=None, targets=("box-a", "box-b"), n=30, gap=False, note=None):
    """A finished recording with samples on each box, and optionally a hole."""
    profile = parse_profile(PROFILE)
    recording_id = store.new_id(started or at(1))
    store.create(
        conn,
        recording_id=recording_id,
        profile=profile,
        endpoints=[e for e in profile.endpoints if e.id in targets],
        kind="observation",
        api_version="0.0.0",
        interval_s=1.0,
        started_at=started or at(1),
    )
    for target in targets:
        store.start_phase(conn, recording_id, target, "measure", 0)
        store.end_phase(conn, recording_id, target, "measure", (n - 1) * 1000)
        for i in range(n):
            t = i * 1000
            if gap and target == "box-a" and 10000 <= t <= 14000:
                continue
            store.add_sample(
                conn,
                recording_id,
                Sample(
                    target_id=target,
                    t_ms=t,
                    metrics={"cpu.busy": 10.0 + i * 0.1, "mem.used_bytes": 3.2e9},
                ),
            )
    if gap:
        store.add_gap(
            conn,
            recording_id,
            Gap(target_id="box-a", from_ms=10000, to_ms=14000, reason="ssh: reset"),
        )
    if note:
        store.add_annotation(conn, recording_id, note)
    store.finish(conn, recording_id, duration_ms=n * 1000)
    return recording_id


def bulk(home, recording_id, *, files=3, size=1024):
    """Stand in for what the engine writes: request events and error bodies."""
    directory = home.run_dir(recording_id)
    directory.mkdir(parents=True, exist_ok=True)
    for i in range(files):
        (directory / f"events-{i}.ndjson").write_bytes(b"x" * size)
    return directory


# ------------------------------------------------------------------------- purge


class TestPurge:
    def test_it_measures_what_is_there_rather_than_estimating(self, home) -> None:
        """A dialog naming a number it did not check is one nobody should act on."""
        with open_store(home.database) as conn:
            recording_id = record(conn)
            bulk(home, recording_id, files=4, size=2048)

            found = purging.describe(home, conn, recording_id)
            assert found.files == 4
            assert found.bytes == 4 * 2048
            assert found.anything

    def test_purging_drops_the_bulk_and_costs_no_figure(self, home) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn)
            bulk(home, recording_id)
            before = analysis.summaries(conn, recording_id)

            result = purging.purge(home, conn, recording_id)

            assert result.recordings == [recording_id]
            assert result.files == 3
            assert not any(home.run_dir(recording_id).iterdir()), "the bulk is gone"
            assert analysis.summaries(conn, recording_id) == before, "every figure survives"

    def test_the_notes_gaps_and_phases_survive(self, home) -> None:
        with open_store(home.database) as conn:
            recording_id = record(
                conn,
                gap=True,
                note=Annotation(code="host_count_changed", severity="warn", from_ms=0,
                                message="2 -> 3"),
            )
            bulk(home, recording_id)
            purging.purge(home, conn, recording_id)

            assert len(store.annotations(conn, recording_id)) == 2, "the note and the gap's"
            assert len(store.gaps(conn, recording_id)) == 1
            assert len(store.phases(conn, recording_id)) == 2

    def test_a_purged_run_keeps_its_place_in_the_trend(self, home) -> None:
        """Trend charts read the real numbers at every age (design-api 17.1)."""
        with open_store(home.database) as conn:
            ids = [record(conn, started=at(i + 1)) for i in range(3)]
            for recording_id in ids:
                bulk(home, recording_id)
            key = store.series_list(conn)[0].key
            before = [p.value for p in analysis.series_trends(conn, key).trends["cpu.busy"].points]

            for recording_id in ids:
                purging.purge(home, conn, recording_id)

            after = analysis.series_trends(conn, key).trends["cpu.busy"].points
            assert [p.value for p in after] == before
            assert len(after) == 3

    def test_the_run_directory_survives_as_an_empty_one(self, home) -> None:
        """A recording whose directory has vanished is a different kind of missing."""
        with open_store(home.database) as conn:
            recording_id = record(conn)
            bulk(home, recording_id)
            purging.purge(home, conn, recording_id)
            assert home.run_dir(recording_id).is_dir()

    def test_purging_is_stamped_even_when_nothing_was_there(self, home) -> None:
        """Purged-and-empty and never-written are different facts, and only one of
        them is a decision somebody made."""
        with open_store(home.database) as conn:
            recording_id = record(conn)
            assert store.get(conn, recording_id).purged_at is None

            result = purging.purge(home, conn, recording_id)
            assert result.skipped == [recording_id], "nothing freed"
            assert result.recordings == []
            assert store.get(conn, recording_id).purged_at is not None

    def test_purging_twice_is_not_an_error(self, home) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn)
            bulk(home, recording_id)
            purging.purge(home, conn, recording_id)
            again = purging.purge(home, conn, recording_id)
            assert again.skipped == [recording_id]

    def test_a_whole_series_goes_in_one_action(self, home) -> None:
        with open_store(home.database) as conn:
            ids = [record(conn, started=at(i + 1)) for i in range(3)]
            for recording_id in ids[:2]:
                bulk(home, recording_id, files=2)
            key = store.series_list(conn)[0].key

            result = purging.purge_series(home, conn, key)
            assert sorted(result.recordings) == sorted(ids[:2])
            assert result.files == 4
            assert result.skipped == [ids[2]], "named, not counted as done"


class TestPurgeRoutes:
    def test_the_confirmation_is_told_what_goes_and_what_stays(self, home, client) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn)
        bulk(home, recording_id, files=2, size=512)

        body = client.get(f"/api/recordings/{recording_id}/purgeable").json()
        assert body["files"] == 2
        assert body["bytes"] == 1024
        assert body["anything"] is True
        assert any("sample count" in k for k in body["keeps"]), "what survives is stated"
        assert body["drops"]

    def test_purging_over_the_wire_frees_the_files(self, home, client) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn)
        bulk(home, recording_id)

        body = client.post(f"/api/recordings/{recording_id}/purge").json()
        assert body["purged"] == [recording_id]
        assert body["files"] == 3
        assert client.get(f"/api/recordings/{recording_id}").json()["purged_at"]

    def test_a_purged_recording_still_serves_its_summary(self, home, client) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn)
        bulk(home, recording_id)
        client.post(f"/api/recordings/{recording_id}/purge")

        body = client.get(f"/api/recordings/{recording_id}/summary").json()
        assert body["targets"]["*"]["cpu.busy"]["n"] == 60

    def test_purging_something_that_does_not_exist_is_a_404(self, client) -> None:
        assert client.post("/api/recordings/not-a-run/purge").status_code == 404
        assert client.get("/api/recordings/not-a-run/purgeable").status_code == 404

    def test_a_series_purge_names_what_it_skipped(self, home, client) -> None:
        with open_store(home.database) as conn:
            ids = [record(conn, started=at(i + 1)) for i in range(2)]
            key = store.series_list(conn)[0].key
        bulk(home, ids[0])

        body = client.post("/api/series/purge", params={"key": key}).json()
        assert body["purged"] == [ids[0]]
        assert body["skipped"] == [ids[1]]

    def test_a_series_nobody_recorded_is_a_404(self, client) -> None:
        assert client.post("/api/series/purge", params={"key": "made|up"}).status_code == 404


# ------------------------------------------------------------------------ export


class TestJson:
    def test_it_carries_everything_including_the_samples(self, home, client) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn, gap=True)

        body = client.get(f"/api/recordings/{recording_id}/export.json").json()
        assert body["recording"]["id"] == recording_id
        assert body["summaries"]["*"]["cpu.busy"]["n"] > 0
        assert body["gaps"] and body["annotations"]
        assert len(body["samples"]) > 0
        assert set(body["samples"][0]) == {"target_id", "t_ms", "metric", "value"}

    def test_every_summary_in_it_carries_its_count(self, home, client) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn, n=4)

        body = client.get(f"/api/recordings/{recording_id}/export.json").json()
        for by_metric in body["summaries"].values():
            for summary in by_metric.values():
                assert "n" in summary and "supported" in summary

    def test_a_figure_the_count_cannot_support_is_absent_rather_than_zero(
        self, home, client
    ) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn, n=4)

        body = client.get(f"/api/recordings/{recording_id}/export.json").json()
        assert body["summaries"]["*"]["cpu.busy"]["p95"] is None


class TestCsv:
    def rows(self, text):
        return list(csv.DictReader(io.StringIO(text)))

    def test_the_series_is_one_sample_per_row(self, home, client) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn, n=5)

        response = client.get(f"/api/recordings/{recording_id}/export.csv")
        assert response.headers["content-type"].startswith("text/csv")
        assert "attachment" in response.headers["content-disposition"]

        rows = self.rows(response.text)
        assert len(rows) == 5 * 2 * 2, "five moments, two boxes, two metrics"
        assert rows[0].keys() >= {"recording_id", "target_id", "t_ms", "metric", "value"}

    def test_the_summary_carries_n_as_a_column_not_a_footnote(self, home, client) -> None:
        """A spreadsheet is exactly where a figure gets separated from its caveat."""
        with open_store(home.database) as conn:
            recording_id = record(conn)

        rows = self.rows(
            client.get(
                f"/api/recordings/{recording_id}/export.csv", params={"kind": "summary"}
            ).text
        )
        assert rows[0]["n"]
        assert {r["target"] for r in rows} == {"box-a", "box-b", "*"}

    def test_an_unsupported_figure_is_an_empty_cell_never_a_zero(self, home, client) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn, n=4)

        rows = self.rows(
            client.get(
                f"/api/recordings/{recording_id}/export.csv", params={"kind": "summary"}
            ).text
        )
        assert all(r["p95"] == "" for r in rows), "a blank a person can see is blank"
        assert all(r["median"] != "" for r in rows), "four samples support one"

    def test_an_unknown_shape_is_refused_rather_than_guessed(self, home, client) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn)
        response = client.get(
            f"/api/recordings/{recording_id}/export.csv", params={"kind": "everything"}
        )
        assert response.status_code == 422


class TestReport:
    def html(self, client, recording_id):
        response = client.get(f"/api/recordings/{recording_id}/report.html")
        assert response.status_code == 200
        assert response.headers["content-type"].startswith("text/html")
        return response.text

    def test_it_fetches_nothing(self, home, client) -> None:
        """One file that renders identically on a laptop with no network — the same
        reason the application vendors everything it draws with."""
        with open_store(home.database) as conn:
            recording_id = record(conn)

        page = self.html(client, recording_id)
        assert "<script" not in page
        assert "http://" not in page and "https://" not in page
        assert "<link" not in page
        assert "<style>" in page, "its own CSS, inline"

    def test_it_leads_with_whether_the_numbers_can_be_trusted(self, home, client) -> None:
        """Its reader cannot open the recording and check, so a note found after the
        figures are quoted is a note that arrived too late."""
        with open_store(home.database) as conn:
            clean = record(conn)
            broken = record(
                conn,
                started=at(2),
                note=Annotation(
                    code="target_unreachable", severity="invalid", from_ms=0,
                    message="never answered",
                ),
            )

        assert "No warnings" in self.html(client, clean)
        page = self.html(client, broken)
        assert "cannot be trusted" in page
        assert "target_unreachable" in page
        assert page.index("cannot be trusted") < page.index("The numbers")

    def test_every_figure_carries_its_count(self, home, client) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn, n=4)

        page = self.html(client, recording_id)
        assert "n=4" in page, "the unsupported p95 says what it rests on"
        assert ">4<" in page, "and the count is its own column"

    def test_a_gap_breaks_the_line_rather_than_being_drawn_across(self, home, client) -> None:
        """The one drawing rule this tool cannot compromise on, held in a third place."""
        with open_store(home.database) as conn:
            gapped = record(conn, gap=True)
            whole = record(conn, started=at(2))

        assert self.html(client, gapped).count("<polyline") > self.html(
            client, whole
        ).count("<polyline"), "box-a's line is drawn in two pieces"

    def test_a_recording_with_no_samples_says_so_rather_than_drawing_nothing(
        self, home, client
    ) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn, n=0)

        page = self.html(client, recording_id)
        assert "No samples were collected" in page

    def test_a_purge_is_recorded_in_the_report_and_costs_it_nothing(
        self, home, client
    ) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn)
        bulk(home, recording_id)
        before = self.html(client, recording_id)
        client.post(f"/api/recordings/{recording_id}/purge")
        after = self.html(client, recording_id)

        assert "was purged on" in after
        assert "every figure above is unaffected" in after
        # The numbers are identical; only the footer changed.
        assert before.split("<footer>")[0] == after.split("<footer>")[0]

    def test_a_message_with_markup_in_it_is_escaped(self, home, client) -> None:
        with open_store(home.database) as conn:
            recording_id = record(
                conn,
                note=Annotation(
                    code="odd", severity="warn", from_ms=0,
                    message='<script>alert("x")</script>',
                ),
            )

        page = self.html(client, recording_id)
        assert "<script>" not in page
        assert "&lt;script&gt;" in page

    def test_the_report_names_the_series_it_belongs_to(self, home, client) -> None:
        with open_store(home.database) as conn:
            recording_id = record(conn)
        assert "observation|staging" in self.html(client, recording_id)


def test_the_json_export_round_trips(home) -> None:
    """It has to be JSON, not merely dict-shaped: a datetime or a Row in there would
    only fail at the one moment somebody needed the file."""
    with open_store(home.database) as conn:
        recording_id = record(conn, gap=True)
        text = json.dumps(export.document(conn, recording_id))
    assert json.loads(text)["recording"]["id"] == recording_id
