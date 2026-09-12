"""The shipped IAM policy and the calls discovery actually makes, checked against
each other.

`policy/metrix-readonly.json` is in the repo because working out a permission set
from a sequence of AccessDenied errors is a poor introduction to a tool
(design-api 3.2). A document that says that is only worth shipping if it stays true,
so this reads the calls out of `ecs.py` itself: a new hop that needs a permission the
policy does not grant fails here rather than in someone's account, and a permission
the walk stopped needing is caught too -- read-only or not, a policy should not grant
what nothing uses.
"""

from __future__ import annotations

import ast
import fnmatch
import json
import re
from pathlib import Path

import pytest

from metrix_api.discovery.ecs import SERVICES

REPO = Path(__file__).resolve().parents[2]
POLICY = REPO / "policy" / "metrix-readonly.json"
WALK = REPO / "api" / "src" / "metrix_api" / "discovery" / "ecs.py"


def api_action(client: str, operation: str) -> str:
    """`ecs` + `list_tasks` -> `ecs:ListTasks`, the spelling IAM uses."""
    return f"{SERVICES[client]}:" + "".join(word.title() for word in operation.split("_"))


def calls_made() -> set[str]:
    """Every AWS call in the walk, as an IAM action.

    Read from the source rather than from a run, because a hop taken only by an
    account shaped a particular way is exactly the one a fixture suite would miss.
    """
    tree = ast.parse(WALK.read_text(encoding="utf-8"))
    found = set()
    for node in ast.walk(tree):
        # `<anything>.clients.<service>.<operation>`
        if not isinstance(node, ast.Attribute) or not isinstance(node.value, ast.Attribute):
            continue
        client = node.value
        if not isinstance(client.value, ast.Attribute) or client.value.attr != "clients":
            continue
        if client.attr in SERVICES:
            found.add(api_action(client.attr, node.attr))
    return found


def allowed() -> list[str]:
    document = json.loads(POLICY.read_text(encoding="utf-8"))
    return [
        action
        for statement in document["Statement"]
        if statement["Effect"] == "Allow"
        for action in statement["Action"]
    ]


def test_the_walk_makes_calls_worth_checking():
    """A guard on the guard: an AST that matched nothing would pass everything."""
    made = calls_made()
    assert len(made) >= 10
    assert "ecs:DescribeTasks" in made
    assert "elasticloadbalancing:DescribeTargetHealth" in made


@pytest.mark.parametrize("action", sorted(calls_made()))
def test_the_policy_allows_every_call_discovery_makes(action: str):
    assert any(fnmatch.fnmatch(action, pattern) for pattern in allowed()), (
        f"{action} is called by discovery and policy/metrix-readonly.json does not "
        f"allow it; add it there, not to an inline note"
    )


@pytest.mark.parametrize("pattern", sorted(allowed()))
def test_the_policy_grants_nothing_that_is_never_called(pattern: str):
    made = calls_made()
    assert any(fnmatch.fnmatch(action, pattern) for action in made), (
        f"policy/metrix-readonly.json allows {pattern} and nothing in discovery calls "
        f"it; a permission set that outlives its use is one nobody trusts"
    )


def test_every_permission_is_a_read():
    """The engine never gets a cloud identity, and this identity never writes."""
    for pattern in allowed():
        verb = pattern.split(":", 1)[1]
        assert re.match(r"^(Describe|List|Get)", verb), f"{pattern} is not a read"
