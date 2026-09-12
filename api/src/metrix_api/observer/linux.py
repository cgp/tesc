"""Reading a Linux host's own accounting, and turning counters into rates.

The remote side is deliberately dumb: a shell loop that prints `/proc` files with
markers between them. All parsing and arithmetic happens here, which means the
interesting part is testable against captured text with no host involved -- and
nothing has to be installed on the target to get value on day one.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field

from metrix_api.observer import metrics as m

#: Marks the start of a sample block; the number is the remote host's epoch seconds.
SAMPLE_MARKER = "===metrix"
_SECTION = re.compile(r"^--(\w+)$")

#: A POSIX shell loop, no installation required. One long-lived channel rather than
#: one SSH exec per second: per-sample process spawn would dominate at 1s and the
#: collector would be measuring its own overhead.
#:
#: `{interval}` is substituted by the transport.
REMOTE_SCRIPT = r"""
MEM='^(MemTotal|MemFree|MemAvailable|Buffers|Cached|SwapTotal|SwapFree):'
while :; do
  echo "===metrix $(date +%s)"
  echo "--stat"
  grep -E '^(cpu |intr )' /proc/stat
  echo "--loadavg"
  cat /proc/loadavg
  echo "--meminfo"
  grep -E "$MEM" /proc/meminfo
  echo "--diskstats"
  cat /proc/diskstats
  echo "--netdev"
  cat /proc/net/dev
  echo "--snmp"
  grep -E '^Tcp:' /proc/net/snmp
  echo "--netstat"
  grep -E '^TcpExt:' /proc/net/netstat
  echo "--filenr"
  cat /proc/sys/fs/file-nr
  echo "--sockstat"
  grep -E '^TCP:' /proc/net/sockstat
  echo "--end"
  sleep {interval}
done
"""

#: Partitions and virtual devices double-count the physical device beneath them.
_REAL_DISK = re.compile(r"^(sd[a-z]+|nvme\d+n\d+|vd[a-z]+|xvd[a-z]+)$")
#: Loopback traffic is not network traffic for any question worth asking.
_SKIP_IFACE = re.compile(r"^(lo|docker\d+|veth|br-)")

_USER_HZ = 100.0


@dataclass(slots=True)
class RawSample:
    """Counters exactly as the host reported them. Meaningless alone: most are
    cumulative, and only the difference between two samples says anything."""

    wall_epoch_s: float | None = None
    cpu: dict[str, float] = field(default_factory=dict)
    load: tuple[float, float, float] | None = None
    mem: dict[str, float] = field(default_factory=dict)
    disk: dict[str, float] = field(default_factory=dict)
    net: dict[str, float] = field(default_factory=dict)
    tcp: dict[str, float] = field(default_factory=dict)
    fd_open: float | None = None
    proc_count: float | None = None


def split_blocks(text: str) -> list[str]:
    """Split a stream into complete sample blocks, discarding a partial tail.

    A block only counts once its `--end` marker has arrived: half a sample parsed as
    a whole one would produce a plausible wrong number, which is worse than a gap.
    """
    blocks: list[str] = []
    current: list[str] | None = None
    for line in text.splitlines():
        if line.startswith(SAMPLE_MARKER):
            current = [line]
        elif current is not None:
            current.append(line)
            if line.strip() == "--end":
                blocks.append("\n".join(current))
                current = None
    return blocks


def parse_sample(block: str) -> RawSample:
    """Read one block of `/proc` text into raw counters."""
    sample = RawSample()
    section = ""

    for line in block.splitlines():
        if line.startswith(SAMPLE_MARKER):
            parts = line.split()
            if len(parts) > 1:
                try:
                    sample.wall_epoch_s = float(parts[1])
                except ValueError:
                    sample.wall_epoch_s = None
            continue
        if match := _SECTION.match(line.strip()):
            section = match.group(1)
            continue
        if not line.strip():
            continue

        handler = _HANDLERS.get(section)
        if handler is not None:
            handler(sample, line)

    return sample


def _stat(sample: RawSample, line: str) -> None:
    fields = line.split()
    if fields[0] != "cpu":
        return
    # user nice system idle iowait irq softirq steal
    names = ("user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal")
    for name, value in zip(names, fields[1:9], strict=False):
        sample.cpu[name] = float(value)


def _loadavg(sample: RawSample, line: str) -> None:
    fields = line.split()
    if len(fields) >= 4:
        sample.load = (float(fields[0]), float(fields[1]), float(fields[2]))
        # "running/total" -- total is the process count.
        if "/" in fields[3]:
            sample.proc_count = float(fields[3].split("/")[1])


def _meminfo(sample: RawSample, line: str) -> None:
    key, _, rest = line.partition(":")
    fields = rest.split()
    if fields:
        # /proc/meminfo is in kB.
        sample.mem[key.strip()] = float(fields[0]) * 1024.0


def _diskstats(sample: RawSample, line: str) -> None:
    fields = line.split()
    if len(fields) < 14 or not _REAL_DISK.match(fields[2]):
        return
    sample.disk["reads"] = sample.disk.get("reads", 0.0) + float(fields[3])
    sample.disk["read_sectors"] = sample.disk.get("read_sectors", 0.0) + float(fields[5])
    sample.disk["writes"] = sample.disk.get("writes", 0.0) + float(fields[7])
    sample.disk["write_sectors"] = sample.disk.get("write_sectors", 0.0) + float(fields[9])
    # Milliseconds spent doing I/O: the closest thing to a saturation signal here.
    sample.disk["io_ms"] = sample.disk.get("io_ms", 0.0) + float(fields[12])


def _netdev(sample: RawSample, line: str) -> None:
    name, _, rest = line.partition(":")
    name = name.strip()
    if not rest or _SKIP_IFACE.match(name):
        return
    fields = rest.split()
    if len(fields) < 16:
        return
    sample.net["rx_bytes"] = sample.net.get("rx_bytes", 0.0) + float(fields[0])
    sample.net["rx_drops"] = sample.net.get("rx_drops", 0.0) + float(fields[3])
    sample.net["tx_bytes"] = sample.net.get("tx_bytes", 0.0) + float(fields[8])
    sample.net["tx_drops"] = sample.net.get("tx_drops", 0.0) + float(fields[11])


def _snmp(sample: RawSample, line: str) -> None:
    _header_or_values(sample, line, "Tcp:", ("RetransSegs", "CurrEstab"))


def _netstat(sample: RawSample, line: str) -> None:
    _header_or_values(sample, line, "TcpExt:", ("ListenOverflows", "ListenDrops"))


def _header_or_values(sample: RawSample, line: str, prefix: str, wanted: tuple[str, ...]) -> None:
    """/proc/net/snmp alternates a header line and a values line."""
    fields = line.split()
    if not fields or fields[0] != prefix:
        return
    if fields[1].isalpha() and not fields[1].isdigit():
        try:
            int(fields[1])
        except ValueError:
            sample.tcp.setdefault("_headers", {}).setdefault(prefix, fields[1:])  # type: ignore[arg-type]
            return
    headers = sample.tcp.get("_headers", {}).get(prefix)  # type: ignore[union-attr]
    if not headers:
        return
    for name, value in zip(headers, fields[1:], strict=False):
        if name in wanted:
            sample.tcp[name] = float(value)


def _filenr(sample: RawSample, line: str) -> None:
    fields = line.split()
    if fields:
        # allocated, free, max -- allocated minus free is what is actually held.
        allocated = float(fields[0])
        free = float(fields[1]) if len(fields) > 1 else 0.0
        sample.fd_open = allocated - free


def _sockstat(sample: RawSample, line: str) -> None:
    fields = line.split()
    if fields and fields[0] == "TCP:" and "inuse" in fields:
        sample.tcp["inuse"] = float(fields[fields.index("inuse") + 1])


_HANDLERS = {
    "stat": _stat,
    "loadavg": _loadavg,
    "meminfo": _meminfo,
    "diskstats": _diskstats,
    "netdev": _netdev,
    "snmp": _snmp,
    "netstat": _netstat,
    "filenr": _filenr,
    "sockstat": _sockstat,
}


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

    # CPU is a cumulative tick counter; the percentage is this interval's share.
    busy_ticks = 0.0
    total_ticks = 0.0
    for name, value in current.cpu.items():
        delta = _delta(previous.cpu.get(name), value)
        total_ticks += delta
        if name not in ("idle", "iowait"):
            busy_ticks += delta
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
        out[m.CPU_BUSY] = busy_ticks * scale

    def rate(store: str, key: str) -> float | None:
        before = getattr(previous, store).get(key)
        after = getattr(current, store).get(key)
        if before is None or after is None:
            return None
        return _delta(before, after) / elapsed_s

    # Sectors are 512 bytes by convention in /proc/diskstats, regardless of the
    # device's real sector size.
    for key, metric, factor in (
        ("read_sectors", m.DISK_READ_BPS, 512.0),
        ("write_sectors", m.DISK_WRITE_BPS, 512.0),
        ("reads", m.DISK_READS, 1.0),
        ("writes", m.DISK_WRITES, 1.0),
    ):
        if (value := rate("disk", key)) is not None:
            out[metric] = value * factor
    if (io_ms := rate("disk", "io_ms")) is not None:
        out[m.DISK_IO_BUSY] = min(io_ms / 10.0, 100.0)

    for key, metric in (
        ("rx_bytes", m.NET_RX_BPS),
        ("tx_bytes", m.NET_TX_BPS),
        ("rx_drops", m.NET_RX_DROPS),
        ("tx_drops", m.NET_TX_DROPS),
    ):
        if (value := rate("net", key)) is not None:
            out[metric] = value

    for key, metric in (
        ("RetransSegs", m.NET_RETRANSMITS),
        ("ListenOverflows", m.CONN_LISTEN_OVERFLOWS),
    ):
        if (value := rate("tcp", key)) is not None:
            out[metric] = value

    return out


def _delta(before: float | None, after: float) -> float:
    """Counter difference, treating a decrease as a reboot or wrap rather than as a
    negative rate."""
    if before is None or after < before:
        return 0.0
    return after - before
