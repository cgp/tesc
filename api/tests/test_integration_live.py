"""Collection against a real host.

Everything else in the observer suite runs against captured text, which proves the
arithmetic but not the assumptions: that the remote shell accepts the script, that
`/proc` looks the way this distribution writes it, that a real exporter names the
series we expect. Those only fail against a real machine.

Skipped unless `api/tests/integration.toml` exists (see `integration.sample.toml`).
That file is gitignored: someone's LAN address is not a repository fact.

Authentication uses the native SSH setup -- `~/.ssh/config` and default keys -- so if
`ssh <host>` works from a shell, this works.
"""

from __future__ import annotations

import asyncio
import tomllib
from datetime import timedelta
from pathlib import Path

import pytest

from metrix_api.observer import metrics as m
from metrix_api.observer.collector import Clock, collect
from metrix_api.observer.metrics import Gap, Sample
from metrix_api.observer.prometheus import ScrapeTransport
from metrix_api.observer.ssh import SshTransport
from metrix_api.profiles import Collection, Endpoint

CONFIG = Path(__file__).parent / "integration.toml"
pytestmark = pytest.mark.integration


def config() -> dict:
    if not CONFIG.is_file():
        pytest.skip(f"no {CONFIG.name}; copy integration.sample.toml to enable live tests")
    return tomllib.loads(CONFIG.read_text(encoding="utf-8"))


def ssh_config() -> dict:
    section = config().get("ssh")
    if not section or not section.get("host"):
        pytest.skip("integration.toml has no [ssh] host")
    return section


async def gather(transport, *, endpoint: Endpoint, count: int, timeout: float = 30.0):
    """Collect `count` samples, or fail with whatever went wrong instead."""
    samples: list[Sample] = []
    gaps: list[Gap] = []
    stop = asyncio.Event()

    async def on_sample(sample: Sample) -> None:
        samples.append(sample)
        if len(samples) >= count:
            stop.set()

    task = asyncio.create_task(
        collect(
            endpoint,
            transport,
            interval=timedelta(seconds=1),
            clock=Clock.start(),
            on_sample=on_sample,
            on_gap=gaps.append,
            stop=stop,
        )
    )
    try:
        await asyncio.wait_for(stop.wait(), timeout=timeout)
    except TimeoutError:
        reasons = "; ".join(g.reason for g in gaps) or "no samples and no error"
        pytest.fail(f"collected {len(samples)}/{count} samples in {timeout}s: {reasons}")
    finally:
        task.cancel()
    return samples, gaps


@pytest.mark.asyncio
class TestSsh:
    async def test_a_real_host_produces_real_samples(self) -> None:
        """The one that finds out whether any of this works outside a fixture."""
        section = ssh_config()
        transport = SshTransport(
            host=section["host"],
            port=int(section.get("port", 22)),
            user=section.get("user"),
        )
        endpoint = Endpoint(id="live", address=f"{section['host']}:22")

        samples, gaps = await gather(transport, endpoint=endpoint, count=3)

        assert not gaps, f"collection reported gaps: {[g.reason for g in gaps]}"
        assert len(samples) >= 3

        # First sample: gauges only, since a rate needs a predecessor.
        first = samples[0].metrics
        assert first[m.MEM_TOTAL] > 0, "a host with no memory is a parsing failure"
        assert m.LOAD_1M in first
        assert m.CPU_USER not in first

        # Later samples carry rates, and they have to be physically possible.
        later = samples[-1].metrics
        assert 0.0 <= later[m.CPU_BUSY] <= 100.0
        assert 0.0 <= later[m.CPU_IDLE] <= 100.0
        total = sum(
            later.get(k, 0.0)
            for k in (m.CPU_USER, m.CPU_SYSTEM, m.CPU_IOWAIT, m.CPU_STEAL, m.CPU_IDLE)
        )
        assert total == pytest.approx(100.0, abs=5.0), f"CPU modes sum to {total}, not ~100"

        for name, value in later.items():
            assert value >= 0.0, f"{name} is negative: {value}"

        assert later[m.MEM_USED] <= later[m.MEM_TOTAL]

    async def test_the_metrics_this_distribution_actually_yields(self) -> None:
        """Records what a real host gives us, and fails loudly if a group is empty.

        A metric group that silently produces nothing is an empty chart later, and
        the fixtures cannot detect it -- they contain whatever I wrote.
        """
        section = ssh_config()
        transport = SshTransport(
            host=section["host"], port=int(section.get("port", 22)), user=section.get("user")
        )
        endpoint = Endpoint(id="live", address=f"{section['host']}:22")

        samples, _ = await gather(transport, endpoint=endpoint, count=3)
        produced = set(samples[-1].metrics)

        missing_by_group = {
            group: sorted(set(names) - produced)
            for group, names in m.GROUPS.items()
            if set(names) - produced
        }
        assert not missing_by_group, (
            "this host produced no value for: "
            + "; ".join(f"{g}: {', '.join(names)}" for g, names in missing_by_group.items())
        )

    async def test_the_clocks_are_close_enough_to_align_series(self) -> None:
        """Skew is measured rather than assumed; series alignment depends on it."""
        import time

        section = ssh_config()
        transport = SshTransport(
            host=section["host"], port=int(section.get("port", 22)), user=section.get("user")
        )
        endpoint = Endpoint(id="live", address=f"{section['host']}:22")

        samples, _ = await gather(transport, endpoint=endpoint, count=2)
        remote = samples[-1].wall_epoch_s
        assert remote is not None, "the remote clock is needed to measure skew"

        skew = abs(time.time() - remote)
        assert skew < 120, f"clocks differ by {skew:.0f}s; host and load series will not align"


@pytest.mark.asyncio
class TestScrape:
    async def test_an_exporter_on_the_same_host(self) -> None:
        section = config().get("scrape")
        if not section or not section.get("host"):
            pytest.skip("integration.toml has no [scrape] host")

        transport = ScrapeTransport.from_collection(
            Collection(
                transport="scrape",
                host=section["host"],
                port=int(section.get("port", 9100)),
                path=section.get("path", "/metrics"),
            )
        )
        endpoint = Endpoint(id="live-scrape", address=f"{section['host']}:9100")

        samples, gaps = await gather(transport, endpoint=endpoint, count=3)

        assert not gaps, f"scrape reported gaps: {[g.reason for g in gaps]}"
        assert samples[0].metrics[m.MEM_TOTAL] > 0
        assert 0.0 <= samples[-1].metrics[m.CPU_BUSY] <= 100.0
