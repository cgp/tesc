"""Target profiles: what the UI lists and inspects."""

from __future__ import annotations

import sqlite3
from datetime import UTC, datetime
from typing import Any

from fastapi import APIRouter, Body, Depends, HTTPException, Response

from metrix_api import profiles, reachability
from metrix_api.config import Config, format_duration
from metrix_api.deps import get_config, get_db
from metrix_api.discovery import DiscoveryError, Resolver, to_endpoints
from metrix_api.observer import transport_for
from metrix_api.store import inventories

router = APIRouter(prefix="/api/profiles", tags=["profiles"])


def _collects_from(endpoint: profiles.Endpoint) -> str | None:
    """Where host statistics will be read from, exactly as the collector will read it.

    Built from the transport rather than re-formatted here, so the page cannot show
    an address the collector does not use. `address` is the load target and says
    nothing about this: the two are different sockets and usually different ports.
    """
    if endpoint.collect.transport == "none":
        return None
    try:
        return transport_for(endpoint).describe()
    except ValueError as exc:  # an unusable collect block, e.g. a host we cannot infer
        return f"unusable: {exc}"


def _discover(profile: profiles.Profile) -> dict[str, Any] | None:
    """Where the endpoints come from, when they are not written down."""
    if profile.discover is None:
        return None
    return {
        "source": profile.discover.source,
        "hostname": profile.discover.hostname,
        "cluster": profile.discover.cluster,
        "service": profile.discover.service,
        "ttl": format_duration(profile.discover.ttl),
        "transport": profile.discover.collect.transport,
    }


def _summary(profile: profiles.Profile) -> dict[str, Any]:
    return {
        "name": profile.name,
        "description": profile.description,
        "addressing": profile.addressing,
        "discover": _discover(profile),
        "endpoints": [
            {
                "id": e.id,
                "address": e.address,
                "host_header": e.host_header,
                "load": e.load,
                "transport": e.collect.transport,
                "collects_from": _collects_from(e),
                "attributes": e.attributes,
            }
            for e in profile.endpoints
        ],
        # What the observer can actually reach, which is what a recording will cover.
        "observed": [e.id for e in profile.observed],
        # Where traffic goes: the default selection for the engine's targets
        # document. Usually disjoint from `observed` -- see Profile.targets.
        "targets": [e.id for e in profile.targets],
    }


@router.get("")
def list_profiles(
    config: Config = Depends(get_config), conn: sqlite3.Connection = Depends(get_db)
) -> dict[str, Any]:
    names = profiles.list_profiles(config)
    items, broken = [], []
    for name in names:
        try:
            resolved, inventory = _resolved(conn, profiles.load_profile(config, name))
            items.append({**_summary(resolved), "inventory": inventory})
        except profiles.ProfileError as exc:
            # A profile that will not parse is reported rather than dropped: an
            # environment silently missing from the list is worse than a visible error.
            broken.append({"name": name, "error": str(exc)})
    return {"profiles": items, "broken": broken}


@router.get("/{name}")
def get_profile(
    name: str,
    config: Config = Depends(get_config),
    conn: sqlite3.Connection = Depends(get_db),
) -> dict[str, Any]:
    try:
        profile = profiles.load_profile(config, name)
    except profiles.ProfileError as exc:
        raise HTTPException(status_code=404, detail=str(exc)) from exc
    resolved, inventory = _resolved(conn, profile)
    return {**_summary(resolved), "inventory": inventory}


def _resolved(
    conn: sqlite3.Connection, profile: profiles.Profile
) -> tuple[profiles.Profile, dict[str, Any] | None]:
    """A discovered profile, filled in from the snapshot already stored for it.

    Reads the store, never AWS: opening a page must not start a walk. A profile whose
    inventory has never been resolved comes back with no endpoints, which is the
    truth about it.
    """
    if profile.discover is None:
        return profile, None
    stored = inventories.latest(conn, profile=profile.name)
    if stored is None:
        return profile, None
    endpoints, notes = to_endpoints(
        stored.inventory,
        addressing=profile.addressing,
        collect=profile.discover.collect,
        host_header=profile.discover.header,
        tls=profile.discover.tls,
    )
    return profile.with_endpoints(endpoints), _inventory_summary(stored, notes)


def _inventory_summary(
    stored: inventories.Stored, extra: list[Any] | None = None
) -> dict[str, Any]:
    return {
        "id": stored.id,
        "source": stored.inventory.source,
        "reached": stored.inventory.reached,
        "partial": stored.inventory.partial,
        "discovered_at": stored.discovered_at,
        "confirmed_at": stored.confirmed_at,
        "resources": stored.inventory.to_document()["resources"],
        "notes": [
            {"hop": n.hop, "message": n.message}
            for n in [*stored.inventory.notes, *(extra or [])]
        ],
    }


@router.post("/{name}/resolve")
def resolve_profile(
    name: str,
    config: Config = Depends(get_config),
    conn: sqlite3.Connection = Depends(get_db),
) -> dict[str, Any]:
    """Walk discovery now and store what it finds.

    Always a fresh walk: this endpoint exists because someone pressed a button, and
    handing them the cached answer they were trying to get past would be a button
    that does nothing.
    """
    try:
        profile = profiles.load_profile(config, name)
    except profiles.ProfileError as exc:
        raise HTTPException(status_code=404, detail=str(exc)) from exc

    resolver = Resolver(conn=conn, aws=config.aws)
    try:
        resolution = resolver.resolve(profile, force=True)
    except DiscoveryError as exc:
        raise HTTPException(status_code=502, detail=str(exc)) from exc
    except profiles.ProfileError as exc:
        raise HTTPException(status_code=422, detail=str(exc)) from exc
    except Exception as exc:  # ResolveError: the profile has nothing to resolve
        raise HTTPException(status_code=422, detail=str(exc)) from exc

    return {
        **_summary(profile.with_endpoints(resolution.endpoints)),
        "inventory": _inventory_summary(resolution.stored, resolution.notes),
        "cached": resolution.cached,
    }


@router.post("/{name}/verify")
async def verify_profile(
    name: str,
    config: Config = Depends(get_config),
    conn: sqlite3.Connection = Depends(get_db),
) -> dict[str, Any]:
    """Check that every endpoint can actually be reached, right now.

    Two questions per endpoint, reported separately because they fail for different
    reasons: does the load target accept a connection, and does the collector answer
    a real probe. Never run as a side effect of loading a page -- it is several
    seconds of timeouts against someone else's network.
    """
    try:
        profile = profiles.load_profile(config, name)
    except profiles.ProfileError as exc:
        raise HTTPException(status_code=404, detail=str(exc)) from exc

    # A discovered profile is checked against what it last resolved to. Verifying
    # against a fresh walk would conflate two failures -- discovery is broken, and
    # the boxes are unreachable -- which are fixed in different places.
    resolved, _ = _resolved(conn, profile)
    report = await reachability.verify(
        resolved, timeout=config.observe.timeout.total_seconds()
    )
    return {
        "profile": report.profile,
        "ok": report.ok,
        "summary": report.summary(),
        "checked_at": datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "checks": [
            {
                "endpoint": c.endpoint,
                "kind": c.kind,
                "result": c.result,
                "address": c.address,
                "detail": c.detail,
                "ms": c.ms,
            }
            for c in report.checks
        ],
    }


@router.get("/{name}/targets")
def get_targets(
    name: str,
    only: str | None = None,
    config: Config = Depends(get_config),
    conn: sqlite3.Connection = Depends(get_db),
) -> dict[str, Any]:
    """The engine's `targets.json`, built from this profile.

    This is the point where a profile becomes something the engine understands, and
    the engine knows nothing about profiles. The shape is held to
    `schema/targets.schema.json` by the test suite rather than at runtime: the schema
    is the contract between two programs, so it is checked where a mismatch can be
    fixed, not where it can only be reported.

    `only` selects a subset by id, comma-separated. Without it the selection is the
    profile's own: the endpoints that take traffic.
    """
    try:
        profile = profiles.load_profile(config, name)
    except profiles.ProfileError as exc:
        raise HTTPException(status_code=404, detail=str(exc)) from exc

    resolved, _ = _resolved(conn, profile)
    chosen = [i for i in (only or "").split(",") if i] or None
    try:
        document = profiles.to_targets(resolved, only=chosen)
    except profiles.ProfileError as exc:
        raise HTTPException(status_code=422, detail=str(exc)) from exc

    return document


@router.get("/{name}/document")
def get_profile_document(name: str, config: Config = Depends(get_config)) -> dict[str, Any]:
    """The profile exactly as it is written on disk.

    What the editor loads, and what a person gets if they would rather keep the file
    in version control than click through a form.
    """
    try:
        return profiles.to_document(profiles.load_profile(config, name))
    except profiles.ProfileError as exc:
        raise HTTPException(status_code=404, detail=str(exc)) from exc


#: Stands in for the file path in validation messages. Stripped before the message
#: is returned -- see `_validated`.
SUBMITTED = "<submitted>"


def _validated(document: Any, *, name: str | None = None) -> profiles.Profile:
    """Parse a submitted document, turning a rejection into a 422.

    The editor sends a whole document rather than a patch, so there is one write
    path and it is the same validation a hand-written file goes through. The
    messages already name the field and the reason, so they are passed through
    rather than restated -- minus the source prefix, which is a useful file path
    when a profile is loaded from disk and noise beside a form that submitted it.
    """
    try:
        return profiles.parse_profile(document, name=name, source=SUBMITTED)
    except profiles.ProfileError as exc:
        raise HTTPException(
            status_code=422, detail=str(exc).removeprefix(f"{SUBMITTED}: ")
        ) from exc


@router.post("", status_code=201)
def create_profile(
    document: Any = Body(...), config: Config = Depends(get_config)
) -> dict[str, Any]:
    profile = _validated(document)
    if profiles.profile_path(config, profile.name).exists():
        raise HTTPException(
            status_code=409,
            detail=f"a profile named {profile.name!r} already exists; edit it instead",
        )
    profiles.save_profile(config, profile)
    return _summary(profile)


@router.put("/{name}")
def replace_profile(
    name: str, document: Any = Body(...), config: Config = Depends(get_config)
) -> dict[str, Any]:
    """Replace a profile in place. The name is fixed.

    A profile's name is part of a recording's series identity, so renaming one would
    silently split its history in two. Renaming is a copy and a delete, done
    deliberately, not a side effect of editing an endpoint.
    """
    if not profiles.profile_path(config, name).is_file():
        raise HTTPException(status_code=404, detail=f"no profile named {name!r}")
    profile = _validated(document, name=name)
    profiles.save_profile(config, profile)
    return _summary(profile)


@router.delete("/{name}", status_code=204)
def remove_profile(name: str, config: Config = Depends(get_config)) -> Response:
    if not profiles.delete_profile(config, name):
        raise HTTPException(status_code=404, detail=f"no profile named {name!r}")
    return Response(status_code=204)
