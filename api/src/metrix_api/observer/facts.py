"""What a box *is*, as opposed to what it is doing.

Identity -- OS, kernel, architecture, core count -- does not change during a run, so
it is not a series. Storing it once per recording keeps it out of every chart while
still answering "what was this measured on?" months later, which a number alone never
does.

Filesystem usage is a gauge that *could* be a series, and deliberately is not. What a
run needs to answer is "did this consume disk, and how much", and two readings answer
that for the cost of two `df` calls rather than one per second on every mount.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field

#: Pseudo-filesystems. They report sizes, none of which mean anything about space a
#: run could consume, and on a container host there are dozens of them.
PSEUDO_MOUNT = re.compile(r"^/(proc|sys|dev|run)(/|$)")

#: Identity keys, in the order a person reads them. A transport supplies what it can;
#: anything it cannot is absent rather than guessed.
IDENTITY_KEYS = ("hostname", "os", "kernel", "arch", "cpus")


@dataclass(frozen=True, slots=True)
class Filesystem:
    """One mounted filesystem at one instant."""

    mount: str
    total_bytes: int
    used_bytes: int

    @property
    def available_bytes(self) -> int:
        return max(0, self.total_bytes - self.used_bytes)

    @property
    def used_pct(self) -> float:
        return 100.0 * self.used_bytes / self.total_bytes if self.total_bytes else 0.0


@dataclass(frozen=True, slots=True)
class HostFacts:
    """Everything true of a host at one moment that is not a rate."""

    identity: dict[str, str] = field(default_factory=dict)
    filesystems: list[Filesystem] = field(default_factory=list)

    def __bool__(self) -> bool:
        return bool(self.identity or self.filesystems)


def keep(mount: str) -> bool:
    return not PSEUDO_MOUNT.match(mount)


def parse_df(text: str) -> list[Filesystem]:
    """Read `df -Pk` output.

    POSIX `-P` guarantees one line per filesystem and a fixed column order; `-k`
    guarantees 1024-byte blocks. `-B1` and `--output` would be easier to read and are
    GNU-only, which is no use on the busybox end of the range this has to run on.

    A mount point containing spaces would break the column split, so the mount is
    taken as everything from column six onward rather than as a single field.
    """
    found: list[Filesystem] = []
    for line in text.splitlines()[1:]:  # the header
        fields = line.split(None, 5)
        if len(fields) < 6:
            continue
        try:
            total = int(fields[1]) * 1024
            used = int(fields[2]) * 1024
        except ValueError:
            continue
        mount = fields[5].strip()
        # A zero-sized filesystem is an overlay or a bind mount with nothing behind
        # it; reporting 0/0 as "100% full" would be a false alarm every run.
        if total > 0 and keep(mount):
            found.append(Filesystem(mount=mount, total_bytes=total, used_bytes=used))
    return found


def parse_identity(text: str) -> dict[str, str]:
    """Read `key=value` lines, keeping only the keys we asked for."""
    identity: dict[str, str] = {}
    for line in text.splitlines():
        key, _, value = line.partition("=")
        key = key.strip()
        value = value.strip().strip('"')
        if key in IDENTITY_KEYS and value and value != "unknown":
            identity[key] = value
    return identity
