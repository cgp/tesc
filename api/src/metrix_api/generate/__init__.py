"""Turning a service description into a draft plan.

**The tool does the mechanical part; the judgment stays with whoever reviews it**
(§8.1). Given a description of a service this emits one call per operation, with
parameters filled from the schema's own types and examples, assertions taken from the
response codes it declares, and a starter mixture with flat weights. Nothing here
guesses at intent.

**There is no model inside this.** Generation is deterministic: the same input gives
the same skeleton, so a diff between two generated plans means the service changed
and not that a sampler rolled differently. That is the property that makes a
generated plan worth regenerating.

**Chains are never inferred** (§8.3). Guessing that one call feeds another from
repeated ids in a capture is unreliable in exactly the cases that matter, and a wrong
chain is worse than none: it yields a plan that runs cleanly while testing a flow the
service does not have. Every generated chain is one step long, and the todo list says
where a sequence probably belongs.

What comes back is marked a draft and carries its todos with it, because the gap
between "a skeleton arrived" and "somebody decided this is the mixture" is exactly
where a provisional plan gets mistaken for a reviewed one.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from typing import Any

from metrix_api.plans import MIX, document_bytes

#: Where generated calls are written inside a plan. One file: a plan's calls come
#: from one source at a time, and regenerating replaces the file rather than merging
#: into it.
CALLS_FILE = "calls/generated.json"

#: The sidecar naming a plan as generated but unreviewed. Deliberately not a field in
#: `mix.json`: that document's shape is the engine's, the plan hash covers its bytes,
#: and a control-plane review state has no business moving a run's identity or
#: appearing in a schema the load generator parses.
DRAFT_FILE = "draft.json"

#: The starter volume. 75/s for a minute clears the §12.1 floor with room, so a
#: generated plan is runnable as it stands rather than arriving already warned about.
#: It is a placeholder all the same, and says so in the todos.
STARTER_RATE = 75
STARTER_DURATION = "60s"

_SLUG = re.compile(r"[^a-zA-Z0-9]+")


class GenerationError(Exception):
    """A source that cannot be read as what it claims to be."""


@dataclass(frozen=True, slots=True)
class Todo:
    """One thing the generator could not decide, and where it sits.

    `where` is a path into the draft rather than a sentence about it, so the editor
    can point at the field instead of making somebody search for it.
    """

    where: str
    message: str

    def to_document(self) -> dict[str, str]:
        return {"where": self.where, "message": self.message}


@dataclass(frozen=True, slots=True)
class Operation:
    """One request a source described.

    Every source reduces to this before anything is written, so the differences
    between an OpenAPI document and an afternoon of access logs stop here rather than
    running through the whole generator.
    """

    name: str
    method: str
    path: str
    description: str | None = None
    headers: dict[str, str] = field(default_factory=dict)
    query: dict[str, str] = field(default_factory=dict)
    body: Any = None
    asserts: list[dict[str, Any]] = field(default_factory=list)
    #: Share of observed traffic, when the source counted any. `None` means the
    #: source described a shape rather than a history, and the weights will be flat.
    weight: float | None = None
    todos: list[Todo] = field(default_factory=list)


@dataclass(frozen=True, slots=True)
class Draft:
    """A generated plan, and everything about it that is still provisional."""

    name: str
    source: str
    mix: dict[str, Any]
    calls: dict[str, Any]
    todos: list[Todo]
    #: True when the weights came from counted traffic rather than from division.
    observed_weights: bool

    def to_document(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "source": self.source,
            "draft": True,
            "observed_weights": self.observed_weights,
            "mix": self.mix,
            "calls": {CALLS_FILE: self.calls},
            "todos": [todo.to_document() for todo in self.todos],
        }

    def marker(self) -> dict[str, Any]:
        """The `draft.json` sidecar: what made this plan, and what is unfinished."""
        return {
            "draft": True,
            "source": self.source,
            "observed_weights": self.observed_weights,
            "todos": [todo.to_document() for todo in self.todos],
        }


def slug(text: str) -> str:
    """A call name from free text: lowercase, hyphens, nothing else."""
    cleaned = _SLUG.sub("-", text).strip("-").lower()
    return cleaned or "call"


def unique(name: str, taken: set[str]) -> str:
    """A name nothing else in this plan has.

    Numbered rather than hashed, because the number is a signal in itself: two
    operations that collapsed to one name are usually two that want reading.
    """
    if name not in taken:
        taken.add(name)
        return name
    for index in range(2, 1000):
        candidate = f"{name}-{index}"
        if candidate not in taken:
            taken.add(candidate)
            return candidate
    raise GenerationError(f"cannot find a free name for {name!r}")


def shares(operations: list[Operation]) -> list[float]:
    """Each chain's percentage, totalling exactly 100.

    Weighted by observation when the source counted requests, flat when it described
    a shape. Either way the residual from rounding is given to the largest share
    rather than left on the floor: a generated plan that will not run because its
    percentages come to 99.99 is a generated plan nobody trusts again.
    """
    if not operations:
        return []
    weights = [op.weight for op in operations]
    if any(weight is None for weight in weights):
        weights = [1.0] * len(operations)
    total = sum(weights) or 1.0
    percents = [round(weight / total * 100, 4) for weight in weights]
    largest = max(range(len(percents)), key=lambda i: percents[i])
    percents[largest] = round(percents[largest] + (100 - sum(percents)), 4)
    return percents


def draft(name: str, source: str, operations: list[Operation]) -> Draft:
    """Assemble the two documents and the list of what is still guesswork."""
    if not operations:
        raise GenerationError(f"{source}: no operations found to generate calls from")

    calls: dict[str, Any] = {}
    for operation in operations:
        call: dict[str, Any] = {"method": operation.method, "path": operation.path}
        if operation.description:
            call["description"] = operation.description
        if operation.headers:
            call["headers"] = operation.headers
        if operation.query:
            call["query"] = operation.query
        if operation.body is not None:
            call["body"] = operation.body
        call["assert"] = operation.asserts or [{"status": 200}]
        calls[operation.name] = call

    percents = shares(operations)
    observed = all(op.weight is not None for op in operations)
    mix = {
        "version": 1,
        "name": name,
        "calls": [CALLS_FILE],
        "load": {
            "mode": "fixed",
            "model": "open",
            "rate": STARTER_RATE,
            "duration": STARTER_DURATION,
            "max_concurrency": 200,
        },
        "chains": [
            {
                "name": operation.name,
                "percent": percent,
                # One step, always. See the module docstring: a chain this tool
                # invented is a flow nobody has, tested convincingly.
                "steps": [{"id": "call", "call": operation.name}],
            }
            for operation, percent in zip(operations, percents, strict=True)
        ],
    }

    todos = [
        Todo(
            f"{MIX}/load",
            f"{STARTER_RATE}/s for {STARTER_DURATION} is a placeholder that clears the "
            "sample-count floor; the rate a service should be asked to hold is not "
            "something a schema knows",
        )
    ]
    if not observed:
        todos.append(
            Todo(
                f"{MIX}/chains",
                "the shares are an even split, not a mixture: nothing in a service "
                "description says what it actually gets asked for",
            )
        )
    todos.append(
        Todo(
            f"{MIX}/chains",
            "every chain is one step long — generation never infers a sequence, so "
            "any flow where one call feeds the next has to be written",
        )
    )
    for operation in operations:
        todos.extend(operation.todos)

    return Draft(
        name=name,
        source=source,
        mix=mix,
        calls=calls,
        todos=todos,
        observed_weights=observed,
    )


def parse_document(content: str, *, where: str) -> Any:
    """Read a description as JSON, or as the YAML most of them are written in."""
    text = content.strip()
    if not text:
        raise GenerationError(f"{where}: the document is empty")
    if text.startswith(("{", "[")):
        try:
            return json.loads(text)
        except json.JSONDecodeError as exc:
            raise GenerationError(
                f"{where}: invalid JSON at line {exc.lineno}, column {exc.colno}"
            ) from exc
    try:
        import yaml
    except ModuleNotFoundError:  # pragma: no cover - declared in pyproject
        raise GenerationError(f"{where}: YAML support is not installed") from None
    try:
        # `safe_load` and nothing else: a description fetched from a service someone
        # else runs must not be able to construct objects here.
        return yaml.safe_load(text)
    except yaml.YAMLError as exc:
        raise GenerationError(f"{where}: not valid JSON or YAML — {str(exc)[:120]}") from exc


def write(root, draftish: Draft) -> None:
    """Write a draft into a plan directory, bytes-for-bytes as a plan is written."""
    (root / "calls").mkdir(parents=True, exist_ok=True)
    (root / MIX).write_bytes(document_bytes(draftish.mix))
    (root / CALLS_FILE).write_bytes(document_bytes(draftish.calls))
    (root / DRAFT_FILE).write_bytes(document_bytes(draftish.marker()))


def generate(kind: str, content: str, *, name: str) -> Draft:
    """One source, one draft. The dispatch is here so the sources stay unaware."""
    from metrix_api.generate import openapi, swagger, traffic, wadl, wsdl

    sources = {
        "openapi": openapi.operations,
        "swagger": swagger.operations,
        "wadl": wadl.operations,
        "wsdl": wsdl.operations,
        "har": traffic.from_har,
        "access_log": traffic.from_access_log,
        "routes": traffic.from_routes,
    }
    if kind not in sources:
        raise GenerationError(f"unknown source {kind!r}; expected one of {', '.join(sources)}")
    return draft(name, kind, sources[kind](content))
