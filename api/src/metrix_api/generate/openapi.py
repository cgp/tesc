"""OpenAPI 3 as a source of calls.

The richest structural source there is: every operation, its parameters with their
types, the shape of its request body, and the status codes it says it returns. What
it cannot tell anyone is which of those operations a service actually gets asked for,
which is why the weights it produces are flat and say so.

Examples are preferred to types wherever the document carries one. An example is a
value somebody chose because it works; a type is a guess that it is a string.
"""

from __future__ import annotations

import json
from typing import Any

from metrix_api.generate import GenerationError, Operation, Todo, parse_document, slug, unique

WHERE = "openapi"

METHODS = ("get", "put", "post", "delete", "patch", "head", "options")

#: How deep a generated body follows a schema into itself. A recursive schema is
#: normal (a tree node holding children); a body that chases it is not.
MAX_DEPTH = 6


def operations(content: str) -> list[Operation]:
    document = parse_document(content, where=WHERE)
    if not isinstance(document, dict):
        raise GenerationError(f"{WHERE}: expected an object at the top level")
    if "swagger" in document and "openapi" not in document:
        raise GenerationError(
            f"{WHERE}: this is Swagger 2.0; choose the Swagger 2.0 source type so "
            "its bodies and parameters are read by the matching adapter"
        )
    paths = document.get("paths")
    if not isinstance(paths, dict):
        raise GenerationError(f"{WHERE}: no `paths` object, so there are no operations to read")

    found: list[Operation] = []
    taken: set[str] = set()
    secured = bool(document.get("security")) or bool(
        document.get("components", {}).get("securitySchemes")
    )
    for path, item in sorted(paths.items()):
        if not isinstance(item, dict):
            continue
        shared = item.get("parameters", [])
        for method in METHODS:
            operation = item.get(method)
            if isinstance(operation, dict):
                found.append(
                    _operation(
                        document, path, method.upper(), operation, shared, taken, secured
                    )
                )
    if not found:
        raise GenerationError(f"{WHERE}: `paths` holds no operations")
    return found


def _operation(
    document: dict[str, Any],
    path: str,
    method: str,
    operation: dict[str, Any],
    shared: list[Any],
    taken: set[str],
    secured: bool,
) -> Operation:
    name = unique(slug(operation.get("operationId") or f"{method} {path}"), taken)
    todos: list[Todo] = []
    at = f"calls/generated.json/{name}"

    parameters = [
        _resolve(document, parameter)
        for parameter in [*shared, *operation.get("parameters", [])]
        if isinstance(parameter, dict)
    ]

    rendered = path
    query: dict[str, str] = {}
    headers: dict[str, str] = {}
    for parameter in parameters:
        location = parameter.get("in")
        key = parameter.get("name")
        if not isinstance(key, str) or not key:
            continue
        value = _example(document, parameter)
        if location == "path":
            # A path parameter with no example becomes a template rather than a
            # made-up id. A plan that requests /pets/1 against a service whose ids
            # are uuids reports a 404 rate, which reads as a broken service.
            if value is None:
                rendered = rendered.replace("{" + key + "}", "{{ " + key + " }}")
                todos.append(
                    Todo(
                        f"{at}/path",
                        f"{{{{ {key} }}}} has no source: bind it to a dataset, or make "
                        "this a step after the call that returns one",
                    )
                )
            else:
                rendered = rendered.replace("{" + key + "}", str(value))
        elif location == "query" and parameter.get("required"):
            query[key] = str(value) if value is not None else "{{ " + key + " }}"
            if value is None:
                todos.append(
                    Todo(f"{at}/query/{key}", f"required query parameter {key!r} has no example")
                )
        elif location == "header" and parameter.get("required"):
            headers[key] = str(value) if value is not None else "{{ " + key + " }}"

    body, content_type, body_todo = _body(document, operation, at)
    if content_type:
        headers.setdefault("Content-Type", content_type)
    if body_todo:
        todos.append(body_todo)

    if secured:
        # Deliberately not a header on the call. Credentials belong to the mix's auth
        # block, which knows how to refresh one and keeps it out of a document that
        # ends up in a repository.
        todos.append(
            Todo(
                "mix.json/auth",
                "the description declares security schemes; authentication is set "
                "once in the mix rather than as a header on every call",
            )
        )

    asserts, assert_todo = _asserts(operation, at)
    if assert_todo:
        todos.append(assert_todo)

    return Operation(
        name=name,
        method=method,
        path=rendered,
        description=_description(operation),
        headers=headers,
        query=query,
        body=body,
        asserts=asserts,
        todos=todos,
    )


def _description(operation: dict[str, Any]) -> str | None:
    """The summary, which is what a description field is for (§20.3)."""
    for key in ("summary", "description"):
        value = operation.get(key)
        if isinstance(value, str) and value.strip():
            return " ".join(value.split())[:200]
    return None


def _asserts(operation: dict[str, Any], at: str) -> tuple[list[dict[str, Any]], Todo | None]:
    """What the document says success looks like.

    Only the success codes: a load test asserts the path it is exercising, and a
    declared 404 is a documented outcome rather than an expected one.
    """
    responses = operation.get("responses")
    codes = sorted(
        int(code)
        for code in (responses or {})
        if isinstance(code, str) and code.isdigit() and 200 <= int(code) < 400
    )
    if not codes:
        return [{"status": 200}], Todo(
            f"{at}/assert",
            "no success response is declared, so the assertion is a bare 200",
        )
    if len(codes) == 1:
        return [{"status": codes[0]}], None
    return [{"status_in": codes}], None


def _body(
    document: dict[str, Any], operation: dict[str, Any], at: str
) -> tuple[str | None, str | None, Todo | None]:
    """A request body from the example the document carries, or from its types."""
    request = _resolve(document, operation.get("requestBody"))
    if not isinstance(request, dict):
        return None, None, None
    content = request.get("content")
    if not isinstance(content, dict) or not content:
        return None, None, None

    for media_type in ("application/json", *sorted(content)):
        entry = content.get(media_type)
        if isinstance(entry, dict):
            break
    else:  # pragma: no cover - `content` is non-empty, so the loop always breaks
        return None, None, None

    example = entry.get("example")
    if example is None:
        examples = entry.get("examples")
        if isinstance(examples, dict) and examples:
            first = _resolve(document, examples[sorted(examples)[0]])
            if isinstance(first, dict):
                example = first.get("value")
    if example is not None:
        return _serialise(example, media_type), media_type, None

    schema = _resolve(document, entry.get("schema"))
    if not isinstance(schema, dict):
        return None, media_type, Todo(f"{at}/body", "the request body has no schema or example")
    skeleton = _from_schema(document, schema, set(), 0)
    if skeleton is None:
        # A schema that could not be followed -- most often a `$ref` to another
        # document, which is deliberately not fetched. No body at all beats a body
        # of `null`, which is a request nobody meant to send.
        return None, media_type, Todo(
            f"{at}/body",
            "the request body's schema could not be followed to a shape, so no body "
            "was written; a `$ref` to another document is not fetched",
        )
    return (
        _serialise(skeleton, media_type),
        media_type,
        Todo(
            f"{at}/body",
            "the body is built from the schema's types, not from an example: the "
            "values are the right shape and mean nothing",
        ),
    )


def _serialise(value: Any, media_type: str) -> str:
    """Bodies are text in a call document, templated as they are sent."""
    if isinstance(value, str):
        return value
    if "json" in media_type:
        return json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False)
    return str(value)


def _example(document: dict[str, Any], parameter: dict[str, Any]) -> Any:
    """A value somebody wrote down, from wherever the document keeps it."""
    if "example" in parameter:
        return parameter["example"]
    examples = parameter.get("examples")
    if isinstance(examples, dict) and examples:
        first = _resolve(document, examples[sorted(examples)[0]])
        if isinstance(first, dict) and "value" in first:
            return first["value"]
    schema = _resolve(document, parameter.get("schema"))
    if isinstance(schema, dict):
        if "example" in schema:
            return schema["example"]
        if isinstance(schema.get("default"), (str, int, float, bool)):
            return schema["default"]
        enum = schema.get("enum")
        if isinstance(enum, list) and enum:
            # Sorted, not first: the same document must give the same plan, and
            # nothing guarantees the order an enum was written in is stable.
            return sorted(enum, key=repr)[0]
    return None


def _from_schema(document: dict[str, Any], schema: Any, seen: set[int], depth: int) -> Any:
    """A value of the right shape, and no more than that.

    Every field the schema declares is present with a value of its declared type, so
    the service's parser sees what it expects. None of it is meaningful, which is
    what the todo beside it says.
    """
    schema = _resolve(document, schema)
    if not isinstance(schema, dict) or depth > MAX_DEPTH:
        return None
    if "example" in schema:
        return schema["example"]
    if isinstance(schema.get("enum"), list) and schema["enum"]:
        return sorted(schema["enum"], key=repr)[0]
    for combinator in ("allOf", "oneOf", "anyOf"):
        parts = schema.get(combinator)
        if isinstance(parts, list) and parts:
            if combinator == "allOf":
                merged: dict[str, Any] = {}
                for part in parts:
                    value = _from_schema(document, part, seen, depth + 1)
                    if isinstance(value, dict):
                        merged.update(value)
                return merged
            return _from_schema(document, parts[0], seen, depth + 1)

    kind = schema.get("type")
    if kind == "array":
        # One element: the point is the shape, and a load test that posts a
        # thousand-element array because `maxItems` said so is a different test.
        return [_from_schema(document, schema.get("items"), seen, depth + 1)]
    if kind == "object" or "properties" in schema:
        marker = id(schema)
        if marker in seen:
            return {}
        result = {}
        for key, child in sorted((schema.get("properties") or {}).items()):
            result[key] = _from_schema(document, child, seen | {marker}, depth + 1)
        return result
    return {
        "string": _string(schema),
        "integer": 0,
        "number": 0,
        "boolean": False,
        "null": None,
    }.get(kind if isinstance(kind, str) else "")


def _string(schema: dict[str, Any]) -> str:
    """A string of the declared format, so a service that parses one gets one."""
    return {
        "date-time": "2026-01-01T00:00:00Z",
        "date": "2026-01-01",
        "uuid": "00000000-0000-0000-0000-000000000000",
        "email": "user@example.com",
        "uri": "https://example.com",
    }.get(schema.get("format", ""), "string")


def _resolve(document: dict[str, Any], node: Any, depth: int = 0) -> Any:
    """Follow a local `$ref`. Remote ones are left alone rather than fetched.

    Generation reads what it was given. A `$ref` to another URL would make this
    reach out to whatever host the document names, which is not something a plan
    generator should do on someone's behalf.
    """
    if not isinstance(node, dict) or "$ref" not in node or depth > MAX_DEPTH:
        return node
    reference = node["$ref"]
    if not isinstance(reference, str) or not reference.startswith("#/"):
        return node
    target: Any = document
    for part in reference[2:].split("/"):
        part = part.replace("~1", "/").replace("~0", "~")
        if not isinstance(target, dict) or part not in target:
            return node
        target = target[part]
    return _resolve(document, target, depth + 1)
