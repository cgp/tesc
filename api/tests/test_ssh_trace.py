"""The SSH probe's step-by-step account, against a real SSH server on loopback.

A mocked connection would pass whether or not the steps were marked where AsyncSSH
actually reaches them, so these tests run an in-process AsyncSSH server.
"""

from __future__ import annotations

import asyncio
import logging
import re

import asyncssh
import pytest

from metrix_api.observer.ssh import SshTransport
from metrix_api.observer.ssh_trace import STEPS

TRACE_LOGGER = "metrix_api.observer.ssh"


@pytest.fixture
async def sshd(tmp_path):
    """A loopback SSH server accepting one key, whose command takes `delay` seconds."""
    accepted = asyncssh.generate_private_key("ssh-ed25519")
    accepted.write_private_key(tmp_path / "accepted")
    asyncssh.generate_private_key("ssh-ed25519").write_private_key(tmp_path / "refused")
    delay = {"seconds": 0.0}

    class Server(asyncssh.SSHServer):
        def begin_auth(self, username):
            return True

        def public_key_auth_supported(self):
            return True

        def validate_public_key(self, username, key):
            return key.public_data == accepted.public_data

    async def handle(process):
        await asyncio.sleep(delay["seconds"])
        process.stdout.write("hostname=loopback-box\n--filesystems\n")
        process.exit(0)

    server = await asyncssh.create_server(
        Server,
        "127.0.0.1",
        0,
        server_host_keys=[asyncssh.generate_private_key("ssh-ed25519")],
        process_factory=handle,
    )
    port = server.sockets[0].getsockname()[1]

    def transport(key: str) -> SshTransport:
        return SshTransport(
            host="127.0.0.1", port=port, user="probe", key=tmp_path / key, use_ssh_config=False
        )

    yield transport, delay
    server.close()


def traces(caplog) -> list[str]:
    return [r.getMessage() for r in caplog.records if r.name == TRACE_LOGGER]


async def test_a_probe_reports_every_step_in_order(sshd, caplog) -> None:
    caplog.set_level(logging.INFO, logger=TRACE_LOGGER)
    transport, _ = sshd
    facts = await transport("accepted").probe()
    assert facts.identity["hostname"] == "loopback-box"

    [report] = traces(caplog)
    assert report.startswith("ssh probe probe@127.0.0.1:")
    assert " ok in " in report.splitlines()[0]
    steps = [line.split()[0] for line in report.splitlines()[1 : 1 + len(STEPS)]]
    assert steps == list(STEPS)
    assert "Trying public key auth with ssh-ed25519 key" in report
    assert "Auth for user probe succeeded" in report


async def test_a_refused_key_says_it_failed_during_auth(sshd, caplog) -> None:
    caplog.set_level(logging.INFO, logger=TRACE_LOGGER)
    transport, _ = sshd
    with pytest.raises(asyncssh.PermissionDenied):
        await transport("refused").probe()
    [report] = traces(caplog)
    assert "failed during auth" in report.splitlines()[0]
    assert "<- still running when it failed" in report
    assert "Auth failed for user probe" in report


async def test_a_timeout_names_the_step_it_interrupted(sshd, caplog) -> None:
    caplog.set_level(logging.INFO, logger=TRACE_LOGGER)
    transport, delay = sshd
    delay["seconds"] = 5.0
    with pytest.raises(TimeoutError):
        await asyncio.wait_for(transport("accepted").probe(), 0.5)
    [report] = traces(caplog)
    assert "failed during script" in report.splitlines()[0]
    assert "verification timeout" in report.splitlines()[0]


async def test_concurrent_probes_keep_their_own_events(sshd, caplog) -> None:
    caplog.set_level(logging.INFO, logger=TRACE_LOGGER)
    transport, _ = sshd
    await asyncio.gather(transport("accepted").probe(), transport("accepted").probe())
    reports = traces(caplog)
    assert len(reports) == 2
    tags = [set(re.findall(r"\[conn=(\d+)", report)) for report in reports]
    assert all(len(found) == 1 for found in tags), f"one connection per report: {tags}"
    assert tags[0] != tags[1], "each report is about its own connection"


async def test_asyncssh_debug_output_goes_nowhere_else(sshd) -> None:
    """Debug is switched on to trace a probe, and must not reach a root handler."""
    seen: list[logging.LogRecord] = []

    class Collect(logging.Handler):
        def emit(self, record):
            seen.append(record)

    handler = Collect(level=logging.NOTSET)
    root = logging.getLogger()
    root.addHandler(handler)
    asyncssh_logger = logging.getLogger("asyncssh")
    level_before = asyncssh_logger.level
    try:
        transport, _ = sshd
        await transport("accepted").probe()
    finally:
        root.removeHandler(handler)
    assert not [r for r in seen if r.name.startswith("asyncssh") and r.levelno < logging.WARNING]
    assert asyncssh_logger.level == level_before
