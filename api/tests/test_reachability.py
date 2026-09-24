"""Verifying a profile before trusting it, and the targets document it produces.

Both halves of A2.3 answer the same question in different tenses: *would this work*,
asked at setup, and *where does the traffic go*, asked when a run is assembled. The
checks here run against real sockets on the loopback interface rather than against
mocks, because what they are testing is precisely the behaviour of a socket -- a
mocked connect would pass whether or not the code opened one.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pytest
from jsonschema import Draft202012Validator

from metrix_api import reachability
from metrix_api.observer.facts import Filesystem, HostFacts
from metrix_api.observer.ssh import SshTransport
from metrix_api.profiles import ProfileError, parse_profile, to_targets

REPO = Path(__file__).resolve().parents[2]
TARGETS_SCHEMA = REPO / "schema" / "targets.schema.json"


@pytest.fixture
async def listening():
    """A real socket on the loopback interface, and the port it took.

    Port 0 lets the OS choose, so the suite never collides with something already
    running on this machine.
    """
    async def respond(reader, writer):
        writer.write(
            b"HTTP/1.1 404 Not Found\r\n"
            b"Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
        await writer.drain()
        writer.close()

    server = await asyncio.start_server(respond, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    async with server:
        yield port


@pytest.fixture
def closed_port():
    """A port nothing is listening on: bound to find a free one, then released."""
    import socket

    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def profile(**endpoint):
    return parse_profile(
        {
            "name": "under-test",
            "endpoints": [{"id": "box", "address": "127.0.0.1:1", **endpoint}],
        }
    )


def collector(**collect):
    """A profile whose load target is deliberately not dialled.

    These tests are about the collector, and connecting to a dead port costs a real
    two seconds on Windows -- paid four times over, for nothing being tested.
    """
    return profile(load=False, collect=collect)


class FakeTransport:
    """Stands in for a collector. Answers, refuses, or hangs."""

    def __init__(self, *, facts=None, raises=None, hang=False):
        self.facts = facts
        self.raises = raises
        self.hang = hang

    def describe(self) -> str:
        return "ssh probe@10.0.0.1:22"

    def diagnostics(self) -> dict[str, object]:
        return {
            "requested_host": "10.0.0.1",
            "requested_port": 22,
            "host": "10.0.0.1",
            "port": 22,
            "username": "probe",
            "ssh_config": ["C:/Users/test/.ssh/config"],
            "identity_files": ["C:/Users/test/.ssh/id_ed25519"],
            "loaded_client_keys": 1,
        }

    async def probe(self):
        if self.hang:
            await asyncio.sleep(30)
        if self.raises is not None:
            raise self.raises
        return self.facts


def using(monkeypatch, transport):
    monkeypatch.setattr(reachability, "transport_for", lambda endpoint, **kw: transport)


def check(report, kind):
    return next(c for c in report.checks if c.kind == kind)


def test_ssh_accepts_ephemeral_host_keys() -> None:
    """ALB replacements must not fail solely because their host key changed."""
    assert SshTransport(host="10.0.0.1")._options()["known_hosts"] is None


async def test_concurrent_first_imports_never_see_a_half_initialised_module(
    tmp_path, monkeypatch
) -> None:
    """Two SSH probes starting together both import the SSH stack off the loop.

    A module is in `sys.modules` from the moment its import starts, so the second
    probe must wait for the first import to finish rather than read it from there:
    "partially initialized module 'asyncssh' has no attribute 'connect'".
    """
    import sys

    from metrix_api.observer import ssh

    (tmp_path / "slow_stack.py").write_text(
        "import time\ntime.sleep(0.3)\n\ndef connect():\n    return 'connected'\n"
    )
    monkeypatch.syspath_prepend(str(tmp_path))
    monkeypatch.delitem(sys.modules, "slow_stack", raising=False)
    monkeypatch.setattr(ssh, "_imported", {})

    async def probe(delay: float) -> str:
        await asyncio.sleep(delay)
        # Used the moment it is handed over, as `probe()` does with `connect`: read
        # after both imports finish, a half-initialised module would look complete.
        return (await ssh._import("slow_stack")).connect()

    # The second starts while the first import is asleep inside the module body.
    assert await asyncio.gather(probe(0), probe(0.1)) == ["connected", "connected"]


# ------------------------------------------------------------------- load targets


class TestLoadTarget:
    async def test_a_socket_that_accepts_is_reachable(self, listening) -> None:
        report = await reachability.verify(profile(address=f"127.0.0.1:{listening}"))
        result = check(report, reachability.LOAD)
        assert result.ok
        assert result.detail == "HTTP 404"
        assert result.ms is not None

    async def test_a_dual_stack_name_does_not_wait_for_its_ipv6_address(
        self, listening
    ) -> None:
        """`localhost` resolves to ::1 first, and this listener is IPv4 only.

        Dialled in turn, Windows retries the refused IPv6 attempt for two seconds
        before IPv4 is tried. Raced, the IPv4 connection answers in milliseconds.
        """
        report = await reachability.verify(profile(address=f"localhost:{listening}"))
        result = check(report, reachability.LOAD)
        assert result.ok
        assert result.ms < 1000

    async def test_a_loop_without_happy_eyeballs_still_connects(
        self, listening, monkeypatch
    ) -> None:
        """uvicorn runs on uvloop wherever it is installed, and uvloop's
        `create_connection` rejects `happy_eyeballs_delay` -- a 500 on every check."""
        loop = asyncio.get_running_loop()
        original = loop.create_connection

        async def like_uvloop(*args, **kwargs):
            for name in ("happy_eyeballs_delay", "interleave"):
                if name in kwargs:
                    raise TypeError(f"create_connection() got an unexpected keyword: {name}")
            return await original(*args, **kwargs)

        monkeypatch.setattr(loop, "create_connection", like_uvloop)
        for address in (f"localhost:{listening}", f"127.0.0.1:{listening}"):
            result = check(await reachability.verify(profile(address=address)), reachability.LOAD)
            assert result.ok, result.detail

    async def test_a_check_logs_its_connect_and_response_times(self, listening, caplog) -> None:
        """A slow application root and a slow network look alike without the split."""
        caplog.set_level("INFO", logger="metrix_api.reachability")
        await reachability.verify(profile(address=f"127.0.0.1:{listening}"))
        messages = [r.getMessage() for r in caplog.records]
        line = next(m for m in messages if "front end check ok" in m)
        assert "status=404" in line
        assert "connect=" in line and "response=" in line

    async def test_a_refused_connection_says_so(self, closed_port) -> None:
        report = await reachability.verify(
            profile(address=f"127.0.0.1:{closed_port}"), timeout=2.0
        )
        result = check(report, reachability.LOAD)
        assert result.result == reachability.FAILED
        assert not report.ok
        assert report.failures == [result]

    async def test_an_address_that_goes_nowhere_times_out_rather_than_hanging(self) -> None:
        """A blackholed address is the firewall case, and it must not hang the page."""
        # 203.0.113.0/24 is TEST-NET-3: reserved for documentation, routed nowhere.
        report = await reachability.verify(profile(address="203.0.113.1:9"), timeout=0.4)
        result = check(report, reachability.LOAD)
        assert result.result == reachability.FAILED
        assert "0.4s" in result.detail

    async def test_an_observation_only_endpoint_is_not_dialled(self, closed_port) -> None:
        """Traffic never goes there, so a closed port is not a finding."""
        report = await reachability.verify(
            profile(address=f"127.0.0.1:{closed_port}", load=False)
        )
        result = check(report, reachability.LOAD)
        assert result.result == reachability.SKIPPED
        assert report.ok, "a skipped check is not a failed one"

    async def test_tls_against_a_plain_socket_fails_at_the_handshake(self, listening) -> None:
        report = await reachability.verify(
            profile(
                address=f"127.0.0.1:{listening}",
                host_header="api.example.com",
                tls={"enabled": True},
            ),
            timeout=2.0,
        )
        result = check(report, reachability.LOAD)
        assert result.result == reachability.FAILED


# ---------------------------------------------------------------------- collectors


class TestCollector:
    async def test_a_probe_that_answers_names_the_box(self, monkeypatch) -> None:
        using(
            monkeypatch,
            FakeTransport(
                facts=HostFacts(
                    identity={"hostname": "ip-10-0-3-41", "os": "Amazon Linux 2023"},
                    filesystems=[Filesystem("/", 100, 40)],
                )
            ),
        )
        report = await reachability.verify(collector(transport="ssh"))
        result = check(report, reachability.COLLECT)
        assert result.ok
        assert "ip-10-0-3-41" in result.detail
        assert result.address == "ssh probe@10.0.0.1:22"

    async def test_a_probe_that_answers_does_not_build_diagnostics(self, monkeypatch) -> None:
        """Diagnostics load private keys synchronously; on success nobody reads them."""

        def unwanted() -> dict[str, object]:
            raise AssertionError("diagnostics built for a probe that answered")

        transport = FakeTransport(facts=HostFacts(identity={"hostname": "h"}))
        transport.diagnostics = unwanted
        using(monkeypatch, transport)
        report = await reachability.verify(collector(transport="ssh"))
        assert check(report, reachability.COLLECT).ok

    async def test_a_refused_probe_reports_what_the_transport_said(self, monkeypatch) -> None:
        """The reason is the useful part: a key problem and a firewall differ here."""
        using(monkeypatch, FakeTransport(raises=PermissionError("Permission denied (publickey)")))
        report = await reachability.verify(collector(transport="ssh"))
        result = check(report, reachability.COLLECT)
        assert result.result == reachability.FAILED
        assert "publickey" in result.detail
        assert "host=10.0.0.1" in result.detail
        assert "port=22" in result.detail
        assert "username=probe" in result.detail
        assert "id_ed25519" in result.detail

    async def test_a_probe_that_answers_with_nothing_is_a_failure(self, monkeypatch) -> None:
        """Reached, and not what we think it is -- the wrong path on an exporter."""
        using(monkeypatch, FakeTransport(facts=HostFacts()))
        report = await reachability.verify(collector(transport="scrape"))
        result = check(report, reachability.COLLECT)
        assert result.result == reachability.FAILED
        assert "reported nothing" in result.detail

    async def test_a_hanging_probe_is_bounded(self, monkeypatch) -> None:
        using(monkeypatch, FakeTransport(hang=True))
        report = await reachability.verify(collector(transport="ssh"), timeout=0.2)
        result = check(report, reachability.COLLECT)
        assert result.result == reachability.FAILED
        assert "no answer" in result.detail

    async def test_an_endpoint_with_no_collector_is_skipped(self) -> None:
        report = await reachability.verify(collector())
        result = check(report, reachability.COLLECT)
        assert result.result == reachability.SKIPPED
        assert "contributes no host series" in result.detail


class TestReport:
    async def test_each_endpoint_checks_load_before_collector(self, monkeypatch) -> None:
        events: list[tuple[str, str]] = []
        two = parse_profile(
            {
                "name": "ordered",
                "endpoints": [
                    {"id": "a", "address": "127.0.0.1:1", "load": False},
                    {"id": "b", "address": "127.0.0.1:2", "load": False},
                ],
            }
        )

        async def fake_load(endpoint, timeout):
            events.append((endpoint.id, "load"))
            return reachability.Check(
                endpoint.id, reachability.LOAD, reachability.SKIPPED, "—", "skip"
            )

        async def fake_collect(endpoint, timeout):
            events.append((endpoint.id, "collect"))
            return reachability.Check(
                endpoint.id, reachability.COLLECT, reachability.SKIPPED, "—", "skip"
            )

        monkeypatch.setattr(reachability, "_load", fake_load)
        monkeypatch.setattr(reachability, "_collect", fake_collect)

        await reachability.verify(two)

        assert events == [
            ("a", "load"),
            ("a", "collect"),
            ("b", "load"),
            ("b", "collect"),
        ]

    async def test_checks_are_grouped_by_endpoint(self, listening, monkeypatch) -> None:
        """One box at a time, both of its answers together: that is how it is read."""
        using(monkeypatch, FakeTransport(facts=HostFacts(identity={"hostname": "h"})))
        two = parse_profile(
            {
                "name": "pair",
                "endpoints": [
                    {"id": "a", "address": f"127.0.0.1:{listening}",
                     "collect": {"transport": "ssh"}},
                    {"id": "b", "address": f"127.0.0.1:{listening}",
                     "collect": {"transport": "ssh"}},
                ],
            }
        )
        report = await reachability.verify(two)
        assert [(c.endpoint, c.kind) for c in report.checks] == [
            ("a", reachability.LOAD),
            ("a", reachability.COLLECT),
            ("b", reachability.LOAD),
            ("b", reachability.COLLECT),
        ]

    async def test_the_summary_counts_only_what_was_attempted(self, listening) -> None:
        report = await reachability.verify(profile(address=f"127.0.0.1:{listening}"))
        assert report.summary() == "1 of 1 reachable", "the absent collector is not a check"

    async def test_slow_endpoints_are_checked_at_once_rather_than_in_turn(self) -> None:
        """Eight boxes behind one firewall must cost one timeout, not eight."""
        many = parse_profile(
            {
                "name": "many",
                "endpoints": [
                    {"id": f"b{i}", "address": f"203.0.113.{i}:9"} for i in range(1, 9)
                ],
            }
        )
        started = asyncio.get_running_loop().time()
        report = await reachability.verify(many, timeout=0.5)
        elapsed = asyncio.get_running_loop().time() - started

        assert len(report.failures) == 8
        assert elapsed < 2.0, f"checks were serialised: {elapsed:.1f}s for eight timeouts"


# ---------------------------------------------------- the engine's targets document


TARGETS = {
    "name": "staging",
    "addressing": "load_balancer",
    "endpoints": [
        {"id": "alb", "address": "10.0.1.9:443", "tls": {"enabled": True}},
        {
            "id": "app-1",
            "address": "10.0.3.41:8080",
            "load": False,
            "collect": {"transport": "ssh"},
        },
        {
            "id": "app-2",
            "address": "10.0.3.42:8080",
            "load": False,
            "collect": {"transport": "ssh"},
        },
    ],
}


@pytest.fixture(scope="module")
def validator():
    return Draft202012Validator(json.loads(TARGETS_SCHEMA.read_text(encoding="utf-8")))


class TestTargetsDocument:
    def test_observation_only_boxes_are_not_targets(self, validator) -> None:
        """The run points at the balancer and watches the boxes behind it."""
        document = to_targets(parse_profile(TARGETS))
        assert [t["id"] for t in document["list"]] == ["alb"]
        assert not list(validator.iter_errors(document))

    def test_the_two_roles_are_separate_lists(self) -> None:
        parsed = parse_profile(TARGETS)
        assert [e.id for e in parsed.targets] == ["alb"]
        assert [e.id for e in parsed.observed] == ["app-1", "app-2"]

    def test_an_explicit_selection_may_name_an_observed_box(self) -> None:
        """Asking for it by name is a decision, not an accident."""
        document = to_targets(parse_profile(TARGETS), only=["app-1"])
        assert [t["id"] for t in document["list"]] == ["app-1"]

    def test_a_profile_with_nowhere_to_send_traffic_says_which_problem_it_is(self) -> None:
        nowhere = {**TARGETS, "endpoints": [
            {**e, "load": False} for e in TARGETS["endpoints"]
        ]}
        with pytest.raises(ProfileError, match="every endpoint is marked observation-only"):
            to_targets(parse_profile(nowhere))

    def test_the_flag_round_trips(self) -> None:
        from metrix_api.profiles import to_document

        once = to_document(parse_profile(TARGETS))
        assert once["endpoints"][1]["load"] is False
        assert "load" not in once["endpoints"][0], "the default is not written out"
        assert to_document(parse_profile(once)) == once
