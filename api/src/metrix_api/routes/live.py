"""Starting, stopping, and watching a recording."""

from __future__ import annotations

import asyncio
import contextlib
import logging
from collections.abc import AsyncIterator
from typing import Any

from fastapi import APIRouter, Depends, Header, HTTPException, Request
from fastapi.responses import StreamingResponse
from pydantic import BaseModel, Field

from metrix_api import plans
from metrix_api import profiles as profile_store
from metrix_api.config import Config
from metrix_api.deps import get_config
from metrix_api.live import Event, LiveRecording, Registry
from metrix_api.recording import RecordingError

log = logging.getLogger(__name__)

router = APIRouter(prefix="/api/recordings", tags=["live"])

#: Sent when nothing else has been for a while. Without it an idle recording looks
#: identical to a broken connection, and proxies close quiet streams.
HEARTBEAT_S = 15.0


def get_registry(request: Request) -> Registry:
    return request.app.state.registry


class StartRequest(BaseModel):
    profile: str
    #: Name a plan and the recording sends traffic while it watches. Without one it
    #: is an observation: the same collectors, nothing generating load.
    plan: str | None = None
    #: Which of the profile's endpoints take traffic, if not all of them. Never
    #: changes what is watched -- the two are different sockets on different boxes.
    targets: list[str] | None = None
    interval_s: float = Field(default=1.0, gt=0, le=3600)
    groups: list[str] | None = None
    note: str | None = None


@router.post("", status_code=201)
async def start_recording(
    body: StartRequest,
    config: Config = Depends(get_config),
    registry: Registry = Depends(get_registry),
) -> dict[str, Any]:
    """Start watching an environment, and optionally send a plan at it.

    One route for both because they are one thing: a load run is an observation with
    traffic attached, and everything downstream -- the live table, the stream, the
    stop button, the archive -- is the same either way. The difference is `kind`,
    which keeps an environment watched at rest out of the same series as the same
    environment under load.
    """
    from datetime import timedelta

    try:
        profile = profile_store.load_profile(config, body.profile)
    except profile_store.ProfileError as exc:
        raise HTTPException(status_code=404, detail=str(exc)) from exc

    common = {
        "interval": timedelta(seconds=body.interval_s),
        "groups": body.groups,
        "note": body.note,
    }
    try:
        if body.plan:
            live = await registry.start_load(
                config, profile, body.plan, only=body.targets, **common
            )
        else:
            live = await registry.start(config, profile, **common)
    except plans.PlanError as exc:
        # A plan that does not exist is a 404; one that exists and will not run is a
        # 422. Telling those apart is the difference between "typo" and "fix your mix".
        status = 404 if str(exc).startswith("no plan ") else 422
        raise HTTPException(status_code=status, detail=str(exc)) from exc
    except RecordingError as exc:
        # A profile with nothing to collect from, a plan that cannot run, or no engine
        # to run it: the caller's situation rather than a fault.
        raise HTTPException(status_code=422, detail=str(exc)) from exc

    return {
        "recording_id": live.recording_id,
        "profile": profile.name,
        "plan": live.plan_name,
        "status": "running",
    }


@router.post("/{recording_id}/stop")
async def stop_recording(
    recording_id: str, registry: Registry = Depends(get_registry)
) -> dict[str, Any]:
    row = await registry.stop(recording_id)
    if row is None:
        raise HTTPException(status_code=404, detail=f"no live recording {recording_id!r}")
    return {"recording_id": row.id, "status": row.status, "duration_ms": row.duration_ms}


@router.get("/live")
async def list_live(registry: Registry = Depends(get_registry)) -> dict[str, Any]:
    return {"live": registry.ids()}


@router.get("/{recording_id}/stream")
async def stream(
    recording_id: str,
    request: Request,
    registry: Registry = Depends(get_registry),
    last_event_id: str | None = Header(default=None, alias="Last-Event-ID"),
) -> StreamingResponse:
    """Server-Sent Events for one live recording.

    The browser resends `Last-Event-ID` on reconnect; anything missed is replayed, and
    a client that fell past the buffer gets a fresh snapshot instead of a partial
    catch-up that would silently omit the middle.
    """
    live = registry.get(recording_id)
    if live is None:
        raise HTTPException(status_code=404, detail=f"no live recording {recording_id!r}")

    since = _parse_last_id(last_event_id)

    return StreamingResponse(
        _events(live, since, request),
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            # Tell any proxy in the way not to sit on the stream.
            "X-Accel-Buffering": "no",
        },
    )


def _parse_last_id(raw: str | None) -> int | None:
    if not raw:
        return None
    try:
        return int(raw)
    except ValueError:
        return None


async def _events(
    live: LiveRecording, since: int | None, request: Request
) -> AsyncIterator[str]:
    missed, too_old = live.hub.replay_since(since)

    # A snapshot first for a new or resyncing client, so a late joiner is immediately
    # correct rather than blank until something next changes.
    if since is None or too_old:
        yield Event(id=live.hub.last_id, kind="snapshot", data=live.snapshot()).encode()
    for event in missed:
        yield event.encode()

    subscription = live.hub.subscribe()
    try:
        while True:
            try:
                event = await asyncio.wait_for(
                    subscription.__anext__(), timeout=HEARTBEAT_S
                )
            except TimeoutError:
                # A comment line: keeps the connection open and proves it is alive.
                yield ": keepalive\n\n"
                continue
            except StopAsyncIteration:
                return

            if event.kind == "resync":
                yield Event(
                    id=event.id, kind="snapshot", data=live.snapshot()
                ).encode()
            else:
                yield event.encode()

            if await request.is_disconnected():
                return
    finally:
        with contextlib.suppress(Exception):
            await subscription.aclose()
