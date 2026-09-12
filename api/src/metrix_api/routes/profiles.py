"""Target profiles: what the UI lists and inspects."""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Depends, HTTPException

from metrix_api import profiles
from metrix_api.config import Config
from metrix_api.deps import get_config

router = APIRouter(prefix="/api/profiles", tags=["profiles"])


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
