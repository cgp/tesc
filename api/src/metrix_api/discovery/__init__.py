"""ECS discovery and the resolved inventory. The ONLY place boto3 is imported.

`ecs.py` walks the chain from a hostname (design-api 3.1); `inventory.py` is the
shape it produces and the snapshot that is pinned into a run. Nothing else in the
tool holds a cloud identity: what leaves here is addresses and identities.
"""

from metrix_api.discovery.ecs import Clients, DiscoveryError, discover
from metrix_api.discovery.inventory import (
    HOPS,
    NOTHING,
    ROLES,
    Asg,
    Inventory,
    InventoryError,
    Note,
    Resource,
    from_document,
    host_key,
    hosts,
    to_endpoints,
)
from metrix_api.discovery.resolve import Change, Resolution, ResolveError, Resolver, compare

__all__ = [
    "HOPS",
    "NOTHING",
    "ROLES",
    "Asg",
    "Change",
    "Clients",
    "DiscoveryError",
    "Inventory",
    "InventoryError",
    "Note",
    "ResolveError",
    "Resolution",
    "Resolver",
    "Resource",
    "compare",
    "discover",
    "from_document",
    "host_key",
    "hosts",
    "to_endpoints",
]
