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

from metrix_api.observer.facts import HostFacts
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
    #: Annotation counts by severity. The archive needs to say which recordings have
    #: something wrong with them without opening each one, and `invalid` is the
    #: difference between a recording worth comparing and one that is not.
    annotations: dict[str, int] = field(default_factory=dict)

    @property
    def worst(self) -> str | None:
        for severity in ("invalid", "warn", "info"):
            if self.annotations.get(severity):
                return severity
        return None

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


def sample_counts(conn: sqlite3.Connection, recording_id: str) -> dict[str, int]:
    """Rows per target. Zero for a target that never answered is the point of it."""
    return {
        row["target_id"]: row["n"]
        for row in conn.execute(
            "SELECT target_id, COUNT(*) AS n FROM host_sample WHERE recording_id = ?"
            " GROUP BY target_id",
            (recording_id,),
        )
    }


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


class BaselineRefused(Exception):
    """A recording that must not silently become the thing everything is judged by."""


def mark_baseline(
    conn: sqlite3.Connection, recording_id: str, *, override: bool = False
) -> RecordingRow:
    """Make this the baseline for its series, and demote whatever held it.

    Refused for a recording carrying an `invalid` annotation unless someone overrides
    it deliberately (design-api 17.1). This is the point of the whole severity
    mechanism: a baseline is what every later recording is measured against, so
    adopting one taken while a target was unreachable would silently poison the
    comparison rather than fail it.

    One baseline per series, because a series is the set of recordings that are
    comparable at all -- two would mean "normal" depended on which one you opened.
    """
    row = get(conn, recording_id)
    if not override and (blocking := invalid_annotations(conn, recording_id)):
        raise BaselineRefused(
            f"{recording_id} carries "
            + ", ".join(sorted({a['code'] for a in blocking}))
            + " and cannot be a baseline; fix the recording, take another, or override "
            "deliberately"
        )

    with transaction(conn):
        conn.execute(
            "UPDATE recording SET is_baseline = 0 WHERE series_key = ? AND id != ?",
            (row.series_key, recording_id),
        )
        conn.execute("UPDATE recording SET is_baseline = 1 WHERE id = ?", (recording_id,))
    return get(conn, recording_id)


def clear_baseline(conn: sqlite3.Connection, recording_id: str) -> RecordingRow:
    with transaction(conn):
        conn.execute("UPDATE recording SET is_baseline = 0 WHERE id = ?", (recording_id,))
    return get(conn, recording_id)


def baseline_for(conn: sqlite3.Connection, series_key: str) -> RecordingRow | None:
    """The recording everything in this series is compared against, if one is set."""
    row = conn.execute(
        "SELECT id FROM recording WHERE series_key = ? AND is_baseline = 1 LIMIT 1",
        (series_key,),
    ).fetchone()
    return get(conn, row["id"]) if row else None


def invalid_annotations(conn: sqlite3.Connection, recording_id: str) -> list[sqlite3.Row]:
    """Everything on this recording that says its numbers cannot be trusted."""
    return conn.execute(
        "SELECT * FROM annotation WHERE recording_id = ? AND severity = 'invalid'"
        " ORDER BY from_ms",
        (recording_id,),
    ).fetchall()


def abandon_running(conn: sqlite3.Connection) -> list[str]:
    """Close recordings left `running` by a process that did not exit cleanly.

    A live recording is a task in memory, so nothing can still be running when the
    application starts. Without this a killed process leaves rows that are
    indistinguishable from one still going -- and they would never close.
    """
    stale = [r["id"] for r in conn.execute("SELECT id FROM recording WHERE status = ?", (RUNNING,))]
    if not stale:
        return []
    with transaction(conn):
        conn.executemany(
            "UPDATE recording SET status = ?, finished_at = COALESCE(finished_at,"
            " strftime('%Y-%m-%dT%H:%M:%SZ', 'now')) WHERE id = ?",
            [(ABORTED, recording_id) for recording_id in stale],
        )
        for recording_id in stale:
            _insert_annotation(
                conn,
                recording_id,
                Annotation(
                    code="recording_abandoned",
                    severity="warn",
                    from_ms=0,
                    message=(
                        "The application restarted while this recording was running, so it "
                        "was closed. Whatever had been collected up to that point is kept."
                    ),
                ),
            )
    return stale


def save_facts(
    conn: sqlite3.Connection, recording_id: str, target_id: str, facts: HostFacts, *, at: str
) -> None:
    """Store one probe.

    Identity is written once -- the first probe that reports any wins, and the second
    does not overwrite it with a thinner answer if the box got busy. Filesystem rows
    are per `at`, so start and finish sit side by side.
    """
    with transaction(conn):
        if facts.identity:
            conn.execute(
                "INSERT OR IGNORE INTO host_identity (recording_id, target_id, facts) "
                "VALUES (?, ?, ?)",
                (recording_id, target_id, json.dumps(facts.identity, sort_keys=True)),
            )
        for filesystem in facts.filesystems:
            conn.execute(
                """
                INSERT OR REPLACE INTO filesystem_usage
                    (recording_id, target_id, at, mount, total_bytes, used_bytes)
                VALUES (?, ?, ?, ?, ?, ?)
                """,
                (
                    recording_id,
                    target_id,
                    at,
                    filesystem.mount,
                    filesystem.total_bytes,
                    filesystem.used_bytes,
                ),
            )


def identities(conn: sqlite3.Connection, recording_id: str) -> dict[str, dict[str, str]]:
    rows = conn.execute(
        "SELECT target_id, facts FROM host_identity WHERE recording_id = ? ORDER BY target_id",
        (recording_id,),
    ).fetchall()
    return {row["target_id"]: json.loads(row["facts"]) for row in rows}


def filesystem_usage(conn: sqlite3.Connection, recording_id: str) -> list[dict[str, object]]:
    """Start and finish side by side, one row per mount.

    The delta is computed here rather than in the UI so "how much did this run
    consume" has one definition. A mount with no finish reading has no delta rather
    than a delta of zero: the probe failing is not the same as nothing being written.
    """
    rows = conn.execute(
        """
        SELECT target_id, mount, at, total_bytes, used_bytes
        FROM filesystem_usage WHERE recording_id = ?
        ORDER BY target_id, mount, at
        """,
        (recording_id,),
    ).fetchall()

    merged: dict[tuple[str, str], dict[str, object]] = {}
    for row in rows:
        key = (row["target_id"], row["mount"])
        entry = merged.setdefault(
            key,
            {
                "target_id": row["target_id"],
                "mount": row["mount"],
                "total_bytes": row["total_bytes"],
                "start_used_bytes": None,
                "finish_used_bytes": None,
                "used_delta_bytes": None,
            },
        )
        entry["total_bytes"] = row["total_bytes"]
        entry[f"{row['at']}_used_bytes"] = row["used_bytes"]

    for entry in merged.values():
        start, finish = entry["start_used_bytes"], entry["finish_used_bytes"]
        if start is not None and finish is not None:
            entry["used_delta_bytes"] = finish - start
    return list(merged.values())


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
    status: str | None = None,
    baseline: bool | None = None,
    severity: str | None = None,
    query: str | None = None,
    limit: int = 100,
) -> list[RecordingRow]:
    """Newest first, which is the order anyone opening the archive wants.

    Filtering happens here rather than in the browser. The list is capped, so a page
    that filtered the rows it happened to be given would answer "no matches" for a
    recording that exists two pages down -- which is worse than not offering filters.
    """
    clauses, params = [], []
    for column, value in (
        ("kind", kind),
        ("profile", profile),
        ("series_key", series),
        ("status", status),
    ):
        if value is not None:
            clauses.append(f"{column} = ?")
            params.append(value)
    if baseline is not None:
        clauses.append("is_baseline = ?")
        params.append(1 if baseline else 0)
    if severity is not None:
        clauses.append(
            "EXISTS (SELECT 1 FROM annotation a"
            " WHERE a.recording_id = recording.id AND a.severity = ?)"
        )
        params.append(severity)
    if query:
        # Id, profile and the note a person typed: the three things anyone would
        # search an archive by, and all of them are short.
        clauses.append("(id LIKE ? OR IFNULL(profile, '') LIKE ? OR IFNULL(note, '') LIKE ?)")
        params.extend([f"%{query}%"] * 3)

    where = f" WHERE {' AND '.join(clauses)}" if clauses else ""
    params.append(limit)
    rows = conn.execute(
        f"SELECT * FROM recording{where} ORDER BY started_at DESC, id DESC LIMIT ?", params
    ).fetchall()
    found = [RecordingRow.from_row(r) for r in rows]
    if not found:
        return found

    # Targets and annotation counts in one query each rather than one per recording:
    # the archive shows both, and a list view should not fan out.
    placeholders = ",".join("?" * len(found))
    ids = [r.id for r in found]

    by_recording: dict[str, list[str]] = {}
    for row in conn.execute(
        f"SELECT recording_id, target_id FROM recording_target"
        f" WHERE recording_id IN ({placeholders}) ORDER BY position",
        ids,
    ):
        by_recording.setdefault(row["recording_id"], []).append(row["target_id"])

    notes: dict[str, dict[str, int]] = {}
    for row in conn.execute(
        f"SELECT recording_id, severity, COUNT(*) AS n FROM annotation"
        f" WHERE recording_id IN ({placeholders}) GROUP BY recording_id, severity",
        ids,
    ):
        notes.setdefault(row["recording_id"], {})[row["severity"]] = row["n"]

    for recording in found:
        recording.targets = by_recording.get(recording.id, [])
        recording.annotations = notes.get(recording.id, {})
    return found


@dataclass(slots=True)
class SeriesRow:
    """One setup, and the runs made against it.

    The identity is the readable key itself (design-api 17.2) plus the parts it was
    built from, so a list can show *what changed* rather than making anyone diff two
    pipe-separated strings by eye.
    """

    key: str
    kind: str
    profile: str | None
    addressing_mode: str
    api_version: str
    runs: int
    first_at: str
    last_at: str
    baseline_id: str | None = None
    #: How many of those runs carry an `invalid` note. Shown because they are the
    #: runs the noise band is measured without, and a series that is half invalid is
    #: a series whose band rests on far less than its run count suggests.
    invalid_runs: int = 0


def series_list(conn: sqlite3.Connection) -> list[SeriesRow]:
    """Every series, most recently run first.

    Grouped by the identity tuple rather than by anything anyone tags: manual
    grouping gets skipped exactly when it matters, and comparing across a changed
    setup is the most expensive mistake this view can make (design-api 17.2).
    """
    rows = conn.execute(
        """
        SELECT series_key, kind, profile, addressing_mode, api_version,
               COUNT(*) AS runs,
               MIN(started_at) AS first_at,
               MAX(started_at) AS last_at,
               MAX(CASE WHEN is_baseline = 1 THEN id END) AS baseline_id,
               SUM(CASE WHEN EXISTS (
                     SELECT 1 FROM annotation a
                     WHERE a.recording_id = recording.id AND a.severity = 'invalid'
                   ) THEN 1 ELSE 0 END) AS invalid_runs
        FROM recording
        GROUP BY series_key
        ORDER BY last_at DESC, series_key
        """
    ).fetchall()
    return [
        SeriesRow(
            key=row["series_key"],
            kind=row["kind"],
            profile=row["profile"],
            addressing_mode=row["addressing_mode"],
            api_version=row["api_version"],
            runs=row["runs"],
            first_at=row["first_at"],
            last_at=row["last_at"],
            baseline_id=row["baseline_id"],
            invalid_runs=row["invalid_runs"],
        )
        for row in rows
    ]


def pooled_values(
    conn: sqlite3.Connection,
    recording_ids: list[str],
    *,
    phase: str | None = None,
) -> dict[str, dict[str, list[float]]]:
    """Every box's readings, per recording and per metric, in one query.

    One query rather than one per recording because a trend reads a whole series at
    once, and a request that fans out per run gets slower exactly as the history
    becomes worth looking at.

    Pooled across targets: a discovered environment replaces its tasks on every
    deployment, so a per-box trend would start a new line each time and answer
    nothing (design-api 10.2). Naming a `phase` restricts the window to that phase
    on each target, which is how baseline-phase drift is read.
    """
    if not recording_ids:
        return {}

    placeholders = ",".join("?" * len(recording_ids))
    if phase is None:
        sql = (
            f"SELECT recording_id, metric, value FROM host_sample"
            f" WHERE recording_id IN ({placeholders})"
        )
        params: list[object] = [*recording_ids]
    else:
        sql = (
            f"SELECT s.recording_id AS recording_id, s.metric AS metric, s.value AS value"
            f" FROM host_sample s"
            f" JOIN phase p ON p.recording_id = s.recording_id"
            f"   AND p.target_id = s.target_id AND p.phase = ?"
            f"   AND s.t_ms >= p.from_ms AND (p.to_ms IS NULL OR s.t_ms <= p.to_ms)"
            f" WHERE s.recording_id IN ({placeholders})"
        )
        params = [phase, *recording_ids]

    gathered: dict[str, dict[str, list[float]]] = {}
    for row in conn.execute(sql, params):
        gathered.setdefault(row["recording_id"], {}).setdefault(row["metric"], []).append(
            row["value"]
        )
    return gathered


def facets(conn: sqlite3.Connection) -> dict[str, list[str]]:
    """What there is to filter by, across the whole archive.

    Taken from every recording rather than from the filtered page, so the choices do
    not disappear as they are used -- a filter list that empties itself as you narrow
    is a filter list you cannot get back out of.
    """
    return {
        name: [
            row[0]
            for row in conn.execute(
                f"SELECT DISTINCT {name} FROM recording WHERE {name} IS NOT NULL"
                f" ORDER BY {name}"
            )
        ]
        for name in ("kind", "profile", "status")
    }


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


def window(
    conn: sqlite3.Connection,
    recording_id: str,
    *,
    target_id: str,
    from_ms: int = 0,
    to_ms: int | None = None,
) -> dict[str, list[float]]:
    """Every metric's values for one target over one slice of the timeline.

    A phase is a slice, so this is what a per-phase summary reads. Returned per
    metric rather than as rows because that is the shape the summariser wants, and
    regrouping it in three callers is how the three drift apart.
    """
    clauses = ["recording_id = ?", "target_id = ?", "t_ms >= ?"]
    params: list[object] = [recording_id, target_id, from_ms]
    if to_ms is not None:
        clauses.append("t_ms <= ?")
        params.append(to_ms)

    values: dict[str, list[float]] = {}
    for row in conn.execute(
        f"SELECT metric, value FROM host_sample WHERE {' AND '.join(clauses)} ORDER BY t_ms",
        params,
    ):
        values.setdefault(row["metric"], []).append(row["value"])
    return values


def spans(conn: sqlite3.Connection, recording_id: str) -> dict[str, dict[str, int]]:
    """First and last sample time per target, and how many there were.

    The diagnostic design-api 14.2 asks Start and Finish for: a target whose last
    sample is thirty seconds before the recording ended stopped answering, and a
    column of medians will not say so.
    """
    return {
        row["target_id"]: {
            "first_ms": row["first_ms"],
            "last_ms": row["last_ms"],
            "n": row["n"],
        }
        for row in conn.execute(
            # DISTINCT t_ms, not COUNT(*): a sample is one moment, and the table
            # stores a row per metric within it. Counting rows would say a box with
            # four metrics was sampled four times as often as it was.
            "SELECT target_id, MIN(t_ms) AS first_ms, MAX(t_ms) AS last_ms,"
            " COUNT(DISTINCT t_ms) AS n"
            " FROM host_sample WHERE recording_id = ? GROUP BY target_id",
            (recording_id,),
        )
    }


def window_series(
    conn: sqlite3.Connection,
    recording_id: str,
    *,
    target_id: str,
    from_ms: int = 0,
    to_ms: int | None = None,
) -> dict[str, list[tuple[int, float]]]:
    """The same slice, keeping the clock: what a recovery curve is measured from."""
    clauses = ["recording_id = ?", "target_id = ?", "t_ms >= ?"]
    params: list[object] = [recording_id, target_id, from_ms]
    if to_ms is not None:
        clauses.append("t_ms <= ?")
        params.append(to_ms)

    points: dict[str, list[tuple[int, float]]] = {}
    for row in conn.execute(
        f"SELECT metric, t_ms, value FROM host_sample WHERE {' AND '.join(clauses)}"
        " ORDER BY t_ms",
        params,
    ):
        points.setdefault(row["metric"], []).append((row["t_ms"], row["value"]))
    return points


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
