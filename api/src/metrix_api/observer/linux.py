"""Reading a Linux host's own accounting, and turning counters into rates.

The remote side is deliberately dumb: a shell loop that prints `/proc` files with
markers between them. All parsing and arithmetic happens here, which means the
interesting part is testable against captured text with no host involved -- and
nothing has to be installed on the target to get value on day one.
"""

from __future__ import annotations

import re

from metrix_api.observer.facts import HostFacts, parse_df, parse_identity
from metrix_api.observer.raw import RawSample

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
  grep -E '^(cpu |intr |procs_running )' /proc/stat
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
  echo "--procs"
  set -- /proc/[0-9]*
  [ -e "$1" ] && echo $# || echo 0
  echo "--end"
  sleep {interval}
done
"""

#: Run once at the start and again at the end of a recording, not in the loop.
#:
#: `.` rather than `source`, `uname` rather than /proc/version, and `df -Pk` rather
#: than `df -B1 --output`: everything here is POSIX, because the boxes worth
#: measuring include the ones with busybox on them.
PROBE_SCRIPT = r"""
PRETTY_NAME=""
[ -r /etc/os-release ] && . /etc/os-release
echo "hostname=$(uname -n)"
echo "os=${PRETTY_NAME:-$(uname -s)}"
echo "kernel=$(uname -sr)"
echo "arch=$(uname -m)"
echo "cpus=$(grep -c '^processor' /proc/cpuinfo 2>/dev/null || echo 0)"
echo "--filesystems"
df -Pk 2>/dev/null
"""

#: Splits the probe's two halves. The identity lines come first because a `df` that
#: fails should not cost us the identity.
PROBE_SEPARATOR = "--filesystems"

#: Partitions and virtual devices double-count the physical device beneath them.
_REAL_DISK = re.compile(r"^(sd[a-z]+|nvme\d+n\d+|vd[a-z]+|xvd[a-z]+)$")
#: Loopback traffic is not network traffic for any question worth asking.
_SKIP_IFACE = re.compile(r"^(lo|docker\d+|veth|br-)")

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


def parse_probe(text: str) -> HostFacts:
    """Split the probe output into identity and filesystems."""
    identity_text, _, df_text = text.partition(PROBE_SEPARATOR)
    return HostFacts(
        identity=parse_identity(identity_text),
        filesystems=parse_df(df_text),
    )


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
    if not fields:
        return
    # Runnable tasks, not a total: on an idle box with hundreds of threads this is
    # about 1. It is a different question from "how many processes exist".
    if fields[0] == "procs_running" and len(fields) > 1:
        sample.proc_running = float(fields[1])
        return
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
        # "runnable/total", and `total` counts kernel scheduling entities --
        # processes *and* threads. It was read as a process count for a while,
        # which made it ~100x the number the scrape transport reported for the
        # same host under the same metric name.
        if "/" in fields[3]:
            sample.thread_count = float(fields[3].split("/")[1])


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
    # Sectors are 512 bytes by convention in /proc/diskstats regardless of the
    # device's real sector size. Converting here keeps derive() free of units.
    sample.disk["reads"] = sample.disk.get("reads", 0.0) + float(fields[3])
    sample.disk["read_bytes"] = sample.disk.get("read_bytes", 0.0) + float(fields[5]) * 512.0
    sample.disk["writes"] = sample.disk.get("writes", 0.0) + float(fields[7])
    sample.disk["write_bytes"] = sample.disk.get("write_bytes", 0.0) + float(fields[9]) * 512.0
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


def _procs(sample: RawSample, line: str) -> None:
    """The count of numeric directories in /proc: one entry per process.

    Counted by glob in the remote shell rather than by piping `ls` into `wc`, which
    would be two more processes per second on a box being measured.
    """
    text = line.strip()
    if text.isdigit():
        sample.proc_count = float(text)


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
    "procs": _procs,
}
