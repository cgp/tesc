"""WADL 2009/02 as a source of HTTP calls.

WADL describes the same useful boundary as OpenAPI, but as an XML resource tree:
the resources supply paths, methods supply verbs and request/response shapes, and
parameters describe the values that must be bound before a call can run.  This
adapter reads that tree directly.  It does not fetch WADL grammars, method references
or XML schemas named elsewhere in the document.
"""

from __future__ import annotations

import re
import xml.etree.ElementTree as ElementTree
from collections.abc import Iterable
from urllib.parse import urlsplit

from metrix_api.generate import GenerationError, Operation, Todo, slug, unique

WHERE = "wadl"
WADL = "http://wadl.dev.java.net/2009/02"
METHODS = {"GET", "PUT", "POST", "DELETE", "PATCH", "HEAD", "OPTIONS"}
_TEMPLATE = re.compile(r"\{([^{}]+)\}")


def operations(content: str) -> list[Operation]:
    root = _parse(content)
    if root.tag != f"{{{WADL}}}application":
        raise GenerationError(f"{WHERE}: expected a WADL 2009/02 <application> document")

    found: list[Operation] = []
    taken: set[str] = set()
    for resources in root.findall(f"{{{WADL}}}resources"):
        base = _base_path(resources.get("base"))
        for resource in resources.findall(f"{{{WADL}}}resource"):
            found.extend(_resource(resource, base, [], taken))
    if not found:
        raise GenerationError(f"{WHERE}: no resource methods to read")
    return found


def _parse(content: str) -> ElementTree.Element:
    """Read XML without accepting entities that could expand while parsing."""
    if "<!DOCTYPE" in content or "<!ENTITY" in content:
        raise GenerationError(
            f"{WHERE}: the document declares a DOCTYPE or entities, which this does not read"
        )
    try:
        return ElementTree.fromstring(content)
    except ElementTree.ParseError as exc:
        raise GenerationError(f"{WHERE}: not well-formed XML — {exc}") from exc


def _base_path(base: str | None) -> str:
    """A plan has a profile-selected host, so retain only WADL's base path."""
    if not base:
        return "/"
    path = urlsplit(base).path or "/"
    return path if path.startswith("/") else "/" + path


def _resource(
    resource: ElementTree.Element,
    path: str,
    inherited_templates: list[ElementTree.Element],
    taken: set[str],
) -> list[Operation]:
    rendered = _join(path, resource.get("path", ""))
    own = list(resource.findall(f"{{{WADL}}}param"))
    templates = [param for param in own if param.get("style") in {"template", "matrix"}]
    method_params = [*inherited_templates, *own]
    found = [
        _method(method, rendered, method_params, taken)
        for method in resource.findall(f"{{{WADL}}}method")
        if method.get("name")
    ]
    for child in resource.findall(f"{{{WADL}}}resource"):
        # WADL says query/header parameters do not inherit into child resources;
        # template/matrix ones do because they are part of the URI itself.
        found.extend(_resource(child, rendered, [*inherited_templates, *templates], taken))
    return found


def _join(parent: str, child: str) -> str:
    if not child:
        return parent or "/"
    return f"{parent.rstrip('/')}/{child.lstrip('/')}" if parent != "/" else "/" + child.lstrip("/")


def _method(
    method: ElementTree.Element,
    path: str,
    resource_params: list[ElementTree.Element],
    taken: set[str],
) -> Operation:
    verb = (method.get("name") or "").upper()
    if verb not in METHODS:
        raise GenerationError(f"{WHERE}: method {verb!r} is not an HTTP/1.1 method Metrix supports")
    name = unique(slug(method.get("id") or f"{verb} {path}"), taken)
    at = f"calls/generated.json/{name}"
    request = method.find(f"{{{WADL}}}request")
    request_params = request.findall(f"{{{WADL}}}param") if request is not None else []
    params = [*resource_params, *request_params]
    rendered, query, headers, parameter_todos = _parameters(path, params, at)
    todos = parameter_todos

    if request is not None:
        representations = request.findall(f"{{{WADL}}}representation")
        if representations:
            media_type = representations[0].get("mediaType")
            if media_type:
                headers["Content-Type"] = media_type
            todos.append(
                Todo(
                    f"{at}/body",
                    "the WADL declares a request representation but no inline body example; "
                    "add a body before exercising this call",
                )
            )

    asserts, assert_todo = _asserts(method, at)
    if assert_todo:
        todos.append(assert_todo)
    return Operation(
        name=name,
        method=verb,
        path=rendered,
        description=_description(method),
        headers=headers,
        query=query,
        asserts=asserts,
        todos=todos,
    )


def _parameters(
    path: str, params: Iterable[ElementTree.Element], at: str
) -> tuple[str, dict[str, str], dict[str, str], list[Todo]]:
    rendered, query, headers, todos = path, {}, {}, []
    by_name = {param.get("name"): param for param in params if param.get("name")}
    for key in _TEMPLATE.findall(path):
        param = by_name.get(key)
        value = _value(param) if param is not None else None
        rendered = rendered.replace("{" + key + "}", _bound(key, value))
        if value is None:
            todos.append(
                Todo(
                    f"{at}/path",
                    f"{{{{ {key} }}}} has no WADL default or option: bind it to a "
                    "dataset, or make this a step after the call that returns one",
                )
            )

    for param in params:
        name, style = param.get("name"), param.get("style")
        if not name or style not in {"query", "header", "matrix"} or not _required(param):
            continue
        value = _value(param)
        bound = _bound(name, value)
        if style == "query":
            query[name] = bound
        elif style == "header":
            headers[name] = bound
        else:
            rendered += f";{name}={bound}"
        if value is None:
            todos.append(
                Todo(
                    f"{at}/{style}/{name}",
                    f"required {style} parameter {name!r} has no WADL default or option",
                )
            )
    return rendered, query, headers, todos


def _required(param: ElementTree.Element) -> bool:
    return param.get("required", "false").lower() == "true"


def _value(param: ElementTree.Element) -> str | None:
    for attribute in ("fixed", "default"):
        value = param.get(attribute)
        if value is not None:
            return value
    values = sorted(option.get("value", "") for option in param.findall(f"{{{WADL}}}option"))
    return next((value for value in values if value), None)


def _bound(name: str, value: str | None) -> str:
    return value if value is not None else "{{ " + name + " }}"


def _asserts(method: ElementTree.Element, at: str) -> tuple[list[dict[str, object]], Todo | None]:
    codes = sorted(
        {
            int(status)
            for response in method.findall(f"{{{WADL}}}response")
            for status in response.get("status", "").split()
            if status.isdigit() and 200 <= int(status) < 400
        }
    )
    if not codes:
        return [{"status": 200}], Todo(
            f"{at}/assert", "no success response is declared, so the assertion is a bare 200"
        )
    return ([{"status": codes[0]}] if len(codes) == 1 else [{"status_in": codes}]), None


def _description(method: ElementTree.Element) -> str | None:
    doc = method.find(f"{{{WADL}}}doc")
    if doc is None:
        return None
    text = doc.get("title") or " ".join(doc.itertext())
    return " ".join(text.split())[:200] if text and text.strip() else None
