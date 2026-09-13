"""Connection handling and migrations.

Migrations are numbered SQL files applied in order inside one transaction each, with
the applied set recorded in ``schema_migration``. Forward-only: there is no down
migration, because rolling a schema backwards over recorded data loses data, and the
recordings are the product.
"""

from __future__ import annotations

import re
import sqlite3
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path

MIGRATIONS_DIR = Path(__file__).parent / "migrations"
_MIGRATION_NAME = re.compile(r"^(\d{3})_([a-z0-9_]+)\.sql$")


class StoreError(Exception):
    """The database cannot be opened or brought up to date."""


@dataclass(frozen=True, slots=True)
class Migration:
    version: int
    name: str
    sql: str


def discover_migrations(directory: Path = MIGRATIONS_DIR) -> list[Migration]:
    """Read ``NNN_name.sql`` files in version order, rejecting gaps and duplicates."""
    found: dict[int, Migration] = {}
    for path in sorted(directory.glob("*.sql")):
        match = _MIGRATION_NAME.match(path.name)
        if not match:
            raise StoreError(f"{path.name}: migrations must be named NNN_lower_snake.sql")
        version = int(match.group(1))
        if version in found:
            raise StoreError(f"two migrations numbered {version:03d}")
        found[version] = Migration(version, match.group(2), path.read_text(encoding="utf-8"))

    expected = list(range(1, len(found) + 1))
    if sorted(found) != expected:
        raise StoreError(
            f"migration numbers must run 001..{len(found):03d} with no gaps, found "
            f"{sorted(found)}"
        )
    return [found[v] for v in expected]


def connect(path: Path | str) -> sqlite3.Connection:
    """Open a connection with the settings this application depends on.

    Foreign keys are off by default in SQLite and must be enabled per connection --
    the cascade deletes in the schema are silently inert without this.
    """
    target = Path(path)
    if target.parent and str(target) != ":memory:":
        target.parent.mkdir(parents=True, exist_ok=True)

    # check_same_thread=False because a request's connection is created, used, and
    # closed on whichever threadpool threads the server happens to pick -- FastAPI
    # runs a sync dependency's setup and its teardown on different threads. Safe
    # here only because a connection is never shared between concurrent users: one
    # per request, one per recorder.
    conn = sqlite3.connect(target, isolation_level=None, check_same_thread=False)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA foreign_keys = ON")
    conn.execute("PRAGMA busy_timeout = 5000")
    if str(target) != ":memory:":
        # Concurrent readers while a recording writes: the live view reads the same
        # database the collector is filling.
        conn.execute("PRAGMA journal_mode = WAL")
        conn.execute("PRAGMA synchronous = NORMAL")
    return conn


def applied_versions(conn: sqlite3.Connection) -> list[int]:
    row = conn.execute(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'schema_migration'"
    ).fetchone()
    if row is None:
        return []
    return [r[0] for r in conn.execute("SELECT version FROM schema_migration ORDER BY version")]


def migrate(conn: sqlite3.Connection, directory: Path = MIGRATIONS_DIR) -> list[Migration]:
    """Apply pending migrations. Returns the ones applied; idempotent when up to date."""
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS schema_migration (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        ) STRICT
        """
    )

    done = set(applied_versions(conn))
    applied: list[Migration] = []

    for migration in discover_migrations(directory):
        if migration.version in done:
            continue
        # executescript() commits any open transaction before it runs, so an outer
        # BEGIN would be discarded. The transaction goes inside the script instead,
        # which also makes the schema change and its version row atomic. Inlining the
        # values is safe: version is an int and name is matched against [a-z0-9_]+.
        version_row = (
            "INSERT INTO schema_migration (version, name) VALUES "
            f"({migration.version}, '{migration.name}');"
        )
        script = f"BEGIN;\n{migration.sql}\n{version_row}\nCOMMIT;"
        try:
            conn.executescript(script)
        except sqlite3.Error as exc:
            if conn.in_transaction:
                conn.execute("ROLLBACK")
            raise StoreError(
                f"migration {migration.version:03d}_{migration.name} failed: {exc}"
            ) from exc
        applied.append(migration)

    return applied


@contextmanager
def open_store(path: Path | str) -> Iterator[sqlite3.Connection]:
    """Open a migrated database and close it afterwards."""
    conn = connect(path)
    try:
        migrate(conn)
        yield conn
    finally:
        conn.close()


@contextmanager
def transaction(conn: sqlite3.Connection) -> Iterator[sqlite3.Connection]:
    """Run a unit of work, rolling back on any exception."""
    conn.execute("BEGIN")
    try:
        yield conn
    except BaseException:
        conn.execute("ROLLBACK")
        raise
    conn.execute("COMMIT")
