"""The walk: a hostname, or an ECS service named directly, resolved to what runs.

    api.staging.example.com
      -> Route 53                      the load balancer's own DNS name
      -> elasticloadbalancing          listeners, host-header rules, target groups
      -> elasticloadbalancing          registered targets: instance ids, or IP:port
      -> ecs                           the service registered against that group
      -> ecs                           tasks, containers, image digests, ENIs
      -> ec2                           instance id, type, AZ, private DNS
      -> autoscaling                   the group, and desired/min/max

**Every hop is optional and partial resolution is the normal case** (design-api 3.1).
A hostname that is an NLB with nothing behind it, a task with no network interface, a
target mid-deregistration: each of those stops the walk somewhere sensible and is
recorded as a note. Only the spine -- "is this hostname a load balancer at all, and
what is registered with it" -- raises, because a failure there means the answer would
be silently empty rather than short.

This is the only module in the tool that imports boto3 (rule 1 of both plans). What
leaves it is an `Inventory`: concrete addresses and identities, no cloud handles.
The read-only permissions it needs are `policy/metrix-readonly.json`, and
`tests/test_discovery_policy.py` fails if this file starts calling something that
document does not allow.
"""

from __future__ import annotations

import re
from collections.abc import Callable, Iterable, Iterator, Sequence
from dataclasses import dataclass, field
from typing import Any

from botocore.exceptions import BotoCoreError, ClientError

from metrix_api.config import AwsConfig
from metrix_api.discovery.inventory import (
    HOPS,
    NOTHING,
    Asg,
    Inventory,
    Note,
    Resource,
)

#: boto3 client name -> the IAM prefix its calls are authorised under. Both halves
#: are load-bearing: the left is how the walk reaches a client, the right is how the
#: policy test decides whether `policy/metrix-readonly.json` covers a call.
SERVICES = {
    "route53": "route53",
    "elbv2": "elasticloadbalancing",
    "ecs": "ecs",
    "ec2": "ec2",
    "autoscaling": "autoscaling",
}

#: A CNAME chain has to end somewhere, and a misconfigured zone can point at itself.
MAX_DNS_HOPS = 8

#: API batch limits. Exceeding one is an error, not a truncation, so they are here
#: rather than discovered the hard way against a service with eleven tasks.
DESCRIBE_SERVICES_MAX = 10
DESCRIBE_TASKS_MAX = 100
DESCRIBE_ASG_INSTANCES_MAX = 50

#: How each API spells "there is more". ELBv2 says Marker, ECS says nextToken, EC2
#: and autoscaling say NextToken. Checked in this order against every response.
_TOKENS = (("NextMarker", "Marker"), ("nextToken", "nextToken"), ("NextToken", "NextToken"))

#: An alias record for a load balancer is published dual-stack; the same balancer is
#: listed by `describe_load_balancers` without the prefix.
_DUALSTACK = "dualstack."


class DiscoveryError(Exception):
    """Discovery could not start. Partial results are notes; this is not one."""


# ------------------------------------------------------------------- the clients


@dataclass(frozen=True, slots=True)
class Clients:
    """The five read-only clients the walk uses.

    A plain object with one attribute per service, rather than a session passed
    around, so the seam is a method call: the fixture tests replay recorded
    `describe_*` responses through the same attribute names and never reach AWS.
    """

    route53: Any
    elbv2: Any
    ecs: Any
    ec2: Any
    autoscaling: Any

    @classmethod
    def from_config(cls, aws: AwsConfig) -> Clients:
        # Imported here rather than at module scope: boto3 costs the better part of a
        # second to import, and observation against a hand-written profile -- the
        # day-one path -- never touches AWS at all.
        import boto3

        try:
            session = boto3.Session(profile_name=aws.profile, region_name=aws.region)
            return cls(**{name: session.client(name) for name in SERVICES})
        except (ClientError, BotoCoreError) as exc:
            # No credentials, an unknown profile, no region: all of them are settings
            # in config.toml, and none of them are worth a traceback.
            raise DiscoveryError(
                f"could not open an AWS session: {_reason(exc)}; check [aws] profile "
                "and region in config.toml"
            ) from exc


# ------------------------------------------------------------------ the entry point


def discover(
    clients: Clients,
    *,
    hostname: str | None = None,
    cluster: str | None = None,
    service: str | None = None,
) -> Inventory:
    """Resolve a hostname, or an ECS cluster and service named directly.

    The second form skips DNS and the load balancer, which is what you want when the
    service is reached some other way, or when the balancer is shared and its rules
    are not worth reading.
    """
    if hostname and (cluster or service):
        raise DiscoveryError("give a hostname, or a cluster and a service -- not both")

    if hostname:
        walk = _Walk(clients, hostname)
        _from_hostname(walk, _normalise(hostname))
    elif cluster and service:
        walk = _Walk(clients, f"{cluster}/{service}")
        _from_named_service(walk, cluster, service)
    else:
        raise DiscoveryError("discovery needs a hostname, or both a cluster and a service")

    return walk.inventory()


@dataclass
class _Walk:
    """One resolution, in progress: what it found, and how far it got."""

    clients: Clients
    source: str
    resources: list[Resource] = field(default_factory=list)
    notes: list[Note] = field(default_factory=list)
    reached: str = NOTHING
    #: target group ARN -> ECS service, built at most once. The scan behind it is
    #: the expensive hop, and a balancer with several target groups would otherwise
    #: pay for it once per group.
    services: dict[str, dict[str, Any]] | None = None

    def add(self, resource: Resource) -> Resource:
        """Record a resource, first sighting winning.

        A blue/green service is registered against two target groups at once, and
        both walk down to the same tasks and the same instances. An inventory that
        listed a box twice would double every host it is the collection list for.
        """
        if existing := next((r for r in self.resources if r.id == resource.id), None):
            return existing
        self.resources.append(resource)
        return resource

    def note(self, hop: str, message: str) -> None:
        self.notes.append(Note(hop=hop, message=message))

    def done(self, hop: str) -> None:
        """Record a hop as reached, keeping the furthest.

        The walk branches: an instance behind a bare NLB is reached without ever
        passing through a service, and a task resolves before the instance under it.
        Taking the maximum stops one short branch from rewinding the answer.
        """
        if self.reached == NOTHING or HOPS.index(hop) > HOPS.index(self.reached):
            self.reached = hop

    def inventory(self) -> Inventory:
        return Inventory(
            source=self.source,
            resources=self.resources,
            reached=self.reached,
            notes=self.notes,
        )


# ---------------------------------------------------------------------- hop 1: DNS


def _from_hostname(walk: _Walk, hostname: str) -> None:
    balancers = {
        str(lb["DNSName"]).lower(): lb
        for lb in _require(
            "listing load balancers",
            _collect,
            walk.clients.elbv2.describe_load_balancers,
            "LoadBalancers",
        )
    }

    resolved = _resolve_dns(walk, hostname, set(balancers))
    balancer = balancers.get(resolved)
    if balancer is None:
        walk.note(
            "dns",
            f"{hostname} does not resolve to a load balancer in this account and region"
            + (f" (followed to {resolved})" if resolved != hostname else "")
            + "; give the endpoints explicitly, or name the ECS cluster and service",
        )
        return

    walk.done("load_balancer")
    _from_load_balancer(walk, balancer, hostname)


def _resolve_dns(walk: _Walk, hostname: str, known: set[str]) -> str:
    """Follow Route 53 from a hostname towards a load balancer's own DNS name.

    A hostname that is already a balancer needs no zone lookup, which is the common
    case in a staging account and costs nothing to check first.
    """
    if hostname in known:
        return hostname

    zones = _try(
        walk,
        "dns",
        "listing hosted zones",
        _collect,
        walk.clients.route53.list_hosted_zones,
        "HostedZones",
    )
    if not zones:
        return hostname

    name = hostname
    for _ in range(MAX_DNS_HOPS):
        target = _record_target(walk, zones, name)
        if target is None or target == name:
            return name
        name = target
        if name in known:
            return name

    walk.note("dns", f"stopped following {hostname} after {MAX_DNS_HOPS} records")
    return name


def _record_target(walk: _Walk, zones: Sequence[dict[str, Any]], name: str) -> str | None:
    """One DNS hop: the alias or CNAME target of `name`, if a hosted zone has one."""
    candidates = [
        zone
        for zone in zones
        if name == (zone_name := _normalise(zone["Name"])) or name.endswith("." + zone_name)
    ]
    # Longest suffix wins: a delegated sub-zone holds the record, not its parent.
    for zone in sorted(candidates, key=lambda z: len(z["Name"]), reverse=True):
        records = _try(
            walk,
            "dns",
            f"reading records in {zone['Name']}",
            _collect,
            walk.clients.route53.list_resource_record_sets,
            "ResourceRecordSets",
            HostedZoneId=zone["Id"],
            StartRecordName=name,
        )
        for record in records or []:
            if _normalise(record.get("Name", "")) != name:
                continue
            if record.get("Type") not in ("A", "AAAA", "CNAME"):
                continue
            if alias := record.get("AliasTarget"):
                return _normalise(alias.get("DNSName", "")).removeprefix(_DUALSTACK)
            values = record.get("ResourceRecords") or []
            if record.get("Type") == "CNAME" and values:
                return _normalise(values[0].get("Value", ""))
    return None


def _normalise(name: str) -> str:
    """DNS names compare lowercase and without the root dot; Route 53 supplies both."""
    return name.strip().rstrip(".").lower()


# ------------------------------------------- hops 2-3: listeners and target groups


def _from_load_balancer(walk: _Walk, balancer: dict[str, Any], hostname: str) -> None:
    name = balancer.get("LoadBalancerName", "lb")
    kind = balancer.get("Type", "application")
    pairs = _target_groups(walk, balancer, hostname)
    if not pairs:
        walk.note(
            "load_balancer",
            f"{name} has no listener that forwards {hostname} to a target group",
        )
        return

    groups = _describe_groups(walk, [arn for _, arn in pairs])
    for listener, arn in pairs:
        port = listener.get("Port")
        parent = walk.add(
            Resource(
                id=f"lb/{name}/{port}",
                role="lb",
                address=balancer["DNSName"],
                port=port,
                health=(balancer.get("State") or {}).get("Code"),
                attributes=_present(
                    {
                        "load_balancer": name,
                        "type": kind,
                        "scheme": balancer.get("Scheme"),
                        "availability_zones": _azs(balancer),
                        "protocol": listener.get("Protocol"),
                        "target_group": _group_name(arn),
                    }
                ),
            )
        ).id
        _from_target_group(walk, groups.get(arn, {"TargetGroupArn": arn}), parent)


def _target_groups(walk: _Walk, balancer: dict[str, Any], hostname: str) -> list[tuple[dict, str]]:
    """The (listener, target group) pairs this hostname actually reaches.

    A host-based rule is how one balancer serves several hostnames, so a rule naming
    this host wins over the listener's default action. Listeners that only redirect
    contribute nothing and are passed over -- an :80 that sends everyone to :443 is
    not a place to send load.
    """
    listeners = _require(
        f"listing listeners on {balancer.get('LoadBalancerName')}",
        _collect,
        walk.clients.elbv2.describe_listeners,
        "Listeners",
        LoadBalancerArn=balancer["LoadBalancerArn"],
    )

    pairs: list[tuple[dict, str]] = []
    for listener in listeners:
        arns: list[str] = []
        if balancer.get("Type") == "application":
            # Rules exist on application load balancers only. Asking a network
            # balancer for them is an error, not an empty list.
            rules = _try(
                walk,
                "target_group",
                f"reading rules on listener {listener.get('Port')}",
                _collect,
                walk.clients.elbv2.describe_rules,
                "Rules",
                ListenerArn=listener["ListenerArn"],
            )
            arns = [
                arn
                for rule in rules or []
                if _rule_matches(rule, hostname)
                for arn in _forwarded(rule.get("Actions", []))
            ]
        if not arns:
            arns = _forwarded(listener.get("DefaultActions", []))
        pairs.extend((listener, arn) for arn in arns)

    seen: set[tuple[Any, str]] = set()
    unique = []
    for listener, arn in pairs:
        key = (listener.get("ListenerArn"), arn)
        if key not in seen:
            seen.add(key)
            unique.append((listener, arn))
    return unique


def _rule_matches(rule: dict[str, Any], hostname: str) -> bool:
    """Whether a rule names this hostname. A rule with no host condition does not."""
    for condition in rule.get("Conditions", []):
        if condition.get("Field") != "host-header":
            continue
        patterns = (condition.get("HostHeaderConfig") or {}).get("Values") or condition.get(
            "Values", []
        )
        if any(_host_matches(str(p), hostname) for p in patterns):
            return True
    return False


def _host_matches(pattern: str, hostname: str) -> bool:
    """ALB host patterns allow `*` and `?` and nothing else.

    Not `fnmatch`, which would also honour `[abc]` -- a literal bracket in a rule
    would then match something the balancer would not, which is worse than not
    supporting it.
    """
    translated = "".join(
        {"*": ".*", "?": "."}.get(piece, re.escape(piece))
        for piece in re.split(r"([*?])", pattern.strip().lower())
    )
    return re.fullmatch(translated, hostname.lower()) is not None


def _forwarded(actions: Iterable[dict[str, Any]]) -> list[str]:
    """Target group ARNs an action forwards to, weighted groups included."""
    arns = []
    for action in actions:
        if action.get("Type") != "forward":
            continue
        if arn := action.get("TargetGroupArn"):
            arns.append(arn)
        for entry in (action.get("ForwardConfig") or {}).get("TargetGroups", []):
            if (arn := entry.get("TargetGroupArn")) and arn not in arns:
                arns.append(arn)
    return arns


def _describe_groups(walk: _Walk, arns: Sequence[str]) -> dict[str, dict[str, Any]]:
    described = _try(
        walk,
        "target_group",
        "describing target groups",
        _collect,
        walk.clients.elbv2.describe_target_groups,
        "TargetGroups",
        TargetGroupArns=list(dict.fromkeys(arns)),
    )
    return {g["TargetGroupArn"]: g for g in described or []}


# ------------------------------------------------- hops 4-5: targets, then services


def _from_target_group(walk: _Walk, group: dict[str, Any], parent: str) -> None:
    arn = group["TargetGroupArn"]
    health = _require(
        f"reading target health for {_group_name(arn)}",
        _collect,
        walk.clients.elbv2.describe_target_health,
        "TargetHealthDescriptions",
        TargetGroupArn=arn,
    )
    walk.done("target_group")
    if not health:
        walk.note(
            "target_group", f"target group {_group_name(arn)} has no registered targets"
        )

    targets = [_target(entry) for entry in health]
    if drained := [t for t in targets if t.state not in ("healthy", "initial", None)]:
        # A deregistering target is still answering and still worth observing, but a
        # measurement that includes one and does not say so is a measurement of a
        # deployment rather than of a service.
        walk.note(
            "target_group",
            f"target group {_group_name(arn)}: "
            + ", ".join(f"{t.id}:{t.port} is {t.state}" for t in drained),
        )

    service = _service_index(walk).get(arn)
    if service is None:
        walk.note(
            "service",
            f"no ECS service is registered against target group {_group_name(arn)}; "
            "resolving its targets directly",
        )
        _bare_targets(walk, targets, group, parent)
        return

    walk.done("service")
    _from_service(walk, service, targets, group, parent)


@dataclass(frozen=True, slots=True)
class _Target:
    """One registered target, as the balancer sees it."""

    id: str
    port: int | None
    state: str | None
    reason: str | None
    availability_zone: str | None

    @property
    def is_instance(self) -> bool:
        return self.id.startswith("i-")


def _target(entry: dict[str, Any]) -> _Target:
    target = entry.get("Target", {})
    health = entry.get("TargetHealth", {})
    return _Target(
        id=str(target.get("Id", "")),
        port=target.get("Port"),
        state=health.get("State"),
        reason=health.get("Reason"),
        availability_zone=target.get("AvailabilityZone"),
    )


def _service_index(walk: _Walk) -> dict[str, dict[str, Any]]:
    """target group ARN -> the ECS service registered against it.

    There is no call that answers this directly, so the only way to know is to look
    at every service in every cluster. It is the expensive hop, which is one more
    reason discovery happens at setup and refresh time rather than anywhere near a
    send loop (design-api 3.2) -- and why the answer is built once per walk.
    """
    if walk.services is not None:
        return walk.services

    index: dict[str, dict[str, Any]] = {}
    clusters = _try(
        walk,
        "service",
        "listing ECS clusters",
        _collect,
        walk.clients.ecs.list_clusters,
        "clusterArns",
    )
    for cluster in clusters or []:
        names = _try(
            walk,
            "service",
            f"listing services in {_tail(cluster)}",
            _collect,
            walk.clients.ecs.list_services,
            "serviceArns",
            cluster=cluster,
        )
        for chunk in _chunks(names or [], DESCRIBE_SERVICES_MAX):
            described = _try(
                walk,
                "service",
                f"describing services in {_tail(cluster)}",
                walk.clients.ecs.describe_services,
                cluster=cluster,
                services=list(chunk),
            )
            for service in (described or {}).get("services", []):
                for balancer in service.get("loadBalancers", []):
                    if arn := balancer.get("targetGroupArn"):
                        index[arn] = service

    walk.services = index
    return index


def _bare_targets(
    walk: _Walk, targets: Sequence[_Target], group: dict[str, Any], parent: str
) -> None:
    """Targets with no ECS behind them: instance ids, or IPs to look up as interfaces."""
    instances: dict[str, _Target] = {}
    for target in targets:
        if target.is_instance:
            instances[target.id] = target
            continue
        if instance_id := _instance_behind_ip(walk, target.id):
            instances[instance_id] = target
        else:
            walk.note(
                "target_group",
                f"{target.id}:{target.port} is an IP target with no network interface in "
                "this account; it is addressable but nothing more is known about it",
            )
            walk.add(
                Resource(
                    id=f"target/{target.id}/{target.port}",
                    role="instance",
                    address=target.id,
                    port=target.port,
                    parent=parent,
                    health=target.state,
                    availability_zone=target.availability_zone,
                    attributes=_present(
                        {"target_group": _group_name(group["TargetGroupArn"])}
                    ),
                )
            )
    _instances(walk, instances, parent=parent, group=group)


def _instance_behind_ip(walk: _Walk, address: str) -> str | None:
    """The EC2 instance an IP target belongs to, when the interface is attached to one."""
    described = _try(
        walk,
        "instance",
        f"looking up the network interface for {address}",
        walk.clients.ec2.describe_network_interfaces,
        Filters=[{"Name": "addresses.private-ip-address", "Values": [address]}],
    )
    for interface in (described or {}).get("NetworkInterfaces", []):
        if instance_id := (interface.get("Attachment") or {}).get("InstanceId"):
            return str(instance_id)
    return None


# -------------------------------------------------- hops 5-6: tasks and containers


def _from_named_service(walk: _Walk, cluster: str, service_name: str) -> None:
    described = _require(
        f"describing service {service_name}",
        walk.clients.ecs.describe_services,
        cluster=cluster,
        services=[service_name],
    )
    services = described.get("services", [])
    if not services:
        raise DiscoveryError(
            f"no service named {service_name!r} in cluster {cluster!r}"
            + (f"; {_failure(described)}" if _failure(described) else "")
        )
    walk.done("service")
    _from_service(walk, services[0], targets=[], group=None, parent=None)


def _from_service(
    walk: _Walk,
    service: dict[str, Any],
    targets: Sequence[_Target],
    group: dict[str, Any] | None,
    parent: str | None,
) -> None:
    cluster = service.get("clusterArn", "")
    name = service.get("serviceName", "")
    desired, running = service.get("desiredCount"), service.get("runningCount")
    if desired is not None and running is not None and desired != running:
        # Worth saying out loud: a measurement taken mid-deployment against fewer
        # boxes than the service is meant to have is a different measurement.
        walk.note("service", f"service {name}: {running} of {desired} tasks running")

    arns = _try(
        walk,
        "task",
        f"listing tasks for {name}",
        _collect,
        walk.clients.ecs.list_tasks,
        "taskArns",
        cluster=cluster,
        serviceName=name,
        desiredStatus="RUNNING",
    )
    if not arns:
        walk.note("task", f"service {name} has no running tasks")
        return

    tasks: list[dict[str, Any]] = []
    for chunk in _chunks(arns, DESCRIBE_TASKS_MAX):
        described = _try(
            walk,
            "task",
            f"describing tasks for {name}",
            walk.clients.ecs.describe_tasks,
            cluster=cluster,
            tasks=list(chunk),
        )
        tasks.extend((described or {}).get("tasks", []))
    if not tasks:
        return
    walk.done("task")

    hosts = _container_instances(walk, cluster, tasks)
    by_ip = {t.id: t for t in targets if not t.is_instance}
    by_instance = {t.id: t for t in targets if t.is_instance}
    container_port = next(
        (
            balancer.get("containerPort")
            for balancer in service.get("loadBalancers", [])
            if balancer.get("containerPort")
        ),
        None,
    )

    addresses = {_task_id(task): _task_address(task) for task in tasks}
    if unaddressed := sorted(t for t, address in addresses.items() if address is None):
        # One note, not one per task: a service in bridge networking has this true of
        # every task it runs, and fifty identical notes would bury the useful ones.
        walk.note(
            "task",
            f"service {name}: {len(unaddressed)} task(s) have no network interface and are "
            f"addressed through the instance hosting them ({_first(unaddressed)})",
        )

    for task in tasks:
        instance_id = hosts.get(task.get("containerInstanceArn", ""))
        address = addresses[_task_id(task)]
        target = by_ip.get(address or "") or by_instance.get(instance_id or "")
        walk.add(
            Resource(
                id=_task_id(task),
                role="task",
                address=address,
                port=(
                    (target.port if target else None) or container_port or (group or {}).get("Port")
                ),
                parent=instance_id,
                health=(target.state if target else None) or task.get("healthStatus"),
                instance_id=instance_id,
                availability_zone=task.get("availabilityZone"),
                cluster=_tail(cluster),
                service=name,
                task_arn=task.get("taskArn"),
                task_definition=_tail(task.get("taskDefinitionArn", "")) or None,
                attributes=_present(
                    {
                        "launch_type": task.get("launchType"),
                        "target_group": (
                            _group_name(group["TargetGroupArn"]) if group else None
                        ),
                        "reached_via": parent,
                    }
                ),
            )
        )
        _containers(walk, task)

    # Fargate resolves no instances, and the dict is empty: the box under a task is
    # AWS's business there, and there is nothing to log into.
    _instances(walk, {i: by_instance.get(i) for i in hosts.values()}, parent=parent, group=group)


def _task_address(task: dict[str, Any]) -> str | None:
    """The task's own IP, from its awsvpc interface.

    A task in bridge or host networking has none: it is reachable at its instance,
    which the instance hop supplies. Saying so is more useful than a blank address.
    """
    for attachment in task.get("attachments", []):
        if attachment.get("type") != "ElasticNetworkInterface":
            continue
        for detail in attachment.get("details", []):
            if detail.get("name") == "privateIPv4Address" and detail.get("value"):
                return str(detail["value"])
    return None


def _containers(walk: _Walk, task: dict[str, Any]) -> None:
    """One resource per container, digest included -- the identity of the build."""
    task_id = _task_id(task)
    for container in task.get("containers", []):
        name = container.get("name", "container")
        bindings = container.get("networkBindings") or []
        walk.add(
            Resource(
                id=f"{task_id}/{name}",
                role="container",
                parent=task_id,
                port=next((b.get("hostPort") for b in bindings if b.get("hostPort")), None),
                health=container.get("healthStatus"),
                container=name,
                image=container.get("image"),
                image_digest=container.get("imageDigest"),
                attributes=_present({"status": container.get("lastStatus")}),
            )
        )


def _container_instances(
    walk: _Walk, cluster: str, tasks: Sequence[dict[str, Any]]
) -> dict[str, str]:
    """container instance ARN -> EC2 instance id, for the EC2 launch type."""
    arns = sorted({t["containerInstanceArn"] for t in tasks if t.get("containerInstanceArn")})
    if not arns:
        return {}
    described = _try(
        walk,
        "instance",
        "describing container instances",
        walk.clients.ecs.describe_container_instances,
        cluster=cluster,
        containerInstances=arns,
    )
    return {
        entry["containerInstanceArn"]: entry["ec2InstanceId"]
        for entry in (described or {}).get("containerInstances", [])
        if entry.get("ec2InstanceId")
    }


# ------------------------------------------------------- hops 7-8: EC2, autoscaling


def _instances(
    walk: _Walk,
    targets: dict[str, _Target | None],
    *,
    parent: str | None,
    group: dict[str, Any] | None,
) -> None:
    """Describe the instances behind a set of ids, with their autoscaling context."""
    if not targets:
        return
    described = _try(
        walk,
        "instance",
        "describing EC2 instances",
        walk.clients.ec2.describe_instances,
        InstanceIds=sorted(targets),
    )
    if not described:
        return

    found = [
        instance
        for reservation in described.get("Reservations", [])
        for instance in reservation.get("Instances", [])
    ]
    if not found:
        return
    walk.done("instance")

    groups = _autoscaling(walk, [i["InstanceId"] for i in found])
    for instance in found:
        instance_id = instance["InstanceId"]
        target = targets.get(instance_id)
        walk.add(
            Resource(
                id=instance_id,
                role="instance",
                address=instance.get("PrivateIpAddress"),
                port=target.port if target else None,
                parent=parent,
                health=(
                    (target.state if target else None)
                    or (instance.get("State") or {}).get("Name")
                ),
                instance_id=instance_id,
                instance_type=instance.get("InstanceType"),
                availability_zone=(instance.get("Placement") or {}).get("AvailabilityZone"),
                private_dns=instance.get("PrivateDnsName") or None,
                asg=groups.get(instance_id),
                attributes=_present(
                    {"target_group": _group_name(group["TargetGroupArn"]) if group else None}
                ),
            )
        )


def _autoscaling(walk: _Walk, instance_ids: Sequence[str]) -> dict[str, Asg]:
    """instance id -> its autoscaling group, sized.

    Two calls: which group each instance is in, then how big those groups are meant
    to be. Desired/min/max is what says whether a host count that moved mid-run was
    the environment scaling or something falling over (design-api 3.3).
    """
    membership: dict[str, str] = {}
    for chunk in _chunks(sorted(instance_ids), DESCRIBE_ASG_INSTANCES_MAX):
        described = _try(
            walk,
            "asg",
            "reading autoscaling membership",
            _collect,
            walk.clients.autoscaling.describe_auto_scaling_instances,
            "AutoScalingInstances",
            InstanceIds=list(chunk),
        )
        for entry in described or []:
            if name := entry.get("AutoScalingGroupName"):
                membership[entry["InstanceId"]] = name
    if not membership:
        return {}

    described = _try(
        walk,
        "asg",
        "describing autoscaling groups",
        _collect,
        walk.clients.autoscaling.describe_auto_scaling_groups,
        "AutoScalingGroups",
        AutoScalingGroupNames=sorted(set(membership.values())),
    )
    sized = {
        entry["AutoScalingGroupName"]: Asg(
            name=entry["AutoScalingGroupName"],
            desired=entry.get("DesiredCapacity"),
            minimum=entry.get("MinSize"),
            maximum=entry.get("MaxSize"),
        )
        for entry in described or []
    }
    if sized:
        walk.done("asg")
    return {
        instance_id: sized.get(name, Asg(name=name)) for instance_id, name in membership.items()
    }


# ------------------------------------------------------------------------- plumbing


def _pages(operation: Callable[..., dict[str, Any]], **kwargs: Any) -> Iterator[dict[str, Any]]:
    """Every page of a paginated call.

    Hand-rolled rather than `client.get_paginator`, which would put a boto3 object
    between the walk and the call and leave the fixture tests with nothing to stand
    in for. The three token spellings in `_TOKENS` are all AWS uses here.
    """
    request = dict(kwargs)
    while True:
        page = operation(**request)
        yield page
        for out_key, in_key in _TOKENS:
            if token := page.get(out_key):
                request[in_key] = token
                break
        else:
            return


def _collect(operation: Callable[..., dict[str, Any]], key: str, **kwargs: Any) -> list[Any]:
    return [item for page in _pages(operation, **kwargs) for item in page.get(key, [])]


def _require(what: str, call: Callable[..., Any], *args: Any, **kwargs: Any) -> Any:
    """A call on the spine. Failing it means an empty answer, so it raises instead."""
    try:
        return call(*args, **kwargs)
    except (ClientError, BotoCoreError) as exc:
        raise DiscoveryError(
            f"{what}: {_reason(exc)}; discovery needs the read-only permissions in "
            "policy/metrix-readonly.json"
        ) from exc


def _try(
    walk: _Walk, hop: str, what: str, call: Callable[..., Any], *args: Any, **kwargs: Any
) -> Any:
    """A call the walk can do without. A refusal becomes a note and the walk goes on.

    An account that withholds `autoscaling:Describe*` still has tasks worth finding,
    and an error at that hop should cost the scaling context, not the answer.
    """
    try:
        return call(*args, **kwargs)
    except (ClientError, BotoCoreError) as exc:
        walk.note(hop, f"{what}: {_reason(exc)}")
        return None


def _reason(exc: Exception) -> str:
    if isinstance(exc, ClientError):
        error = exc.response.get("Error", {})
        return f"{error.get('Code', 'error')}: {error.get('Message', exc)}"
    return str(exc)


def _failure(described: dict[str, Any]) -> str:
    """ECS reports a missing thing in `failures` with a 200, not as an error."""
    return "; ".join(
        f"{f.get('arn', '?')}: {f.get('reason', 'unknown')}" for f in described.get("failures", [])
    )


def _first(items: Sequence[str], limit: int = 3) -> str:
    """A few names and a count. A note that lists forty ids is not read."""
    shown = ", ".join(items[:limit])
    return shown if len(items) <= limit else f"{shown}, and {len(items) - limit} more"


def _chunks(items: Sequence[Any], size: int) -> Iterator[Sequence[Any]]:
    for start in range(0, len(items), size):
        yield items[start : start + size]


def _group_name(arn: str) -> str:
    """The name in a target group ARN, which ends `.../targetgroup/<name>/<id>`.

    The last segment is the id, so `_tail` would put a hex string on screen where a
    name belongs.
    """
    parts = arn.rsplit("/", 2)
    return parts[-2] if len(parts) == 3 else _tail(arn)


def _tail(arn: str) -> str:
    """The last path segment of an ARN: `family:12`, a cluster name, a group name."""
    return arn.rsplit("/", 1)[-1] if arn else ""


def _task_id(task: dict[str, Any]) -> str:
    return f"task/{_tail(task.get('taskArn', '')) or 'unknown'}"


def _azs(balancer: dict[str, Any]) -> str | None:
    zones = sorted(z.get("ZoneName", "") for z in balancer.get("AvailabilityZones", []))
    return ", ".join(z for z in zones if z) or None


def _present(values: dict[str, Any]) -> dict[str, str]:
    return {k: str(v) for k, v in values.items() if v is not None}
