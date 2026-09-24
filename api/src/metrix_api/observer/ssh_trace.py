"""A step-by-step account of one SSH probe, for the server log.

An SSH login that takes seconds could be spending them anywhere: loading keys,
resolving the name, the TCP handshake, the key exchange, authentication (where the
server may be doing reverse DNS or PAM), opening the session (where it may be
generating a motd), or the script. The total says none of that, so a probe is
traced: each step is timestamped as it completes, and AsyncSSH's own debug account
of the same connection -- every auth attempt, every channel request -- is kept
beside it with offsets from the start.

Only probes are traced. AsyncSSH's debug logging is switched on while one runs and
off again when the last finishes, so a long recording's stream is not narrated.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import time
from collections.abc import Iterator
from contextvars import ContextVar

#: In order. Each is marked as it completes, so on a failure the first unmarked step
#: is the one that was running.
STEPS = ("options", "dns", "connect", "handshake", "auth", "session", "script", "close")

#: What each step covers, where that is not obvious from its name.
COVERS = {
    "options": "read SSH config, load client keys",
    "dns": "resolve the effective host",
    "connect": "TCP handshake",
    "handshake": "SSH version exchange and key exchange",
    "auth": "authentication, including anything sshd does first (reverse DNS, PAM)",
    "session": "open a session channel and start the command (PAM session, motd)",
    "script": "run the probe script and read its output",
    "close": "close the connection",
}

#: Enough for a full login; bounded so a chatty failure cannot flood the log.
MAX_EVENTS = 80

_current: ContextVar[Trace | None] = ContextVar("ssh_trace", default=None)
_active: set[Trace] = set()
_saved_level: int | None = None


class Trace:
    def __init__(self, destination: str) -> None:
        self.destination = destination
        self.started = time.perf_counter()
        self.marks: list[tuple[str, float, str]] = []
        self.events: list[tuple[float, str]] = []
        #: AsyncSSH's log prefix for this connection, e.g. "[conn=3", once known.
        self.tag: str | None = None

    def mark(self, step: str, note: str = "") -> None:
        self.marks.append((step, time.perf_counter(), note))

    def owns(self, message: str) -> bool:
        tag = self.tag
        return tag is not None and message.startswith((tag + "]", tag + ","))

    def report(self, error: BaseException | None = None) -> str:
        now = time.perf_counter()
        total = _ms(now - self.started)
        if error is None:
            head = f"ssh probe {self.destination} ok in {total}ms"
        else:
            running = STEPS[len(self.marks)] if len(self.marks) < len(STEPS) else "?"
            if isinstance(error, asyncio.CancelledError):
                reason = "cancelled, usually because the verification timeout ran out"
            else:
                reason = f"{type(error).__name__}: {str(error) or 'no message'}"
            head = f"ssh probe {self.destination} failed during {running} after {total}ms: {reason}"

        lines = [head]
        previous = self.started
        for step, at, note in self.marks:
            lines.append(_step_line(step, _ms(at - previous), note))
            previous = at
        if error is not None and len(self.marks) < len(STEPS):
            running = STEPS[len(self.marks)]
            note = "<- still running when it failed"
            lines.append(_step_line(running, _ms(now - previous), note))

        if self.events:
            lines.append("asyncssh:")
            lines.extend(
                f"  +{_ms(at - self.started):>8}ms {message}" for at, message in self.events
            )
        return "\n".join(lines)


def _step_line(step: str, ms: float, note: str) -> str:
    detail = f"{COVERS.get(step, '')}{'; ' if note else ''}{note}"
    return f"  {step:<10}{ms:>9}ms  {detail}"


def _ms(seconds: float) -> float:
    return round(seconds * 1000, 1)


class _Capture(logging.Filter):
    """Copies AsyncSSH records into whichever trace they belong to.

    A filter on the `asyncssh` logger rather than a handler, so it can capture a
    debug record and then drop it: the logger is lowered to DEBUG only so this sees
    those records, and anything below the level it had before must go no further --
    a root handler would otherwise print every debug line while a probe runs.

    A record belongs to a trace when it is emitted in that probe's context (AsyncSSH
    callbacks inherit the context of the task that opened the connection), or when
    it carries that connection's `[conn=N]` prefix.
    """

    def __init__(self) -> None:
        super().__init__()
        self.passthrough = logging.WARNING

    def filter(self, record: logging.LogRecord) -> bool:
        if _active:
            with contextlib.suppress(Exception):  # tracing must never break a probe
                self._capture(record)
        return record.levelno >= self.passthrough

    @staticmethod
    def _capture(record: logging.LogRecord) -> None:
        message = record.getMessage()
        trace = _current.get()
        if trace is None or trace not in _active:
            trace = next((t for t in _active if t.owns(message)), None)
        if trace is not None and len(trace.events) < MAX_EVENTS:
            # The first line only: the command event carries the whole script.
            first, _, rest = message.partition("\n")
            first = first[:200] + (" ..." if rest else "")
            trace.events.append((time.perf_counter(), first))


_capture = _Capture()


@contextlib.contextmanager
def tracing(trace: Trace) -> Iterator[Trace]:
    """Collect AsyncSSH's account of the connection opened inside this block."""
    global _saved_level
    logger = logging.getLogger("asyncssh")
    if not _active:
        if _capture not in logger.filters:
            logger.addFilter(_capture)
        _saved_level = logger.level
        _capture.passthrough = logger.getEffectiveLevel()
        logger.setLevel(logging.DEBUG)
    _active.add(trace)
    token = _current.set(trace)
    try:
        yield trace
    finally:
        _current.reset(token)
        _active.discard(trace)
        if not _active and _saved_level is not None:
            logger.setLevel(_saved_level)
            _saved_level = None
