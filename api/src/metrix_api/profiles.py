"""Target profiles: where to send traffic and what to observe.

A profile is a named description of an environment. Plans reference it by name, so
one mixture runs against staging, against production, or against a single suspect
container with no edit to the plan.

A profile gets its endpoints one of two ways (design-api 3.1): written down by a
person, or resolved from a hostname by discovery. The second is a ``discover`` block
here and an :class:`Endpoint` list produced from an inventory there, so everything
downstream is indifferent to which one was used.

A profile serves two consumers:

* the engine, via :func:`to_targets` -- concrete addresses, no cloud context
* the observer, via :meth:`Endpoint.collection` -- where to collect host stats from
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field, replace
from datetime import timedelta
from pathlib import Path
from typing import Any

from metrix_api.config import Config, format_duration, parse_duration

#: Profile names are used as filenames and as series-identity components, so they are
#: restricted rather than sanitised -- a name that changes shape when written to disk
#: would silently split a series.
NAME = re.compile(r"^[a-z0-9][a-z0-9-]{0,62}$")
_ADDRESS = re.compile(r"^(?P<host>\[[0-9a-fA-F:]+\]|[^:\s]+):(?P<port>\d{1,5})$")

#: Legacy profile-wide discovery choices. Explicit endpoints now classify their own
#: network path; these remain for old files and for choosing which discovery result
#: receives traffic.
ADDRESSING = ("load_balancer", "direct")
ENDPOINT_ADDRESSING = ("ip", "alb", "elb", "ecs", "fargate")
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
class Discover:
    """Where a profile's endpoints come from, when a person did not write them down.

    Resolution is cached and refreshed on a TTL rather than performed per run: the
    walk behind it is several seconds of rate-limited control-plane calls, and an
    environment does not change between two runs an hour apart (design-api 3.2).
    """

    hostname: str | None = None
    #: The other entry point: an ECS service named directly, skipping DNS and the
    #: load balancer.
    cluster: str | None = None
    service: str | None = None
    #: How long a resolution stands before it is walked again.
    ttl: timedelta = timedelta(minutes=10)
    #: Sent as Host to every discovered endpoint. Defaults to the hostname asked for,
    #: which is what the service is vhosted on.
    host_header: str | None = None
    tls: Tls = field(default_factory=Tls)
    #: Applied to every host found. One setting for the environment, because
    #: discovery yields boxes that are alike by construction.
    collect: Collection = field(default_factory=Collection)

    @property
    def source(self) -> str:
        return self.hostname or f"{self.cluster}/{self.service}"

    @property
    def header(self) -> str | None:
        return self.host_header or self.hostname


@dataclass(frozen=True, slots=True)
class Endpoint:
    id: str
    address: str
    #: How this concrete address is reached. Metadata for the API and series
    #: identity; the engine still receives only `address`.
    addressing: str = "ip"
    #: Sent as Host, and used for SNI. Required when addressing a container directly:
    #: most services vhost on it, and a raw IP gets a 404 or a default backend.
    host_header: str | None = None
    tls: Tls = field(default_factory=Tls)
    #: Whether traffic is sent here. False for a box that is observed but not
    #: addressed -- the tasks behind a load balancer, when the balancer is what the
    #: run points at. Every endpoint still carries an address, because that is where
    #: the box *is*; this says whether it is also where the load goes.
    load: bool = True
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
    #: Written down, or produced from an inventory by discovery. A profile with a
    #: `discover` block and no endpoints has simply not been resolved yet.
    endpoints: list[Endpoint]
    description: str = ""
    #: Discovery path for discovered profiles; the broad network-path class derived
    #: from endpoint kinds for explicit profiles. Kept in the two-value recording
    #: contract so old and new recordings remain comparable.
    addressing: str = "ip"
    discover: Discover | None = None
    #: Sweep defaults. Shuffling decouples results from position, since the first
    #: target pays cold-cache costs on shared dependencies that the rest do not.
    order: str = "as_resolved"
    gap: timedelta | None = None
    #: Collection defaults, overridable per endpoint.
    interval: timedelta | None = None
    #: SSH login applied to every endpoint during observation when set.
    ssh_user: str | None = None
    collect_metrics: list[str] = field(default_factory=list)

    @property
    def observed(self) -> list[Endpoint]:
        """Endpoints the observer can actually collect from."""
        return [e for e in self.endpoints if e.collect.transport != "none"]

    @property
    def targets(self) -> list[Endpoint]:
        """Endpoints traffic is sent to: the default selection for `targets.json`.

        Not the same list as `observed`, and usually not overlapping it. Under
        load-balancer addressing the run points at the balancer and watches the boxes
        behind it; the two roles are what `load` and `collect.transport` say.
        """
        return [e for e in self.endpoints if e.load]

    def endpoint(self, endpoint_id: str) -> Endpoint:
        for candidate in self.endpoints:
            if candidate.id == endpoint_id:
                return candidate
        known = ", ".join(e.id for e in self.endpoints) or "none"
        raise ProfileError(f"profile {self.name!r} has no endpoint {endpoint_id!r}; has: {known}")

    def with_endpoints(self, endpoints: list[Endpoint]) -> Profile:
        """The same profile, resolved. Everything else about it is unchanged.

        A discovered profile is never written back to disk with its endpoints filled
        in: the file says what to discover, and the answer belongs in the inventory,
        which is stored with a timestamp and pinned to the runs that used it.
        """
        return replace(self, endpoints=endpoints)

    def with_observation_defaults(self) -> Profile:
        """Apply profile-wide observation settings without changing the file form."""
        if not self.ssh_user:
            return self
        return replace(
            self,
            endpoints=[
                replace(
                    endpoint,
                    collect=(
                        replace(endpoint.collect, user=self.ssh_user)
                        if endpoint.collect.transport == "ssh"
                        else endpoint.collect
                    ),
                )
                for endpoint in self.endpoints
            ],
        )


def to_targets(profile: Profile, *, only: list[str] | None = None) -> dict[str, Any]:
    """Build the engine's targets document.

    Validated against ``schema/targets.schema.json`` -- this is the point where a
    profile becomes something the engine understands, and the engine knows nothing
    about profiles.
    """
    # By default the endpoints that take traffic; an explicit selection overrides
    # that, including with an endpoint marked observation-only -- the caller asked
    # for it by name, which is a decision, not an accident.
    chosen = profile.targets if only is None else [profile.endpoint(i) for i in only]
    if only is not None and not chosen:
        raise ProfileError(f"profile {profile.name!r}: no targets selected")
    if not chosen:
        raise ProfileError(
            f"profile {profile.name!r}: no targets selected"
            + (
                " -- every endpoint is marked observation-only, so there is nowhere "
                "to send traffic"
                if profile.endpoints
                else " -- it has no endpoints yet"
            )
        )

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


def delete_profile(config: Config, name: str) -> bool:
    """Remove a profile. False if it was not there.

    Recordings are not touched: one names the profile it ran against, and deleting
    the profile does not make the measurements untrue.
    """
    path = profile_path(config, name)
    if not path.is_file():
        return False
    path.unlink()
    return True


def to_document(profile: Profile) -> dict[str, Any]:
    """The on-disk form. Round-trips through :func:`parse_profile`."""
    doc: dict[str, Any] = {"name": profile.name}
    if profile.description:
        doc["description"] = profile.description
    if profile.discover is not None:
        # Discovery still needs to choose which resolved tier receives traffic.
        # Explicit endpoint documents carry this choice on each row instead.
        doc["addressing"] = profile.addressing
        doc["discover"] = _discover_document(profile.discover)
    if profile.order != "as_resolved":
        doc["order"] = profile.order
    if profile.gap is not None:
        doc["gap"] = format_duration(profile.gap)

    observe: dict[str, Any] = {}
    if profile.interval is not None:
        observe["interval"] = format_duration(profile.interval)
    if profile.ssh_user:
        observe["ssh_user"] = profile.ssh_user
    if profile.collect_metrics:
        observe["collect"] = list(profile.collect_metrics)
    if observe:
        doc["observe"] = observe

    # A discovered profile's endpoints are the inventory's, not the file's: writing
    # them back would freeze one resolution into a document that asks for a fresh one.
    doc["endpoints"] = []
    for endpoint in profile.endpoints if profile.discover is None else ():
        entry: dict[str, Any] = {
            "id": endpoint.id,
            "addressing": endpoint.addressing,
            "address": endpoint.address,
        }
        if endpoint.host_header:
            entry["host_header"] = endpoint.host_header
        if not endpoint.load:
            entry["load"] = False
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
        if (
            endpoint.collect.transport != "none"
            or endpoint.collect.host is not None
            or endpoint.collect.user is not None
            or endpoint.collect.port is not None
        ):
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


def _discover_document(discover: Discover) -> dict[str, Any]:
    doc: dict[str, Any] = {}
    for key, value in (
        ("hostname", discover.hostname),
        ("cluster", discover.cluster),
        ("service", discover.service),
        ("host_header", discover.host_header),
    ):
        if value:
            doc[key] = value
    doc["ttl"] = format_duration(discover.ttl)
    if discover.tls != Tls():
        doc["tls"] = {
            "enabled": discover.tls.enabled,
            **({"sni": discover.tls.sni} if discover.tls.sni else {}),
            **({"insecure_skip_verify": True} if discover.tls.insecure_skip_verify else {}),
        }
    if discover.collect.transport != "none":
        collect: dict[str, Any] = {"transport": discover.collect.transport}
        for key, value in (
            ("host", discover.collect.host),
            ("port", discover.collect.port),
            ("user", discover.collect.user),
        ):
            if value is not None:
                collect[key] = value
        if discover.collect.key is not None:
            collect["key"] = str(discover.collect.key)
        if discover.collect.path != "/metrics":
            collect["path"] = discover.collect.path
        doc["collect"] = collect
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
        {
            "name",
            "description",
            "addressing",
            "discover",
            "order",
            "gap",
            "observe",
            "endpoints",
        },
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

    legacy_addressing = _choice(
        raw.get("addressing", "load_balancer"), ADDRESSING, f"{where}: addressing"
    )

    observe = raw.get("observe", {})
    if not isinstance(observe, dict):
        raise ProfileError(f"{where}: 'observe' must be an object")
    _reject_unknown(observe, {"interval", "ssh_user", "collect"}, f"{where}: observe")

    discover = (
        _discover(raw["discover"], legacy_addressing, f"{where}: discover")
        if "discover" in raw
        else None
    )

    endpoints_raw = raw.get("endpoints") or []
    if not isinstance(endpoints_raw, list):
        raise ProfileError(f"{where}: 'endpoints' must be a list")
    if not endpoints_raw and discover is None:
        raise ProfileError(
            f"{where}: 'endpoints' must be a non-empty list, or a 'discover' block "
            "saying where to find them"
        )

    endpoints = [
        _endpoint(entry, legacy_addressing, f"{where}: endpoints[{i}]")
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
        addressing=(
            legacy_addressing
            if discover is not None
            else _addressing_identity(endpoints)
        ),
        discover=discover,
        order=_choice(
            raw.get("order", "as_resolved"), ("as_resolved", "shuffle"), f"{where}: order"
        ),
        gap=parse_duration(raw["gap"], key=f"{where}: gap") if "gap" in raw else None,
        interval=(
            parse_duration(observe["interval"], key=f"{where}: observe.interval")
            if "interval" in observe
            else None
        ),
        ssh_user=str(observe["ssh_user"]).strip() if observe.get("ssh_user") else None,
        collect_metrics=[str(m) for m in observe.get("collect", [])],
        endpoints=endpoints,
    )


def _endpoint(raw: Any, addressing: str, where: str) -> Endpoint:
    if not isinstance(raw, dict):
        raise ProfileError(f"{where}: must be an object")
    _reject_unknown(
        raw,
        {
            "id",
            "addressing",
            "address",
            "host_header",
            "load",
            "tls",
            "attributes",
            "collect",
        },
        where,
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

    endpoint_addressing = _choice(
        raw.get("addressing", "ip" if addressing == "direct" else "alb"),
        ENDPOINT_ADDRESSING,
        f"{where}: addressing",
    )
    host_header = raw.get("host_header")
    if endpoint_addressing == "ip" and not host_header:
        # Getting this wrong produces a plausible-looking test of nothing.
        raise ProfileError(
            f"{where}: addressing is 'ip' (direct), so host_header is required -- most services "
            "route on it and a raw address gets a 404 or a default backend"
        )

    return Endpoint(
        id=str(raw["id"]),
        address=address,
        addressing=endpoint_addressing,
        host_header=str(host_header) if host_header else None,
        load=bool(raw.get("load", True)),
        tls=_tls(raw.get("tls", {}), f"{where}: tls"),
        attributes={str(k): str(v) for k, v in raw.get("attributes", {}).items()},
        collect=_collect(raw.get("collect", {}), f"{where}: collect"),
    )


def _addressing_identity(endpoints: list[Endpoint]) -> str:
    """Map detailed endpoint kinds onto the recording's established path classes."""
    targets = [endpoint for endpoint in endpoints if endpoint.load] or endpoints
    kinds = {endpoint.addressing for endpoint in targets}
    return "load_balancer" if kinds & {"alb", "elb"} else "direct"


def _discover(raw: Any, addressing: str, where: str) -> Discover:
    if not isinstance(raw, dict):
        raise ProfileError(f"{where}: must be an object")
    _reject_unknown(
        raw, {"hostname", "cluster", "service", "ttl", "host_header", "tls", "collect"}, where
    )

    hostname = str(raw["hostname"]).strip() if raw.get("hostname") else None
    cluster = str(raw["cluster"]).strip() if raw.get("cluster") else None
    service = str(raw["service"]).strip() if raw.get("service") else None

    # The two entry points of design-api 3.1, and they are alternatives: a hostname
    # resolves through the load balancer, a cluster and service skip it entirely.
    if hostname and (cluster or service):
        raise ProfileError(f"{where}: give a hostname, or a cluster and a service -- not both")
    if not hostname and not (cluster and service):
        raise ProfileError(f"{where}: needs a hostname, or both a cluster and a service")

    host_header = str(raw["host_header"]).strip() if raw.get("host_header") else None
    if addressing == "direct" and not (host_header or hostname):
        # Same rule the endpoint form enforces, applied where the answer is decided:
        # discovered boxes are addressed by IP, and most services route on the header.
        raise ProfileError(
            f"{where}: addressing is 'direct' and discovery is by cluster and service, "
            "so host_header is required -- a raw address gets a 404 or a default backend"
        )

    return Discover(
        hostname=hostname,
        cluster=cluster,
        service=service,
        ttl=(
            parse_duration(raw["ttl"], key=f"{where}: ttl")
            if "ttl" in raw
            else Discover().ttl
        ),
        host_header=host_header,
        tls=_tls(raw.get("tls", {}), f"{where}: tls"),
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
