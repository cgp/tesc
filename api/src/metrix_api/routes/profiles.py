"""Target profiles: what the UI lists and inspects."""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Body, Depends, HTTPException, Response

from metrix_api import profiles
from metrix_api.config import Config
from metrix_api.deps import get_config
from metrix_api.observer import transport_for

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


def _summary(profile: profiles.Profile) -> dict[str, Any]:
    return {
        "name": profile.name,
        "description": profile.description,
        "addressing": profile.addressing,
        "endpoints": [
            {
                "id": e.id,
                "address": e.address,
                "host_header": e.host_header,
                "transport": e.collect.transport,
                "collects_from": _collects_from(e),
                "attributes": e.attributes,
            }
            for e in profile.endpoints
        ],
        # What the observer can actually reach, which is what a recording will cover.
        "observed": [e.id for e in profile.observed],
    }


@router.get("")
def list_profiles(config: Config = Depends(get_config)) -> dict[str, Any]:
    names = profiles.list_profiles(config)
    items, broken = [], []
    for name in names:
        try:
            items.append(_summary(profiles.load_profile(config, name)))
        except profiles.ProfileError as exc:
            # A profile that will not parse is reported rather than dropped: an
            # environment silently missing from the list is worse than a visible error.
            broken.append({"name": name, "error": str(exc)})
    return {"profiles": items, "broken": broken}


@router.get("/{name}")
def get_profile(name: str, config: Config = Depends(get_config)) -> dict[str, Any]:
    try:
        return _summary(profiles.load_profile(config, name))
    except profiles.ProfileError as exc:
        raise HTTPException(status_code=404, detail=str(exc)) from exc


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
