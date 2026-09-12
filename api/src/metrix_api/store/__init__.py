"""SQLite schema, migrations, queries.

SQLite holds what gets queried: recordings, series membership, host samples,
annotations. Bulk streams stay as files under ``runs/<id>/`` so purging them is a
directory delete.
"""

from metrix_api.store.db import (
    Migration,
    StoreError,
    applied_versions,
    connect,
    discover_migrations,
    migrate,
    open_store,
    transaction,
)

__all__ = [
    "Migration",
    "StoreError",
    "applied_versions",
    "connect",
    "discover_migrations",
    "migrate",
    "open_store",
    "transaction",
]
