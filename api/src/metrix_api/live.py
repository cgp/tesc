"""Live recordings and the stream that carries them to the browser.

Server-Sent Events, not WebSocket (design §2.5): the view is one-way, control actions
are ordinary POSTs, and SSE hands us reconnection and `Last-Event-ID` replay for free
-- so a dropped connection resumes where it left off instead of leaving a hole in the
chart.

Two properties this module exists to guarantee:

* **Cadence is decoupled from the source.** Samples arrive as the collectors produce
  them; subscribers get one aggregated event per second. Changing either does not
  disturb the other.
* **A slow client coalesces rather than queues.** A view thirty seconds behind is
  worse than one that skipped thirty seconds, so an overrun subscriber is resynced
  with a fresh snapshot instead of being fed a backlog.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import sqlite3
from collections import deque
from collections.abc import AsyncIterator, Awaitable, Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from typing import Any

from metrix_api import plans
from metrix_api.analysis import ENVIRONMENT
from metrix_api.config import Config
from metrix_api.discovery.ecs import Clients
from metrix_api.discovery.resolve import Resolver
from metrix_api.observer.collector import Clock
from metrix_api.observer.metrics import Annotation, Gap, Sample
from metrix_api.profiles import Profile
from metrix_api.recording import Recorder, RecordingError, start_observation
from metrix_api.runner.engine import engine_binary, run_engine
from metrix_api.stats import summarize
from metrix_api.store import recordings as store
from metrix_api.store.db import connect, migrate

log = logging.getLogger(__name__)

#: How often subscribers are pushed to, regardless of how fast samples arrive.
TICK = timedelta(seconds=1)

#: Events kept for replay. At one per second this is several minutes of history --
#: long enough to cover a browser tab that was asleep, short enough to stay small.
REPLAY = 600

#: Events a subscriber may fall behind by before it is resynced rather than filled in.
BACKLOG = 30


@dataclass(slots=True)
class Event:
    id: int
    kind: str
    data: dict[str, Any]

    def encode(self) -> str:
        """SSE wire format. The id is what the browser echoes as Last-Event-ID."""
        return f"id: {self.id}\nevent: {self.kind}\ndata: {json.dumps(self.data)}\n\n"


class Hub:
    """Fan-out for one recording, with a bounded replay buffer."""

    def __init__(self, replay: int = REPLAY) -> None:
        self._next_id = 1
        self._history: deque[Event] = deque(maxlen=replay)
        self._subscribers: set[asyncio.Queue[Event | None]] = set()
        self.closed = False

    @property
    def subscriber_count(self) -> int:
        return len(self._subscribers)

    @property
    def last_id(self) -> int:
        return self._next_id - 1

    def publish(self, kind: str, data: dict[str, Any]) -> Event:
        event = Event(id=self._next_id, kind=kind, data=data)
        self._next_id += 1
        self._history.append(event)

        for queue in list(self._subscribers):
            try:
                queue.put_nowait(event)
            except asyncio.QueueFull:
                # Drop what this subscriber has not read and tell it to resync. The
                # alternative -- letting the queue grow -- is a view that falls
                # further behind the longer it struggles.
                _drain(queue)
                with contextlib.suppress(asyncio.QueueFull):
                    queue.put_nowait(Event(id=event.id, kind="resync", data={}))
        return event

    def close(self) -> None:
        self.closed = True
        for queue in list(self._subscribers):
            with contextlib.suppress(asyncio.QueueFull):
                queue.put_nowait(None)

    def replay_since(self, last_event_id: int | None) -> tuple[list[Event], bool]:
        """Events after `last_event_id`, and whether history had already dropped some.

        A caller that has fallen off the end of the buffer needs a snapshot, not a
        partial catch-up that would silently omit the middle.
        """
        if last_event_id is None or not self._history:
            return list(self._history) if last_event_id is not None else [], False
        oldest = self._history[0].id
        if last_event_id < oldest - 1:
            return [], True
        return [e for e in self._history if e.id > last_event_id], False

    @contextlib.contextmanager
    def _registered(self, queue: asyncio.Queue[Event | None]):
        self._subscribers.add(queue)
        try:
            yield
        finally:
            self._subscribers.discard(queue)

    async def subscribe(self) -> AsyncIterator[Event | None]:
        queue: asyncio.Queue[Event | None] = asyncio.Queue(maxsize=BACKLOG)
        with self._registered(queue):
            while True:
                event = await queue.get()
                if event is None:
                    return
                yield event


def _drain(queue: asyncio.Queue) -> None:
    while not queue.empty():
        with contextlib.suppress(asyncio.QueueEmpty):
            queue.get_nowait()


@dataclass(slots=True)
class LiveRecording:
    """A recording in progress, its hub, and the ticker that feeds it."""

    recorder: Recorder
    hub: Hub
    conn: sqlite3.Connection
    profile_name: str
    #: The plan this run is sending, when one is attached. None for an observation,
    #: which is the same recorder with nothing generating traffic.
    plan_name: str | None = None
    #: Latest value per metric per target. The snapshot a new subscriber receives, so
    #: a late joiner is immediately correct rather than blank until the next change.
    latest: dict[str, dict[str, float]] = field(default_factory=dict)
    counts: dict[str, int] = field(default_factory=lambda: {"samples": 0, "gaps": 0})
    _pending: dict[tuple[str, int], dict[str, float]] = field(default_factory=dict)
    _ticker: asyncio.Task | None = None

    #: Every value seen, per target and metric, so the table can show a distribution
    #: while the recording is still running rather than only after it stops.
    #:
    #: Held here and summarised server-side because the sample-count rule lives in
    #: `stats/` and the UI must not be able to bypass it -- a median computed in
    #: JavaScript would be a second implementation of exactly the rule that has to
    #: hold in one place. The cost is a sort per metric per second: at 1s sampling an
    #: hour-long recording is a few hundred thousand floats, which sorts in
    #: milliseconds, and observation recordings are minutes rather than hours.
    _values: dict[tuple[str, str], list[float]] = field(default_factory=dict)
    #: First and last sample time per target: the Start/Finish diagnostic of 14.2.
    _spans: dict[str, dict[str, int]] = field(default_factory=dict)
    #: The engine, when one is attached: the task supervising it, the event that asks
    #: it to finish, and what it said when it did.
    _engine: asyncio.Task | None = None
    _engine_stop: asyncio.Event | None = None
    engine_error: str | None = None
    #: Called once the recording has closed itself, so whoever is holding it can let
    #: go. A load run ends when the traffic does rather than when somebody asks, so
    #: the registry cannot be the only thing that removes it.
    _on_finished: Callable[[str], None] | None = None
    #: True once somebody has begun closing this recording. Two callers is the
    #: ordinary case rather than a race: the engine finishing and a person pressing
    #: Stop can arrive in either order, and the second must not stop the recorder a
    #: second time on a connection the first has already closed.
    _closing: bool = False

    @property
    def recording_id(self) -> str:
        return self.recorder.recording_id

    def note_sample(self, sample: Sample) -> None:
        self._pending[(sample.target_id, sample.t_ms)] = dict(sample.metrics)
        for metric, value in sample.metrics.items():
            self.latest.setdefault(metric, {})[sample.target_id] = value
            self._values.setdefault((sample.target_id, metric), []).append(value)
        # One sample is one moment, however many metrics it carried.
        span = self._spans.setdefault(
            sample.target_id, {"first_ms": sample.t_ms, "last_ms": sample.t_ms, "n": 0}
        )
        span["last_ms"] = sample.t_ms
        span["n"] += 1

        self.counts["samples"] += 1

    def summaries(self) -> dict[str, dict[str, dict[str, Any]]]:
        """The distribution so far, per target, plus the pooled view across them.

        Same shape the finished recording serves from `/summary`, so the table is one
        view rather than two that drift (design-api 14.3).
        """
        pooled: dict[str, list[float]] = {}
        found: dict[str, dict[str, dict[str, Any]]] = {}
        for (target, metric), values in self._values.items():
            found.setdefault(target, {})[metric] = summarize(metric, values).to_document()
            pooled.setdefault(metric, []).extend(values)
        found[ENVIRONMENT] = {
            metric: summarize(metric, values).to_document() for metric, values in pooled.items()
        }
        return found

    def note_gap(self, gap: Gap) -> None:
        self.counts["gaps"] += 1
        self.hub.publish(
            "gap",
            {
                "target_id": gap.target_id,
                "from_ms": gap.from_ms,
                "to_ms": gap.to_ms,
                "reason": gap.reason,
            },
        )
        note = Annotation.from_gap(gap)
        self.hub.publish(
            "annotation",
            {
                "code": note.code,
                "severity": note.severity,
                "target_id": note.target_id,
                "from_ms": note.from_ms,
                "to_ms": note.to_ms,
                "message": note.message,
            },
        )

    def snapshot(self) -> dict[str, Any]:
        return {
            "recording_id": self.recording_id,
            "profile": self.profile_name,
            # A load run and an observation look the same on this page until
            # something goes wrong with the traffic, and then the difference is the
            # first thing worth knowing.
            "plan": self.plan_name,
            "engine_error": self.engine_error,
            # Whether any box in this run has a collector. A load run against
            # somebody else's service has none, and a table with no rows in it
            # should say which of those it is rather than looking broken.
            "collecting": bool(self.recorder.profile.observed),
            "status": store.RUNNING if self.recorder.running else store.FINISHED,
            "elapsed_ms": self.recorder.clock.now_ms(),
            "targets": [e.id for e in self.recorder.profile.observed],
            "metrics": sorted(self.latest),
            "latest": self.latest,
            "counts": dict(self.counts),
            "summaries": self.summaries(),
            "spans": {t: dict(s) for t, s in self._spans.items()},
            # Which phase produced these numbers. It sits in the table header because
            # a figure read without knowing its phase is a figure read wrong (14.3).
            "phase": store.OBSERVATION_PHASE,
        }

    async def _tick(self) -> None:
        """Push once per second, whatever arrived in between."""
        while True:
            await asyncio.sleep(TICK.total_seconds())
            if self._pending:
                samples = [
                    {"target_id": target, "t_ms": t_ms, "metrics": metrics}
                    for (target, t_ms), metrics in sorted(
                        self._pending.items(), key=lambda item: item[0][1]
                    )
                ]
                self._pending.clear()
                self.hub.publish(
                    "samples",
                    {
                        "samples": samples,
                        "elapsed_ms": self.recorder.clock.now_ms(),
                        "counts": dict(self.counts),
                        # Not a delta, unlike the samples beside it: a distribution
                        # cannot be sent as one, and recomputing it in the browser
                        # would put a second copy of the sample-count rule there.
                        "summaries": self.summaries(),
                        "spans": {t: dict(s) for t, s in self._spans.items()},
                    },
                )

    def start_ticker(self) -> None:
        self._ticker = asyncio.create_task(self._tick())

    async def attach(self, engine: Awaitable[Any], stop: asyncio.Event) -> None:
        """Run the engine alongside the observer, and end the recording when it ends.

        A load run finishes when the traffic does. The observer has no opinion about
        when that is -- it would happily keep collecting -- so the engine's exit is
        what closes the recording, and a person pressing Stop asks the engine to
        finish rather than stopping the collectors out from under it.
        """
        self._engine_stop = stop

        async def supervise() -> None:
            try:
                await engine
            except Exception as exc:  # noqa: BLE001 - surfaced on the recording
                # The run still happened, and whatever the observer collected is
                # worth keeping. What is lost is the load half, and saying so beats
                # a recording that simply stops with no reason attached.
                log.exception("engine failed for recording %s", self.recording_id)
                self.engine_error = str(exc)
                self.hub.publish("annotation", {
                    "code": "engine_failed",
                    "severity": "invalid",
                    "target_id": None,
                    "from_ms": self.recorder.clock.now_ms(),
                    "to_ms": None,
                    "message": f"the load generator failed: {exc}",
                })
                store.add_annotation(self.conn, self.recording_id, Annotation(
                    code="engine_failed",
                    severity="invalid",
                    from_ms=self.recorder.clock.now_ms(),
                    message=f"the load generator failed: {exc}",
                ))
            finally:
                await self.stop()

        self._engine = asyncio.create_task(supervise())

    async def _finish_engine(self) -> None:
        """Ask the engine to finish, and wait for it.

        Skipped when the caller *is* the supervising task, which is the ordinary way
        a load run ends: the engine exited, and the task is now closing the recording
        it was attached to. Waiting on itself there would hang the run forever.
        """
        if self._engine is None or self._engine is asyncio.current_task():
            return
        if self._engine_stop is not None:
            self._engine_stop.set()
        with contextlib.suppress(Exception):
            await self._engine

    async def stop(self) -> store.RecordingRow | None:
        """Close the recording. `None` when somebody else is already closing it.

        The second caller is not an error and not a race. A load run ends when the
        engine ends, and a person can press Stop at the same moment; whichever
        arrives first does the work, and the other is told there was nothing left to
        do rather than stopping a recorder that has already let go of its database.
        """
        if self._closing:
            return None
        self._closing = True
        await self._finish_engine()
        if self._ticker is not None:
            self._ticker.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._ticker
        row = await self.recorder.stop()
        self.hub.publish("status", {"status": row.status, "duration_ms": row.duration_ms})
        self.hub.close()
        self.conn.close()
        if self._on_finished is not None:
            self._on_finished(self.recording_id)
        return row


class Registry:
    """The live recordings this process is running.

    Held in memory deliberately: a recording is a running task, and a task does not
    survive a restart. What survives is in SQLite, which is the point of writing as
    we go rather than at the end.
    """

    def __init__(self) -> None:
        self._live: dict[str, LiveRecording] = {}

    def get(self, recording_id: str) -> LiveRecording | None:
        return self._live.get(recording_id)

    def ids(self) -> list[str]:
        return list(self._live)

    async def start(
        self,
        config: Config,
        profile: Profile,
        *,
        interval: timedelta = TICK,
        groups: list[str] | None = None,
        note: str | None = None,
        clock: Clock | None = None,
        clients: Clients | None = None,
    ) -> LiveRecording:
        # The recorder keeps its own connection for the life of the recording; request
        # connections come and go, and a collector must not depend on one.
        conn = connect(config.database)
        # Idempotent, and cheap. A recorder holds its own connection for its whole
        # life, so it cannot rely on whoever else happened to open the database.
        migrate(conn)
        # On the recorder's own connection, because it refreshes at phase boundaries
        # long after the request that started it has gone. Clients are built lazily,
        # so an explicit profile never opens an AWS session.
        recorder = await start_observation(
            conn,
            profile,
            interval=interval,
            groups=groups,
            note=note,
            clock=clock,
            resolver=Resolver(conn=conn, aws=config.aws, clients=clients),
        )

        live = LiveRecording(
            recorder=recorder, hub=Hub(), conn=conn, profile_name=profile.name
        )
        # Persistence stays first: the recorder writes the row, then calls these, so
        # a subscriber can never see a value that was not stored.
        recorder.tee_sample = live.note_sample
        recorder.tee_gap = live.note_gap

        live.start_ticker()
        self._live[live.recording_id] = live
        return live

    async def start_load(
        self,
        config: Config,
        profile: Profile,
        plan_name: str,
        *,
        only: list[str] | None = None,
        interval: timedelta = TICK,
        groups: list[str] | None = None,
        note: str | None = None,
        clients: Clients | None = None,
    ) -> LiveRecording:
        """Send a plan at a profile, and watch the boxes while it runs.

        A load run is an observation with traffic attached: the same recorder, the
        same collectors, the same live view. What is added is a generator pointed at
        the same environment, and one clock resolving the two (design-api 17.2, and
        the contract's C1).

        The order matters. The plan is refused before anything is opened if it cannot
        run, and the engine binary is looked for before a row exists -- a recording
        created for a run that never starts is a recording somebody has to explain
        later. The bundle is assembled *after* the observer starts, from the profile
        the observer actually resolved, so the boxes traffic goes to are the boxes
        this recording is about rather than whatever discovery said a minute ago.
        """
        plan = plans.load_plan(config, plan_name)
        problems = plans.check(plan)
        if not plans.ready(problems):
            raise RecordingError(
                f"plan {plan_name!r} cannot run yet: "
                + "; ".join(f"{p.where}: {p.message}" for p in problems if p.severity == "error")
            )
        if engine_binary() is None:
            raise RecordingError(
                "no engine binary: set METRIX_ENGINE, or build one with "
                "`cargo build --manifest-path engine/Cargo.toml`"
            )

        conn = connect(config.database)
        migrate(conn)
        # Taken together, and that pairing is the point: the recorder's clock is
        # monotonic and has no wall time in it, while the engine reports the wall
        # instant of its own zero. One of each, read in the same breath, is what lets
        # the two timelines be resolved into one.
        clock = Clock.start()
        started_at = datetime.now(UTC)

        recorder = await start_observation(
            conn,
            profile,
            interval=interval,
            groups=groups,
            note=note,
            clock=clock,
            resolver=Resolver(conn=conn, aws=config.aws, clients=clients),
            kind="load",
            plan_name=plan_name,
        )

        try:
            bundle = plans.assemble(plan, recorder.profile, only=only)
            exported = bundle.write(config.run_dir(recorder.recording_id) / "plan.snapshot")
        except Exception as exc:
            # The row exists and nothing ran. Marked failed rather than left running
            # forever or deleted: it is a real attempt, and the reason it did not
            # start is worth keeping.
            store.add_annotation(
                conn,
                recorder.recording_id,
                Annotation(
                    code="bundle_failed",
                    severity="invalid",
                    from_ms=0,
                    message=f"the plan could not be assembled for this profile: {exc}",
                ),
            )
            await recorder.stop(status=store.FAILED)
            conn.close()
            raise RecordingError(f"plan {plan_name!r} could not be assembled: {exc}") from exc

        live = LiveRecording(
            recorder=recorder,
            hub=Hub(),
            conn=conn,
            profile_name=profile.name,
            plan_name=plan_name,
            # Discarding a key that is already gone: a person pressing Stop takes
            # it out of the registry first, and the recording closing itself must not
            # then fail on its way out.
            _on_finished=lambda key: self._live.pop(key, None),
        )
        recorder.tee_sample = live.note_sample
        recorder.tee_gap = live.note_gap
        live.start_ticker()
        self._live[live.recording_id] = live

        stop = asyncio.Event()
        await live.attach(
            run_engine(
                conn,
                live.recording_id,
                bundle=exported,
                started_at=started_at,
                stop=stop,
                targets=tuple(store.get(conn, live.recording_id).targets),
            ),
            stop,
        )
        log.info(
            "recording %s running plan %s (%s) against profile %s",
            live.recording_id,
            plan_name,
            bundle.hash,
            profile.name,
        )
        return live

    async def stop(self, recording_id: str) -> store.RecordingRow | None:
        live = self._live.pop(recording_id, None)
        if live is None:
            return None
        return await live.stop()

    async def stop_all(self) -> None:
        for recording_id in list(self._live):
            with contextlib.suppress(Exception):
                await self.stop(recording_id)
