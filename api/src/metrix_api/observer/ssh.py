"""SSH transport: a long-lived channel running the sampling loop.

One connection per host and one remote loop, rather than an SSH exec per sample.
At a one-second interval, per-sample connection and process spawn would dominate,
and the collector would be measuring its own overhead rather than the host.

Nothing is installed on the target: the remote side is a POSIX shell loop reading
`/proc`, which is what makes this useful on day one against a box you were given an
hour ago.
"""

from __future__ import annotations

import asyncio
import importlib
import logging
import socket
from collections.abc import AsyncIterator
from dataclasses import dataclass
from datetime import timedelta
from pathlib import Path
from types import ModuleType

from metrix_api.observer.facts import HostFacts
from metrix_api.observer.linux import (
    PROBE_SCRIPT,
    REMOTE_SCRIPT,
    parse_probe,
    parse_sample,
    split_blocks,
)
from metrix_api.observer.raw import RawSample
from metrix_api.observer.ssh_trace import Trace, tracing
from metrix_api.profiles import Collection

log = logging.getLogger(__name__)

#: Read at most this much before giving up on finding a complete block, so a host
#: emitting garbage cannot grow the buffer without bound.
MAX_BUFFER = 1 << 20


#: Modules whose import has *finished*. Not `sys.modules`: a module is entered there
#: when its import starts, so a second probe reading it while the first is still
#: importing gets a half-initialised module with no `connect` on it yet.
_imported: dict[str, ModuleType] = {}


async def _import(name: str) -> ModuleType:
    """Import a module off the event loop, and hand it out only once complete.

    `importlib.import_module` takes the per-module import lock, so concurrent first
    callers each wait in their own thread for the one import in progress, rather
    than any of them seeing it half done.
    """
    module = _imported.get(name)
    if module is None:
        module = await asyncio.to_thread(importlib.import_module, name)
        _imported[name] = module
    return module


async def _asyncssh() -> ModuleType:
    """AsyncSSH, imported off the event loop the first time it is needed.

    Imported lazily so the API starts without paying for an SSH stack it may never
    use -- but the import pulls in the cryptography backend, which is hundreds of
    milliseconds of synchronous work. Done on the loop, that stalls every check and
    stream in flight, so the first one happens in a thread.
    """
    return await _import("asyncssh")


@dataclass(slots=True)
class SshTransport:
    """Streams sample blocks over one SSH connection."""

    host: str
    port: int = 22
    user: str | None = None
    key: Path | None = None
    #: ALB-resolved hosts are ephemeral: accept the presented host key for now.
    #: A future policy can warn when it changes without blocking collection.
    known_hosts: Path | None = None
    connect_timeout: float = 10.0
    #: OpenSSH client config to honour. Defaults to the user's own ~/.ssh/config, so
    #: a host that works from a shell works here without being re-described. A
    #: profile that names a key explicitly still wins.
    ssh_config: Path | None = None
    use_ssh_config: bool = True

    @classmethod
    def from_collection(cls, collection: Collection) -> SshTransport:
        if collection.host is None:
            raise ValueError("ssh transport needs a host")
        return cls(
            host=collection.host,
            port=collection.port or 22,
            user=collection.user,
            key=collection.key,
        )

    def describe(self) -> str:
        """Where this will connect, for the Config page.

        The destination only. Which transport it is, the page says separately, and
        two labels for one fact read as a bug.
        """
        user = f"{self.user}@" if self.user else ""
        return f"{user}{self.host}:{self.port}"

    def diagnostics(self) -> dict[str, object]:
        """Return the effective connection settings used for a probe.

        AsyncSSH applies OpenSSH config after this transport is built, so the raw
        dataclass fields are not enough to explain an authentication failure. Build
        the same options object used by ``connect`` and expose its effective target,
        login, config, identity files, and loaded-key count. Private key contents are
        never included.
        """
        try:
            import asyncssh

            raw_options = self._options()
            options = asyncssh.SSHClientConnectionOptions(
                host=self.host, port=self.port, **raw_options
            )
            config_paths = raw_options.get("config") or []
            identity_files = options.config.get("IdentityFile", [])
            return {
                "requested_host": self.host,
                "requested_port": self.port,
                "host": options.host,
                "port": options.port,
                "username": options.username,
                "ssh_config": list(config_paths),
                "identity_files": list(identity_files)
                if isinstance(identity_files, (list, tuple))
                else identity_files,
                "loaded_client_keys": len(options.client_keys or []),
                "explicit_key": str(self.key) if self.key else None,
            }
        except Exception as exc:  # noqa: BLE001 - diagnostics must not mask the probe
            return {
                "requested_host": self.host,
                "requested_port": self.port,
                "host": self.host,
                "port": self.port,
                "username": self.user or "<config/default>",
                "ssh_config": str(self.ssh_config) if self.ssh_config else None,
                "identity_files": "unavailable",
                "loaded_client_keys": "unavailable",
                "diagnostics_error": str(exc) or type(exc).__name__,
            }

    def _options(self) -> dict[str, object]:
        options: dict[str, object] = {"connect_timeout": self.connect_timeout}
        if self.user:
            options["username"] = self.user
        if self.key:
            # Explicit beats inherited: a profile naming a key means that key.
            options["client_keys"] = [str(self.key)]

        config = self.ssh_config
        if config is None and self.use_ssh_config:
            default = Path.home() / ".ssh" / "config"
            config = default if default.is_file() else None
        if config is not None:
            options["config"] = [str(config)]
        # Passing None explicitly disables AsyncSSH known-host checking. ALB-resolved
        # hosts are ephemeral, so accept the presented key for now; a later policy can
        # report changes without blocking collection.
        options["known_hosts"] = str(self.known_hosts) if self.known_hosts else None
        return options

    async def probe(self) -> HostFacts:
        """Identity and filesystem usage, once. Its own short connection.

        Twice a recording, not once a second, so the cost of a second handshake is
        not worth threading this through the streaming connection's lifetime.
        """
        asyncssh = await _asyncssh()
        trace = Trace(self.describe())
        with tracing(trace):
            try:
                output = await self._traced_probe(asyncssh, trace)
            except BaseException as exc:  # a timeout arrives here as a cancellation
                log.warning("%s", trace.report(exc))
                raise
        log.info("%s", trace.report())
        return parse_probe(output)

    async def _traced_probe(self, asyncssh: ModuleType, trace: Trace) -> str:
        """The probe, one step at a time, each marked as it completes (see ssh_trace)."""
        options = await asyncssh.SSHClientConnectionOptions.construct(
            host=self.host, port=self.port, **self._options()
        )
        trace.mark(
            "options",
            f"{options.username}@{options.host}:{options.port}, "
            f"{len(options.client_keys or [])} client key(s)",
        )

        if getattr(options, "tunnel", None) or getattr(options, "proxy_command", None):
            trace.mark("dns", "skipped: the connection goes through a proxy")
        else:
            infos = await asyncio.get_running_loop().getaddrinfo(
                options.host, options.port, type=socket.SOCK_STREAM
            )
            trace.mark("dns", ", ".join(dict.fromkeys(str(info[4][0]) for info in infos)))

        class Client(asyncssh.SSHClient):
            """Marks the steps only AsyncSSH can see from inside `connect`."""

            def connection_made(self, conn) -> None:
                self._conn = conn
                trace.tag = "[" + str(getattr(conn.logger, "_context", "") or "")
                peer = conn.get_extra_info("peername") or ("?", "?")
                trace.mark("connect", f"to {peer[0]}:{peer[1]}")

            def begin_auth(self, username: str) -> None:
                version = self._conn.get_extra_info("server_version")
                trace.mark("handshake", f"server {version}, user {username}")

            def auth_completed(self) -> None:
                trace.mark("auth")

        async with asyncssh.connect(
            self.host, port=self.port, options=options, client_factory=Client
        ) as conn:
            process = await conn.create_process(PROBE_SCRIPT)
            trace.mark("session")
            result = await process.wait(check=False)
            trace.mark("script", f"exit status {result.exit_status}")
        trace.mark("close")
        return str(result.stdout or "")

    async def stream(self, interval: timedelta) -> AsyncIterator[RawSample]:
        asyncssh = await _asyncssh()

        seconds = max(1, int(interval.total_seconds()))
        script = REMOTE_SCRIPT.replace("{interval}", str(seconds))
        options = self._options()

        async with (
            asyncssh.connect(self.host, port=self.port, **options) as conn,
            conn.create_process(script) as process,
        ):
                buffer = ""
                async for chunk in process.stdout:
                    buffer += chunk
                    if len(buffer) > MAX_BUFFER:
                        buffer = buffer[-MAX_BUFFER:]
                    blocks = split_blocks(buffer)
                    if not blocks:
                        continue
                    # Keep whatever follows the last complete block: it is the start
                    # of the next sample, arriving one chunk at a time.
                    tail = buffer.rfind("--end")
                    buffer = buffer[tail + len("--end") :]
                    for block in blocks:
                        yield parse_sample(block)
