"""Schema, migrations, and the constraints that protect the recordings."""

from __future__ import annotations

import sqlite3
from pathlib import Path

import pytest

from metrix_api.store import (
    StoreError,
    applied_versions,
    connect,
    discover_migrations,
    migrate,
    open_store,
    transaction,
)

RECORDING = """
    INSERT INTO recording
        (id, kind, status, profile, api_version, series_key, started_at)
    VALUES (?, ?, 'running', 'staging', '0.0.0', 'staging/observation', '2026-09-12T14:03:11Z')
"""


@pytest.fixture
def db(tmp_path: Path):
    with open_store(tmp_path / "metrix.db") as conn:
        yield conn


class TestMigrations:
    def test_files_are_discovered_in_order(self) -> None:
        migrations = discover_migrations()
        assert [m.version for m in migrations] == list(range(1, len(migrations) + 1))
        assert migrations[0].name == "observation"

    def test_applying_records_the_version(self, tmp_path: Path) -> None:
        """Every shipped migration, in order, with no gaps -- rather than a literal
        list that has to be edited each time one is added."""
        expected = [m.version for m in discover_migrations()]
        assert expected == list(range(1, len(expected) + 1))

        conn = connect(tmp_path / "metrix.db")
        applied = migrate(conn)
        assert [m.version for m in applied] == expected
        assert applied_versions(conn) == expected
        conn.close()

    def test_migrating_twice_is_a_no_op(self, tmp_path: Path) -> None:
        conn = connect(tmp_path / "metrix.db")
        migrate(conn)
        assert migrate(conn) == [], "an up-to-date database should apply nothing"
        conn.close()

    def test_a_gap_in_numbering_is_refused(self, tmp_path: Path) -> None:
        (tmp_path / "001_first.sql").write_text("SELECT 1;", encoding="utf-8")
        (tmp_path / "003_third.sql").write_text("SELECT 1;", encoding="utf-8")
        with pytest.raises(StoreError, match="no gaps"):
            discover_migrations(tmp_path)

    def test_a_misnamed_file_is_refused(self, tmp_path: Path) -> None:
        (tmp_path / "initial.sql").write_text("SELECT 1;", encoding="utf-8")
        with pytest.raises(StoreError, match="NNN_lower_snake"):
            discover_migrations(tmp_path)

    def test_a_failing_migration_leaves_nothing_behind(self, tmp_path: Path) -> None:
        (tmp_path / "001_broken.sql").write_text(
            "CREATE TABLE good (x INTEGER); CREATE TABLE bad (;", encoding="utf-8"
        )
        conn = connect(tmp_path / "metrix.db")
        with pytest.raises(StoreError, match="001_broken"):
            migrate(conn, tmp_path)
        tables = [r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")]
        assert "good" not in tables, "a failed migration must roll back entirely"
        assert applied_versions(conn) == []
        conn.close()


class TestConnection:
    def test_foreign_keys_are_on(self, db: sqlite3.Connection) -> None:
        # Off by default in SQLite, which would make every cascade below inert.
        assert db.execute("PRAGMA foreign_keys").fetchone()[0] == 1

    def test_write_ahead_logging_is_on(self, db: sqlite3.Connection) -> None:
        # The live view reads the same database the collector is writing.
        assert db.execute("PRAGMA journal_mode").fetchone()[0].lower() == "wal"


class TestSchema:
    def test_a_recording_round_trips(self, db: sqlite3.Connection) -> None:
        db.execute(RECORDING, ("rec-1", "observation"))
        row = db.execute("SELECT * FROM recording WHERE id = 'rec-1'").fetchone()
        assert row["kind"] == "observation"
        assert row["addressing_mode"] == "load_balancer"
        assert row["is_baseline"] == 0

    def test_an_unknown_kind_is_rejected(self, db: sqlite3.Connection) -> None:
        with pytest.raises(sqlite3.IntegrityError):
            db.execute(RECORDING, ("rec-2", "smoke-test"))

    def test_an_unknown_phase_is_rejected(self, db: sqlite3.Connection) -> None:
        db.execute(RECORDING, ("rec-3", "observation"))
        with pytest.raises(sqlite3.IntegrityError):
            db.execute(
                "INSERT INTO phase (recording_id, target_id, phase, from_ms) VALUES (?,?,?,?)",
                ("rec-3", "host-a", "cooldown", 0),
            )

    def test_samples_cannot_outlive_their_recording(self, db: sqlite3.Connection) -> None:
        db.execute(RECORDING, ("rec-4", "observation"))
        db.execute(
            "INSERT INTO host_sample (recording_id, target_id, t_ms, metric, value)"
            " VALUES (?,?,?,?,?)",
            ("rec-4", "host-a", 1000, "cpu.user", 12.5),
        )
        db.execute("DELETE FROM recording WHERE id = 'rec-4'")
        assert db.execute("SELECT count(*) FROM host_sample").fetchone()[0] == 0

    def test_an_orphan_sample_is_refused(self, db: sqlite3.Connection) -> None:
        with pytest.raises(sqlite3.IntegrityError):
            db.execute(
                "INSERT INTO host_sample (recording_id, target_id, t_ms, metric, value)"
                " VALUES (?,?,?,?,?)",
                ("no-such-recording", "host-a", 0, "cpu.user", 1.0),
            )

    def test_annotation_severity_is_constrained(self, db: sqlite3.Connection) -> None:
        db.execute(RECORDING, ("rec-5", "observation"))
        db.execute(
            "INSERT INTO annotation (recording_id, code, severity, from_ms, message)"
            " VALUES (?,?,?,?,?)",
            ("rec-5", "collection_gap", "warn", 0, "lost host-a for 4s"),
        )
        with pytest.raises(sqlite3.IntegrityError):
            db.execute(
                "INSERT INTO annotation (recording_id, code, severity, from_ms, message)"
                " VALUES (?,?,?,?,?)",
                ("rec-5", "whatever", "catastrophic", 0, "made-up severity"),
            )

    def test_a_series_groups_recordings_of_the_same_setup(self, db: sqlite3.Connection) -> None:
        for n in range(3):
            db.execute(
                """
                INSERT INTO recording
                    (id, kind, status, profile, api_version, series_key, started_at)
                VALUES (?, 'observation', 'finished', 'staging', '0.0.0', ?, ?)
                """,
                (f"rec-s{n}", "staging/observation", f"2026-09-12T14:0{n}:00Z"),
            )
        db.execute(
            """
            INSERT INTO recording
                (id, kind, status, profile, api_version, series_key, started_at)
            VALUES ('rec-other', 'observation', 'finished', 'prod', '0.0.0',
                    'prod/observation', '2026-09-12T14:09:00Z')
            """
        )
        rows = db.execute(
            "SELECT id FROM recording WHERE series_key = 'staging/observation' ORDER BY started_at"
        ).fetchall()
        assert [r["id"] for r in rows] == ["rec-s0", "rec-s1", "rec-s2"]


class TestTransaction:
    def test_a_failure_rolls_the_whole_unit_back(self, db: sqlite3.Connection) -> None:
        with pytest.raises(RuntimeError), transaction(db) as conn:
            conn.execute(RECORDING, ("rec-tx", "observation"))
            raise RuntimeError("collector died mid-write")
        assert db.execute("SELECT count(*) FROM recording").fetchone()[0] == 0


class TestThreading:
    def test_a_connection_survives_moving_between_threads(self, tmp_path: Path) -> None:
        """FastAPI runs a sync dependency's setup and teardown on different threads.

        sqlite3 objects are thread-bound by default, so without check_same_thread the
        server raises on teardown -- and TestClient does not catch it, because it
        tends to reuse one thread.
        """
        import concurrent.futures

        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
            conn = pool.submit(connect, tmp_path / "metrix.db").result()
            pool.submit(migrate, conn).result()
            count = pool.submit(
                lambda: conn.execute("SELECT count(*) FROM recording").fetchone()[0]
            ).result()
            assert count == 0
            pool.submit(conn.close).result()
