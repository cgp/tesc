"""Discovery: turning a hostname into what is actually running behind it.

One endpoint, and it is deliberately an action rather than a resource. A walk is a
few seconds of rate-limited control-plane calls (design-api 3.2), so it happens when
someone asks for it -- at profile setup and refresh -- and never as a side effect of
loading a page.

What comes back is the inventory as it will be stored and pinned (A2.2), including
the notes: a partial answer is the normal one, and the front end shows how far the
walk got rather than an error.
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Body, Depends, HTTPException

from metrix_api.config import Config
from metrix_api.deps import get_config
from metrix_api.discovery import Clients, DiscoveryError, discover

router = APIRouter(prefix="/api/discovery", tags=["discovery"])


@router.post("/resolve")
def resolve(
    body: dict[str, Any] = Body(default_factory=dict),
    config: Config = Depends(get_config),
) -> dict[str, Any]:
    """Resolve a hostname, or an ECS cluster and service named directly.

    Sync rather than async on purpose: boto3 blocks, and FastAPI runs a sync endpoint
    in its thread pool. Making this `async def` would park the event loop -- and with
    it every open SSE stream -- for the length of the walk.
    """
    hostname = (body.get("hostname") or "").strip()
    cluster = (body.get("cluster") or "").strip()
    service = (body.get("service") or "").strip()
    if not hostname and not (cluster and service):
        raise HTTPException(
            status_code=422,
            detail="give a hostname, or both a cluster and a service",
        )

    try:
        clients = Clients.from_config(config.aws)
        inventory = discover(
            clients,
            hostname=hostname or None,
            cluster=cluster or None,
            service=service or None,
        )
    except DiscoveryError as exc:
        # 502: the request was fine and AWS is what did not answer. A 500 here would
        # send someone reading our logs rather than their permissions.
        raise HTTPException(status_code=502, detail=str(exc)) from exc

    return {"inventory": inventory.to_document(), "partial": inventory.partial}
