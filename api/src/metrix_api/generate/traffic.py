"""Sources that counted requests rather than describing them.

These are the valuable ones (§8.2), because they answer the question a schema cannot:
what does this service *actually* get asked for? A mixture whose weights come from
observed traffic is worth considerably more than one whose weights were guessed, and
the difference does not show up until a run is compared against production.

**Nothing is copied out of a capture except the shape of the request.** A HAR from a
browser session carries session cookies, bearer tokens and whatever was in the login
form; an access log carries query strings that were never meant to be re-sent.
Redaction belongs at capture, so what is read here is the method, the path and the
status, and everything else is left where it was found.
"""

from __future__ import annotations

import json
import re
from collections import Counter
from urllib.parse import urlsplit

from metrix_api.generate import GenerationError, Operation, Todo, slug, unique

#: A path segment that is almost certainly an identifier rather than a route. Left as
#: literals, a week of logs becomes four thousand calls that are one operation.
_NUMERIC = re.compile(r"^\d+$")
_UUID = re.compile(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
_HEX = re.compile(r"^[0-9a-fA-F]{16,}$")

#: Combined and common log formats: the request line and the status that followed it.
_LOG = re.compile(r'"(?P<method>[A-Z]+) (?P<target>\S+)[^"]*"\s+(?P<status>\d{3})')

_ROUTE = re.compile(r"^(?P<method>[A-Z]+)\s+(?P<path>/\S*)$")

METHODS = ("GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS")


def _identifier(segment: str) -> bool:
    return bool(_NUMERIC.match(segment) or _UUID.match(segment) or _HEX.match(segment))


def _route(path: str) -> tuple[str, list[str]]:
    """Collapse identifier-looking segments so one operation is one call.

    This is a guess, and the only one these sources make. It is named in the todos
    rather than done quietly, because a service whose routes really are numeric --
    `/v1/`, `/2024/` -- deserves to have the guess visible rather than buried in a
    generated file.
    """
    parts = path.split("/")
    collapsed: list[str] = []
    found: list[str] = []
    for part in parts:
        if part and _identifier(part):
            found.append(part)
            collapsed.append("{{ id" + str(len(found)) + " }}")
        else:
            collapsed.append(part)
    return "/".join(collapsed), found


def _assemble(
    counts: Counter[tuple[str, str]],
    statuses: dict[tuple[str, str], Counter[int]],
    source: str,
) -> list[Operation]:
    """Counted requests into operations, weighted by how often each was seen."""
    if not counts:
        raise GenerationError(f"{source}: no requests found")
    taken: set[str] = set()
    operations = []
    # Descending by count so the mixture reads in the order it matters, ties broken
    # by name: the same capture has to produce the same plan.
    for (method, path), count in sorted(counts.items(), key=lambda item: (-item[1], item[0])):
        template, identifiers = _route(path)
        name = unique(slug(f"{method} {template}"), taken)
        at = f"calls/generated.json/{name}"
        todos = []
        if identifiers:
            todos.append(
                Todo(
                    f"{at}/path",
                    f"{len(identifiers)} path segment(s) were read as identifiers and "
                    f"replaced with templates (for example {identifiers[0]!r}); each "
                    "needs a dataset or an earlier step to come from",
                )
            )
        seen = statuses.get((method, path)) or Counter()
        modal = seen.most_common(1)[0][0] if seen else 200
        if len(seen) > 1:
            todos.append(
                Todo(
                    f"{at}/assert",
                    "the capture shows more than one status for this request "
                    f"({', '.join(str(code) for code in sorted(seen))}); the assertion "
                    f"takes the most common one, {modal}",
                )
            )
        operations.append(
            Operation(
                name=name,
                method=method,
                path=template,
                description=f"Seen {count} time(s) in the {source}",
                asserts=[{"status": modal}],
                weight=float(count),
                todos=todos,
            )
        )
    return operations


def from_har(content: str) -> list[Operation]:
    """A browser or proxy capture: real paths, real frequencies, nothing else."""
    try:
        document = json.loads(content)
    except json.JSONDecodeError as exc:
        raise GenerationError(
            f"har: invalid JSON at line {exc.lineno}, column {exc.colno}"
        ) from exc
    entries = (document or {}).get("log", {}).get("entries")
    if not isinstance(entries, list):
        raise GenerationError("har: no `log.entries`, so this is not a HAR capture")

    counts: Counter[tuple[str, str]] = Counter()
    statuses: dict[tuple[str, str], Counter[int]] = {}
    bodies = 0
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        request = entry.get("request")
        if not isinstance(request, dict):
            continue
        method = str(request.get("method", "")).upper()
        url = request.get("url")
        if method not in METHODS or not isinstance(url, str) or not url:
            continue
        path = urlsplit(url).path or "/"
        key = (method, path)
        counts[key] += 1
        status = (entry.get("response") or {}).get("status")
        if isinstance(status, int) and 100 <= status < 600:
            statuses.setdefault(key, Counter())[status] += 1
        if (request.get("postData") or {}).get("text"):
            bodies += 1

    operations = _assemble(counts, statuses, "har")
    if bodies:
        # Not copied. A capture's bodies are where the passwords are, and a plan is a
        # file that ends up in a repository.
        operations[0].todos.append(
            Todo(
                "calls/generated.json",
                f"{bodies} captured request(s) had a body; none was copied, because a "
                "capture holds whatever was typed into it. Write the bodies, or point "
                "the calls at a dataset",
            )
        )
    return operations


def from_access_log(content: str) -> list[Operation]:
    """Common or combined log format: path frequencies grounded in real traffic."""
    counts: Counter[tuple[str, str]] = Counter()
    statuses: dict[tuple[str, str], Counter[int]] = {}
    read = 0
    for line in content.splitlines():
        match = _LOG.search(line)
        if not match:
            continue
        read += 1
        method = match.group("method").upper()
        if method not in METHODS:
            continue
        path = urlsplit(match.group("target")).path or "/"
        key = (method, path)
        counts[key] += 1
        statuses.setdefault(key, Counter())[int(match.group("status"))] += 1
    if not read:
        raise GenerationError(
            "access_log: no request lines recognised; the common and combined formats "
            'both carry a quoted request line like "GET /path HTTP/1.1" followed by a '
            "status"
        )
    return _assemble(counts, statuses, "access log")


def from_routes(content: str) -> list[Operation]:
    """The bare minimum: a list of method and path lines.

    For when nothing else exists. It yields no weights and no assertions beyond a
    200, which is exactly as much as a list of routes knows.
    """
    taken: set[str] = set()
    operations = []
    for number, line in enumerate(content.splitlines(), start=1):
        text = line.strip()
        if not text or text.startswith("#"):
            continue
        match = _ROUTE.match(text)
        if not match:
            raise GenerationError(
                f"routes: line {number} is not `METHOD /path` — read {text[:60]!r}"
            )
        method, path = match.group("method"), match.group("path")
        if method not in METHODS:
            raise GenerationError(f"routes: line {number} has an unknown method {method!r}")
        template, identifiers = _route(urlsplit(path).path)
        name = unique(slug(f"{method} {template}"), taken)
        todos = []
        if identifiers:
            todos.append(
                Todo(
                    f"calls/generated.json/{name}/path",
                    "a path segment was read as an identifier and replaced with a "
                    "template; it needs somewhere to come from",
                )
            )
        operations.append(
            Operation(method=method, path=template, name=name, todos=todos)
        )
    if not operations:
        raise GenerationError("routes: the list is empty")
    return operations
