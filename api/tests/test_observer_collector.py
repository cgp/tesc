"""Collector behavior: sampling, gaps, and surviving a transport that dies.

Driven by a fake transport, so the failure modes that matter -- a host going away
mid-recording, a stalled stream -- are tested deterministically rather than hoped for.
"""

from __future__ import annotations

import asyncio
from datetime import timedelta

import pytest

from metrix_api.observer import metrics as m
from metrix_api.observer.collector import Clock, collect, collect_all
from metrix_api.observer.linux import parse_sample
from metrix_api.observer.metrics import Gap, Sample
from metrix_api.profiles import Endpoint
from tests.test_observer_linux import block

pytestmark = pytest.mark.asyncio

INTERVAL = timedelta(seconds=1)


class FakeClock(Clock):
    """Advances only when the test says so."""

    def __init__(self) -> None:
        self.t = 0

    def now_ms(self) -> int:
        return self.t


class FakeTransport:
    """Yields the given blocks, then does whatever `then` says."""

    def __init__(self, blocks: list[str], *, then: str = "end", clock: FakeClock | None = None,
                 step_ms: int = 1000) -> None:
        self.blocks = blocks
        self.then = then
        self.clock = clock
        self.step_ms = step_ms
        self.streams = 0

    async def stream(self, interval: timedelta):
        self.streams += 1
        for b in self.blocks:
            if self.clock is not None:
                self.clock.t += self.step_ms
            yield parse_sample(b)
        if self.then == "raise":
            raise ConnectionResetError("host went away")
        if self.then == "hang":
            await asyncio.sleep(3600)


def endpoint() -> Endpoint:
    return Endpoint(id="host-a", address="10.0.3.41:8080")


# A "hang" transport never returns, so the collector is expected to be cancelled
# rather than to finish. Keep the wait short: the assertions are about what was
# already emitted, not about shutdown.
async def run_collect(transport, *, clock, groups=None, timeout=0.2):
    samples: list[Sample] = []
    gaps: list[Gap] = []
    stop = asyncio.Event()

    task = asyncio.create_task(
        collect(
            endpoint(),
            transport,
            interval=INTERVAL,
            clock=clock,
            on_sample=samples.append,
            on_gap=gaps.append,
            groups=groups,
            stop=stop,
        )
    )
    # Let the transport drain, then ask the collector to finish.
    await asyncio.sleep(0.05)
    stop.set()
    with suppress_timeout():
        await asyncio.wait_for(task, timeout=timeout)
    task.cancel()
    return samples, gaps


class suppress_timeout:
    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        return exc_type is not None and issubclass(exc_type, (TimeoutError, asyncio.TimeoutError))


class TestSampling:
    async def test_samples_carry_the_run_clock_not_the_hosts(self) -> None:
        clock = FakeClock()
        transport = FakeTransport(
            [block(cpu_user=100, cpu_idle=900), block(cpu_user=200, cpu_idle=1800)],
            then="hang",
            clock=clock,
        )
        samples, _ = await run_collect(transport, clock=clock)

        assert [s.t_ms for s in samples] == [1000, 2000]
        # The host's own clock is kept, but only for measuring skew.
        assert samples[0].wall_epoch_s == 1789000000

    async def test_the_first_sample_has_gauges_and_the_second_has_rates(self) -> None:
        clock = FakeClock()
        transport = FakeTransport(
            [block(cpu_user=100, cpu_idle=900), block(cpu_user=200, cpu_idle=1800)],
            then="hang",
            clock=clock,
        )
        samples, _ = await run_collect(transport, clock=clock)

        assert m.CPU_USER not in samples[0].metrics
        assert m.LOAD_1M in samples[0].metrics
        assert samples[1].metrics[m.CPU_USER] == pytest.approx(10.0)

    async def test_groups_limit_what_is_stored(self) -> None:
        clock = FakeClock()
        transport = FakeTransport(
            [block(cpu_user=100, cpu_idle=900), block(cpu_user=200, cpu_idle=1800)],
            then="hang",
            clock=clock,
        )
        samples, _ = await run_collect(transport, clock=clock, groups=["cpu"])

        assert m.CPU_USER in samples[1].metrics
        assert m.MEM_USED not in samples[1].metrics, "memory was not requested"


class TestFailure:
    async def test_a_dead_transport_becomes_a_gap_and_a_retry(self) -> None:
        clock = FakeClock()
        transport = FakeTransport([block(cpu_user=1, cpu_idle=1)], then="raise", clock=clock)
        samples, gaps = await run_collect(transport, clock=clock)

        assert len(samples) >= 1
        assert gaps, "a transport failure must be recorded, not swallowed"
        assert "host went away" in gaps[0].reason
        assert gaps[0].target_id == "host-a"

    async def test_a_stream_that_simply_ends_is_still_a_gap(self) -> None:
        clock = FakeClock()
        transport = FakeTransport([block(cpu_user=1, cpu_idle=1)], then="end", clock=clock)
        _, gaps = await run_collect(transport, clock=clock)
        assert gaps and gaps[0].reason == "stream ended"

    async def test_a_stalled_stream_does_not_average_a_rate_across_the_hole(self) -> None:
        """A rate computed over a 30s stall would be an average over missing time."""
        clock = FakeClock()
        transport = FakeTransport(
            [block(cpu_user=100, cpu_idle=900), block(cpu_user=200, cpu_idle=1800)],
            then="hang",
            clock=clock,
            step_ms=30_000,
        )
        samples, gaps = await run_collect(transport, clock=clock)

        assert any(g.reason == "samples stalled" for g in gaps)
        assert m.CPU_USER not in samples[1].metrics, "rates must restart after a stall"


class TestMultipleHosts:
    async def test_one_unreachable_host_does_not_stop_the_others(self) -> None:
        clock = FakeClock()
        good = Endpoint(id="good", address="10.0.0.1:80")
        bad = Endpoint(id="bad", address="10.0.0.2:80")

        transports = {
            "good": FakeTransport([block(cpu_user=1, cpu_idle=1)], then="hang", clock=clock),
            "bad": FakeTransport([], then="raise"),
        }

        samples: list[Sample] = []
        gaps: list[Gap] = []
        stop = asyncio.Event()
        task = asyncio.create_task(
            collect_all(
                [good, bad],
                lambda e: transports[e.id],
                interval=INTERVAL,
                clock=clock,
                on_sample=samples.append,
                on_gap=gaps.append,
                stop=stop,
            )
        )
        await asyncio.sleep(0.05)
        stop.set()
        with suppress_timeout():
            await asyncio.wait_for(task, timeout=0.2)
        task.cancel()

        assert any(s.target_id == "good" for s in samples), "the reachable host was still recorded"
        assert any(g.target_id == "bad" for g in gaps), "the unreachable host is named in a gap"

    async def test_no_endpoints_is_not_an_error(self) -> None:
        await collect_all(
            [], lambda e: FakeTransport([]), interval=INTERVAL, clock=FakeClock(),
            on_sample=lambda s: None, on_gap=lambda g: None,
        )


async def test_an_async_sink_is_awaited() -> None:
    clock = FakeClock()
    received: list[Sample] = []

    async def sink(sample: Sample) -> None:
        await asyncio.sleep(0)
        received.append(sample)

    transport = FakeTransport([block(cpu_user=1, cpu_idle=1)], then="hang", clock=clock)
    stop = asyncio.Event()
    task = asyncio.create_task(
        collect(
            endpoint(), transport, interval=INTERVAL, clock=clock,
            on_sample=sink, on_gap=lambda g: None, stop=stop,
        )
    )
    await asyncio.sleep(0.05)
    stop.set()
    with suppress_timeout():
        await asyncio.wait_for(task, timeout=0.2)
    task.cancel()
    assert received
