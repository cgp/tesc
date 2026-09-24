"""What this process has been doing, kept where the page can read it.

Operations on the server -- a discovery walk, an SSH probe, an engine that exited --
report a summary to whichever page started them, and the evidence behind it goes to
a log nobody is watching. This keeps the recent part of that log in memory and hands
it to the Logs page (§2.6).

Only `metrix_api` loggers are captured. Their messages are written by this code,
which logs addresses, usernames and key paths and never key material or header
values -- so nothing here needs redacting at display, which is the one place it may
not happen. Third-party loggers stay out: botocore and asyncssh are noise at INFO,
and uvicorn's access log would fill with this page's own polling.
"""

from __future__ import annotations

import logging
import threading
import traceback
from collections import deque
from datetime import UTC, datetime
from typing import Any

#: The logger tree that is captured.
ROOT = "metrix_api"

#: Enough for a long session of verification and discovery, bounded so a process
#: that runs for weeks does not grow with its own history.
CAPACITY = 2000


class Buffer(logging.Handler):
    """A bounded, sequence-numbered copy of recent records.

    Sequence numbers let a reader ask for only what it has not seen, and let it tell
    that records were evicted between two reads (the oldest one held is newer than
    the last one it saw).
    """

    def __init__(self, capacity: int = CAPACITY) -> None:
        super().__init__(level=logging.INFO)
        self.capacity = capacity
        self._entries: deque[dict[str, Any]] = deque(maxlen=capacity)
        self._seq = 0
        self._guard = threading.Lock()

    def emit(self, record: logging.LogRecord) -> None:
        try:
            entry = {
                "time": datetime.fromtimestamp(record.created, UTC).isoformat(
                    timespec="milliseconds"
                ).replace("+00:00", "Z"),
                "level": record.levelname.lower(),
                # The package prefix is the same on every line; the module is not.
                "source": record.name.removeprefix(f"{ROOT}."),
                "message": record.getMessage(),
                "trace": "".join(traceback.format_exception(*record.exc_info)).rstrip()
                if record.exc_info
                else None,
            }
        except Exception:  # noqa: BLE001 - a bad format string must not break logging
            self.handleError(record)
            return
        # Records arrive from the event loop and from threadpool routes alike.
        with self._guard:
            self._seq += 1
            self._entries.append({"seq": self._seq, **entry})

    def since(self, after: int = 0, limit: int | None = None) -> dict[str, Any]:
        """Records newer than `after`, oldest first, and the newest sequence number."""
        with self._guard:
            entries = [e for e in self._entries if e["seq"] > after]
            latest = self._seq
            oldest = self._entries[0]["seq"] if self._entries else latest + 1
        if limit is not None:
            entries = entries[-limit:]
        return {
            "entries": entries,
            "latest": latest,
            "capacity": self.capacity,
            # True when records the reader never saw have already been evicted.
            "truncated": after + 1 < oldest and bool(entries),
        }


_buffer: Buffer | None = None


def install() -> Buffer:
    """Attach the buffer to the `metrix_api` logger tree, once per process.

    Idempotent because the app factory runs once per test. The logger is lowered to
    INFO only if nothing chose its level already: without that, INFO records are
    dropped before any handler sees them, because the root logger defaults to WARNING.
    """
    global _buffer
    if _buffer is None:
        _buffer = Buffer()
        logger = logging.getLogger(ROOT)
        logger.addHandler(_buffer)
        if logger.level == logging.NOTSET:
            logger.setLevel(logging.INFO)
    return _buffer
