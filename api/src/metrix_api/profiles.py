"""Target profiles: where to send traffic and what to observe.

A profile is a named description of an environment. Plans reference it by name, so
one mixture runs against staging, against production, or against a single suspect
container with no edit to the plan.

This module covers **explicit endpoint lists** -- hosts written down by a person.
ECS discovery (A2) produces the same :class:`Endpoint` shape from a hostname, so
everything downstream is indifferent to which one was used.

A profile serves two consumers:

* the engine, via :func:`to_targets` -- concrete addresses, no cloud context
* the observer, via :meth:`Endpoint.collection` -- where to collect host stats from
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from datetime import timedelta
from pathlib import Path
from typing import Any

from metrix_api.config import Config, format_duration, parse_duration

#: Profile names are used as filenames and as series-identity components, so they are
#: restricted rather than sanitised -- a name that changes shape when written to disk
#: would silently split a series.
NAME = re.compile(r"^[a-z0-9][a-z0-9-]{0,62}$")
_ADDRESS = re.compile(r"^(?P<host>\[[0-9a-fA-F:]+\]|[^:\s]+):(?P<port>\d{1,5})$")

#: Through the load balancer, or straight at one container. Part of series identity:
#: the two measure different network paths and are never compared.
ADDRESSING = ("load_balancer", "direct")
TRANSPORTS = ("ssh", "scrape", "none")


class ProfileError(Exception):
    """A profile that cannot be used. The message names the field and the reason."""


@dataclass(frozen=True, slots=True)
class Tls:
    enabled: bool = False
    #: Overrides the Host header for SNI when the two must differ.
    sni: str | None = None
    #: Raises a run annotation when used.
    insecure_skip_verify: bool = False


@dataclass(frozen=True, slots=True)
class Collection:
    """How to collect host statistics from one endpoint."""

    transport: str = "none"
    #: Defaults to the endpoint's own host.
    host: str | None = None
    port: int | None = None
    user: str | None = None
    key: Path | None = None
    #: Scrape transport only.
    path: str = "/metrics"


@dataclass(frozen=True, slots=True)
class Endpoint:
    id: str
    address: str
    #: Sent as Host, and used for SNI. Required when addressing a container directly:
    #: most services vhost on it, and a raw IP gets a 404 or a default backend.
    host_header: str | None = None
    tls: Tls = field(default_factory=Tls)
    #: Inventory detail -- instance type, AZ, image digest. Opaque; it is what
    #: explains an outlier in a sweep.
    attributes: dict[str, str] = field(default_factory=dict)
    collect: Collection = field(default_factory=Collection)

    @property
    def host(self) -> str:
        host = _ADDRESS.match(self.address).group("host")  # type: ignore[union-attr]
        return host[1:-1] if host.startswith("[") else host

    @property
    def port(self) -> int:
        return int(_ADDRESS.match(self.address).group("port"))  # type: ignore[union-attr]

    def collection(self) -> Collection:
        """The collection settings with defaults filled in from the address."""
        if self.collect.transport == "none":
            return self.collect
        return Collection(
            transport=self.collect.transport,
            host=self.collect.host or self.host,
            port=self.collect.port,
            user=self.collect.user,
            key=self.collect.key,
            path=self.collect.path,
        )


@dataclass(frozen=True, slots=True)
class Profile:
    name: str
    endpoints: list[Endpoint]
    description: str = ""
    addressing: str = "load_balancer"
    #: Sweep defaults. Shuffling decouples results from position, since the first
    #: target pays cold-cache costs on shared dependencies that the rest do not.
    order: str = "as_resolved"
    gap: timedelta | None = None
    #: Collection defaults, overridable per endpoint.
    interval: timedelta | None = None
    collect_metrics: list[str] = field(default_factory=list)

    @property
    def observed(self) -> list[Endpoint]:
        """Endpoints the observer can actually collect from."""
        return [e for e in self.endpoints if e.collect.transport != "none"]

    def endpoint(self, endpoint_id: str) -> Endpoint:
        for candidate in self.endpoints:
            if candidate.id == endpoint_id:
                return candidate
        known = ", ".join(e.id for e in self.endpoints)
        raise ProfileError(f"profile {self.name!r} has no endpoint {endpoint_id!r}; has: {known}")


def to_targets(profile: Profile, *, only: list[str] | None = None) -> dict[str, Any]:
    """Build the engine's targets document.

    Validated against ``schema/targets.schema.json`` -- this is the point where a
    profile becomes something the engine understands, and the engine knows nothing
    about profiles.
    """
    chosen = profile.endpoints if only is None else [profile.endpoint(i) for i in only]
    if not chosen:
        raise ProfileError(f"profile {profile.name!r}: no targets selected")

    targets: dict[str, Any] = {"order": profile.order, "list": []}
    if profile.gap is not None:
        targets["gap"] = format_duration(profile.gap)

    for endpoint in chosen:
        entry: dict[str, Any] = {"id": endpoint.id, "address": endpoint.address}
        if endpoint.host_header:
            entry["host_header"] = endpoint.host_header
        if endpoint.tls.enabled or endpoint.tls.sni or endpoint.tls.insecure_skip_verify:
            tls: dict[str, Any] = {"enabled": endpoint.tls.enabled}
            if endpoint.tls.sni:
                tls["sni"] = endpoint.tls.sni
            if endpoint.tls.insecure_skip_verify:
                tls["insecure_skip_verify"] = True
            entry["tls"] = tls
        if endpoint.attributes:
            entry["attributes"] = dict(endpoint.attributes)
        targets["list"].append(entry)

    return targets


# --------------------------------------------------------------------------- io


def profile_path(config: Config, name: str) -> Path:
    return config.profiles_dir / f"{name}.json"


def list_profiles(config: Config) -> list[str]:
    if not config.profiles_dir.is_dir():
        return []
    return sorted(p.stem for p in config.profiles_dir.glob("*.json"))


def load_profile(config: Config, name: str) -> Profile:
    path = profile_path(config, name)
    if not path.is_file():
        known = ", ".join(list_profiles(config)) or "none"
        raise ProfileError(f"no profile named {name!r} in {config.profiles_dir} (have: {known})")
    return parse_profile(json.loads(path.read_text(encoding="utf-8")), name=name, source=path)


def save_profile(config: Config, profile: Profile) -> Path:
    config.profiles_dir.mkdir(parents=True, exist_ok=True)
    path = profile_path(config, profile.name)
    path.write_text(json.dumps(to_document(profile), indent=2) + "\n", encoding="utf-8")
    return path


def to_document(profile: Profile) -> dict[str, Any]:
    """The on-disk form. Round-trips through :func:`parse_profile`."""
    doc: dict[str, Any] = {"name": profile.name}
    if profile.description:
        doc["description"] = profile.description
    doc["addressing"] = profile.addressing
    if profile.order != "as_resolved":
        doc["order"] = profile.order
    if profile.gap is not None:
        doc["gap"] = format_duration(profile.gap)

    observe: dict[str, Any] = {}
    if profile.interval is not None:
        observe["interval"] = format_duration(profile.interval)
    if profile.collect_metrics:
        observe["collect"] = list(profile.collect_metrics)
    if observe:
        doc["observe"] = observe

    doc["endpoints"] = []
    for endpoint in profile.endpoints:
        entry: dict[str, Any] = {"id": endpoint.id, "address": endpoint.address}
        if endpoint.host_header:
            entry["host_header"] = endpoint.host_header
        if endpoint.tls != Tls():
            entry["tls"] = {
                "enabled": endpoint.tls.enabled,
                **({"sni": endpoint.tls.sni} if endpoint.tls.sni else {}),
                **(
                    {"insecure_skip_verify": True} if endpoint.tls.insecure_skip_verify else {}
                ),
            }
        if endpoint.attributes:
            entry["attributes"] = dict(endpoint.attributes)
        if endpoint.collect.transport != "none":
            collect: dict[str, Any] = {"transport": endpoint.collect.transport}
            for key, value in (
                ("host", endpoint.collect.host),
                ("port", endpoint.collect.port),
                ("user", endpoint.collect.user),
            ):
                if value is not None:
                    collect[key] = value
            if endpoint.collect.key is not None:
                collect["key"] = str(endpoint.collect.key)
            if endpoint.collect.path != "/metrics":
                collect["path"] = endpoint.collect.path
            entry["collect"] = collect
        doc["endpoints"].append(entry)

    return doc


# --------------------------------------------------------------------- parsing


def _reject_unknown(raw: dict[str, Any], allowed: set[str], where: str) -> None:
    if unknown := set(raw) - allowed:
        raise ProfileError(
            f"{where}: unknown key(s) {', '.join(sorted(unknown))}; expected "
            f"{', '.join(sorted(allowed))}"
        )


def _choice(value: Any, options: tuple[str, ...], where: str) -> str:
    if value not in options:
        raise ProfileError(f"{where}: expected one of {', '.join(options)}, got {value!r}")
    return str(value)


def parse_profile(
    raw: Any, *, name: str | None = None, source: Path | str = "<profile>"
) -> Profile:
    where = str(source)
    if not isinstance(raw, dict):
        raise ProfileError(f"{where}: a profile must be an object")

    _reject_unknown(
        raw,
        {"name", "description", "addressing", "order", "gap", "observe", "endpoints"},
        where,
    )

    declared = raw.get("name")
    if declared is None:
        raise ProfileError(f"{where}: missing 'name'")
    if not NAME.match(str(declared)):
        raise ProfileError(
            f"{where}: name {declared!r} must be lowercase letters, digits and hyphens"
        )
    if name is not None and declared != name:
        # The filename is how the profile is referenced, so a mismatch would make the
        # same profile answer to two names.
        raise ProfileError(f"{where}: name {declared!r} does not match the filename {name!r}")

    addressing = _choice(raw.get("addressing", "load_balancer"), ADDRESSING, f"{where}: addressing")

    observe = raw.get("observe", {})
    if not isinstance(observe, dict):
        raise ProfileError(f"{where}: 'observe' must be an object")
    _reject_unknown(observe, {"interval", "collect"}, f"{where}: observe")

    endpoints_raw = raw.get("endpoints")
    if not isinstance(endpoints_raw, list) or not endpoints_raw:
        raise ProfileError(f"{where}: 'endpoints' must be a non-empty list")

    endpoints = [
        _endpoint(entry, addressing, f"{where}: endpoints[{i}]")
        for i, entry in enumerate(endpoints_raw)
    ]

    seen: set[str] = set()
    for endpoint in endpoints:
        if endpoint.id in seen:
            raise ProfileError(
                f"{where}: duplicate endpoint id {endpoint.id!r}; ids identify a box across "
                "runs and must be unique"
            )
        seen.add(endpoint.id)

    return Profile(
        name=str(declared),
        description=str(raw.get("description", "")),
        addressing=addressing,
        order=_choice(
            raw.get("order", "as_resolved"), ("as_resolved", "shuffle"), f"{where}: order"
        ),
        gap=parse_duration(raw["gap"], key=f"{where}: gap") if "gap" in raw else None,
        interval=(
            parse_duration(observe["interval"], key=f"{where}: observe.interval")
            if "interval" in observe
            else None
        ),
        collect_metrics=[str(m) for m in observe.get("collect", [])],
        endpoints=endpoints,
    )


def _endpoint(raw: Any, addressing: str, where: str) -> Endpoint:
    if not isinstance(raw, dict):
        raise ProfileError(f"{where}: must be an object")
    _reject_unknown(
        raw, {"id", "address", "host_header", "tls", "attributes", "collect"}, where
    )

    for required in ("id", "address"):
        if required not in raw:
            raise ProfileError(f"{where}: missing {required!r}")

    address = str(raw["address"])
    if not _ADDRESS.match(address):
        raise ProfileError(f"{where}: address {address!r} must be host:port, e.g. 10.0.3.41:8080")
    port = int(_ADDRESS.match(address).group("port"))  # type: ignore[union-attr]
    if not 1 <= port <= 65535:
        raise ProfileError(f"{where}: port {port} is out of range")

    host_header = raw.get("host_header")
    if addressing == "direct" and not host_header:
        # Getting this wrong produces a plausible-looking test of nothing.
        raise ProfileError(
            f"{where}: addressing is 'direct', so host_header is required -- most services "
            "route on it and a raw address gets a 404 or a default backend"
        )

    return Endpoint(
        id=str(raw["id"]),
        address=address,
        host_header=str(host_header) if host_header else None,
        tls=_tls(raw.get("tls", {}), f"{where}: tls"),
        attributes={str(k): str(v) for k, v in raw.get("attributes", {}).items()},
        collect=_collect(raw.get("collect", {}), f"{where}: collect"),
    )


def _tls(raw: Any, where: str) -> Tls:
    if not isinstance(raw, dict):
        raise ProfileError(f"{where}: must be an object")
    _reject_unknown(raw, {"enabled", "sni", "insecure_skip_verify"}, where)
    return Tls(
        enabled=bool(raw.get("enabled", False)),
        sni=str(raw["sni"]) if raw.get("sni") else None,
        insecure_skip_verify=bool(raw.get("insecure_skip_verify", False)),
    )


def _collect(raw: Any, where: str) -> Collection:
    if not isinstance(raw, dict):
        raise ProfileError(f"{where}: must be an object")
    _reject_unknown(raw, {"transport", "host", "port", "user", "key", "path"}, where)

    transport = _choice(raw.get("transport", "none"), TRANSPORTS, f"{where}: transport")
    key = raw.get("key")
    return Collection(
        transport=transport,
        host=str(raw["host"]) if raw.get("host") else None,
        port=int(raw["port"]) if raw.get("port") is not None else None,
        user=str(raw["user"]) if raw.get("user") else None,
        key=Path(str(key)).expanduser() if key else None,
        path=str(raw.get("path", "/metrics")),
    )
