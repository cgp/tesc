"""Discovery, against recorded AWS responses.

Every case here is driven by a committed JSON fixture of `describe_*` responses, so
the suite never needs credentials, a network, or an account that happens to be shaped
the right way. The fixtures cover what matters about this code: not the happy chain,
which is the easy half, but the partial ones -- an NLB with no ECS behind it, a task
with no network interface, a target mid-deregistration, a hop the account refuses.
"""

from __future__ import annotations

import copy
import json
from pathlib import Path
from typing import Any

import pytest
from botocore.exceptions import ClientError

from metrix_api.discovery import Clients, DiscoveryError, discover, from_document
from metrix_api.discovery.inventory import NOTHING

FIXTURES = Path(__file__).parent / "fixtures" / "aws"


# ------------------------------------------------------------------- the replayer


class RecordedClient:
    """One AWS client, answering from a recording rather than from AWS.

    Matching is exact first and by subset second, so a fixture can pin a call it
    cares about -- the second page of a paginated response, one IP looked up rather
    than another -- while leaving calls it does not care about matched loosely by
    the arguments it did write down.
    """

    def __init__(self, service: str, recorded: dict[str, Any], calls: list[tuple[str, str, dict]]):
        self._service = service
        self._recorded = recorded
        self._calls = calls

    def __getattr__(self, operation: str):
        if operation.startswith("_"):
            raise AttributeError(operation)

        def call(**kwargs: Any) -> dict[str, Any]:
            self._calls.append((self._service, operation, kwargs))
            entries = self._recorded.get(operation)
            if entries is None:
                raise AssertionError(
                    f"{self._service}.{operation} was called and the fixture has no "
                    f"recording of it; arguments were {kwargs!r}"
                )
            for entry in _ranked(entries, kwargs):
                if error := entry.get("error"):
                    raise ClientError({"Error": error}, operation)
                return entry["response"]
            raise AssertionError(
                f"{self._service}.{operation} was called with {kwargs!r}, which matches "
                f"none of the {len(entries)} recorded response(s)"
            )

        return call


def _ranked(entries: list[dict[str, Any]], kwargs: dict[str, Any]) -> list[dict[str, Any]]:
    exact = [e for e in entries if e.get("request", {}) == kwargs]
    subset = [
        e
        for e in entries
        if all(kwargs.get(key) == value for key, value in e.get("request", {}).items())
    ]
    return exact + subset


def account(name: str) -> dict[str, Any]:
    """A recorded account, as a plain dict a test may bend into the shape it needs."""
    return json.loads((FIXTURES / f"{name}.json").read_text(encoding="utf-8"))


def clients_from(recording: dict[str, Any]) -> tuple[Clients, list[tuple[str, str, dict]]]:
    calls: list[tuple[str, str, dict]] = []
    return (
        Clients(
            **{
                service: RecordedClient(service, recording.get(service, {}), calls)
                for service in ("route53", "elbv2", "ecs", "ec2", "autoscaling")
            }
        ),
        calls,
    )


def recorded(name: str, **overrides: dict[str, Any]) -> tuple[Clients, list[tuple[str, str, dict]]]:
    """Clients backed by a fixture, and the list of calls they are asked to make."""
    recording = account(name)
    for service, operations in overrides.items():
        recording.setdefault(service, {}).update(operations)
    return clients_from(recording)


def called(calls: list[tuple[str, str, dict]], service: str, operation: str) -> int:
    return sum(1 for s, o, _ in calls if s == service and o == operation)


def note_text(inventory) -> str:
    return " | ".join(n.message for n in inventory.notes)


# --------------------------------------------------------- the chain, end to end


def test_a_hostname_resolves_to_tasks_with_image_digests():
    clients, _ = recorded("alb-fargate")
    inventory = discover(clients, hostname="api.staging.example.com")

    assert inventory.reached == "task"
    assert inventory.source == "api.staging.example.com"

    tasks = inventory.by_role("task")
    assert [t.endpoint for t in tasks] == ["10.0.11.21:8080", "10.0.12.22:8080"]
    assert {t.task_definition for t in tasks} == {"metrix-api:47"}
    assert {t.cluster for t in tasks} == {"staging"}
    assert {t.service for t in tasks} == {"api"}
    assert [t.health for t in tasks] == ["healthy", "healthy"]
    assert [t.availability_zone for t in tasks] == ["eu-west-1a", "eu-west-1b"]

    # The digest is the identity of the build -- the field that later says "different
    # deployment" rather than "regression".
    digests = {c.container: c.image_digest for c in inventory.by_role("container")}
    assert digests["api"].startswith("sha256:9c1d0f2a")
    assert digests["log-router"].startswith("sha256:1a2b3c4d")
    assert len(inventory.by_role("container")) == 4

    for container in inventory.by_role("container"):
        assert inventory.resource(container.parent).role == "task"


def test_the_load_balancer_is_addressed_at_its_listener_port():
    clients, _ = recorded("alb-fargate")
    inventory = discover(clients, hostname="api.staging.example.com")

    (balancer,) = inventory.by_role("lb")
    assert balancer.endpoint == "metrix-staging-alb-1904471234.eu-west-1.elb.amazonaws.com:443"
    assert balancer.health == "active"
    assert balancer.attributes["scheme"] == "internet-facing"
    assert balancer.attributes["availability_zones"] == "eu-west-1a, eu-west-1b"
    # The :80 listener only redirects to :443. Sending load at a redirect measures the
    # redirect, so a listener that forwards nowhere contributes no endpoint.
    assert balancer.attributes["protocol"] == "HTTPS"


def test_a_host_rule_wins_over_the_listener_default():
    """One balancer serves several hostnames; the rule naming this one is the answer."""
    clients, calls = recorded("alb-fargate")
    inventory = discover(clients, hostname="api.staging.example.com")

    reached = {r.attributes.get("target_group") for r in inventory.by_role("lb")}
    assert reached == {"metrix-staging-api"}
    assert "metrix-staging-web" not in json.dumps(inventory.to_document())
    # The default action's group is never even asked about.
    assert not [c for c in calls if "metrix-staging-web" in json.dumps(c[2])]


def test_pagination_is_followed():
    """The balancer under test is on the second page, as it would be in a real account."""
    clients, calls = recorded("alb-fargate")
    inventory = discover(clients, hostname="api.staging.example.com")

    assert called(calls, "elbv2", "describe_load_balancers") == 2
    assert inventory.by_role("lb")


def test_the_expensive_service_scan_happens_once():
    clients, calls = recorded("alb-fargate")
    discover(clients, hostname="api.staging.example.com")
    assert called(calls, "ecs", "list_clusters") == 1


def test_a_service_behind_two_target_groups_is_not_listed_twice():
    """A traffic shift puts one service in two groups, and both walk to the same tasks."""
    recording = account("alb-fargate")
    blue = (
        "arn:aws:elasticloadbalancing:eu-west-1:123456789012:targetgroup/"
        "metrix-staging-api/1122334455667788"
    )
    green = (
        "arn:aws:elasticloadbalancing:eu-west-1:123456789012:targetgroup/"
        "metrix-staging-api-green/99aabbccddeeff00"
    )

    matched = recording["elbv2"]["describe_rules"][1]["response"]["Rules"][0]
    matched["Actions"] = [
        {
            "Type": "forward",
            "ForwardConfig": {
                "TargetGroups": [
                    {"TargetGroupArn": blue, "Weight": 90},
                    {"TargetGroupArn": green, "Weight": 10},
                ]
            },
        }
    ]
    recording["ecs"]["describe_services"][0]["response"]["services"][0]["loadBalancers"].append(
        {"targetGroupArn": green, "containerName": "api", "containerPort": 8080}
    )
    recording["elbv2"]["describe_target_groups"] = [
        {
            "request": {},
            "response": {
                "TargetGroups": [
                    {"TargetGroupArn": blue, "TargetGroupName": "metrix-staging-api",
                     "Port": 8080, "TargetType": "ip"},
                    {"TargetGroupArn": green, "TargetGroupName": "metrix-staging-api-green",
                     "Port": 8080, "TargetType": "ip"},
                ]
            },
        }
    ]
    recording["elbv2"]["describe_target_health"].append(
        {"request": {"TargetGroupArn": green}, "response": {"TargetHealthDescriptions": []}}
    )

    clients, _ = clients_from(recording)
    inventory = discover(clients, hostname="api.staging.example.com")

    assert len(inventory.by_role("task")) == 2
    assert len(inventory.by_role("container")) == 4
    assert len(inventory.by_role("lb")) == 1
    assert "has no registered targets" in note_text(inventory)


# ------------------------------------------------------- EC2, autoscaling, and ASGs


def test_the_ec2_launch_type_reaches_the_autoscaling_group():
    clients, _ = recorded("alb-ecs-ec2")
    inventory = discover(clients, hostname="orders.example.com")

    assert inventory.reached == "asg"
    instances = {i.id: i for i in inventory.by_role("instance")}
    assert set(instances) == {"i-0aaa1111bbbb2222c", "i-0bbb2222cccc3333d"}

    first = instances["i-0aaa1111bbbb2222c"]
    assert first.endpoint == "10.1.1.11:32768"
    assert (first.instance_type, first.availability_zone) == ("m6i.large", "us-east-1a")
    assert first.private_dns == "ip-10-1-1-11.ec2.internal"
    assert first.asg.name == "orders-ecs-asg"
    assert (first.asg.desired, first.asg.minimum, first.asg.maximum) == (2, 2, 6)
    # Two instance types under one service: the kind of difference that explains an
    # outlier, and the reason placement is kept per resource rather than per run.
    assert instances["i-0bbb2222cccc3333d"].instance_type == "m6i.xlarge"


def test_two_revisions_of_one_service_are_both_recorded():
    clients, _ = recorded("alb-ecs-ec2")
    inventory = discover(clients, hostname="orders.example.com")

    assert {t.task_definition for t in inventory.by_role("task")} == {
        "orders-api:118",
        "orders-api:117",
    }


def test_a_cname_is_followed_to_the_load_balancer():
    clients, calls = recorded("alb-ecs-ec2")
    discover(clients, hostname="orders.example.com")
    assert called(calls, "route53", "list_resource_record_sets") == 1


# --------------------------------------------------------------- partial resolution


def test_a_task_with_no_interface_is_addressed_through_its_instance():
    """Bridge networking: the task has no IP of its own, and the note says so once."""
    clients, _ = recorded("alb-ecs-ec2")
    inventory = discover(clients, hostname="orders.example.com")

    task = next(t for t in inventory.by_role("task") if t.task_definition == "orders-api:118")
    assert task.address is None
    assert task.instance_id == "i-0aaa1111bbbb2222c"
    assert task.port == 32768  # the host port the balancer is registered against
    assert inventory.resource(task.parent).role == "instance"

    interfaces = [n for n in inventory.notes if "no network interface" in n.message]
    assert len(interfaces) == 1
    assert "2 task(s)" in interfaces[0].message


def test_a_service_short_of_its_desired_count_is_noted():
    clients, _ = recorded("alb-ecs-ec2")
    inventory = discover(clients, hostname="orders.example.com")
    assert "2 of 3 tasks running" in note_text(inventory)


def test_a_deregistering_target_is_a_note_and_not_a_task():
    clients, _ = recorded("alb-fargate")
    inventory = discover(clients, hostname="api.staging.example.com")

    assert "10.0.11.23:8080 is draining" in note_text(inventory)
    assert "10.0.11.23" not in json.dumps(
        [r.address for r in inventory.resources if r.address]
    )
    assert inventory.partial


def test_an_nlb_with_no_ecs_stops_at_the_instance():
    clients, calls = recorded("nlb-no-ecs")
    inventory = discover(
        clients, hostname="metrix-edge-nlb-abc123def456.elb.eu-west-1.amazonaws.com"
    )

    assert inventory.reached == "instance"
    assert "no ECS service is registered" in note_text(inventory)
    assert not inventory.by_role("task")

    (instance,) = [r for r in inventory.by_role("instance") if r.instance_id]
    assert instance.endpoint == "10.0.5.7:443"
    assert instance.instance_type == "c6i.2xlarge"

    # A hostname that is already a load balancer needs no zone lookup, and rules do
    # not exist on a network load balancer -- asking for them is an error, not [].
    assert called(calls, "route53", "list_hosted_zones") == 0
    assert called(calls, "elbv2", "describe_rules") == 0


def test_an_ip_target_belonging_to_nothing_is_still_an_address():
    clients, _ = recorded("nlb-no-ecs")
    inventory = discover(
        clients, hostname="metrix-edge-nlb-abc123def456.elb.eu-west-1.amazonaws.com"
    )

    orphan = inventory.resource("target/10.0.6.8/443")
    assert orphan is not None
    assert orphan.endpoint == "10.0.6.8:443"
    assert orphan.instance_id is None
    assert "no network interface in this account" in note_text(inventory)


def test_a_hostname_nothing_recognises_is_a_note_rather_than_an_error():
    clients, _ = recorded("alb-fargate")
    inventory = discover(clients, hostname="nothing.staging.example.com")

    assert inventory.reached == NOTHING
    assert inventory.resources == []
    assert "does not resolve to a load balancer" in note_text(inventory)


# ------------------------------------------------------------- refused permissions


def test_a_refused_optional_hop_costs_its_context_and_not_the_answer():
    """An account withholding autoscaling still has tasks worth finding."""
    denied = {
        "describe_auto_scaling_instances": [
            {
                "request": {},
                "error": {
                    "Code": "AccessDenied",
                    "Message": "not authorized to perform: "
                    "autoscaling:DescribeAutoScalingInstances",
                },
            }
        ]
    }
    clients, _ = recorded("alb-ecs-ec2", autoscaling=denied)
    inventory = discover(clients, hostname="orders.example.com")

    assert inventory.reached == "instance"
    assert len(inventory.by_role("task")) == 2
    assert all(i.asg is None for i in inventory.by_role("instance"))
    assert "AccessDenied" in note_text(inventory)


def test_a_refused_spine_call_raises_and_names_the_policy():
    denied = {
        "describe_load_balancers": [
            {
                "request": {},
                "error": {"Code": "AccessDenied", "Message": "not authorized"},
            }
        ]
    }
    clients, _ = recorded("alb-fargate", elbv2=denied)
    with pytest.raises(DiscoveryError) as raised:
        discover(clients, hostname="api.staging.example.com")
    assert "policy/metrix-readonly.json" in str(raised.value)


# ---------------------------------------------------------- naming a service directly


def test_naming_a_cluster_and_service_skips_dns_and_the_balancer():
    clients, calls = recorded("alb-fargate")
    inventory = discover(
        clients,
        cluster="arn:aws:ecs:eu-west-1:123456789012:cluster/staging",
        service="api",
    )

    assert inventory.reached == "task"
    assert inventory.source.endswith("cluster/staging/api")
    assert [t.endpoint for t in inventory.by_role("task")] == [
        "10.0.11.21:8080",
        "10.0.12.22:8080",
    ]
    assert not inventory.by_role("lb")
    assert not [c for c in calls if c[0] in ("elbv2", "route53")]


def test_a_hostname_and_a_service_together_is_a_mistake_worth_naming():
    clients, _ = recorded("alb-fargate")
    with pytest.raises(DiscoveryError, match="not both"):
        discover(clients, hostname="api.staging.example.com", cluster="staging", service="api")


def test_neither_a_hostname_nor_a_service_is_a_mistake_worth_naming():
    clients, _ = recorded("alb-fargate")
    with pytest.raises(DiscoveryError, match="needs a hostname"):
        discover(clients)


# ----------------------------------------------------------------- the snapshot


def test_an_inventory_round_trips_through_its_document():
    """A pinned snapshot is only useful if it reads back as what was written."""
    clients, _ = recorded("alb-ecs-ec2")
    inventory = discover(clients, hostname="orders.example.com")

    restored = from_document(copy.deepcopy(inventory.to_document()))
    assert restored.to_document() == inventory.to_document()
    assert restored.resources == inventory.resources
    assert restored.reached == inventory.reached
    assert restored.notes == inventory.notes


def test_a_field_that_was_not_determined_is_absent_rather_than_null():
    clients, _ = recorded("alb-fargate")
    inventory = discover(clients, hostname="api.staging.example.com")

    task = inventory.by_role("task")[0]
    document = next(r for r in inventory.to_document()["resources"] if r["id"] == task.id)
    assert "instance_type" not in document
    assert "asg" not in document
    assert document["task_definition"] == "metrix-api:47"


@pytest.mark.parametrize("name", sorted(p.stem for p in FIXTURES.glob("*.json")))
def test_every_fixture_says_what_it_is(name: str):
    """A recording nobody can read is a recording nobody will update."""
    account = json.loads((FIXTURES / f"{name}.json").read_text(encoding="utf-8"))
    assert account.get("description")


# ----------------------------------------------------------------------- the route


@pytest.fixture
def api(tmp_path, monkeypatch):
    """The app, with the AWS session replaced by a recording.

    The route is the only thing that builds real clients, so this is the one place a
    test has to intercept anything: everything above works on `Clients` it was handed.
    """
    from fastapi.testclient import TestClient

    from metrix_api.config import load_config
    from metrix_api.main import create_app

    clients, _ = recorded("alb-fargate")
    monkeypatch.setattr(
        "metrix_api.routes.discovery.Clients.from_config", classmethod(lambda cls, aws: clients)
    )
    return TestClient(create_app(load_config(tmp_path).ensure_layout()))


def test_the_route_returns_the_inventory_and_says_it_is_partial(api):
    response = api.post("/api/discovery/resolve", json={"hostname": "api.staging.example.com"})
    assert response.status_code == 200

    body = response.json()
    assert body["partial"] is True  # a target is draining
    assert body["inventory"]["reached"] == "task"
    assert [r["id"] for r in body["inventory"]["resources"] if r["role"] == "task"]
    assert body["inventory"]["notes"]


def test_the_route_needs_something_to_resolve(api):
    response = api.post("/api/discovery/resolve", json={"cluster": "staging"})
    assert response.status_code == 422
    assert "cluster and a service" in response.json()["detail"]


def test_an_aws_failure_is_reported_as_an_upstream_one(api, monkeypatch):
    def refuse(cls, aws):
        raise DiscoveryError("could not open an AWS session: NoCredentialsError")

    monkeypatch.setattr(
        "metrix_api.routes.discovery.Clients.from_config", classmethod(refuse)
    )
    response = api.post("/api/discovery/resolve", json={"hostname": "api.staging.example.com"})
    assert response.status_code == 502
    assert "AWS session" in response.json()["detail"]
