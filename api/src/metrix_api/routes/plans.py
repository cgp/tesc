"""Plans, and the bundle a plan plus a profile becomes.

One of the two things that cross the boundary to the engine. Everything here exists
so that what the UI runs and what a person runs by hand are the same directory.
"""

from __future__ import annotations

import json
from typing import Any

from fastapi import APIRouter, Body, HTTPException, Query, Request, Response
from pydantic import BaseModel, Field

from metrix_api import generate, plans
from metrix_api.config import Config
from metrix_api.profiles import ProfileError, load_profile

router = APIRouter(prefix="/api/plans", tags=["plans"])


def _config(request: Request) -> Config:
    return request.app.state.config


def _summary(plan: plans.Plan) -> dict[str, Any]:
    """One plan as the list and the editor see it: what it is, and whether it runs.

    The verdict travels with the plan rather than behind a second request. A list
    that shows six plans and says nothing about which of them are runnable is a list
    that has to be clicked through one at a time to find out.
    """
    load = plan.mix.get("load", {})
    problems = plans.check(plan)
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
        # What is still this tool's guesswork rather than somebody's decision.
        "draft": plan.draft,
        # The arithmetic behind the percentages, and everything wrong with them.
        # Computed here so the browser renders an answer rather than reaching one.
        "figures": plans.figures(plan),
        "problems": [problem.to_document() for problem in problems],
        "ready": plans.ready(problems),
    }


@router.get("")
def list_plans(request: Request) -> dict[str, Any]:
    """Every stored plan, and every one that could not be read.

    Broken plans are listed with their reason rather than skipped: one that vanishes
    because it has a typo is one nobody can find in order to fix it.
    """
    found, broken = plans.list_plans(_config(request))
    return {"plans": [_summary(p) for p in found], "broken": broken}


class Generate(BaseModel):
    """A service description, and what to call the plan made from it."""

    name: str
    source: str = Field(description="openapi, swagger, wadl, wsdl, har, access_log or routes")
    content: str = Field(description="The document itself, not a URL to fetch it from")
    save: bool = True


class Regenerate(BaseModel):
    """A newer description of the same service."""

    source: str
    content: str


@router.post("/generate", status_code=201)
def generate_plan(request: Request, body: Generate) -> dict[str, Any]:
    """Turn a service description into a draft plan.

    The mechanical half of authoring (§8.1): one call per operation, parameters from
    the schema's own examples and types, assertions from the codes it declares, and a
    starter mixture with flat weights. There is no model in here — the same document
    gives the same plan, so a diff between two generated plans means the service
    changed.

    What comes back is marked a draft and carries its todos. The judgment half —
    which calls matter, what a real mixture looks like, which of these belong in a
    sequence — is left to whoever reads it, and the todo list is the handover note.

    The document is posted rather than fetched from a URL. A control plane that
    retrieves whatever address it is handed is a request forwarder sitting inside
    someone's network, which is a larger thing than a plan generator.
    """
    config = _config(request)
    if not plans.NAME.match(body.name):
        raise HTTPException(
            status_code=422,
            detail=f"plan {body.name!r}: names are letters, digits, dot, dash, underscore",
        )
    try:
        draft = generate.generate(body.source, body.content, name=body.name)
    except generate.GenerationError as exc:
        raise HTTPException(status_code=422, detail=str(exc)) from exc

    if not body.save:
        # A preview: everything the save would write, written nowhere. Useful for
        # looking at what a description turns into before committing a name to it.
        return draft.to_document()

    root = plans.plan_path(config, body.name)
    if root.exists():
        raise HTTPException(
            status_code=409,
            detail=f"a plan named {body.name!r} already exists; regenerate its calls "
            "instead, which leaves the mixture alone",
        )
    generate.write(root, draft)
    try:
        plan = plans.load_plan(config, body.name)
    except plans.PlanError as exc:  # pragma: no cover - generation validates as it writes
        raise HTTPException(status_code=500, detail=f"generated an unreadable plan: {exc}") from exc
    return {**_summary(plan), "call_details": plans.call_details(plan)}


@router.post("/{name}/regenerate")
def regenerate_calls(name: str, request: Request, body: Regenerate) -> dict[str, Any]:
    """Read the service again, and replace only the calls.

    This is what the document split is for (§8.1). The calls are the mechanical half
    and go stale when the service changes; the mixture is the judgment half and does
    not. Regenerating rewrites `calls/generated.json` and does not touch `mix.json`,
    so a tuned mixture survives an API change.

    A regeneration that would leave a chain naming a call the service no longer has
    is refused rather than written. The plan would still load — until somebody tried
    to run it — and a plan that breaks at the moment of running is the failure this
    tool exists to move earlier.
    """
    config = _config(request)
    existing = _load(request, name)
    try:
        draft = generate.generate(body.source, body.content, name=name)
    except generate.GenerationError as exc:
        raise HTTPException(status_code=422, detail=str(exc)) from exc

    before = set(existing.call_names)
    after = set(draft.calls)
    orphaned = sorted(
        f"{chain.get('name')}/{step.get('id')} calls {step.get('call')!r}"
        for chain in existing.chains
        for step in chain.get("steps", [])
        if step.get("call") not in after
    )
    if orphaned:
        raise HTTPException(
            status_code=409,
            detail="the new description does not define every call this mixture uses: "
            + "; ".join(orphaned),
        )

    root = plans.plan_path(config, name)
    (root / generate.CALLS_FILE).parent.mkdir(parents=True, exist_ok=True)
    (root / generate.CALLS_FILE).write_bytes(plans.document_bytes(draft.calls))
    plan = plans.load_plan(config, name)
    return {
        **_summary(plan),
        "call_details": plans.call_details(plan),
        # What moved, because a regeneration that changed nothing and one that
        # rewrote every call look identical from the outside.
        "added": sorted(after - before),
        "removed": sorted(before - after),
        "unchanged": sorted(after & before),
    }


@router.delete("/{name}/draft")
def accept_draft(name: str, request: Request) -> dict[str, Any]:
    """Say a generated plan has been read. Removes the draft marker.

    Deliberately its own action rather than a side effect of saving: editing one
    percentage is not a review, and a flag that cleared itself on the first edit
    would mark every generated plan reviewed a minute after it was made.
    """
    _load(request, name)
    if not plans.accept_draft(_config(request), name):
        raise HTTPException(status_code=404, detail=f"plan {name!r} is not a draft")
    plan = plans.load_plan(_config(request), name)
    return {**_summary(plan), "call_details": plans.call_details(plan)}


@router.get("/{name}")
def get_plan(name: str, request: Request) -> dict[str, Any]:
    """One plan, with the calls it invokes spelled out.

    The call details ride along because the page that opens a plan is the page that
    reads its chains, and a chain is unreadable without knowing what its steps do.
    """
    plan = _load(request, name)
    return {**_summary(plan), "call_details": plans.call_details(plan)}


@router.get("/{name}/document")
def get_plan_document(name: str, request: Request) -> dict[str, Any]:
    """The mixture exactly as it is written on disk.

    What the editor loads, and what a person gets if they would rather keep the plan
    in version control than click through a form. The summary is not enough to edit
    from: it drops every field the form does not draw, and a round trip through it
    would quietly delete an auth block.
    """
    return _load(request, name).mix


@router.post("/{name}/validate")
def validate_plan(name: str, request: Request, mix: Any = Body(...)) -> dict[str, Any]:
    """What would be wrong with this mixture, without saving it.

    The editor asks this as fields are committed, so the sample-count consequence of
    a duration and a rate is visible while they are being chosen rather than after a
    run has already been made at them. It is the same function the save path and the
    bundle gate call: the browser has no rules of its own to drift from these.

    A document that does not even parse is a 422 with the field named -- that is the
    schema talking, and it is the same message a hand-written file would get.
    """
    plan = _parse(request, name, mix)
    problems = plans.check(plan)
    return {
        "name": name,
        "figures": plans.figures(plan),
        "problems": [problem.to_document() for problem in problems],
        "ready": plans.ready(problems),
    }


@router.put("/{name}")
def replace_plan(name: str, request: Request, mix: Any = Body(...)) -> dict[str, Any]:
    """Write a submitted mixture back over the stored one.

    Saved even when it has errors in it. A half-finished mixture is a normal state to
    leave an afternoon's work in, and an editor that refuses to save until the
    percentages total 100 is an editor that loses work. What errors stop is running:
    the bundle refuses to assemble, and this response says so.
    """
    try:
        plan = plans.save_mix(_config(request), name, mix)
    except plans.PlanError as exc:
        status = 404 if str(exc).startswith("no plan ") else 422
        raise HTTPException(status_code=status, detail=str(exc)) from exc
    return {**_summary(plan), "call_details": plans.call_details(plan)}


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


def _parse(request: Request, name: str, mix: Any) -> plans.Plan:
    """A submitted mixture, checked against the plan's own calls on disk.

    Not saved: this is the document as it stands in the form, read back through the
    loader so that a reference to a call that does not exist is caught by the same
    code that catches it in a file.
    """
    try:
        return plans.parse_mix(_config(request), name, mix)
    except plans.PlanError as exc:
        status = 404 if str(exc).startswith("no plan ") else 422
        raise HTTPException(status_code=status, detail=str(exc)) from exc


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
    """The gate between editing a plan and running one.

    A plan with errors in it does not become a bundle. The engine would reject the
    same document a moment later, and handing someone a zip that cannot run -- named
    after their plan, with a plan hash on it -- is worse than refusing: it looks like
    an artifact. The reasons are listed rather than counted, since the point is to
    fix them.
    """
    plan = _load(request, name)
    problems = plans.check(plan)
    if not plans.ready(problems):
        raise HTTPException(
            status_code=422,
            detail="this plan cannot run yet: "
            + "; ".join(f"{p.where}: {p.message}" for p in problems if p.severity == "error"),
        )
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
