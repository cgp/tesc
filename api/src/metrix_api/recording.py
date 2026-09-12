"""Running an observation-only recording.

Start it against a profile, let it collect, stop it. What comes out is a row in the
archive that reopens later with its samples, its gaps, and its annotations -- the
mode that makes baseline comparison worth anything, and the whole product until an
engine exists.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import sqlite3
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import timedelta

from metrix_api import __version__
from metrix_api.observer import transport_for
from metrix_api.observer.collector import Clock, collect_all
from metrix_api.observer.metrics import Gap, Sample
from metrix_api.profiles import Profile, ProfileError
from metrix_api.store import recordings as store

log = logging.getLogger(__name__)


class RecordingError(Exception):
    """A recording that cannot be started."""


@dataclass(slots=True)
class Recorder:
    """A recording in progress.

    Writes land in SQLite as they arrive rather than being buffered to the end: a
    recording interrupted by a crash is still worth having up to the point it stopped,
    which is the same reasoning as drawing gaps rather than hiding them.
    """

    conn: sqlite3.Connection
    recording_id: str
    profile: Profile
    clock: Clock
    interval: timedelta
    groups: list[str] = field(default_factory=list)

    #: Optional observers, called *after* the row is written. The live stream uses
    #: these, so a subscriber can never see a value that was not persisted first.
    #: Read per call, so they can be attached after collection has started.
    tee_sample: Callable[[Sample], None] | None = None
    tee_gap: Callable[[Gap], None] | None = None

    _stop: asyncio.Event = field(default_factory=asyncio.Event, init=False)
    _task: asyncio.Task | None = field(default=None, init=False)
    _samples: int = field(default=0, init=False)
    _gaps: int = field(default=0, init=False)

    @property
    def samples_written(self) -> int:
        return self._samples

    @property
    def gaps_recorded(self) -> int:
        return self._gaps

    @property
    def running(self) -> bool:
        return self._task is not None and not self._task.done()

    def _on_sample(self, sample: Sample) -> None:
        self._samples += store.add_sample(self.conn, self.recording_id, sample)
        if self.tee_sample is not None:
            self.tee_sample(sample)

    def _on_gap(self, gap: Gap) -> None:
        store.add_gap(self.conn, self.recording_id, gap)
        self._gaps += 1
        if self.tee_gap is not None:
            self.tee_gap(gap)

    async def probe(self, *, at: str) -> None:
        """Record what each box is, and how full its disks are, at one instant.

        Best-effort and never fatal: a box that will not answer a `df` is still worth
        observing, and a recording that refused to start because an identity probe
        timed out would be a worse tool. A transport with nothing to report, or a
        failure, simply leaves the rows absent -- which the UI draws as "not
        collected" rather than as zero.
        """
        for endpoint in self.profile.observed:
            try:
                transport = transport_for(endpoint)
                probe = getattr(transport, "probe", None)
                if probe is None:
                    continue
                facts = await probe()
            except Exception as exc:  # noqa: BLE001 - any failure here is non-fatal
                log.warning("probe (%s) failed for %s: %s", at, endpoint.id, exc)
                continue
            if facts:
                store.save_facts(self.conn, self.recording_id, endpoint.id, facts, at=at)

    def _start_task(self) -> None:
        endpoints = self.profile.observed
        for endpoint in endpoints:
            store.start_phase(
                self.conn, self.recording_id, endpoint.id, store.OBSERVATION_PHASE, 0
            )
        self._task = asyncio.create_task(
            collect_all(
                endpoints,
                transport_for,
                interval=self.interval,
                clock=self.clock,
                on_sample=self._on_sample,
                on_gap=self._on_gap,
                groups=self.groups or None,
                stop=self._stop,
            )
        )

    async def stop(self, *, status: str = store.FINISHED) -> store.RecordingRow:
        """Stop collecting and close the recording.

        Always closes the row, even if a collector was wedged: a recording left in
        `running` forever would be indistinguishable from one still going.
        """
        self._stop.set()
        if self._task is not None:
            self._task.cancel()
            # Shutdown is best-effort: a collector wedged on a dead socket must not
            # keep the recording open, and whatever it was doing is already recorded
            # as a gap.
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await self._task

        # After the collectors have stopped, so the second reading reflects a box
        # that has finished doing whatever the run asked of it.
        await self.probe(at="finish")

        ended = self.clock.now_ms()
        for endpoint in self.profile.observed:
            store.end_phase(
                self.conn, self.recording_id, endpoint.id, store.OBSERVATION_PHASE, ended
            )
        return store.finish(self.conn, self.recording_id, status=status, duration_ms=ended)


async def start_observation(
    conn: sqlite3.Connection,
    profile: Profile,
    *,
    interval: timedelta = timedelta(seconds=1),
    groups: list[str] | None = None,
    note: str | None = None,
    clock: Clock | None = None,
) -> Recorder:
    """Open a recording and begin collecting.

    Refuses a profile with nothing to collect from rather than producing an empty
    recording that looks like a failed one.
    """
    observed = profile.observed
    if not observed:
        raise RecordingError(
            f"profile {profile.name!r} has no endpoint with a collector; set "
            "`collect.transport` to ssh or scrape on at least one endpoint"
        )

    # Fail before opening a row if a transport cannot even be constructed.
    for endpoint in observed:
        try:
            transport_for(endpoint)
        except (ValueError, ProfileError) as exc:
            raise RecordingError(f"endpoint {endpoint.id!r}: {exc}") from exc

    recording_id = store.new_id()
    store.create(
        conn,
        recording_id=recording_id,
        profile=profile,
        endpoints=observed,
        kind="observation",
        api_version=__version__,
        interval_s=interval.total_seconds(),
        note=note,
    )

    recorder = Recorder(
        conn=conn,
        recording_id=recording_id,
        profile=profile,
        clock=clock or Clock.start(),
        interval=interval,
        groups=list(groups or profile.collect_metrics),
    )
    # Before the collectors, so the first disk reading is of a box the run has not
    # touched yet. It is the baseline half of "how much did this consume".
    await recorder.probe(at="start")
    recorder._start_task()
    log.info("recording %s started against profile %s", recording_id, profile.name)
    return recorder


async def observe_for(
    conn: sqlite3.Connection,
    profile: Profile,
    duration: timedelta,
    **kwargs,
) -> store.RecordingRow:
    """Record for a fixed duration. The scripted form of start-wait-stop."""
    recorder = await start_observation(conn, profile, **kwargs)
    try:
        await asyncio.sleep(duration.total_seconds())
    finally:
        return await recorder.stop()  # noqa: B012 - stopping is the point, even on cancel
