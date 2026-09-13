"""Plans, and the bundle a plan plus a profile becomes.

One of the two things that cross the boundary to the engine. Everything here exists
so that what the UI runs and what a person runs by hand are the same directory.
"""

from __future__ import annotations

import json
from typing import Any

from fastapi import APIRouter, HTTPException, Query, Request, Response

from metrix_api import plans
from metrix_api.config import Config
from metrix_api.profiles import ProfileError, load_profile

router = APIRouter(prefix="/api/plans", tags=["plans"])


def _config(request: Request) -> Config:
    return request.app.state.config


def _summary(plan: plans.Plan) -> dict[str, Any]:
    load = plan.mix.get("load", {})
    return {
        "name": plan.name,
        "description": plan.mix.get("description"),
        "chains": [
            {"name": c.get("name"), "percent": c.get("percent"), "steps": len(c.get("steps", []))}
            for c in plan.chains
        ],
        "calls": plan.call_names,
        "call_files": sorted(plan.calls),
        "extras": sorted(plan.extras),
        "load": {
            "mode": load.get("mode"),
            "model": load.get("model"),
            "rate": load.get("rate"),
            "duration": load.get("duration"),
        },
        "notes": plan.notes,
    }


@router.get("")
def list_plans(request: Request) -> dict[str, Any]:
    """Every stored plan, and every one that could not be read.

    Broken plans are listed with their reason rather than skipped: one that vanishes
    because it has a typo is one nobody can find in order to fix it.
    """
    found, broken = plans.list_plans(_config(request))
    return {"plans": [_summary(p) for p in found], "broken": broken}


@router.get("/{name}")
def get_plan(name: str, request: Request) -> dict[str, Any]:
    return _summary(_load(request, name))


@router.get("/{name}/bundle")
def get_bundle(
    name: str,
    request: Request,
    profile: str = Query(..., description="The profile whose boxes become targets.json"),
    target: list[str] = Query(default=[], description="Endpoint ids, if not all of them"),
    format: str = Query(default="zip", pattern="^(zip|json)$"),
) -> Response:
    """Assemble the runnable directory: this plan against that profile's boxes.

    A profile is required rather than optional. The stored plan is the mix and the
    calls; the only thing the API adds is `targets.json`, and it is the profile that
    knows how to resolve a hostname into the boxes actually behind it. Without one
    there is nothing here that a `cp -r` would not do.

    `format=json` returns the same bundle as text plus its hash, for a page that
    wants to show what it is about to download without downloading it. The zip is
    the artifact: it unpacks to exactly the directory `metrix-engine --plan` takes.
    """
    bundle = _assemble(request, name, profile, target)
    if format == "json":
        return _json_response(
            {
                "plan": name,
                "profile": profile,
                # What the engine will call this plan, computed by the engine's own
                # rule over these exact bytes. It is part of a run's series identity
                # (§17.2), so it is worth being able to see before a run starts.
                "plan_hash": bundle.hash,
                "hashed": sorted(bundle.hashed),
                "files": {
                    path: content.decode("utf-8", errors="replace")
                    for path, content in sorted(bundle.files.items())
                },
            }
        )

    return Response(
        content=bundle.archive(),
        media_type="application/zip",
        headers={
            "Content-Disposition": f'attachment; filename="{name}-{profile}.zip"',
            # The hash of what is inside, so a download can be tied to a run without
            # unpacking it.
            "X-Metrix-Plan-Hash": bundle.hash,
        },
    )


def _load(request: Request, name: str) -> plans.Plan:
    try:
        return plans.load_plan(_config(request), name)
    except plans.PlanError as exc:
        # 404 only when there is no such plan. A plan that exists and does not
        # validate is a 422: the name resolved, the document is wrong, and telling
        # those apart is the difference between "typo" and "fix your mix".
        status = 404 if str(exc).startswith("no plan ") else 422
        raise HTTPException(status_code=status, detail=str(exc)) from exc


def _assemble(request: Request, name: str, profile_name: str, only: list[str]) -> plans.Bundle:
    plan = _load(request, name)
    try:
        profile = load_profile(_config(request), profile_name)
    except (ProfileError, FileNotFoundError) as exc:
        raise HTTPException(status_code=404, detail=f"no profile {profile_name!r}") from exc
    try:
        return plans.assemble(plan, profile, only=only or None)
    except (plans.PlanError, ProfileError) as exc:
        raise HTTPException(status_code=422, detail=str(exc)) from exc


def _json_response(body: dict[str, Any]) -> Response:
    return Response(
        content=json.dumps(body, indent=2, sort_keys=True) + "\n",
        media_type="application/json",
    )
