"""SSH transport: a long-lived channel running the sampling loop.

One connection per host and one remote loop, rather than an SSH exec per sample.
At a one-second interval, per-sample connection and process spawn would dominate,
and the collector would be measuring its own overhead rather than the host.

Nothing is installed on the target: the remote side is a POSIX shell loop reading
`/proc`, which is what makes this useful on day one against a box you were given an
hour ago.
"""

from __future__ import annotations

import logging
from collections.abc import AsyncIterator
from dataclasses import dataclass
from datetime import timedelta
from pathlib import Path

from metrix_api.observer.facts import HostFacts
from metrix_api.observer.linux import (
    PROBE_SCRIPT,
    REMOTE_SCRIPT,
    parse_probe,
    parse_sample,
    split_blocks,
)
from metrix_api.observer.raw import RawSample
from metrix_api.profiles import Collection

log = logging.getLogger(__name__)

#: Read at most this much before giving up on finding a complete block, so a host
#: emitting garbage cannot grow the buffer without bound.
MAX_BUFFER = 1 << 20


@dataclass(slots=True)
class SshTransport:
    """Streams sample blocks over one SSH connection."""

    host: str
    port: int = 22
    user: str | None = None
    key: Path | None = None
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
        # A host key we have never seen should not stop a recording; the profile is
        # already an explicit statement about which boxes these are.
        options["known_hosts"] = str(self.known_hosts) if self.known_hosts else None
        return options

    async def probe(self) -> HostFacts:
        """Identity and filesystem usage, once. Its own short connection.

        Twice a recording, not once a second, so the cost of a second handshake is
        not worth threading this through the streaming connection's lifetime.
        """
        import asyncssh

        async with asyncssh.connect(self.host, port=self.port, **self._options()) as conn:
            result = await conn.run(PROBE_SCRIPT, check=False)
        return parse_probe(str(result.stdout or ""))

    async def stream(self, interval: timedelta) -> AsyncIterator[RawSample]:
        import asyncssh  # imported here so the module loads without a live SSH stack

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
