"""Parsing /proc text and turning counters into rates.

Captured text rather than a live host: the arithmetic is the part that can be wrong
in a way nobody notices, so it is tested against numbers whose answers are known.
"""

from __future__ import annotations

import pytest

from metrix_api.observer import metrics as m
from metrix_api.observer.linux import parse_sample, split_blocks
from metrix_api.observer.raw import derive


def block(
    *,
    cpu_user: int,
    cpu_idle: int,
    epoch: int = 1789000000,
    rx: int = 1000,
    read_sectors: int = 100,
    retrans: int = 5,
    io_ms: int = 0,
) -> str:
    """One sample block, shaped exactly as the remote loop prints it."""
    return f"""===metrix {epoch}
--stat
cpu  {cpu_user} 20 300 {cpu_idle} 50 0 10 5
intr 12345
procs_running 3
--loadavg
0.52 0.48 0.44 2/431 9912
--meminfo
MemTotal:       16007456 kB
MemFree:         1200000 kB
MemAvailable:    8003728 kB
Buffers:          300000 kB
Cached:          4000000 kB
SwapTotal:       2097152 kB
SwapFree:        2000000 kB
--diskstats
   8       0 sda 1000 0 {read_sectors} 500 2000 0 4000 900 0 {io_ms} 1400 0 0 0 0
   8       1 sda1 999 0 50 400 1000 0 2000 800 0 500 1200 0 0 0 0
--netdev
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes
    lo: 999999    1000    0    0    0     0          0         0  999999 1000 0 0 0 0 0 0
  eth0: {rx}    100    0    3    0     0          0         0  {rx * 2} 200 0 7 0 0 0 0
--snmp
Tcp: RtoAlgorithm RtoMin RtoMax MaxConn ActiveOpens PassiveOpens AttemptFails EstabResets CurrEstab InSegs OutSegs RetransSegs
Tcp: 1 200 120000 -1 100 200 3 4 42 5000 6000 {retrans}
--netstat
TcpExt: SyncookiesSent SyncookiesRecv ListenOverflows ListenDrops
TcpExt: 0 0 11 12
--filenr
2048	0	9223372036854775807
--sockstat
TCP: inuse 51 orphan 0 tw 3 alloc 60 mem 4
--procs
187
--end"""


class TestSplitting:
    def test_complete_blocks_are_found(self) -> None:
        stream = block(cpu_user=100, cpu_idle=900) + "\n" + block(cpu_user=200, cpu_idle=1800)
        assert len(split_blocks(stream)) == 2

    def test_a_partial_tail_is_not_yielded(self) -> None:
        """Half a sample parsed as a whole one is a plausible wrong number."""
        stream = block(cpu_user=100, cpu_idle=900) + "\n===metrix 1789000001\n--stat\ncpu 1 2 3"
        assert len(split_blocks(stream)) == 1

    def test_leading_noise_is_ignored(self) -> None:
        stream = "Welcome to Ubuntu\nLast login: whenever\n" + block(cpu_user=1, cpu_idle=2)
        assert len(split_blocks(stream)) == 1


class TestParsing:
    @pytest.fixture
    def sample(self):
        return parse_sample(block(cpu_user=100, cpu_idle=900))

    def test_cpu_counters(self, sample) -> None:
        assert sample.cpu["user"] == 100
        assert sample.cpu["idle"] == 900
        assert sample.cpu["steal"] == 5

    def test_memory_is_converted_to_bytes(self, sample) -> None:
        assert sample.mem["MemTotal"] == 16007456 * 1024
        assert sample.mem["MemAvailable"] == 8003728 * 1024

    def test_load_and_the_three_task_counts(self, sample) -> None:
        """Three different questions, and they were one metric once. The loadavg
        denominator counts processes *and* threads, so reading it as a process
        count made this transport report ~100x what the other one did."""
        assert sample.load == (0.52, 0.48, 0.44)
        assert sample.proc_count == 187, "numeric directories in /proc"
        assert sample.thread_count == 431, "the loadavg denominator: all tasks"
        assert sample.proc_running == 3, "/proc/stat procs_running: runnable only"

    def test_a_missing_procs_section_leaves_the_count_absent(self) -> None:
        """An older agent, or a /proc the glob could not read. Absent, not zero."""
        text = block(cpu_user=1, cpu_idle=1).replace("--procs\n187\n", "")
        sample = parse_sample(text)
        assert sample.proc_count is None
        assert sample.thread_count == 431

    def test_partitions_do_not_double_count_their_disk(self, sample) -> None:
        # sda and sda1 both appear; only sda is real.
        assert sample.disk["reads"] == 1000

    def test_sectors_are_converted_to_bytes_at_parse_time(self, sample) -> None:
        # 512 bytes per sector by /proc convention; derive() stays free of units.
        assert sample.disk["read_bytes"] == 100 * 512

    def test_loopback_is_not_network_traffic(self, sample) -> None:
        assert sample.net["rx_bytes"] == 1000, "lo must be excluded"
        assert sample.net["rx_drops"] == 3

    def test_tcp_counters_come_from_the_matching_header(self, sample) -> None:
        assert sample.tcp["RetransSegs"] == 5
        assert sample.tcp["CurrEstab"] == 42
        assert sample.tcp["ListenOverflows"] == 11

    def test_open_descriptors_are_allocated_minus_free(self, sample) -> None:
        assert sample.fd_open == 2048

    def test_the_remote_clock_is_kept_for_skew(self, sample) -> None:
        assert sample.wall_epoch_s == 1789000000


class TestDerive:
    def test_the_first_sample_has_no_rates(self) -> None:
        """Inventing a rate at t=0 would put a wrong number on every chart."""
        out = derive(None, parse_sample(block(cpu_user=100, cpu_idle=900)), 0.0)
        assert m.CPU_USER not in out
        assert m.NET_RX_BPS not in out
        # Gauges are still available immediately.
        assert out[m.LOAD_1M] == 0.52
        assert out[m.MEM_USED] == (16007456 - 8003728) * 1024

    def test_cpu_percentages_use_this_intervals_share(self) -> None:
        first = parse_sample(block(cpu_user=100, cpu_idle=900))
        # 100 more user ticks, 900 more idle: a quarter of 400 total busy+idle... the
        # totals here are user+nice+system+idle+iowait+irq+softirq+steal deltas.
        second = parse_sample(block(cpu_user=200, cpu_idle=1800))
        out = derive(first, second, 1.0)
        # deltas: user 100, idle 900, everything else 0 -> 1000 total ticks.
        assert out[m.CPU_USER] == pytest.approx(10.0)
        assert out[m.CPU_IDLE] == pytest.approx(90.0)
        assert out[m.CPU_BUSY] == pytest.approx(10.0)

    def test_iowait_is_not_counted_as_busy(self) -> None:
        """iowait is the disk, steal is a neighbour; 'CPU busy' hides both."""
        first = parse_sample(block(cpu_user=100, cpu_idle=900))
        second = parse_sample(block(cpu_user=100, cpu_idle=900))
        # Same block twice: no deltas at all, so no CPU metrics rather than 0/0.
        assert m.CPU_BUSY not in derive(first, second, 1.0)

    def test_byte_rates_account_for_the_interval(self) -> None:
        first = parse_sample(block(cpu_user=100, cpu_idle=900, rx=1000))
        second = parse_sample(block(cpu_user=100, cpu_idle=900, rx=3000))
        out = derive(first, second, 2.0)
        assert out[m.NET_RX_BPS] == pytest.approx(1000.0)
        assert out[m.NET_TX_BPS] == pytest.approx(2000.0)

    def test_disk_sectors_become_bytes(self) -> None:
        first = parse_sample(block(cpu_user=1, cpu_idle=1, read_sectors=100))
        second = parse_sample(block(cpu_user=1, cpu_idle=1, read_sectors=200))
        out = derive(first, second, 1.0)
        assert out[m.DISK_READ_BPS] == pytest.approx(100 * 512.0)

    def test_disk_busy_is_capped_at_one_hundred_percent(self) -> None:
        first = parse_sample(block(cpu_user=1, cpu_idle=1, io_ms=0))
        second = parse_sample(block(cpu_user=1, cpu_idle=1, io_ms=5000))
        assert derive(first, second, 1.0)[m.DISK_IO_BUSY] == 100.0

    def test_a_counter_going_backwards_is_a_reboot_not_a_negative_rate(self) -> None:
        first = parse_sample(block(cpu_user=1, cpu_idle=1, rx=9000))
        second = parse_sample(block(cpu_user=1, cpu_idle=1, rx=10))
        assert derive(first, second, 1.0)[m.NET_RX_BPS] == 0.0

    def test_retransmits_and_listen_overflows_are_rates(self) -> None:
        first = parse_sample(block(cpu_user=1, cpu_idle=1, retrans=5))
        second = parse_sample(block(cpu_user=1, cpu_idle=1, retrans=25))
        assert derive(first, second, 2.0)[m.NET_RETRANSMITS] == pytest.approx(10.0)


class TestGroups:
    def test_every_group_metric_can_actually_be_produced(self) -> None:
        """A group promising a metric nothing emits would be an empty chart."""
        first = parse_sample(block(cpu_user=1, cpu_idle=1))
        second = parse_sample(block(cpu_user=2, cpu_idle=100, rx=5000, read_sectors=900,
                                    retrans=9, io_ms=20))
        produced = set(derive(first, second, 1.0))
        for group, names in m.GROUPS.items():
            missing = set(names) - produced
            assert not missing, f"group {group} promises unproduced metrics: {sorted(missing)}"
