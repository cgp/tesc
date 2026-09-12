"""Driving a transport, turning its output into samples, and recording what was missed.

A collection failure degrades the recording rather than aborting it: the affected
interval becomes a :class:`Gap`, collection retries, and the chart draws a hole
instead of a line through it.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import time
from collections.abc import AsyncIterator, Awaitable, Callable
from dataclasses import dataclass
from datetime import timedelta
from typing import Protocol

from metrix_api.observer import linux
from metrix_api.observer.metrics import Gap, Sample
from metrix_api.profiles import Endpoint

log = logging.getLogger(__name__)

#: A sample is late rather than missing until this much of the interval has passed.
LATE_FACTOR = 2.5
#: Reconnect backoff, seconds. Capped so a host that comes back is picked up promptly.
BACKOFF = (1.0, 2.0, 5.0, 10.0)


class Transport(Protocol):
    """Yields raw sample blocks until cancelled or broken.

    Implementations own their connection. Raising is how a transport reports that it
    is done; the collector turns that into a gap and retries.
    """

    async def stream(self, interval: timedelta) -> AsyncIterator[str]: ...


SampleSink = Callable[[Sample], Awaitable[None] | None]
GapSink = Callable[[Gap], Awaitable[None] | None]


@dataclass(slots=True)
class Clock:
    """Milliseconds since the recording started.

    Monotonic, so a sample's position on the timeline is unaffected by wall-clock
    adjustments on this machine -- which is the whole basis for overlaying host and
    load series.
    """

    started: float

    @classmethod
    def start(cls) -> Clock:
        return cls(started=time.monotonic())

    def now_ms(self) -> int:
        return int((time.monotonic() - self.started) * 1000)


async def _emit(sink: SampleSink | GapSink, value: Sample | Gap) -> None:
    result = sink(value)  # type: ignore[arg-type]
    if asyncio.iscoroutine(result):
        await result


async def collect(
    endpoint: Endpoint,
    transport: Transport,
    *,
    interval: timedelta,
    clock: Clock,
    on_sample: SampleSink,
    on_gap: GapSink,
    groups: list[str] | None = None,
    stop: asyncio.Event | None = None,
) -> None:
    """Collect from one endpoint until `stop` is set or the task is cancelled.

    Runs forever by design: a transport that dies is a gap and a reconnect, not the
    end of the recording. The caller decides when observation is over.
    """
    previous: linux.RawSample | None = None
    previous_ms: int | None = None
    attempt = 0

    while stop is None or not stop.is_set():
        gap_from = clock.now_ms()
        try:
            async for block in transport.stream(interval):
                now_ms = clock.now_ms()
                raw = linux.parse_sample(block)

                elapsed_s = (now_ms - previous_ms) / 1000.0 if previous_ms is not None else 0.0
                # A long pause means the counters span more than one interval, and a
                # rate computed across it would be an average over a hole. Start over.
                if previous_ms is not None and elapsed_s > interval.total_seconds() * LATE_FACTOR:
                    await _emit(on_gap, Gap(endpoint.id, previous_ms, now_ms, "samples stalled"))
                    previous, elapsed_s = None, 0.0

                sample = Sample(
                    target_id=endpoint.id,
                    t_ms=now_ms,
                    metrics=linux.derive(previous, raw, elapsed_s),
                    wall_epoch_s=raw.wall_epoch_s,
                )
                await _emit(on_sample, sample.filtered(groups))

                previous, previous_ms = raw, now_ms
                attempt = 0
                if stop is not None and stop.is_set():
                    return
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001 - any transport failure is a gap
            log.warning("collection from %s failed: %s", endpoint.id, exc)
            reason = str(exc) or type(exc).__name__
            await _emit(on_gap, Gap(endpoint.id, gap_from, clock.now_ms(), reason))
        else:
            # The stream ended without an error, which is still no data.
            await _emit(on_gap, Gap(endpoint.id, gap_from, clock.now_ms(), "stream ended"))

        # Rates cannot span a reconnect: the counters may have been reset underneath.
        previous, previous_ms = None, None

        if stop is not None and stop.is_set():
            return
        delay = BACKOFF[min(attempt, len(BACKOFF) - 1)]
        attempt += 1
        with contextlib.suppress(TimeoutError, asyncio.TimeoutError):
            if stop is not None:
                await asyncio.wait_for(stop.wait(), timeout=delay)
            else:
                await asyncio.sleep(delay)


async def collect_all(
    endpoints: list[Endpoint],
    make_transport: Callable[[Endpoint], Transport],
    *,
    interval: timedelta,
    clock: Clock,
    on_sample: SampleSink,
    on_gap: GapSink,
    groups: list[str] | None = None,
    stop: asyncio.Event | None = None,
) -> None:
    """Collect from every endpoint concurrently.

    One unreachable host does not stop the others: the recording is still worth
    having, and the gap says which target is missing.
    """
    if not endpoints:
        return
    await asyncio.gather(
        *(
            collect(
                endpoint,
                make_transport(endpoint),
                interval=interval,
                clock=clock,
                on_sample=on_sample,
                on_gap=on_gap,
                groups=groups,
                stop=stop,
            )
            for endpoint in endpoints
        )
    )
