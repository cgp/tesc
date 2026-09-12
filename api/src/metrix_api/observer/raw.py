"""Raw host counters, and the arithmetic that turns them into rates.

Every transport parses its own source into a :class:`RawSample` -- `/proc` text over
SSH, Prometheus text over HTTP -- and from there one implementation of :func:`derive`
produces the normalized metrics. That is what keeps "the same metric means the same
thing" true no matter how it was collected.

Counter keys here are already unit-normalized (bytes, milliseconds), so the transport
absorbs the source's quirks and the arithmetic stays about time rather than units.
"""

from __future__ import annotations

from dataclasses import dataclass, field

from metrix_api.observer import metrics as m


@dataclass(slots=True)
class RawSample:
    """Counters as the host reported them, in normalized units.

    Meaningless alone: most are cumulative, and only the difference between two
    samples says anything.
    """

    wall_epoch_s: float | None = None
    #: Per-mode CPU time. Units are arbitrary but must be consistent within a source,
    #: since only the ratio between modes is used.
    cpu: dict[str, float] = field(default_factory=dict)
    load: tuple[float, float, float] | None = None
    #: MemTotal, MemAvailable, Cached, SwapTotal, SwapFree -- in bytes.
    mem: dict[str, float] = field(default_factory=dict)
    #: reads, writes, read_bytes, write_bytes, io_ms.
    disk: dict[str, float] = field(default_factory=dict)
    #: rx_bytes, tx_bytes, rx_drops, tx_drops.
    net: dict[str, float] = field(default_factory=dict)
    #: RetransSegs, CurrEstab, ListenOverflows.
    tcp: dict[str, float] = field(default_factory=dict)
    fd_open: float | None = None
    proc_count: float | None = None


def derive(previous: RawSample | None, current: RawSample, elapsed_s: float) -> dict[str, float]:
    """Turn two raw samples into the normalized metrics.

    The first sample of a recording yields only the gauges: rates need a previous
    value, and inventing one would put a wrong number at t=0 on every chart.
    """
    out: dict[str, float] = {}

    if current.load:
        out[m.LOAD_1M], out[m.LOAD_5M], out[m.LOAD_15M] = current.load
    if current.proc_count is not None:
        out[m.PROC_COUNT] = current.proc_count
    if current.fd_open is not None:
        out[m.FD_OPEN] = current.fd_open

    total = current.mem.get("MemTotal")
    available = current.mem.get("MemAvailable")
    if total:
        out[m.MEM_TOTAL] = total
        if available is not None:
            out[m.MEM_AVAILABLE] = available
            out[m.MEM_USED] = total - available
    if (cached := current.mem.get("Cached")) is not None:
        out[m.MEM_CACHED] = cached
    swap_total = current.mem.get("SwapTotal")
    swap_free = current.mem.get("SwapFree")
    if swap_total is not None and swap_free is not None:
        out[m.SWAP_USED] = swap_total - swap_free

    if (established := current.tcp.get("CurrEstab", current.tcp.get("inuse"))) is not None:
        out[m.CONN_ESTABLISHED] = established

    if previous is None or elapsed_s <= 0:
        return out

    # CPU is cumulative; the percentage is this interval's share of all modes.
    busy = 0.0
    total_ticks = 0.0
    for name, value in current.cpu.items():
        delta = _delta(previous.cpu.get(name), value)
        total_ticks += delta
        if name not in ("idle", "iowait"):
            busy += delta
    if total_ticks > 0:
        scale = 100.0 / total_ticks
        for name, metric in (
            ("user", m.CPU_USER),
            ("system", m.CPU_SYSTEM),
            ("iowait", m.CPU_IOWAIT),
            ("steal", m.CPU_STEAL),
            ("idle", m.CPU_IDLE),
        ):
            if name in current.cpu:
                out[metric] = _delta(previous.cpu.get(name), current.cpu[name]) * scale
        out[m.CPU_BUSY] = busy * scale

    def rate(store: str, key: str) -> float | None:
        before = getattr(previous, store).get(key)
        after = getattr(current, store).get(key)
        if before is None or after is None:
            return None
        return _delta(before, after) / elapsed_s

    for key, metric in (
        ("read_bytes", m.DISK_READ_BPS),
        ("write_bytes", m.DISK_WRITE_BPS),
        ("reads", m.DISK_READS),
        ("writes", m.DISK_WRITES),
        ("rx_bytes", m.NET_RX_BPS),
        ("tx_bytes", m.NET_TX_BPS),
        ("rx_drops", m.NET_RX_DROPS),
        ("tx_drops", m.NET_TX_DROPS),
        ("RetransSegs", m.NET_RETRANSMITS),
        ("ListenOverflows", m.CONN_LISTEN_OVERFLOWS),
    ):
        store = "disk" if key in ("read_bytes", "write_bytes", "reads", "writes") else (
            "net" if key.startswith(("rx_", "tx_")) else "tcp"
        )
        if (value := rate(store, key)) is not None:
            out[metric] = value

    # Time spent doing I/O, as a share of the interval: the closest thing to a
    # saturation signal available without per-queue accounting.
    if (io_ms := rate("disk", "io_ms")) is not None:
        out[m.DISK_IO_BUSY] = min(io_ms / 10.0, 100.0)

    return out


def _delta(before: float | None, after: float) -> float:
    """Counter difference, treating a decrease as a reboot or wrap rather than as a
    negative rate."""
    if before is None or after < before:
        return 0.0
    return after - before
