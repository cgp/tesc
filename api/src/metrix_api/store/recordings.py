"""Reading and writing recordings.

Every write goes through here rather than through SQL scattered across the codebase,
so the invariants the schema cannot express -- a recording's status transitions, a gap
always having a matching annotation -- have one place to live.
"""

from __future__ import annotations

import json
import secrets
import sqlite3
from collections.abc import Iterable
from dataclasses import dataclass, field
from datetime import UTC, datetime

from metrix_api.observer.metrics import Annotation, Gap, Sample
from metrix_api.profiles import Endpoint, Profile
from metrix_api.store.db import transaction

#: An observation-only recording has no traffic, so baseline and settle collapse into
#: one continuous window. It is recorded as `measure` because that is the phase the
#: numbers are read from; the distinction only earns its keep once load exists.
OBSERVATION_PHASE = "measure"

RUNNING = "running"
FINISHED = "finished"
ABORTED = "aborted"
FAILED = "failed"


def new_id(now: datetime | None = None) -> str:
    """Sortable, unique, and readable in a directory listing."""
    stamp = (now or datetime.now(UTC)).strftime("%Y-%m-%dT%H-%M-%SZ")
    return f"{stamp}_{secrets.token_hex(2)}"


def series_key(profile: Profile, *, kind: str, api_version: str, interval_s: float) -> str:
    """The setup identity. Recordings sharing it form a series.

    Readable rather than hashed: when a trend unexpectedly starts a new line, the
    reason should be visible without decoding anything. Collection interval is part
    of it because a metric sampled every 5s is not comparable with one sampled every
    second, and the API version because a collector change can move a number.
    """
    return f"{kind}|{profile.name}|{profile.addressing}|{interval_s:g}s|api={api_version}"


@dataclass(slots=True)
class RecordingRow:
    id: str
    kind: str
    status: str
    profile: str | None
    addressing_mode: str
    api_version: str
    series_key: str
    started_at: str
    finished_at: str | None = None
    duration_ms: int | None = None
    is_baseline: bool = False
    note: str | None = None
    targets: list[str] = field(default_factory=list)

    @classmethod
    def from_row(cls, row: sqlite3.Row) -> RecordingRow:
        return cls(
            id=row["id"],
            kind=row["kind"],
            status=row["status"],
            profile=row["profile"],
            addressing_mode=row["addressing_mode"],
            api_version=row["api_version"],
            series_key=row["series_key"],
            started_at=row["started_at"],
            finished_at=row["finished_at"],
            duration_ms=row["duration_ms"],
            is_baseline=bool(row["is_baseline"]),
            note=row["note"],
        )


def create(
    conn: sqlite3.Connection,
    *,
    recording_id: str,
    profile: Profile,
    endpoints: Iterable[Endpoint],
    kind: str,
    api_version: str,
    interval_s: float,
    note: str | None = None,
    started_at: datetime | None = None,
) -> RecordingRow:
    """Open a recording and pin what it is running against."""
    stamp = (started_at or datetime.now(UTC)).strftime("%Y-%m-%dT%H:%M:%SZ")
    key = series_key(profile, kind=kind, api_version=api_version, interval_s=interval_s)

    with transaction(conn):
        conn.execute(
            """
            INSERT INTO recording
                (id, kind, status, profile, addressing_mode, api_version, series_key,
                 started_at, note)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (recording_id, kind, RUNNING, profile.name, profile.addressing, api_version,
             key, stamp, note),
        )
        for position, endpoint in enumerate(endpoints, start=1):
            conn.execute(
                """
                INSERT INTO recording_target
                    (recording_id, target_id, position, address, host_header, attributes)
                VALUES (?, ?, ?, ?, ?, ?)
                """,
                (recording_id, endpoint.id, position, endpoint.address, endpoint.host_header,
                 json.dumps(endpoint.attributes, sort_keys=True)),
            )

    return get(conn, recording_id)


def start_phase(
    conn: sqlite3.Connection, recording_id: str, target_id: str, phase: str, from_ms: int
) -> None:
    conn.execute(
        "INSERT OR IGNORE INTO phase (recording_id, target_id, phase, from_ms) VALUES (?,?,?,?)",
        (recording_id, target_id, phase, from_ms),
    )


def end_phase(
    conn: sqlite3.Connection, recording_id: str, target_id: str, phase: str, to_ms: int
) -> None:
    conn.execute(
        "UPDATE phase SET to_ms = ? WHERE recording_id = ? AND target_id = ? AND phase = ?",
        (to_ms, recording_id, target_id, phase),
    )


def add_sample(conn: sqlite3.Connection, recording_id: str, sample: Sample) -> int:
    """Store one sample as one row per metric. Returns the number of rows written."""
    rows = [
        (recording_id, sample.target_id, sample.t_ms, metric, value)
        for metric, value in sample.metrics.items()
    ]
    if not rows:
        return 0
    conn.executemany(
        "INSERT INTO host_sample (recording_id, target_id, t_ms, metric, value)"
        " VALUES (?,?,?,?,?)",
        rows,
    )
    return len(rows)


def add_gap(conn: sqlite3.Connection, recording_id: str, gap: Gap) -> None:
    """Record a gap *and* its annotation together.

    Two tables because they answer different questions -- "was this interval
    collected?" and "what should the person reading this chart know?" -- but a gap
    without its annotation would be invisible in the UI, so they are written as one
    unit rather than by two callers who might disagree.
    """
    with transaction(conn):
        conn.execute(
            "INSERT OR REPLACE INTO collection_gap"
            " (recording_id, target_id, from_ms, to_ms, reason) VALUES (?,?,?,?,?)",
            (recording_id, gap.target_id, gap.from_ms, gap.to_ms, gap.reason),
        )
        _insert_annotation(conn, recording_id, Annotation.from_gap(gap))


def add_annotation(conn: sqlite3.Connection, recording_id: str, annotation: Annotation) -> None:
    with transaction(conn):
        _insert_annotation(conn, recording_id, annotation)


def _insert_annotation(
    conn: sqlite3.Connection, recording_id: str, annotation: Annotation
) -> None:
    conn.execute(
        """
        INSERT INTO annotation
            (recording_id, target_id, code, severity, phase, from_ms, to_ms, message,
             detail, source)
        VALUES (?,?,?,?,?,?,?,?,?,?)
        """,
        (
            recording_id,
            annotation.target_id,
            annotation.code,
            annotation.severity,
            annotation.phase,
            annotation.from_ms,
            annotation.to_ms,
            annotation.message,
            json.dumps(annotation.detail, sort_keys=True) if annotation.detail else None,
            annotation.source,
        ),
    )


def finish(
    conn: sqlite3.Connection,
    recording_id: str,
    *,
    status: str = FINISHED,
    duration_ms: int | None = None,
    finished_at: datetime | None = None,
) -> RecordingRow:
    stamp = (finished_at or datetime.now(UTC)).strftime("%Y-%m-%dT%H:%M:%SZ")
    with transaction(conn):
        conn.execute(
            "UPDATE recording SET status = ?, finished_at = ?, duration_ms = ? WHERE id = ?",
            (status, stamp, duration_ms, recording_id),
        )
    return get(conn, recording_id)


def get(conn: sqlite3.Connection, recording_id: str) -> RecordingRow:
    row = conn.execute("SELECT * FROM recording WHERE id = ?", (recording_id,)).fetchone()
    if row is None:
        raise LookupError(f"no recording {recording_id!r}")
    recording = RecordingRow.from_row(row)
    recording.targets = [
        r["target_id"]
        for r in conn.execute(
            "SELECT target_id FROM recording_target WHERE recording_id = ? ORDER BY position",
            (recording_id,),
        )
    ]
    return recording


def list_recordings(
    conn: sqlite3.Connection,
    *,
    kind: str | None = None,
    profile: str | None = None,
    series: str | None = None,
    limit: int = 100,
) -> list[RecordingRow]:
    """Newest first, which is the order anyone opening the archive wants."""
    clauses, params = [], []
    for column, value in (("kind", kind), ("profile", profile), ("series_key", series)):
        if value is not None:
            clauses.append(f"{column} = ?")
            params.append(value)
    where = f" WHERE {' AND '.join(clauses)}" if clauses else ""
    params.append(limit)
    rows = conn.execute(
        f"SELECT * FROM recording{where} ORDER BY started_at DESC, id DESC LIMIT ?", params
    ).fetchall()
    found = [RecordingRow.from_row(r) for r in rows]
    if not found:
        return found

    # Targets in one query rather than one per recording: the archive lists a
    # target count, and a list view should not fan out.
    placeholders = ",".join("?" * len(found))
    by_recording: dict[str, list[str]] = {}
    for row in conn.execute(
        f"SELECT recording_id, target_id FROM recording_target"
        f" WHERE recording_id IN ({placeholders}) ORDER BY position",
        [r.id for r in found],
    ):
        by_recording.setdefault(row["recording_id"], []).append(row["target_id"])
    for recording in found:
        recording.targets = by_recording.get(recording.id, [])
    return found


def samples(
    conn: sqlite3.Connection,
    recording_id: str,
    *,
    target_id: str | None = None,
    metric: str | None = None,
) -> list[tuple[str, int, str, float]]:
    clauses = ["recording_id = ?"]
    params: list[object] = [recording_id]
    if target_id is not None:
        clauses.append("target_id = ?")
        params.append(target_id)
    if metric is not None:
        clauses.append("metric = ?")
        params.append(metric)
    rows = conn.execute(
        f"SELECT target_id, t_ms, metric, value FROM host_sample"
        f" WHERE {' AND '.join(clauses)} ORDER BY t_ms, metric",
        params,
    ).fetchall()
    return [(r["target_id"], r["t_ms"], r["metric"], r["value"]) for r in rows]


def series(conn: sqlite3.Connection, recording_id: str, target_id: str, metric: str):
    """One metric over time: the shape a chart wants."""
    return [
        (t_ms, value)
        for _, t_ms, _, value in samples(conn, recording_id, target_id=target_id, metric=metric)
    ]


def annotations(conn: sqlite3.Connection, recording_id: str) -> list[sqlite3.Row]:
    return conn.execute(
        "SELECT * FROM annotation WHERE recording_id = ? ORDER BY from_ms, id", (recording_id,)
    ).fetchall()


def gaps(conn: sqlite3.Connection, recording_id: str) -> list[sqlite3.Row]:
    return conn.execute(
        "SELECT * FROM collection_gap WHERE recording_id = ? ORDER BY from_ms", (recording_id,)
    ).fetchall()


def phases(conn: sqlite3.Connection, recording_id: str) -> list[sqlite3.Row]:
    return conn.execute(
        "SELECT * FROM phase WHERE recording_id = ? ORDER BY from_ms", (recording_id,)
    ).fetchall()
