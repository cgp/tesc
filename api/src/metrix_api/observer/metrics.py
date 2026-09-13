"""The normalized metric shape.

Every transport -- SSH, scrape, container API -- produces this same shape, so charts
and comparisons never care which one was used. Names are dotted and stable: they end
up as strings in the database, in exports, and in chart legends, so renaming one
breaks history.
"""

from __future__ import annotations

from dataclasses import dataclass, field

# CPU, as a percentage of one interval across all cores. iowait and steal are
# separate because they mean different things: iowait is the disk, steal is a
# neighbour, and "CPU is busy" hides both.
CPU_USER = "cpu.user"
CPU_SYSTEM = "cpu.system"
CPU_IOWAIT = "cpu.iowait"
CPU_STEAL = "cpu.steal"
CPU_IDLE = "cpu.idle"
CPU_BUSY = "cpu.busy"

LOAD_1M = "load.1m"
LOAD_5M = "load.5m"
LOAD_15M = "load.15m"

MEM_TOTAL = "mem.total_bytes"
MEM_AVAILABLE = "mem.available_bytes"
MEM_USED = "mem.used_bytes"
MEM_CACHED = "mem.cached_bytes"
SWAP_USED = "swap.used_bytes"

DISK_READ_BPS = "disk.read_bytes_per_s"
DISK_WRITE_BPS = "disk.write_bytes_per_s"
DISK_READS = "disk.reads_per_s"
DISK_WRITES = "disk.writes_per_s"
DISK_IO_BUSY = "disk.io_busy_pct"

NET_RX_BPS = "net.rx_bytes_per_s"
NET_TX_BPS = "net.tx_bytes_per_s"
NET_RX_DROPS = "net.rx_drops_per_s"
NET_TX_DROPS = "net.tx_drops_per_s"
NET_RETRANSMITS = "net.retransmits_per_s"

# Listen-queue overflow is the signal that a service stopped accepting before it
# started failing requests, which reads as latency rather than as errors.
CONN_ESTABLISHED = "conn.established"
CONN_LISTEN_OVERFLOWS = "conn.listen_overflows_per_s"

#: Processes, threads and runnable tasks are three different numbers, and a box
#: with 200 threads has about 2 of them runnable. They were one metric once, filled
#: from a different quantity by each transport -- 100x apart for the same host.
PROC_COUNT = "proc.count"
THREAD_COUNT = "thread.count"
PROC_RUNNING = "proc.running"
FD_OPEN = "fd.open"

#: Groups a profile can ask for, mapped to the metrics they contain. Used to keep a
#: recording from storing series nobody asked for.
GROUPS: dict[str, tuple[str, ...]] = {
    "cpu": (CPU_USER, CPU_SYSTEM, CPU_IOWAIT, CPU_STEAL, CPU_IDLE, CPU_BUSY,
            LOAD_1M, LOAD_5M, LOAD_15M),
    "memory": (MEM_TOTAL, MEM_AVAILABLE, MEM_USED, MEM_CACHED, SWAP_USED),
    "disk": (DISK_READ_BPS, DISK_WRITE_BPS, DISK_READS, DISK_WRITES, DISK_IO_BUSY),
    "net": (NET_RX_BPS, NET_TX_BPS, NET_RX_DROPS, NET_TX_DROPS, NET_RETRANSMITS,
            CONN_ESTABLISHED, CONN_LISTEN_OVERFLOWS),
    "process": (PROC_COUNT, THREAD_COUNT, PROC_RUNNING, FD_OPEN),
}

ALL_METRICS: tuple[str, ...] = tuple(m for group in GROUPS.values() for m in group)


@dataclass(frozen=True, slots=True)
class Sample:
    """One target, one instant, whatever metrics were readable.

    ``t_ms`` is milliseconds since the recording's monotonic start -- never wall
    clock, so load and host series overlay exactly regardless of skew between
    machines. ``wall_epoch_s`` is the remote host's own clock, kept only to measure
    that skew.
    """

    target_id: str
    t_ms: int
    metrics: dict[str, float] = field(default_factory=dict)
    wall_epoch_s: float | None = None

    def filtered(self, groups: list[str] | None) -> Sample:
        """Keep only the requested groups. No groups means everything."""
        if not groups:
            return self
        wanted = {m for g in groups for m in GROUPS.get(g, ())}
        return Sample(
            target_id=self.target_id,
            t_ms=self.t_ms,
            metrics={k: v for k, v in self.metrics.items() if k in wanted},
            wall_epoch_s=self.wall_epoch_s,
        )


@dataclass(frozen=True, slots=True)
class Gap:
    """An interval with no sample. Drawn as a gap, never interpolated.

    A collection failure degrades the recording rather than aborting it, so the
    reason travels with the gap and reaches the chart.
    """

    target_id: str
    from_ms: int
    to_ms: int
    reason: str

#: The code every collection failure is recorded under, so one query finds them all.
COLLECTION_GAP = "collection_gap"

#: A host metric that never came back inside its band during settle, and drifted the
#: bad way. Warn, not invalid: it is the cheapest evidence of a leak there is, and it
#: is evidence rather than proof -- a box legitimately busier after a run than before
#: it looks identical from here.
NOT_RETURNED_TO_BASELINE = "not_returned_to_baseline"

#: A target that produced not one sample for the whole recording. Invalid rather
#: than warn: the box was in the profile, so a reader would count it among what was
#: measured, and a mean over "the environment" that silently omits one of its
#: machines is worse than no number.
TARGET_UNREACHABLE = "target_unreachable"

#: Raised when a refresh at a phase boundary finds the environment a different size
#: (design-api 2.3). Machine-written codes live together so one list answers "what can
#: appear on a recording", whichever subsystem noticed it.
HOST_COUNT_CHANGED = "host_count_changed"


@dataclass(frozen=True, slots=True)
class Annotation:
    """A structured note attached to a recording.

    Shares its shape with the engine's annotation record (see
    schema/events.schema.json), so host-side and load-side notes read as one list.
    """

    code: str
    severity: str
    from_ms: int
    message: str
    to_ms: int | None = None
    target_id: str | None = None
    phase: str | None = None
    detail: dict[str, object] = field(default_factory=dict)
    source: str = "detector"

    @classmethod
    def from_gap(cls, gap: Gap) -> Annotation:
        """A gap is warn, not invalid: the recording is still worth having, and the
        chart draws a hole rather than a line through it."""
        seconds = max(0, gap.to_ms - gap.from_ms) / 1000.0
        return cls(
            code=COLLECTION_GAP,
            severity="warn",
            from_ms=gap.from_ms,
            to_ms=gap.to_ms,
            target_id=gap.target_id,
            message=(
                f"no samples from {gap.target_id} for {seconds:.1f}s: {gap.reason}. "
                "The interval is drawn as a gap, never interpolated."
            ),
            detail={"reason": gap.reason, "duration_s": round(seconds, 3)},
        )
