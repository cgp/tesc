"""The resolved inventory -- what discovery found, in a shape both subsystems read.

Discovery walks a chain of AWS calls (design-api 3.1) and **every hop of it is
optional**. What comes back is therefore not "the answer, or an error" but *how far
it got*: the resources it resolved, the last hop that produced something, and notes
saying what it could not determine. A hostname that resolves straight to an instance
is a complete answer to a shorter question, not a failure.

One inventory serves both consumers (design-api 3.3). It is the engine's endpoint
list and the observer's collection target list, so load metrics and host metrics
attach to the same identities and join without guesswork.

There is no boto3 here. This module is the shape; `ecs.py` is the walk.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any

#: The chain, in the order it is walked. An inventory records the furthest hop that
#: yielded something, which is what "partial resolution" means concretely.
HOPS = ("dns", "load_balancer", "target_group", "service", "task", "instance", "asg")

#: Before any hop produced anything. A hostname nothing recognises stops here.
NOTHING = "nothing"

#: How a resource can be addressed and observed. Not a hierarchy: a task on Fargate
#: has no instance, and an NLB with no ECS behind it has no tasks.
ROLES = ("lb", "instance", "task", "container")

#: The snapshot format. Bumped when a reader would misread an older file -- an
#: inventory outlives the run it was pinned into, which is the point of keeping one.
VERSION = 1

#: Fields written out and read back verbatim. One list, so the two directions cannot
#: drift and quietly drop something discovery paid an API call for.
_PLAIN_FIELDS = (
    "address",
    "port",
    "parent",
    "health",
    "instance_id",
    "instance_type",
    "availability_zone",
    "private_dns",
    "cluster",
    "service",
    "task_arn",
    "task_definition",
    "container",
    "image",
    "image_digest",
)


class InventoryError(Exception):
    """A snapshot that cannot be read. The message names the field and the reason."""


def now_utc() -> str:
    """RFC 3339, UTC, to the second -- the spelling the run directories use."""
    return datetime.now(UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")


@dataclass(frozen=True, slots=True)
class Asg:
    """Autoscaling context. Kept because a host count that moved explains a lot."""

    name: str
    desired: int | None = None
    minimum: int | None = None
    maximum: int | None = None


@dataclass(frozen=True, slots=True)
class Resource:
    """One thing discovery found, carrying the fields design-api 3.3 says to keep.

    Everything but `id` and `role` is optional, because which fields apply depends on
    the role and on how far the walk got. A field that was not determined is absent
    rather than blank: "we did not find out" and "it is empty" are different answers,
    and only the first is worth re-running discovery over.
    """

    id: str
    role: str
    #: Host or IP, and the port it serves on. Two fields rather than one, because an
    #: instance hosting several tasks has an address and no single port.
    address: str | None = None
    port: int | None = None
    #: What this belongs to: a container's task, a task's instance.
    parent: str | None = None
    #: As reported by whichever hop knew -- target health, ECS health, LB state.
    health: str | None = None

    # placement: hardware and location differences are what explain outliers
    instance_id: str | None = None
    instance_type: str | None = None
    availability_zone: str | None = None
    private_dns: str | None = None

    # deployment: which service, and which revision of it
    cluster: str | None = None
    service: str | None = None
    task_arn: str | None = None
    #: `family:revision`. With the digest below, this is what lets a later comparison
    #: say *this is a different build* rather than *this is a regression*.
    task_definition: str | None = None

    # build
    container: str | None = None
    image: str | None = None
    image_digest: str | None = None

    asg: Asg | None = None
    #: Role-specific detail that has not earned a column: LB scheme and type, launch
    #: type, target group name. Opaque, and carried through to endpoint attributes.
    attributes: dict[str, str] = field(default_factory=dict)

    @property
    def endpoint(self) -> str | None:
        """`host:port`, when both are known. IPv6 is bracketed, as profiles want it."""
        if self.address is None or self.port is None:
            return None
        host = f"[{self.address}]" if ":" in self.address else self.address
        return f"{host}:{self.port}"


@dataclass(frozen=True, slots=True)
class Note:
    """Something the walk could not determine, and the hop it stopped at.

    Notes are the useful half of a partial result. They are reported, never raised:
    an NLB with nothing behind it is an answer.
    """

    hop: str
    message: str


@dataclass(frozen=True, slots=True)
class Inventory:
    """A versioned snapshot: stored with the profile, pinned into every run."""

    #: What was asked for -- a hostname, or `cluster/service`.
    source: str
    #: One timestamp for the whole walk. A walk is one instant; per-resource times
    #: would differ only by the latency of the call that found each one.
    discovered_at: str = field(default_factory=now_utc)
    resources: list[Resource] = field(default_factory=list)
    #: The furthest hop that produced something, or `NOTHING`.
    reached: str = NOTHING
    notes: list[Note] = field(default_factory=list)
    version: int = VERSION

    def by_role(self, role: str) -> list[Resource]:
        return [r for r in self.resources if r.role == role]

    def resource(self, resource_id: str) -> Resource | None:
        return next((r for r in self.resources if r.id == resource_id), None)

    def children(self, resource_id: str) -> list[Resource]:
        return [r for r in self.resources if r.parent == resource_id]

    @property
    def partial(self) -> bool:
        """Whether the walk stopped short of the last hop, or noted something."""
        return self.reached != HOPS[-1] or bool(self.notes)

    def to_document(self) -> dict[str, Any]:
        return {
            "version": self.version,
            "source": self.source,
            "discovered_at": self.discovered_at,
            "reached": self.reached,
            "resources": [_resource_document(r) for r in self.resources],
            "notes": [{"hop": n.hop, "message": n.message} for n in self.notes],
        }


def _resource_document(resource: Resource) -> dict[str, Any]:
    doc: dict[str, Any] = {"id": resource.id, "role": resource.role}
    for key in _PLAIN_FIELDS:
        if (value := getattr(resource, key)) is not None:
            doc[key] = value
    if resource.asg is not None:
        doc["asg"] = {
            "name": resource.asg.name,
            **{
                key: value
                for key, value in (
                    ("desired", resource.asg.desired),
                    ("min", resource.asg.minimum),
                    ("max", resource.asg.maximum),
                )
                if value is not None
            },
        }
    if resource.attributes:
        doc["attributes"] = dict(resource.attributes)
    return doc


def from_document(raw: Any, *, source: str = "<inventory>") -> Inventory:
    """Read a snapshot back. The round trip is what makes a pinned inventory useful."""
    if not isinstance(raw, dict):
        raise InventoryError(f"{source}: an inventory must be an object")
    if (version := raw.get("version")) != VERSION:
        raise InventoryError(f"{source}: inventory version {version!r}, expected {VERSION}")
    reached = raw.get("reached", NOTHING)
    if reached != NOTHING and reached not in HOPS:
        raise InventoryError(f"{source}: unknown hop {reached!r}")

    resources = []
    for i, entry in enumerate(raw.get("resources", [])):
        if not isinstance(entry, dict):
            raise InventoryError(f"{source}: resources[{i}] must be an object")
        if entry.get("role") not in ROLES:
            raise InventoryError(
                f"{source}: resources[{i}]: role {entry.get('role')!r} is not one of "
                f"{', '.join(ROLES)}"
            )
        if "id" not in entry:
            raise InventoryError(f"{source}: resources[{i}]: missing 'id'")
        asg = entry.get("asg")
        resources.append(
            Resource(
                id=str(entry["id"]),
                role=str(entry["role"]),
                asg=(
                    Asg(
                        name=str(asg["name"]),
                        desired=asg.get("desired"),
                        minimum=asg.get("min"),
                        maximum=asg.get("max"),
                    )
                    if isinstance(asg, dict)
                    else None
                ),
                attributes={str(k): str(v) for k, v in entry.get("attributes", {}).items()},
                **{key: entry[key] for key in _PLAIN_FIELDS if key in entry},
            )
        )

    return Inventory(
        source=str(raw.get("source", "")),
        discovered_at=str(raw.get("discovered_at", "")),
        resources=resources,
        reached=reached,
        notes=[
            Note(hop=str(n.get("hop", "")), message=str(n.get("message", "")))
            for n in raw.get("notes", [])
        ],
    )
