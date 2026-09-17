"""Swagger 2.0 as a source of calls.

Swagger and OpenAPI 3 describe the same useful things in different places.  This
adapter reads Swagger directly into :class:`Operation`: it is not a document
converter, does not contact a converter, and leaves no second schema format behind.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import urlencode

from metrix_api.generate import (
    GenerationError,
    Operation,
    Todo,
    openapi,
    parse_document,
    slug,
    unique,
)

WHERE = "swagger"
METHODS = ("get", "put", "post", "delete", "patch", "head", "options")


def operations(content: str) -> list[Operation]:
    document = parse_document(content, where=WHERE)
    if not isinstance(document, dict):
        raise GenerationError(f"{WHERE}: expected an object at the top level")
    if document.get("swagger") != "2.0":
        raise GenerationError(f"{WHERE}: expected a Swagger 2.0 document")
    paths = document.get("paths")
    if not isinstance(paths, dict):
        raise GenerationError(f"{WHERE}: no `paths` object, so there are no operations to read")

    base_path = document.get("basePath", "")
    if not isinstance(base_path, str) or (base_path and not base_path.startswith("/")):
        raise GenerationError(f"{WHERE}: `basePath` must be an absolute path")
    base_path = "" if base_path == "/" else base_path.rstrip("/")
    secured = bool(document.get("security")) or bool(document.get("securityDefinitions"))

    found: list[Operation] = []
    taken: set[str] = set()
    for path, item in sorted(paths.items()):
        if not isinstance(path, str) or not isinstance(item, dict):
            continue
        shared = _parameter_list(item.get("parameters"))
        for method in METHODS:
            operation = item.get(method)
            if isinstance(operation, dict):
                found.append(
                    _operation(
                        document,
                        f"{base_path}{path}" or "/",
                        method.upper(),
                        operation,
                        shared,
                        taken,
                        secured,
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
    shared: list[dict[str, Any]],
    taken: set[str],
    secured: bool,
) -> Operation:
    name = unique(slug(operation.get("operationId") or f"{method} {path}"), taken)
    at = f"calls/generated.json/{name}"
    todos: list[Todo] = []
    rendered = path
    query: dict[str, str] = {}
    headers: dict[str, str] = {}
    parameters = _parameters(document, shared, _parameter_list(operation.get("parameters")))

    for parameter in parameters:
        location = parameter.get("in")
        key = parameter.get("name")
        if not isinstance(key, str) or not key:
            continue
        value = _value(document, parameter)
        if location == "path":
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
                rendered = rendered.replace("{" + key + "}", _text(value))
        elif location == "query" and parameter.get("required"):
            query[key] = _text(value) if value is not None else "{{ " + key + " }}"
            if value is None:
                todos.append(
                    Todo(f"{at}/query/{key}", f"required query parameter {key!r} has no example")
                )
        elif location == "header" and parameter.get("required"):
            headers[key] = _text(value) if value is not None else "{{ " + key + " }}"

    body, content_type, body_todo = _body(document, operation, parameters, at)
    if content_type:
        headers.setdefault("Content-Type", content_type)
    if body_todo:
        todos.append(body_todo)
    if secured or "security" in operation:
        todos.append(
            Todo(
                "mix.json/auth",
                "the description declares security schemes; authentication is set "
                "once in the mix rather than as a header on every call",
            )
        )

    asserts, assert_todo = openapi._asserts(operation, at)
    if assert_todo:
        todos.append(assert_todo)
    return Operation(
        name=name,
        method=method,
        path=rendered,
        description=openapi._description(operation),
        headers=headers,
        query=query,
        body=body,
        asserts=asserts,
        todos=todos,
    )


def _parameter_list(value: Any) -> list[dict[str, Any]]:
    return [item for item in value if isinstance(item, dict)] if isinstance(value, list) else []


def _parameters(
    document: dict[str, Any], shared: list[dict[str, Any]], own: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    """Operation parameters replace matching path-level parameters, as Swagger says."""
    combined: dict[tuple[str, str], dict[str, Any]] = {}
    for raw in [*shared, *own]:
        parameter = openapi._resolve(document, raw)
        if not isinstance(parameter, dict):
            continue
        name, location = parameter.get("name"), parameter.get("in")
        if isinstance(name, str) and isinstance(location, str):
            combined[(name, location)] = parameter
    return list(combined.values())


def _value(document: dict[str, Any], parameter: dict[str, Any]) -> Any:
    """Examples and defaults precede types, including Swagger's top-level fields."""
    for key in ("x-example", "example", "default"):
        if key in parameter:
            return parameter[key]
    enum = parameter.get("enum")
    if isinstance(enum, list) and enum:
        return sorted(enum, key=repr)[0]
    return openapi._example(document, parameter)


def _body(
    document: dict[str, Any], operation: dict[str, Any], parameters: list[dict[str, Any]], at: str
) -> tuple[str | None, str | None, Todo | None]:
    body_parameter = next((p for p in parameters if p.get("in") == "body"), None)
    if isinstance(body_parameter, dict):
        media_type = _content_type(operation, document, "application/json")
        example = _value(document, body_parameter)
        if example is not None:
            return openapi._serialise(example, media_type), media_type, None
        schema = openapi._resolve(document, body_parameter.get("schema"))
        if not isinstance(schema, dict):
            return None, media_type, Todo(f"{at}/body", "the request body has no schema or example")
        skeleton = openapi._from_schema(document, schema, set(), 0)
        if skeleton is None:
            return None, media_type, Todo(
                f"{at}/body",
                "the request body's schema could not be followed to a shape, so no body "
                "was written; a `$ref` to another document is not fetched",
            )
        return (
            openapi._serialise(skeleton, media_type),
            media_type,
            Todo(
                f"{at}/body",
                "the body is built from the schema's types, not from an example: the "
                "values are the right shape and mean nothing",
            ),
        )

    form = [parameter for parameter in parameters if parameter.get("in") == "formData"]
    if not form:
        return None, None, None
    media_type = _content_type(operation, document, "application/x-www-form-urlencoded")
    if media_type == "multipart/form-data":
        return None, None, Todo(
            f"{at}/body", "multipart form data needs a file or boundary and is not generated"
        )
    fields: list[tuple[str, str]] = []
    missing: list[str] = []
    for parameter in sorted(form, key=lambda item: str(item.get("name", ""))):
        if not parameter.get("required"):
            continue
        name = parameter.get("name")
        if not isinstance(name, str) or not name:
            continue
        value = _value(document, parameter)
        fields.append((name, _text(value) if value is not None else "{{ " + name + " }}"))
        if value is None:
            missing.append(name)
    if not fields:
        return None, media_type, None
    todo = (
        Todo(f"{at}/body", f"required form parameter(s) {', '.join(missing)} have no example")
        if missing
        else None
    )
    return urlencode(fields), media_type, todo


def _content_type(operation: dict[str, Any], document: dict[str, Any], default: str) -> str:
    values = operation.get("consumes", document.get("consumes", []))
    choices = (
        [value for value in values if isinstance(value, str)] if isinstance(values, list) else []
    )
    for preferred in (default, "application/json"):
        if preferred in choices:
            return preferred
    return sorted(choices)[0] if choices else default


def _text(value: Any) -> str:
    return str(value).lower() if isinstance(value, bool) else str(value)
