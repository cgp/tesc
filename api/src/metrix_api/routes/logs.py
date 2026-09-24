"""The server log: recent records from this process, for the Logs page."""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Query

from metrix_api import server_log

router = APIRouter(prefix="/api/logs", tags=["logs"])


@router.get("")
def read_logs(
    after: int = Query(0, ge=0),
    limit: int = Query(server_log.CAPACITY, ge=1, le=server_log.CAPACITY),
) -> dict[str, Any]:
    """Records newer than `after`, oldest first.

    The page polls with the last sequence number it holds, so a quiet server answers
    with an empty list rather than the same two thousand lines every two seconds.
    """
    return server_log.install().since(after, limit)
