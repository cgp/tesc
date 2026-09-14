"""WSDL 1.1 as a source of calls.

The same job OpenAPI does for REST, for services that answer SOAP: every operation
the port type declares becomes one POST, with an envelope built from the element the
input message names and the parts of it the inline schema describes.

A SOAP body is not optional the way a REST one often is -- an empty envelope is a
fault, not a request -- so the skeleton is built from types wherever the schema can
be followed, and the todos say which parts of it are shaped rather than meant.
"""

from __future__ import annotations

import xml.etree.ElementTree as ElementTree
from typing import Any
from urllib.parse import urlsplit

from metrix_api.generate import GenerationError, Operation, Todo, slug, unique

WHERE = "wsdl"

WSDL = "http://schemas.xmlsoap.org/wsdl/"
SOAP11 = "http://schemas.xmlsoap.org/wsdl/soap/"
SOAP12 = "http://schemas.xmlsoap.org/wsdl/soap12/"
XSD = "http://www.w3.org/2001/XMLSchema"
ENVELOPE11 = "http://schemas.xmlsoap.org/soap/envelope/"
ENVELOPE12 = "http://www.w3.org/2003/05/soap-envelope"

#: How far into a schema an envelope follows a type before it stops. A type that
#: contains itself is ordinary; a body that unrolls it is not.
MAX_DEPTH = 5

#: A value of the right XSD type, and nothing more than that.
PLACEHOLDERS = {
    "string": "string",
    "normalizedString": "string",
    "token": "string",
    "int": "0",
    "integer": "0",
    "long": "0",
    "short": "0",
    "byte": "0",
    "decimal": "0",
    "double": "0",
    "float": "0",
    "boolean": "false",
    "date": "2026-01-01",
    "dateTime": "2026-01-01T00:00:00Z",
    "time": "00:00:00Z",
    "base64Binary": "",
    "anyURI": "https://example.com",
}


def operations(content: str) -> list[Operation]:
    root = _parse(content)
    if _tag(root) != "definitions":
        raise GenerationError(f"{WHERE}: expected a <definitions> document")

    namespace = root.get("targetNamespace") or ""
    schemas = [
        schema
        for types in root.findall(f"{{{WSDL}}}types")
        for schema in types.findall(f"{{{XSD}}}schema")
    ]
    messages = {
        _local(message.get("name", "")): message for message in root.findall(f"{{{WSDL}}}message")
    }
    actions, soap_version = _bindings(root)
    path, address_todo = _address(root)

    found: list[Operation] = []
    taken: set[str] = set()
    for port_type in root.findall(f"{{{WSDL}}}portType"):
        for operation in port_type.findall(f"{{{WSDL}}}operation"):
            found.append(
                _operation(
                    operation,
                    messages=messages,
                    schemas=schemas,
                    namespace=namespace,
                    actions=actions,
                    soap_version=soap_version,
                    path=path,
                    taken=taken,
                )
            )
    if not found:
        raise GenerationError(f"{WHERE}: no <portType> operations to read")
    if address_todo:
        found[0].todos.append(address_todo)
    return found


def _parse(content: str) -> ElementTree.Element:
    """Read the document, refusing one that defines its own entities.

    A WSDL arrives from wherever the person generating had it, and a document that
    declares entities can expand into gigabytes inside the parser. Nothing legitimate
    here needs a DOCTYPE, so the cheapest defence is to decline one rather than to
    parse carefully.
    """
    if "<!DOCTYPE" in content or "<!ENTITY" in content:
        raise GenerationError(
            f"{WHERE}: the document declares a DOCTYPE or entities, which this does "
            "not read; remove them, or convert the service description first"
        )
    try:
        return ElementTree.fromstring(content)
    except ElementTree.ParseError as exc:
        raise GenerationError(f"{WHERE}: not well-formed XML — {exc}") from exc


def _tag(element: ElementTree.Element) -> str:
    return element.tag.rsplit("}", 1)[-1]


def _local(name: str) -> str:
    """The part of a QName after the prefix. Prefixes are the document's business."""
    return name.rsplit(":", 1)[-1]


def _bindings(root: ElementTree.Element) -> tuple[dict[str, str], str]:
    """Each operation's SOAPAction, and which SOAP the service speaks."""
    actions: dict[str, str] = {}
    version = "1.1"
    for binding in root.findall(f"{{{WSDL}}}binding"):
        if binding.find(f"{{{SOAP12}}}binding") is not None:
            version = "1.2"
        for operation in binding.findall(f"{{{WSDL}}}operation"):
            name = operation.get("name")
            if not name:
                continue
            for namespace in (SOAP11, SOAP12):
                element = operation.find(f"{{{namespace}}}operation")
                if element is not None and element.get("soapAction"):
                    actions[name] = element.get("soapAction", "")
    return actions, version


def _address(root: ElementTree.Element) -> tuple[str, Todo | None]:
    """The path requests go to. The host is the profile's business, never the plan's."""
    for namespace in (SOAP11, SOAP12):
        for address in root.iter(f"{{{namespace}}}address"):
            location = address.get("location")
            if location:
                parts = urlsplit(location)
                path = parts.path or "/"
                return path, Todo(
                    "profiles",
                    f"the service is published at {parts.netloc or location!r}; a plan "
                    "carries only the path, and where to send it comes from the "
                    "profile chosen when the bundle is assembled",
                )
    return "/", Todo(
        "calls/generated.json",
        "no <soap:address> in the document, so every call posts to `/`; set the path "
        "the service actually listens on",
    )


def _operation(
    operation: ElementTree.Element,
    *,
    messages: dict[str, ElementTree.Element],
    schemas: list[ElementTree.Element],
    namespace: str,
    actions: dict[str, str],
    soap_version: str,
    path: str,
    taken: set[str],
) -> Operation:
    raw = operation.get("name") or "operation"
    name = unique(slug(raw), taken)
    at = f"calls/generated.json/{name}"
    todos: list[Todo] = []

    input_element = _part_element(operation, "input", messages)
    output_element = _part_element(operation, "output", messages)

    body, shaped = _envelope(input_element, schemas, namespace, soap_version)
    if input_element is None:
        todos.append(
            Todo(
                f"{at}/body",
                "the input message names no element this document declares, so the "
                "envelope body is empty",
            )
        )
    elif shaped:
        todos.append(
            Todo(
                f"{at}/body",
                "the envelope is built from the schema's types: every field is present "
                "and of the right type, and none of the values mean anything",
            )
        )

    headers = {
        "Content-Type": (
            "text/xml; charset=utf-8"
            if soap_version == "1.1"
            else "application/soap+xml; charset=utf-8"
        )
    }
    if soap_version == "1.1":
        # SOAP 1.2 carries the action as a content-type parameter instead, and a
        # stray SOAPAction header on a 1.2 service is at best ignored.
        headers["SOAPAction"] = f'"{actions.get(raw, "")}"'

    asserts: list[dict[str, Any]] = [{"status": 200}]
    if output_element:
        # A SOAP fault is a 200 with a <Fault> in it, so status alone asserts almost
        # nothing. The response element is what says the operation actually ran.
        asserts.append({"xpath": f"//*[local-name()='{output_element}']", "exists": True})
    else:
        todos.append(
            Todo(
                f"{at}/assert",
                "no output element is declared, so only the status is asserted — a "
                "SOAP fault is a 200 with a <Fault> in it",
            )
        )

    documentation = operation.find(f"{{{WSDL}}}documentation")
    description = None
    if documentation is not None and (documentation.text or "").strip():
        description = " ".join((documentation.text or "").split())[:200]

    return Operation(
        name=name,
        method="POST",
        path=path,
        description=description,
        headers=headers,
        body=body,
        asserts=asserts,
        todos=todos,
    )


def _part_element(
    operation: ElementTree.Element, direction: str, messages: dict[str, ElementTree.Element]
) -> str | None:
    """The element name an input or output message carries."""
    node = operation.find(f"{{{WSDL}}}{direction}")
    if node is None:
        return None
    message = messages.get(_local(node.get("message", "")))
    if message is None:
        return None
    for part in message.findall(f"{{{WSDL}}}part"):
        element = part.get("element")
        if element:
            return _local(element)
    return None


def _envelope(
    element: str | None, schemas: list[ElementTree.Element], namespace: str, version: str
) -> tuple[str, bool]:
    """The request as it goes on the wire, with the body shaped by the schema."""
    envelope_ns = ENVELOPE11 if version == "1.1" else ENVELOPE12
    if element is None:
        inner, shaped = "", False
    else:
        inner, shaped = _element(element, schemas, 0, set())
    return (
        '<?xml version="1.0" encoding="utf-8"?>\n'
        f'<soap:Envelope xmlns:soap="{envelope_ns}" xmlns:tns="{namespace}">\n'
        "  <soap:Body>\n"
        f"{inner}"
        "  </soap:Body>\n"
        "</soap:Envelope>",
        shaped,
    )


def _element(
    name: str, schemas: list[ElementTree.Element], depth: int, seen: set[str]
) -> tuple[str, bool]:
    """One declared element as XML, following its type as far as it is declared."""
    declaration = _find(schemas, "element", name)
    if declaration is None or depth > MAX_DEPTH:
        return f"    <tns:{name}/>\n", False
    children, shaped = _children(declaration, schemas, depth, seen | {name})
    indent = "  " * (depth + 2)
    if not children:
        value = _placeholder(declaration, schemas)
        return f"{indent}<tns:{name}>{value}</tns:{name}>\n", True
    return f"{indent}<tns:{name}>\n{children}{indent}</tns:{name}>\n", shaped or True


def _children(
    declaration: ElementTree.Element, schemas: list[ElementTree.Element], depth: int, seen: set[str]
) -> tuple[str, bool]:
    """The fields of a complex type, in the order the schema declares them."""
    complex_type = declaration.find(f"{{{XSD}}}complexType")
    if complex_type is None:
        named = _local(declaration.get("type", ""))
        if not named or named in seen:
            return "", False
        complex_type = _find(schemas, "complexType", named)
        if complex_type is None:
            return "", False
        seen = seen | {named}

    rendered = ""
    shaped = False
    for container in ("sequence", "all", "choice"):
        group = complex_type.find(f"{{{XSD}}}{container}")
        if group is None:
            continue
        for child in group.findall(f"{{{XSD}}}element"):
            child_name = child.get("name") or _local(child.get("ref", ""))
            if not child_name or child_name in seen or depth + 1 > MAX_DEPTH:
                continue
            nested, nested_shaped = _children(child, schemas, depth + 1, seen | {child_name})
            indent = "  " * (depth + 3)
            if nested:
                rendered += f"{indent}<tns:{child_name}>\n{nested}{indent}</tns:{child_name}>\n"
            else:
                value = _placeholder(child, schemas)
                rendered += f"{indent}<tns:{child_name}>{value}</tns:{child_name}>\n"
            shaped = True
            if container == "choice":
                # One branch of a choice, not all of them: sending every alternative
                # is not a valid message.
                break
        break
    return rendered, shaped


def _placeholder(declaration: ElementTree.Element, schemas: list[ElementTree.Element]) -> str:
    """A value of the declared type, or the first value a restriction allows."""
    named = _local(declaration.get("type", ""))
    if named in PLACEHOLDERS:
        return PLACEHOLDERS[named]
    simple = _find(schemas, "simpleType", named) if named else None
    if simple is None:
        simple = declaration.find(f"{{{XSD}}}simpleType")
    if simple is not None:
        restriction = simple.find(f"{{{XSD}}}restriction")
        if restriction is not None:
            values = [
                value.get("value", "")
                for value in restriction.findall(f"{{{XSD}}}enumeration")
                if value.get("value")
            ]
            if values:
                # Sorted rather than first: the same document has to give the same
                # plan, and nothing says the order an enumeration was written in is
                # stable across exports.
                return sorted(values)[0]
            base = _local(restriction.get("base", ""))
            if base in PLACEHOLDERS:
                return PLACEHOLDERS[base]
    return "string"


def _find(
    schemas: list[ElementTree.Element], kind: str, name: str
) -> ElementTree.Element | None:
    if not name:
        return None
    for schema in schemas:
        for candidate in schema.findall(f"{{{XSD}}}{kind}"):
            if candidate.get("name") == name:
                return candidate
    return None
