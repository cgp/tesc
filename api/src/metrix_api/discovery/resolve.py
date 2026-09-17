"""Resolving a profile to endpoints, and noticing when the environment moved.

A profile that discovers is not walked per run. The walk is several seconds of
rate-limited control-plane calls (design-api 3.2), so a resolution is stored and
reused until its TTL expires, refreshed on demand, and refreshed again at every phase
boundary of a recording -- which is the point at which an instance count that changed
is *detected* rather than inferred afterwards from a series that went quiet.

`Change` is what a refresh produces: the hosts that appeared and disappeared. It is
deliberately plain data. Turning it into an annotation is the recording's job, so
nothing here has to know what a recording is.
"""

from __future__ import annotations

import logging
import sqlite3
from dataclasses import dataclass, field
from datetime import datetime

from metrix_api.config import AwsConfig
from metrix_api.discovery.ecs import Clients, discover
from metrix_api.discovery.inventory import Inventory, Note, host_key, hosts, to_endpoints
from metrix_api.profiles import Endpoint, Profile
from metrix_api.store import inventories

log = logging.getLogger(__name__)


class ResolveError(Exception):
    """A profile that cannot be resolved. Not the same as one resolved only partly."""


@dataclass(frozen=True, slots=True)
class Resolution:
    """A snapshot, and the endpoints it produces for this profile."""

    stored: inventories.Stored
    endpoints: list[Endpoint]
    #: What could not be determined: the walk's own notes, plus anything that was
    #: found but could not be written down as an endpoint.
    notes: list[Note] = field(default_factory=list)
    #: Whether this came from the store rather than from a fresh walk.
    cached: bool = False

    @property
    def inventory(self) -> Inventory:
        return self.stored.inventory

    @property
    def observed(self) -> list[Endpoint]:
        return [e for e in self.endpoints if e.collect.transport != "none"]


@dataclass(frozen=True, slots=True)
class Change:
    """What moved between two resolutions of one environment."""

    added: list[str] = field(default_factory=list)
    removed: list[str] = field(default_factory=list)
    before: int = 0
    after: int = 0

    @property
    def moved(self) -> bool:
        return bool(self.added or self.removed)

    def describe(self) -> str:
        parts = []
        if self.added:
            parts.append(f"{len(self.added)} appeared ({', '.join(self.added)})")
        if self.removed:
            parts.append(f"{len(self.removed)} went away ({', '.join(self.removed)})")
        return (
            f"hosts changed during the recording: {self.before} -> {self.after}; "
            + "; ".join(parts)
        )

    @property
    def detail(self) -> dict[str, object]:
        return {
            "added": list(self.added),
            "removed": list(self.removed),
            "before": self.before,
            "after": self.after,
        }


def compare(before: Inventory, after: Inventory) -> Change:
    """Which hosts appeared and disappeared between two snapshots.

    Over the host set, not the whole document: a task that changed its health or was
    redeployed onto the same box is not the environment changing size, and flagging
    it as one would train people to ignore the flag.
    """
    was = {host.id for host in hosts(before)}
    now = {host.id for host in hosts(after)}
    return Change(
        added=sorted(now - was),
        removed=sorted(was - now),
        before=len(was),
        after=len(now),
    )


def unchanged(before: Inventory, after: Inventory) -> bool:
    return host_key(before) == host_key(after)


@dataclass
class Resolver:
    """Resolves profiles, caching in the store and building AWS clients on demand.

    The clients are lazy because most of what this application does never touches
    AWS: an explicit profile, a recording being read back, the page loading. Building
    a session at startup would make a missing `[aws]` section an error for people who
    have no use for one.
    """

    conn: sqlite3.Connection
    aws: AwsConfig = field(default_factory=AwsConfig)
    #: Injectable for tests, which drive the same code against recorded responses.
    clients: Clients | None = None

    def _aws(self) -> Clients:
        if self.clients is None:
            self.clients = Clients.from_config(self.aws)
        return self.clients

    def resolve(
        self, profile: Profile, *, force: bool = False, now: datetime | None = None
    ) -> Resolution:
        """The profile's current endpoints, walking AWS only when the cache is stale."""
        log.info(
            "profile resolve start profile=%s force=%s source=%s",
            profile.name,
            force,
            profile.discover.source if profile.discover else None,
        )
        if profile.discover is None:
            raise ResolveError(
                f"profile {profile.name!r} has no 'discover' block; its endpoints are "
                "written down, so there is nothing to resolve"
            )

        if not force:
            current = inventories.latest(self.conn, profile=profile.name)
            if current is not None and current.fresh(profile.discover.ttl, now=now):
                log.info(
                    "profile resolve cache hit profile=%s inventory=%s", profile.name, current.id
                )
                return self._resolution(current, profile, cached=True)

        walked = discover(
            self._aws(),
            hostname=profile.discover.hostname,
            cluster=profile.discover.cluster,
            service=profile.discover.service,
        )
        stored = inventories.save(self.conn, walked, profile=profile.name, now=now)
        log.info(
            "profile resolve saved profile=%s inventory=%s reached=%s hosts=%s",
            profile.name,
            stored.id,
            walked.reached,
            [host.address for host in hosts(walked)],
        )
        return self._resolution(stored, profile, cached=False)

    def _resolution(
        self, stored: inventories.Stored, profile: Profile, *, cached: bool
    ) -> Resolution:
        assert profile.discover is not None
        endpoints, notes = to_endpoints(
            stored.inventory,
            addressing=profile.addressing,
            collect=profile.discover.collect,
            host_header=profile.discover.header,
            tls=profile.discover.tls,
        )
        return Resolution(
            stored=stored,
            endpoints=endpoints,
            notes=[*stored.inventory.notes, *notes],
            cached=cached,
        )
