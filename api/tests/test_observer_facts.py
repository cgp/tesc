"""Host identity and filesystem usage: the facts a recording keeps that are not series.

Both transports answer the same two questions from entirely different text, so the
agreement tests at the bottom are the ones that matter -- a recording must not say
something different about a box depending on how it was reached.
"""

from __future__ import annotations

from metrix_api.observer.facts import Filesystem, parse_df, parse_identity
from metrix_api.observer.linux import parse_probe
from metrix_api.observer.prometheus import parse_facts

PROBE = """hostname=web-1
os=Ubuntu 24.04.1 LTS
kernel=Linux 6.8.0-45-generic
arch=x86_64
cpus=8
--filesystems
Filesystem     1024-blocks     Used Available Capacity Mounted on
/dev/root         20510716  8204286  12306430      41% /
tmpfs              4096000        0   4096000       0% /dev/shm
/dev/nvme1n1     104857600 52428800  52428800      50% /var/lib/data
overlay                  0        0         0        -% /var/lib/docker/overlay2/x
"""

EXPORTER = """
node_uname_info{machine="x86_64",nodename="web-1",release="6.8.0-45-generic",sysname="Linux"} 1
node_os_info{id="ubuntu",pretty_name="Ubuntu 24.04.1 LTS",version_id="24.04"} 1
node_cpu_seconds_total{cpu="0",mode="idle"} 100
node_cpu_seconds_total{cpu="1",mode="idle"} 100
node_filesystem_size_bytes{mountpoint="/"} 21003173888
node_filesystem_avail_bytes{mountpoint="/"} 12601784320
node_filesystem_size_bytes{mountpoint="/dev/shm"} 4194304000
node_filesystem_avail_bytes{mountpoint="/dev/shm"} 4194304000
node_filesystem_size_bytes{mountpoint="/var/lib/data"} 107374182400
node_filesystem_avail_bytes{mountpoint="/var/lib/data"} 53687091200
"""


class TestDf:
    def test_sizes_come_back_in_bytes(self) -> None:
        """`df -Pk` is 1024-byte blocks. -B1 would be easier and is GNU-only."""
        found = {f.mount: f for f in parse_df(PROBE.split("--filesystems")[1])}
        assert found["/"].total_bytes == 20510716 * 1024
        assert found["/"].used_bytes == 8204286 * 1024

    def test_pseudo_filesystems_are_dropped(self) -> None:
        mounts = [f.mount for f in parse_df(PROBE.split("--filesystems")[1])]
        assert "/dev/shm" not in mounts, "tmpfs under /dev is not space a run can use"
        assert mounts == ["/", "/var/lib/data"]

    def test_a_zero_sized_filesystem_is_dropped(self) -> None:
        """An overlay with nothing behind it would read as 100% full every run."""
        assert all(f.total_bytes > 0 for f in parse_df(PROBE.split("--filesystems")[1]))

    def test_a_mount_point_with_spaces_survives_the_split(self) -> None:
        text = (
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n"
            "/dev/sdb        1000      400       600      40% /mnt/my data\n"
        )
        assert [f.mount for f in parse_df(text)] == ["/mnt/my data"]

    def test_junk_lines_are_skipped_rather_than_raising(self) -> None:
        text = "Filesystem blocks\n/dev/sdb wat wat wat wat /mnt\n"
        assert parse_df(text) == []


class TestDerivedFields:
    def test_available_and_percentage(self) -> None:
        fs = Filesystem(mount="/", total_bytes=1000, used_bytes=250)
        assert fs.available_bytes == 750
        assert fs.used_pct == 25.0

    def test_an_empty_filesystem_does_not_divide_by_zero(self) -> None:
        assert Filesystem(mount="/", total_bytes=0, used_bytes=0).used_pct == 0.0


class TestIdentity:
    def test_only_the_keys_we_asked_for_are_kept(self) -> None:
        identity = parse_identity("os=Ubuntu\nHOME=/root\nkernel=Linux 6.8\n")
        assert identity == {"os": "Ubuntu", "kernel": "Linux 6.8"}

    def test_an_unanswerable_key_is_absent_rather_than_empty(self) -> None:
        """`uname -n` on a stripped container can come back empty, and "" on screen
        reads as a hostname of nothing rather than as a question not answered."""
        identity = parse_identity("hostname=\nos=unknown\narch=x86_64\n")
        assert identity == {"arch": "x86_64"}


class TestAgreementBetweenTransports:
    """The same box, described by a shell probe and by an exporter."""

    def test_identity_matches(self) -> None:
        ssh = parse_probe(PROBE).identity
        scrape = parse_facts(EXPORTER).identity
        assert ssh["hostname"] == scrape["hostname"] == "web-1"
        assert ssh["os"] == scrape["os"] == "Ubuntu 24.04.1 LTS"
        assert ssh["arch"] == scrape["arch"] == "x86_64"
        assert "6.8.0-45-generic" in ssh["kernel"]
        assert "6.8.0-45-generic" in scrape["kernel"]

    def test_both_count_cores(self) -> None:
        assert parse_probe(PROBE).identity["cpus"] == "8"
        # The exporter has no core count; it is one series per core.
        assert parse_facts(EXPORTER).identity["cpus"] == "2"

    def test_the_same_mounts_survive_filtering(self) -> None:
        ssh = [f.mount for f in parse_probe(PROBE).filesystems]
        scrape = [f.mount for f in parse_facts(EXPORTER).filesystems]
        assert ssh == scrape == ["/", "/var/lib/data"]

    def test_used_bytes_agree_within_rounding(self) -> None:
        """df reports in 1024-blocks and the exporter in bytes, so they differ by
        less than a block -- but not by a factor."""
        ssh = {f.mount: f for f in parse_probe(PROBE).filesystems}
        scrape = {f.mount: f for f in parse_facts(EXPORTER).filesystems}
        for mount in ssh:
            assert abs(ssh[mount].used_bytes - scrape[mount].used_bytes) < 1024 * 1024

    def test_a_failed_probe_yields_nothing_rather_than_junk(self) -> None:
        """A box that answers with an error page or nothing at all is recorded as
        having no facts, which the UI draws as absent."""
        assert not parse_probe("")
        assert not parse_facts("<html>502 Bad Gateway</html>")
