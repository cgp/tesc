"""The scrape transport, and that it agrees with the SSH transport.

The point of a normalized shape is that a metric means the same thing regardless of
how it was collected, so the interesting test is not "does it parse" but "do the two
sources produce the same numbers from the same facts".
"""

from __future__ import annotations

import pytest

from metrix_api.observer import metrics as m
from metrix_api.observer.linux import parse_sample
from metrix_api.observer.metrics import COLLECTION_GAP, Annotation, Gap
from metrix_api.observer.prometheus import ScrapeTransport, parse_text
from metrix_api.observer.raw import derive
from metrix_api.profiles import Collection
from tests.test_observer_linux import block

EXPORTER = """\
# HELP node_cpu_seconds_total Seconds the CPUs spent in each mode.
# TYPE node_cpu_seconds_total counter
node_cpu_seconds_total{{cpu="0",mode="user"}} {user_a}
node_cpu_seconds_total{{cpu="1",mode="user"}} {user_b}
node_cpu_seconds_total{{cpu="0",mode="system"}} 3
node_cpu_seconds_total{{cpu="1",mode="system"}} 3
node_cpu_seconds_total{{cpu="0",mode="idle"}} {idle_a}
node_cpu_seconds_total{{cpu="1",mode="idle"}} {idle_b}
node_cpu_seconds_total{{cpu="0",mode="iowait"}} {iowait}
node_cpu_seconds_total{{cpu="1",mode="iowait"}} {iowait}
node_cpu_seconds_total{{cpu="0",mode="steal"}} {steal}
node_cpu_seconds_total{{cpu="1",mode="steal"}} {steal}
node_load1 0.52
node_load5 0.48
node_load15 0.44
node_memory_MemTotal_bytes 1.6391635e+10
node_memory_MemAvailable_bytes 8.19581747e+09
node_memory_Cached_bytes 4.096e+09
node_memory_SwapTotal_bytes 2.147483648e+09
node_memory_SwapFree_bytes 2.048e+09
node_disk_reads_completed_total{{device="sda"}} 1000
node_disk_read_bytes_total{{device="sda"}} {read_bytes}
node_disk_writes_completed_total{{device="sda"}} 2000
node_disk_written_bytes_total{{device="sda"}} 2048000
node_disk_read_bytes_total{{device="loop0"}} 99999999
node_disk_io_time_seconds_total{{device="sda"}} {io_s}
node_network_receive_bytes_total{{device="eth0"}} {rx}
node_network_transmit_bytes_total{{device="eth0"}} {tx}
node_network_receive_drop_total{{device="eth0"}} 3
node_network_transmit_drop_total{{device="eth0"}} 7
node_network_receive_bytes_total{{device="lo"}} 999999
node_netstat_Tcp_RetransSegs {retrans}
node_netstat_Tcp_CurrEstab 42
node_netstat_TcpExt_ListenOverflows 11
node_filefd_allocated 2048
node_procs_running 3
node_processes_pids 187
node_processes_threads 431
node_time_seconds 1.789e+09
node_dmi_info{{bios_vendor="Whoever"}} 1
node_scrape_collector_success{{collector="cpu"}} NaN
"""


def exporter(*, user_a=50.0, user_b=50.0, idle_a=450.0, idle_b=450.0, read_bytes=51200,
             io_s=0.0, rx=1000, tx=2000, retrans=5, iowait=1, steal=1) -> str:
    return EXPORTER.format(
        user_a=user_a, user_b=user_b, idle_a=idle_a, idle_b=idle_b,
        read_bytes=read_bytes, io_s=io_s, rx=rx, tx=tx, retrans=retrans,
        iowait=iowait, steal=steal,
    )


class TestParsing:
    @pytest.fixture
    def sample(self):
        return parse_text(exporter())

    def test_cpu_is_summed_across_cores(self, sample) -> None:
        assert sample.cpu["user"] == 100.0
        assert sample.cpu["idle"] == 900.0

    def test_gauges(self, sample) -> None:
        assert sample.load == (0.52, 0.48, 0.44)
        assert sample.fd_open == 2048
        assert sample.tcp["CurrEstab"] == 42

    def test_the_three_task_counts_come_from_three_different_series(self, sample) -> None:
        """`node_procs_running` is runnable processes. It was read as the process
        count, which is a different question and about two orders of magnitude out."""
        assert sample.proc_count == 187, "node_processes_pids"
        assert sample.thread_count == 431, "node_processes_threads"
        assert sample.proc_running == 3, "node_procs_running"

    def test_without_the_processes_collector_the_counts_are_absent(self) -> None:
        """node_exporter ships that collector disabled. Absent is the honest answer;
        filling them from node_procs_running is what made the transports disagree."""
        sample = parse_text("node_procs_running 3\nnode_filefd_allocated 10\n")
        assert sample.proc_running == 3
        assert sample.proc_count is None
        assert sample.thread_count is None

    def test_virtual_devices_are_excluded(self, sample) -> None:
        # loop0 would otherwise add 100MB of phantom reads.
        assert sample.disk["read_bytes"] == 51200
        assert sample.net["rx_bytes"] == 1000, "lo must be excluded, as in /proc"

    def test_scientific_notation_is_read(self, sample) -> None:
        assert sample.mem["MemTotal"] == pytest.approx(1.6391635e10)

    def test_a_nan_reading_is_skipped_rather_than_treated_as_zero(self) -> None:
        """A missing reading is not a measurement of zero."""
        parse_text("node_procs_running NaN\n")  # must not raise
        assert parse_text("node_procs_running NaN\n").proc_running is None

    def test_comments_and_unknown_series_are_ignored(self, sample) -> None:
        assert "bios_vendor" not in str(sample.mem)


class TestAgreementWithSsh:
    """Both transports describe the same host; the numbers must match."""

    def test_cpu_percentages_match(self) -> None:
        proc = derive(
            parse_sample(block(cpu_user=100, cpu_idle=900)),
            parse_sample(block(cpu_user=200, cpu_idle=1800)),
            1.0,
        )
        scrape = derive(
            parse_text(exporter(user_a=50, user_b=50, idle_a=450, idle_b=450)),
            parse_text(exporter(user_a=100, user_b=100, idle_a=900, idle_b=900)),
            1.0,
        )
        assert scrape[m.CPU_USER] == pytest.approx(proc[m.CPU_USER])
        assert scrape[m.CPU_IDLE] == pytest.approx(proc[m.CPU_IDLE])

    def test_disk_byte_rates_match(self) -> None:
        proc = derive(
            parse_sample(block(cpu_user=1, cpu_idle=1, read_sectors=100)),
            parse_sample(block(cpu_user=1, cpu_idle=1, read_sectors=200)),
            1.0,
        )
        scrape = derive(
            parse_text(exporter(read_bytes=51200)),
            parse_text(exporter(read_bytes=102400)),
            1.0,
        )
        assert scrape[m.DISK_READ_BPS] == pytest.approx(proc[m.DISK_READ_BPS])

    def test_disk_busy_matches(self) -> None:
        proc = derive(
            parse_sample(block(cpu_user=1, cpu_idle=1, io_ms=0)),
            parse_sample(block(cpu_user=1, cpu_idle=1, io_ms=250)),
            1.0,
        )
        scrape = derive(parse_text(exporter(io_s=0.0)), parse_text(exporter(io_s=0.25)), 1.0)
        assert scrape[m.DISK_IO_BUSY] == pytest.approx(proc[m.DISK_IO_BUSY])

    def test_the_task_counts_agree(self) -> None:
        """The number under a metric name must not depend on how it was collected."""
        proc = derive(None, parse_sample(block(cpu_user=1, cpu_idle=1)), 1.0)
        scrape = derive(None, parse_text(exporter()), 1.0)
        for name in (m.PROC_COUNT, m.THREAD_COUNT, m.PROC_RUNNING):
            assert scrape[name] == proc[name], f"{name} differs between transports"

    def test_every_group_metric_is_produced_by_this_transport_too(self) -> None:
        produced = set(
            derive(
                parse_text(exporter()),
                parse_text(
                    exporter(user_a=60, idle_a=900, iowait=2, steal=2,
                             rx=9000, retrans=9, io_s=1)
                ),
                1.0,
            )
        )
        for group, names in m.GROUPS.items():
            missing = set(names) - produced
            assert not missing, f"group {group} unavailable over scrape: {sorted(missing)}"


class TestUrl:
    def test_built_from_the_profile(self) -> None:
        transport = ScrapeTransport.from_collection(
            Collection(transport="scrape", host="10.0.3.41", port=9100, path="/metrics")
        )
        assert transport.url == "http://10.0.3.41:9100/metrics"

    def test_ipv6_hosts_are_bracketed(self) -> None:
        transport = ScrapeTransport.from_collection(
            Collection(transport="scrape", host="2001:db8::1", port=9100)
        )
        assert transport.url == "http://[2001:db8::1]:9100/metrics"

    def test_a_transport_without_a_host_is_refused(self) -> None:
        with pytest.raises(ValueError, match="needs a host"):
            ScrapeTransport.from_collection(Collection(transport="scrape"))


class TestGapAnnotation:
    def test_a_gap_becomes_a_warning_that_names_the_target_and_the_reason(self) -> None:
        annotation = Annotation.from_gap(Gap("task-a1b2", 4000, 11500, "connection refused"))

        assert annotation.code == COLLECTION_GAP
        # Warn, not invalid: the recording is still worth having.
        assert annotation.severity == "warn"
        assert annotation.target_id == "task-a1b2"
        assert "task-a1b2" in annotation.message
        assert "connection refused" in annotation.message
        assert "7.5s" in annotation.message
        assert annotation.detail["duration_s"] == 7.5

    def test_it_says_the_gap_is_not_interpolated(self) -> None:
        annotation = Annotation.from_gap(Gap("host-a", 0, 1000, "stream ended"))
        assert "never interpolated" in annotation.message
