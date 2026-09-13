"""Dropping the bulk, by a button rather than by a policy.

design-api 17.1: everything is kept by default and nothing rolls up. Summaries,
histograms, inventories and annotations are small — a full series loads into memory
comfortably — so there is no retention tiering and no aged rollup, and trend charts
read the real numbers at every age.

The one bulky thing is the stream: per-request event records and retained error-sample
bodies. Those live as files under `runs/<id>/` rather than in SQLite, which is what
makes this a directory delete instead of a transaction that then has to vacuum a
database.

**Manual and explicit beats a policy that quietly deletes things.** A retention rule
runs on a schedule and is therefore guaranteed to delete the evidence for the one run
somebody needed, on the day they needed it — and to do it without anyone present to
notice. So this only ever happens because a person asked for it, and the asking says
exactly what will go and exactly what will stay.

Nothing here removes a recording. A purged run keeps its numbers, its notes and its
place in every trend; what it loses is the ability to answer *which request*.
"""

from __future__ import annotations

import shutil
import sqlite3
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

from metrix_api.config import Config
from metrix_api.store import recordings as store
from metrix_api.store.db import transaction

#: What survives, in the words the confirmation uses. Written once here rather than in
#: the dialog, because a promise made by the button and a promise kept by the code
#: have to be the same promise.
KEPT = (
    "every figure and its sample count",
    "the notes, gaps and phases",
    "the inventory it ran against",
    "its place in the trend for its series",
)

#: What goes. Empty on an observation-only recording, which has no request-level
#: evidence to lose -- and saying so is more useful than a button that appears to
#: work and frees nothing.
DROPPED = (
    "per-request event records",
    "retained error-sample bodies",
)


@dataclass(frozen=True, slots=True)
class Purgeable:
    """What a purge would actually drop, measured rather than assumed."""

    recording_id: str
    files: int = 0
    bytes: int = 0
    #: Already purged, and when. A second purge is not an error; it just has
    #: nothing to do, and the page should say which of the two it is.
    purged_at: str | None = None
    #: The directory exists but holds nothing, or was never written. Both mean
    #: there is nothing to free; neither means anything went wrong.
    missing: bool = False

    @property
    def anything(self) -> bool:
        return self.files > 0


@dataclass(frozen=True, slots=True)
class Purged:
    """What one action freed, per recording."""

    recordings: list[str] = field(default_factory=list)
    files: int = 0
    bytes: int = 0
    #: Recordings that had nothing to drop. Reported rather than counted as done,
    #: so "purged 12 runs" never covers for 11 of them having been empty.
    skipped: list[str] = field(default_factory=list)


def _measure(directory: Path) -> tuple[int, int]:
    if not directory.is_dir():
        return 0, 0
    files = [p for p in directory.rglob("*") if p.is_file()]
    return len(files), sum(p.stat().st_size for p in files)


def describe(config: Config, conn: sqlite3.Connection, recording_id: str) -> Purgeable:
    """Look before deleting: what is on disk for this run, right now.

    Measured rather than estimated. A confirmation that names a number it did not
    check is a confirmation nobody should act on, and "frees about 2 GB" that turns
    out to be 4 kB teaches people to stop reading the dialog.
    """
    recording = store.get(conn, recording_id)
    directory = config.run_dir(recording_id)
    files, size = _measure(directory)
    return Purgeable(
        recording_id=recording.id,
        files=files,
        bytes=size,
        purged_at=recording.purged_at,
        missing=not directory.is_dir(),
    )


def purge(config: Config, conn: sqlite3.Connection, recording_id: str) -> Purged:
    """Drop one recording's bulk stream. Everything else stays.

    The directory goes and is recreated empty: a run directory that exists is how the
    rest of the code knows the recording is real, and leaving a hole would turn a
    purge into a different kind of missing.

    The stamp is written whether or not anything was on disk, because *purged and
    empty* and *never written* are different facts about a recording and only one of
    them is a decision somebody made.
    """
    found = describe(config, conn, recording_id)
    directory = config.run_dir(recording_id)

    if directory.is_dir():
        shutil.rmtree(directory)
    directory.mkdir(parents=True, exist_ok=True)

    with transaction(conn):
        conn.execute(
            "UPDATE recording SET purged_at = ? WHERE id = ?",
            (datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%SZ"), recording_id),
        )

    if not found.anything:
        return Purged(skipped=[recording_id])
    return Purged(recordings=[recording_id], files=found.files, bytes=found.bytes)


def purge_series(config: Config, conn: sqlite3.Connection, series_key: str) -> Purged:
    """The same, for every run of one setup.

    A series is the unit a decision like this is actually made about — *we are done
    with this setup's request-level data* — and doing it one run at a time is how a
    person gives up halfway and leaves an archive in two states.
    """
    runs = store.list_recordings(conn, series=series_key, limit=10_000)
    total = Purged()
    for run in runs:
        one = purge(config, conn, run.id)
        total = Purged(
            recordings=[*total.recordings, *one.recordings],
            files=total.files + one.files,
            bytes=total.bytes + one.bytes,
            skipped=[*total.skipped, *one.skipped],
        )
    return total


def delete_recording(config: Config, conn: sqlite3.Connection, recording_id: str) -> None:
    """Remove one recording's files and database row permanently."""
    store.get(conn, recording_id)
    directory = config.run_dir(recording_id)
    if directory.is_dir():
        shutil.rmtree(directory)
    with transaction(conn):
        conn.execute("DELETE FROM recording WHERE id = ?", (recording_id,))
