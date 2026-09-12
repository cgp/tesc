"""Storing what discovery found, and pinning it to the recordings that used it.

Two questions this answers that a fresh walk cannot. *What did this run actually
measure?* -- answered months later, from a row, when the tasks are long gone. And
*when did this environment last change?* -- answered because an unchanged
re-resolution extends the existing row rather than inserting a copy of it, so the
table is a history of changes rather than of checks.
"""

from __future__ import annotations

import json
import sqlite3
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta

from metrix_api.discovery.inventory import Inventory, from_document, host_key
from metrix_api.store.db import transaction


@dataclass(frozen=True, slots=True)
class Stored:
    """A snapshot as it sits in the database."""

    id: int
    inventory: Inventory
    #: When this state was first seen, and when it was last confirmed unchanged. They
    #: differ whenever a re-resolution found nothing new, which is the common case.
    discovered_at: str
    confirmed_at: str
    profile: str | None = None

    def age(self, *, now: datetime | None = None) -> timedelta:
        """Time since the snapshot was last confirmed, not since it was first seen."""
        confirmed = datetime.fromisoformat(self.confirmed_at.replace("Z", "+00:00"))
        return (now or datetime.now(UTC)) - confirmed

    def fresh(self, ttl: timedelta, *, now: datetime | None = None) -> bool:
        return self.age(now=now) < ttl


def _comparable(inventory: Inventory) -> str:
    """The snapshot with its timestamp removed -- everything that could have changed.

    Two walks of an untouched account differ only in when they happened, and a row
    per walk would make "when did this change" unanswerable without diffing every
    document in the table.
    """
    document = inventory.to_document()
    document.pop("discovered_at", None)
    return json.dumps(document, sort_keys=True)


def save(
    conn: sqlite3.Connection,
    inventory: Inventory,
    *,
    profile: str | None = None,
    now: datetime | None = None,
) -> Stored:
    """Record a resolution, extending the newest matching row rather than repeating it."""
    stamp = (now or datetime.now(UTC)).strftime("%Y-%m-%dT%H:%M:%SZ")
    current = latest(conn, profile=profile)

    with transaction(conn):
        if current is not None and _comparable(current.inventory) == _comparable(inventory):
            conn.execute(
                "UPDATE inventory SET confirmed_at = ? WHERE id = ?", (stamp, current.id)
            )
            return Stored(
                id=current.id,
                inventory=current.inventory,
                discovered_at=current.discovered_at,
                confirmed_at=stamp,
                profile=profile,
            )

        cursor = conn.execute(
            """
            INSERT INTO inventory
                (profile, source, reached, host_key, document, discovered_at, confirmed_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            """,
            (
                profile,
                inventory.source,
                inventory.reached,
                host_key(inventory),
                json.dumps(inventory.to_document(), sort_keys=True),
                inventory.discovered_at or stamp,
                stamp,
            ),
        )

    return Stored(
        id=int(cursor.lastrowid or 0),
        inventory=inventory,
        discovered_at=inventory.discovered_at or stamp,
        confirmed_at=stamp,
        profile=profile,
    )


def _stored(row: sqlite3.Row) -> Stored:
    return Stored(
        id=row["id"],
        inventory=from_document(json.loads(row["document"]), source=f"inventory {row['id']}"),
        discovered_at=row["discovered_at"],
        confirmed_at=row["confirmed_at"],
        profile=row["profile"],
    )


def latest(conn: sqlite3.Connection, *, profile: str | None) -> Stored | None:
    """The newest snapshot for a profile. A one-off resolution has no profile and
    is never handed back as a cache hit -- it was not resolved for anything."""
    if profile is None:
        return None
    row = conn.execute(
        "SELECT * FROM inventory WHERE profile = ? ORDER BY confirmed_at DESC, id DESC LIMIT 1",
        (profile,),
    ).fetchone()
    return _stored(row) if row else None


def get(conn: sqlite3.Connection, inventory_id: int) -> Stored:
    row = conn.execute("SELECT * FROM inventory WHERE id = ?", (inventory_id,)).fetchone()
    if row is None:
        raise LookupError(f"no inventory {inventory_id}")
    return _stored(row)


def history(conn: sqlite3.Connection, profile: str, *, limit: int = 20) -> list[Stored]:
    """Every distinct state this profile has been found in, newest first."""
    rows = conn.execute(
        "SELECT * FROM inventory WHERE profile = ? ORDER BY confirmed_at DESC, id DESC LIMIT ?",
        (profile, limit),
    ).fetchall()
    return [_stored(row) for row in rows]


def pin(conn: sqlite3.Connection, recording_id: str, inventory_id: int) -> None:
    """Fix a recording to the snapshot it ran against, for as long as both exist."""
    with transaction(conn):
        conn.execute(
            "UPDATE recording SET inventory_id = ? WHERE id = ?", (inventory_id, recording_id)
        )


def for_recording(conn: sqlite3.Connection, recording_id: str) -> Stored | None:
    row = conn.execute(
        "SELECT inventory.* FROM inventory "
        "JOIN recording ON recording.inventory_id = inventory.id WHERE recording.id = ?",
        (recording_id,),
    ).fetchone()
    return _stored(row) if row else None
