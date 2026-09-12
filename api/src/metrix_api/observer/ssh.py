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

from metrix_api.observer.linux import REMOTE_SCRIPT, split_blocks
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

    async def stream(self, interval: timedelta) -> AsyncIterator[str]:
        import asyncssh  # imported here so the module loads without a live SSH stack

        seconds = max(1, int(interval.total_seconds()))
        script = REMOTE_SCRIPT.replace("{interval}", str(seconds))

        options: dict[str, object] = {"connect_timeout": self.connect_timeout}
        if self.user:
            options["username"] = self.user
        if self.key:
            options["client_keys"] = [str(self.key)]
        # A host key we have never seen should not stop a recording; the profile is
        # already an explicit statement about which boxes these are.
        options["known_hosts"] = str(self.known_hosts) if self.known_hosts else None

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
                        yield block
