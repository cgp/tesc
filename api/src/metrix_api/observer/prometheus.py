"""Scrape transport: node_exporter-style metrics over HTTP.

The alternative to SSH when a host already exposes an exporter. It produces the same
:class:`RawSample` shape, so everything downstream -- rates, gaps, charts, comparison
-- is indifferent to which transport collected a number.

Only the handful of series the normalized shape needs are read. An exporter emits
hundreds; parsing all of them into a dict every second would be work done to throw
away.
"""

from __future__ import annotations

import logging
import math
import re
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from datetime import timedelta

from metrix_api.observer.raw import RawSample
from metrix_api.profiles import Collection

log = logging.getLogger(__name__)

_SAMPLE = re.compile(
    r"^(?P<name>[a-zA-Z_:][a-zA-Z0-9_:]*)"
    r"(?:\{(?P<labels>[^}]*)\})?"
    r"\s+(?P<value>\S+)"
)
_LABEL = re.compile(r'(\w+)="((?:[^"\\]|\\.)*)"')

#: Virtual and duplicate devices, matching the /proc parser's exclusions so the two
#: transports do not disagree about what counts as a disk or a network interface.
_SKIP_DEVICE = re.compile(r"^(loop|ram|dm-|sr)")
_SKIP_IFACE = re.compile(r"^(lo|docker|veth|br-)")

_MEM = {
    "node_memory_MemTotal_bytes": "MemTotal",
    "node_memory_MemAvailable_bytes": "MemAvailable",
    "node_memory_Cached_bytes": "Cached",
    "node_memory_SwapTotal_bytes": "SwapTotal",
    "node_memory_SwapFree_bytes": "SwapFree",
}
_DISK = {
    "node_disk_reads_completed_total": "reads",
    "node_disk_writes_completed_total": "writes",
    "node_disk_read_bytes_total": "read_bytes",
    "node_disk_written_bytes_total": "write_bytes",
}
_NET = {
    "node_network_receive_bytes_total": "rx_bytes",
    "node_network_transmit_bytes_total": "tx_bytes",
    "node_network_receive_drop_total": "rx_drops",
    "node_network_transmit_drop_total": "tx_drops",
}
_LOAD = {"node_load1": 0, "node_load5": 1, "node_load15": 2}


def parse_text(text: str) -> RawSample:
    """Read exporter text into the same counter shape the SSH transport produces."""
    sample = RawSample()
    load: list[float | None] = [None, None, None]

    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        match = _SAMPLE.match(line)
        if not match:
            continue
        name = match.group("name")
        try:
            value = float(match.group("value"))
        except ValueError:
            continue
        if not math.isfinite(value):
            # Exporters write NaN for "this collector failed" and +Inf for an
            # unbounded limit. Neither is a measurement, and float() accepts both
            # happily, so they have to be rejected explicitly.
            continue
        labels = dict(_LABEL.findall(match.group("labels") or ""))

        if name == "node_cpu_seconds_total":
            # Summed across cores, like /proc/stat's aggregate `cpu` line.
            mode = labels.get("mode", "")
            sample.cpu[mode] = sample.cpu.get(mode, 0.0) + value
        elif name in _LOAD:
            load[_LOAD[name]] = value
        elif name in _MEM:
            sample.mem[_MEM[name]] = value
        elif name in _DISK and not _SKIP_DEVICE.match(labels.get("device", "")):
            key = _DISK[name]
            sample.disk[key] = sample.disk.get(key, 0.0) + value
        elif name == "node_disk_io_time_seconds_total" and not _SKIP_DEVICE.match(
            labels.get("device", "")
        ):
            sample.disk["io_ms"] = sample.disk.get("io_ms", 0.0) + value * 1000.0
        elif name in _NET and not _SKIP_IFACE.match(labels.get("device", "")):
            key = _NET[name]
            sample.net[key] = sample.net.get(key, 0.0) + value
        elif name == "node_netstat_Tcp_RetransSegs":
            sample.tcp["RetransSegs"] = value
        elif name == "node_netstat_Tcp_CurrEstab":
            sample.tcp["CurrEstab"] = value
        elif name == "node_netstat_TcpExt_ListenOverflows":
            sample.tcp["ListenOverflows"] = value
        elif name == "node_filefd_allocated":
            sample.fd_open = value
        elif name == "node_procs_running":
            sample.proc_count = value
        elif name == "node_time_seconds":
            sample.wall_epoch_s = value

    if all(v is not None for v in load):
        sample.load = (load[0], load[1], load[2])  # type: ignore[assignment]
    return sample


@dataclass(slots=True)
class ScrapeTransport:
    """Polls an exporter on a fixed interval.

    Unlike SSH there is no long-lived channel: each interval is one request. A single
    failed scrape is one missing sample rather than a dead stream, so transient errors
    are counted and only a run of them gives up and lets the collector record a gap.
    """

    url: str
    timeout: float = 5.0
    #: Consecutive failures tolerated before the stream ends and the collector
    #: records a gap and reconnects.
    max_failures: int = 3
    headers: dict[str, str] = field(default_factory=dict)

    @classmethod
    def from_collection(cls, collection: Collection, *, timeout: float = 5.0) -> ScrapeTransport:
        if collection.host is None:
            raise ValueError("scrape transport needs a host")
        port = collection.port or 9100
        path = collection.path if collection.path.startswith("/") else f"/{collection.path}"
        host = f"[{collection.host}]" if ":" in collection.host else collection.host
        return cls(url=f"http://{host}:{port}{path}", timeout=timeout)

    def describe(self) -> str:
        """Where this will fetch from, for the Config page. See SshTransport."""
        return self.url

    async def stream(self, interval: timedelta) -> AsyncIterator[RawSample]:
        import anyio
        import httpx

        failures = 0
        async with httpx.AsyncClient(timeout=self.timeout, headers=self.headers) as client:
            while True:
                try:
                    response = await client.get(self.url)
                    response.raise_for_status()
                except Exception as exc:  # noqa: BLE001 - any scrape failure is the same here
                    failures += 1
                    log.debug("scrape of %s failed (%s/%s): %s",
                              self.url, failures, self.max_failures, exc)
                    if failures >= self.max_failures:
                        raise
                else:
                    failures = 0
                    yield parse_text(response.text)

                await anyio.sleep(interval.total_seconds())
