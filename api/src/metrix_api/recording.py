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
from metrix_api.analysis import SETTLE_PHASE, leaks
from metrix_api.discovery.resolve import Resolver, compare
from metrix_api.observer import transport_for
from metrix_api.observer.collector import Clock, collect_all
from metrix_api.observer.metrics import (
    HOST_COUNT_CHANGED,
    NOT_RETURNED_TO_BASELINE,
    TARGET_UNREACHABLE,
    Annotation,
    Gap,
    Sample,
)
from metrix_api.profiles import Profile, ProfileError
from metrix_api.store import inventories
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

    #: Set for a discovered profile: the snapshot this recording is pinned to, and
    #: the resolver that will walk again at each phase boundary.
    resolver: Resolver | None = None
    pinned: inventories.Stored | None = None

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

    def refresh(self, *, at_ms: int, phase: str = store.OBSERVATION_PHASE) -> None:
        """Walk discovery again at a phase boundary and record what moved.

        Collection stays with the set pinned at the start: a series that begins
        halfway through a recording is worse than an absent one, and every average
        over "the environment" would change meaning mid-chart. What this produces is
        the annotation, which is what tells a later reader that the comparison was
        against a moving target (design-api 2.3).

        Never fatal. A refresh that cannot reach AWS costs the check, not the
        recording -- the samples already collected are still true.
        """
        if self.resolver is None or self.pinned is None or self.profile.discover is None:
            return
        try:
            latest = self.resolver.resolve(self.profile, force=True)
        except Exception as exc:  # noqa: BLE001 - a failed check must not end a recording
            log.warning("inventory refresh failed for %s: %s", self.recording_id, exc)
            return

        change = compare(self.pinned.inventory, latest.inventory)
        if not change.moved:
            return
        log.info("%s: %s", self.recording_id, change.describe())
        store.add_annotation(
            self.conn,
            self.recording_id,
            Annotation(
                code=HOST_COUNT_CHANGED,
                # Warn, not invalid: an environment that scaled under load may be
                # exactly what was being measured. It is the reader's call, and the
                # annotation is what lets them make it.
                severity="warn",
                from_ms=at_ms,
                phase=phase,
                message=change.describe(),
                detail=change.detail,
            ),
        )

    def _note_unreachable(self, ended: int) -> None:
        """Flag any target that produced nothing at all.

        A box that answered intermittently has gaps, which are drawn as gaps and are
        a warning. A box that never answered once is a different fact: it was named
        in the profile, so a reader counts it among what was measured, and an average
        over "the environment" that quietly omits one of its machines is worse than
        no average. Hence `invalid` -- such a recording should not become a baseline
        without someone saying so out loud.
        """
        counts = store.sample_counts(self.conn, self.recording_id)
        for endpoint in self.profile.observed:
            if counts.get(endpoint.id):
                continue
            store.add_annotation(
                self.conn,
                self.recording_id,
                Annotation(
                    code=TARGET_UNREACHABLE,
                    severity="invalid",
                    from_ms=0,
                    to_ms=ended,
                    target_id=endpoint.id,
                    phase=store.OBSERVATION_PHASE,
                    message=(
                        f"{endpoint.id} produced no samples at all; it was collected "
                        f"from at {self._describe(endpoint)} and never answered"
                    ),
                ),
            )

    def _note_leaks(self, ended: int) -> None:
        """Flag anything that did not return to baseline during settle.

        Silent for an observation-only recording, which has no settle phase to
        measure: baseline and settle collapse into one window there, so there is no
        "after" and nothing to claim (design-api 10.2). A phased load run has both,
        and this is where the leak signal is raised.
        """
        for target, result in leaks(self.conn, self.recording_id):
            store.add_annotation(
                self.conn,
                self.recording_id,
                Annotation(
                    code=NOT_RETURNED_TO_BASELINE,
                    severity="warn",
                    from_ms=0,
                    to_ms=ended,
                    target_id=target,
                    phase=SETTLE_PHASE,
                    message=(
                        f"{result.metric} on {target} did not return to its baseline "
                        f"during settle: finished at {result.final:.3g} against a "
                        f"baseline of {result.baseline:.3g} (band ±{result.band:.3g})"
                    ),
                    detail={
                        "metric": result.metric,
                        "final": result.final,
                        "baseline": result.baseline,
                        "band": result.band,
                        "peak": result.peak,
                        "peak_at_ms": result.peak_at_ms,
                        "samples": result.n,
                    },
                ),
            )

    @staticmethod
    def _describe(endpoint) -> str:
        """Where collection was attempted, in the spelling the Profiles page shows."""
        try:
            return transport_for(endpoint).describe()
        except Exception:  # noqa: BLE001 - naming the place is a courtesy, not the point
            return "an address that could not be built"

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
        # The closing phase boundary. An observation-only recording has exactly two,
        # and a phased load run (A4) will call this at each of its own.
        self.refresh(at_ms=ended)
        self._note_unreachable(ended)
        self._note_leaks(ended)
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
    resolver: Resolver | None = None,
) -> Recorder:
    """Open a recording and begin collecting.

    Refuses a profile with nothing to collect from rather than producing an empty
    recording that looks like a failed one.

    A profile that discovers is resolved here rather than read from the cache: run
    start is a phase boundary (design-api 3.2), and the one moment worth paying a
    walk for is the moment the measurement begins. The snapshot it produces is
    pinned to the recording, so what this ran against stays answerable after the
    tasks are gone.
    """
    pinned = None
    if profile.discover is not None:
        if resolver is None:
            raise RecordingError(
                f"profile {profile.name!r} discovers its endpoints and no resolver was "
                "supplied; this is a wiring mistake, not a configuration one"
            )
        try:
            resolution = resolver.resolve(profile, force=True)
        except Exception as exc:  # noqa: BLE001 - surfaced as a start failure
            raise RecordingError(f"could not resolve profile {profile.name!r}: {exc}") from exc
        if not resolution.endpoints:
            raise RecordingError(
                f"profile {profile.name!r}: {profile.discover.source} resolved to nothing "
                f"addressable (the walk reached {resolution.inventory.reached})"
            )
        profile = profile.with_endpoints(resolution.endpoints)
        pinned = resolution.stored

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

    if pinned is not None:
        inventories.pin(conn, recording_id, pinned.id)

    recorder = Recorder(
        conn=conn,
        recording_id=recording_id,
        profile=profile,
        clock=clock or Clock.start(),
        interval=interval,
        groups=list(groups or profile.collect_metrics),
        resolver=resolver,
        pinned=pinned,
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
