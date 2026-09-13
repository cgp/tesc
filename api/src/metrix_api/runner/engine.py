"""Running the engine as a child process, and reading what it says.

The engine never calls back. It has no address for the API, no notion that one
exists, and no behaviour that changes when one is attached (contract C1.1). So
supervision here is exactly three things: spawn it with a bundle, read its summary
stream until it ends, and record how it exited.

**Reading must never stall the engine.** A full pipe is backpressure, and the engine's
answer to backpressure is to drop records and annotate — which means a slow reader
silently costs measurement data. The stream is therefore drained continuously into
the store rather than gathered and processed at the end, and stderr is drained on its
own task so a diagnostic large enough to fill its buffer cannot deadlock the run.

The engine is not asked to stop by being killed. It is asked once, politely, and only
then killed — a killed engine loses the records still in its output queue, including
the `run_finished` that says how it went.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import os
import signal
import sqlite3
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

from metrix_api.runner.ingest import Ingest, IngestError

log = logging.getLogger(__name__)

#: How long the engine gets to finish after being asked. Its own output shutdown is
#: bounded at 500ms (design-engine 2.3), so this only has to cover that plus the
#: drain of requests already in flight.
GRACE = 10.0

#: Exit codes the engine documents: 0 finished, 1 setup or internal failure, 130
#: interrupted. Anything else is a crash and is reported as one rather than guessed at.
FINISHED, FAILED, INTERRUPTED = 0, 1, 130


class EngineError(Exception):
    """The engine could not be started, or ended in a way worth refusing."""


@dataclass(slots=True)
class EngineRun:
    """What one engine process did."""

    exit_code: int | None = None
    records: int = 0
    windows: int = 0
    unplaced: int = 0
    #: The last few stderr lines. The engine writes a traffic diagnostic there, and
    #: on a setup failure that is the only place the reason appears.
    diagnostic: str = ""
    stopped_because: str | None = None

    @property
    def ok(self) -> bool:
        return self.exit_code == FINISHED

    @property
    def interrupted(self) -> bool:
        return self.exit_code == INTERRUPTED


def engine_binary() -> Path | None:
    """Where the engine is, if it is anywhere.

    `METRIX_ENGINE` first so a checkout can point at a build without installing it,
    then the repository's own debug build, then the PATH. Returning `None` rather
    than raising because "no engine installed" is the normal state for most of this
    tool's life, and the caller says so better than an exception thrown from here.
    """
    if override := os.environ.get("METRIX_ENGINE"):
        candidate = Path(override)
        return candidate if candidate.is_file() else None

    root = Path(__file__).resolve().parents[4]
    for build in ("debug", "release"):
        for name in ("metrix-engine.exe", "metrix-engine"):
            candidate = root / "engine" / "target" / build / name
            if candidate.is_file():
                return candidate

    from shutil import which

    found = which("metrix-engine")
    return Path(found) if found else None


async def run_engine(
    conn: sqlite3.Connection,
    recording_id: str,
    *,
    bundle: Path,
    started_at: datetime,
    stop: asyncio.Event,
    binary: Path | None = None,
    grace: float = GRACE,
) -> EngineRun:
    """Run one bundle, streaming its summary into the recording as it arrives.

    `stop` is how a person cancels: it asks the engine to finish, which lets it drain
    what is in flight and emit its terminal records, rather than taking the process
    away mid-measurement.
    """
    binary = binary or engine_binary()
    if binary is None:
        raise EngineError(
            "no engine binary: set METRIX_ENGINE, or build one with "
            "`cargo build --manifest-path engine/Cargo.toml`"
        )
    if not binary.is_file():
        # A path that was configured and is wrong is a different problem from none
        # being configured, and it should not surface as a FileNotFoundError thrown
        # out of the event loop.
        raise EngineError(f"engine binary {binary} does not exist")
    if not bundle.is_dir():
        raise EngineError(f"bundle {bundle} is not a directory")

    process = await asyncio.create_subprocess_exec(
        str(binary),
        "--plan",
        str(bundle),
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )

    ingest = Ingest(conn=conn, recording_id=recording_id, started_at=started_at)
    errors: list[str] = []

    async def read_summary() -> None:
        assert process.stdout is not None
        async for raw in process.stdout:
            try:
                ingest.line(raw.decode("utf-8", errors="replace"))
            except IngestError as exc:
                # One unreadable record does not end a run. It is counted and the
                # rest of the stream is still worth having, the same bargain the
                # observer makes with a failed probe.
                ingest.unplaced += 1
                log.warning("engine stream: %s", exc)

    async def read_errors() -> None:
        """Drained on its own task so a full stderr buffer cannot deadlock stdout."""
        assert process.stderr is not None
        async for raw in process.stderr:
            line = raw.decode("utf-8", errors="replace").rstrip()
            if line:
                errors.append(line)
                del errors[:-20]

    async def wait_for_stop() -> None:
        await stop.wait()
        _ask_to_finish(process)

    readers = asyncio.gather(read_summary(), read_errors())
    cancel = asyncio.ensure_future(wait_for_stop())
    try:
        await readers
        try:
            await asyncio.wait_for(process.wait(), timeout=grace)
        except TimeoutError:
            # Its streams closed but it has not exited. Nothing more is coming.
            log.warning("engine did not exit within %.0fs of closing its output", grace)
            process.kill()
            await process.wait()
    finally:
        cancel.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await cancel

    return EngineRun(
        exit_code=process.returncode,
        records=ingest.records,
        windows=ingest.windows,
        unplaced=ingest.unplaced,
        diagnostic="\n".join(errors),
        stopped_because=ingest.stopped_because,
    )


def _ask_to_finish(process: asyncio.subprocess.Process) -> None:
    """Ask, rather than take it away.

    The engine treats an interrupt as a request to drain and report: it cancels what
    is in flight, writes its terminal records, and exits 130. Killing it instead
    would lose whatever is still in its output queue, which includes the record that
    says how the run ended.
    """
    if process.returncode is not None:
        return
    try:
        if os.name == "nt":
            # No SIGINT to a specific child on Windows without sharing a console
            # group, and CTRL_BREAK reaches the whole group. Terminate is the honest
            # option here, and the cost -- losing the terminal records -- is why the
            # caller annotates a cancelled run rather than assuming it ended cleanly.
            process.terminate()
        else:
            process.send_signal(signal.SIGINT)
    except ProcessLookupError:  # pragma: no cover - it exited between the two lines
        pass
