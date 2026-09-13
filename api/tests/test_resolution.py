"""Storing an inventory, pinning it to a recording, and noticing a moved environment.

The questions here are the ones a snapshot exists to answer months after the fact:
*what did this run measure*, and *when did that environment last change*. Both are
lost if a resolution is re-derived on read, so both are tested against the store
rather than against a fresh walk.
"""

from __future__ import annotations

import asyncio
import copy
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

from metrix_api import reachability
from metrix_api.discovery import compare, to_endpoints
from metrix_api.discovery.resolve import ResolveError, Resolver
from metrix_api.observer.metrics import HOST_COUNT_CHANGED
from metrix_api.profiles import Collection, ProfileError, parse_profile
from metrix_api.recording import RecordingError, start_observation
from metrix_api.store import inventories, open_store
from metrix_api.store import recordings as store
from tests.test_discovery import account, clients_from, recorded
from tests.test_observer_collector import FakeClock, FakeTransport
from tests.test_observer_linux import block

FARGATE = {
    "name": "staging",
    "addressing": "load_balancer",
    "discover": {
        "hostname": "api.staging.example.com",
        "ttl": "10m",
        "collect": {"transport": "scrape", "port": 9100},
    },
}

BLOCKS = [block(cpu_user=100, cpu_idle=900), block(cpu_user=200, cpu_idle=1800)]


@pytest.fixture
def db(tmp_path: Path):
    with open_store(tmp_path / "metrix.db") as conn:
        yield conn


def profile(**overrides):
    return parse_profile({**FARGATE, **overrides})


def walked(name: str = "alb-fargate", hostname: str = "api.staging.example.com"):
    from metrix_api.discovery import discover

    clients, _ = recorded(name)
    return discover(clients, hostname=hostname)


# ------------------------------------------------------------------ the profile side


class TestDiscoverBlock:
    def test_a_profile_may_say_where_to_find_its_endpoints(self) -> None:
        parsed = profile()
        assert parsed.discover is not None
        assert parsed.discover.source == "api.staging.example.com"
        assert parsed.discover.ttl == timedelta(minutes=10)
        assert parsed.discover.collect.transport == "scrape"
        assert parsed.endpoints == [], "not resolved yet, and not pretending otherwise"

    def test_a_profile_with_neither_endpoints_nor_discovery_is_rejected(self) -> None:
        with pytest.raises(ProfileError, match="'endpoints' must be a non-empty list"):
            parse_profile({"name": "empty", "endpoints": []})

    def test_the_two_entry_points_are_alternatives(self) -> None:
        with pytest.raises(ProfileError, match="not both"):
            profile(discover={"hostname": "a.example.com", "cluster": "c", "service": "s"})
        with pytest.raises(ProfileError, match="needs a hostname"):
            profile(discover={"cluster": "c"})

    def test_direct_addressing_without_a_hostname_needs_a_host_header(self) -> None:
        """Discovered boxes are addressed by IP, and most services route on the header."""
        with pytest.raises(ProfileError, match="host_header is required"):
            profile(addressing="direct", discover={"cluster": "staging", "service": "api"})
        assert profile(
            addressing="direct",
            discover={"cluster": "staging", "service": "api", "host_header": "api.example.com"},
        ).discover.header == "api.example.com"

    def test_a_discovered_profile_round_trips_without_freezing_its_endpoints(self) -> None:
        from metrix_api.profiles import to_document

        parsed = profile()
        resolved = parsed.with_endpoints(to_endpoints(walked())[0])
        assert resolved.endpoints, "resolution produced endpoints"
        # Writing them back would freeze one walk into a document that asks for a fresh
        # one, so the file keeps the question and the store keeps the answer.
        assert to_document(resolved)["endpoints"] == []
        assert to_document(resolved)["discover"]["hostname"] == "api.staging.example.com"
        assert parse_profile(to_document(resolved)).discover == parsed.discover


# ------------------------------------------------------- an inventory as an endpoint list


class TestToEndpoints:
    def test_the_endpoint_id_is_the_resource_id(self) -> None:
        """The join between load metrics and host metrics is that identity."""
        inventory = walked()
        endpoints, _ = to_endpoints(inventory, collect=Collection(transport="scrape"))
        addressable = {r.id for r in inventory.resources if r.role != "container"}
        assert {e.id for e in endpoints} == addressable

    def test_the_balancer_is_addressed_and_not_collected_from(self) -> None:
        endpoints, _ = to_endpoints(walked(), collect=Collection(transport="scrape"))
        balancer = next(e for e in endpoints if e.attributes["role"] == "lb")
        assert balancer.address.endswith(":443")
        assert balancer.tls.enabled, "the listener is HTTPS"
        assert balancer.collect.transport == "none", "an ALB cannot be logged into"

    def test_direct_addressing_leaves_the_balancer_out(self) -> None:
        endpoints, _ = to_endpoints(walked(), addressing="direct")
        assert not [e for e in endpoints if e.attributes["role"] == "lb"]

    def test_a_host_carries_the_detail_that_explains_an_outlier(self) -> None:
        endpoints, _ = to_endpoints(
            walked("alb-ecs-ec2", "orders.example.com"), collect=Collection(transport="ssh")
        )
        host = next(e for e in endpoints if e.id == "i-0aaa1111bbbb2222c")
        assert host.attributes["instance_type"] == "m6i.large"
        assert host.attributes["availability_zone"] == "us-east-1a"
        assert host.attributes["asg"] == "orders-ecs-asg"
        assert host.collect.transport == "ssh"

    def test_the_box_is_the_host_when_there_is_one_and_the_task_when_there_is_not(self) -> None:
        """Host statistics are whole-machine: counting a box and its tasks double-counts."""
        ec2, _ = to_endpoints(walked("alb-ecs-ec2", "orders.example.com"), addressing="direct")
        assert {e.attributes["role"] for e in ec2} == {"instance"}

        fargate, _ = to_endpoints(walked(), addressing="direct")
        assert {e.attributes["role"] for e in fargate} == {"task"}

    def test_a_resource_with_no_port_is_noted_rather_than_invented(self) -> None:
        inventory = walked("alb-ecs-ec2", "orders.example.com")
        stripped = copy.deepcopy(inventory.to_document())
        for resource in stripped["resources"]:
            if resource["id"] == "i-0aaa1111bbbb2222c":
                del resource["port"]

        from metrix_api.discovery import from_document

        endpoints, notes = to_endpoints(from_document(stripped), addressing="direct")
        assert "i-0aaa1111bbbb2222c" not in {e.id for e in endpoints}
        assert any("no port" in n.message for n in notes)


# ------------------------------------------------------------------------- the store


class TestStore:
    def test_a_resolution_that_found_nothing_new_extends_the_row(self, db) -> None:
        """The table is a history of changes, not of checks."""
        first = inventories.save(db, walked(), profile="staging", now=_at("12:00:00"))
        again = inventories.save(db, walked(), profile="staging", now=_at("12:05:00"))

        assert again.id == first.id
        assert again.discovered_at == first.discovered_at
        assert again.confirmed_at != first.confirmed_at
        assert len(inventories.history(db, "staging")) == 1

    def test_a_changed_environment_is_a_new_row(self, db) -> None:
        inventories.save(db, walked(), profile="staging", now=_at("12:00:00"))
        inventories.save(db, _scaled_down(), profile="staging", now=_at("12:05:00"))

        history = inventories.history(db, "staging")
        assert len(history) == 2
        assert history[0].inventory.by_role("task") != history[1].inventory.by_role("task")
        assert inventories.latest(db, profile="staging").id == history[0].id

    def test_a_stored_snapshot_reads_back_as_what_was_written(self, db) -> None:
        stored = inventories.save(db, walked(), profile="staging")
        assert inventories.get(db, stored.id).inventory.to_document() == walked().to_document()

    def test_freshness_is_measured_from_the_last_confirmation(self, db) -> None:
        stored = inventories.save(db, walked(), profile="staging", now=_at("12:00:00"))
        assert stored.fresh(timedelta(minutes=10), now=_at("12:09:00"))
        assert not stored.fresh(timedelta(minutes=10), now=_at("12:11:00"))

    def test_a_one_off_resolution_is_never_handed_back_as_a_cache_hit(self, db) -> None:
        inventories.save(db, walked(), profile=None)
        assert inventories.latest(db, profile=None) is None


def _at(clock: str) -> datetime:
    return datetime.fromisoformat(f"2026-09-12T{clock}+00:00").astimezone(UTC)


def _scaled_down():
    """The same environment with one task gone."""
    recording = account("alb-fargate")
    tasks = recording["ecs"]["describe_tasks"][0]["response"]["tasks"]
    del tasks[1]
    listed = recording["ecs"]["list_tasks"][0]["response"]["taskArns"]
    del listed[1]
    health = recording["elbv2"]["describe_target_health"][0]["response"][
        "TargetHealthDescriptions"
    ]
    del health[1]

    from metrix_api.discovery import discover

    clients, _ = clients_from(recording)
    return discover(clients, hostname="api.staging.example.com")


# ---------------------------------------------------------------------- the resolver


class TestResolver:
    def test_the_first_resolution_walks_and_the_second_does_not(self, db) -> None:
        clients, calls = recorded("alb-fargate")
        resolver = Resolver(conn=db, clients=clients)

        first = resolver.resolve(profile(), now=_at("12:00:00"))
        assert not first.cached
        walked_calls = len(calls)
        assert walked_calls > 0

        second = resolver.resolve(profile(), now=_at("12:05:00"))
        assert second.cached
        assert second.stored.id == first.stored.id
        assert len(calls) == walked_calls, "a cache hit is not a walk"

    def test_an_expired_ttl_walks_again(self, db) -> None:
        clients, calls = recorded("alb-fargate")
        resolver = Resolver(conn=db, clients=clients)
        resolver.resolve(profile(), now=_at("12:00:00"))
        before = len(calls)

        assert not resolver.resolve(profile(), now=_at("12:11:00")).cached
        assert len(calls) > before

    def test_force_walks_whatever_the_cache_says(self, db) -> None:
        clients, calls = recorded("alb-fargate")
        resolver = Resolver(conn=db, clients=clients)
        resolver.resolve(profile(), now=_at("12:00:00"))
        before = len(calls)

        assert not resolver.resolve(profile(), force=True, now=_at("12:00:30")).cached
        assert len(calls) > before

    def test_a_profile_with_its_endpoints_written_down_has_nothing_to_resolve(self, db) -> None:
        explicit = parse_profile(
            {"name": "local", "endpoints": [{"id": "a", "address": "127.0.0.1:8080"}]}
        )
        with pytest.raises(ResolveError, match="written down"):
            Resolver(conn=db).resolve(explicit)

    def test_the_resolution_carries_what_could_not_be_determined(self, db) -> None:
        clients, _ = recorded("alb-fargate")
        resolution = Resolver(conn=db, clients=clients).resolve(profile())
        assert any("draining" in n.message for n in resolution.notes)


class TestCompare:
    def test_a_host_that_went_away_is_the_change_worth_naming(self) -> None:
        change = compare(walked(), _scaled_down())
        assert change.moved
        assert (change.before, change.after) == (2, 1)
        assert change.removed and not change.added
        assert "2 -> 1" in change.describe()

    def test_a_redeployed_task_on_the_same_boxes_is_not_a_change(self) -> None:
        """Comparing documents rather than host sets would flag every deployment."""
        recording = account("alb-ecs-ec2")
        before = clients_from(recording)[0]
        for task in recording["ecs"]["describe_tasks"][0]["response"]["tasks"]:
            task["taskDefinitionArn"] = task["taskDefinitionArn"].replace(":118", ":119")
        after = clients_from(recording)[0]

        from metrix_api.discovery import discover

        assert not compare(
            discover(before, hostname="orders.example.com"),
            discover(after, hostname="orders.example.com"),
        ).moved


# --------------------------------------------------- a recording against a discovery


async def record(db, prof, resolver, transport, *, clock=None, settle=0.05):
    """Start a discovered recording against a fake transport, let it collect, stop it."""
    import metrix_api.recording as recording_module

    original = recording_module.transport_for
    recording_module.transport_for = lambda endpoint, **kw: transport
    try:
        recorder = await start_observation(
            db,
            prof,
            interval=timedelta(seconds=1),
            clock=clock or FakeClock(),
            resolver=resolver,
        )
        await asyncio.sleep(settle)
        return recorder, await recorder.stop()
    finally:
        recording_module.transport_for = original


class TestPinning:
    @pytest.mark.asyncio
    async def test_a_recording_collects_from_what_discovery_found(self, db) -> None:
        clients, _ = recorded("alb-fargate")
        clock = FakeClock()
        _, row = await record(
            db,
            profile(),
            Resolver(conn=db, clients=clients),
            FakeTransport(BLOCKS, then="hang", clock=clock),
            clock=clock,
        )

        # The observer attached to the task identities, not to a hand-written list --
        # and the balancer, which has no collector, is not among them.
        assert row.targets == [
            "task/3f1c5a7e9b2d4c6f8a0b1c2d3e4f5a6b",
            "task/7e2d4c6f8a0b1c2d3e4f5a6b3f1c5a7e",
        ]

    @pytest.mark.asyncio
    async def test_the_snapshot_it_ran_against_is_pinned_to_it(self, db) -> None:
        clients, _ = recorded("alb-fargate")
        clock = FakeClock()
        _, row = await record(
            db,
            profile(),
            Resolver(conn=db, clients=clients),
            FakeTransport(BLOCKS, then="hang", clock=clock),
            clock=clock,
        )

        pinned = inventories.for_recording(db, row.id)
        assert pinned is not None
        assert pinned.inventory.source == "api.staging.example.com"
        # The digests are what later says "different build" rather than "regression".
        assert {c.image_digest for c in pinned.inventory.by_role("container")}

    @pytest.mark.asyncio
    async def test_the_pin_does_not_move_when_the_environment_does(self, db) -> None:
        clients, _ = recorded("alb-fargate")
        clock = FakeClock()
        _, row = await record(
            db,
            profile(),
            Resolver(conn=db, clients=clients),
            FakeTransport(BLOCKS, then="hang", clock=clock),
            clock=clock,
        )
        before = inventories.for_recording(db, row.id)

        # The environment moves on, and is resolved again for the next run.
        inventories.save(db, _scaled_down(), profile="staging")
        after = inventories.for_recording(db, row.id)
        assert after.id == before.id
        assert len(after.inventory.by_role("task")) == 2

    @pytest.mark.asyncio
    async def test_a_profile_that_resolves_to_nothing_will_not_start(self, db) -> None:
        clients, _ = recorded("alb-fargate")
        nowhere = profile(discover={"hostname": "nothing.staging.example.com"})
        with pytest.raises(RecordingError, match="resolved to nothing addressable"):
            await start_observation(db, nowhere, resolver=Resolver(conn=db, clients=clients))

    @pytest.mark.asyncio
    async def test_a_discovered_profile_without_a_resolver_is_a_wiring_mistake(self, db) -> None:
        with pytest.raises(RecordingError, match="wiring mistake"):
            await start_observation(db, profile())


class TestRefreshAtPhaseBoundaries:
    @pytest.mark.asyncio
    async def test_an_environment_that_changed_size_is_annotated(self, db) -> None:
        """Refreshing at the closing boundary is what turns this into a fact."""
        clock = FakeClock()
        resolver = Resolver(conn=db, clients=recorded("alb-fargate")[0])
        recorder = None

        import metrix_api.recording as recording_module

        original = recording_module.transport_for
        recording_module.transport_for = lambda endpoint, **kw: FakeTransport(
            BLOCKS, then="hang", clock=clock
        )
        try:
            recorder = await start_observation(
                db, profile(), interval=timedelta(seconds=1), clock=clock, resolver=resolver
            )
            await asyncio.sleep(0.05)
            # A task goes away while the recording is open.
            resolver.clients = clients_from(_shrunk_account())[0]
            row = await recorder.stop()
        finally:
            recording_module.transport_for = original

        annotations = [a for a in store.annotations(db, row.id) if a["code"] == HOST_COUNT_CHANGED]
        assert len(annotations) == 1
        assert annotations[0]["severity"] == "warn", "an ASG that scaled may be the measurement"
        assert "2 -> 1" in annotations[0]["message"]
        import json as _json

        assert _json.loads(annotations[0]["detail"])["removed"] == [
            "task/7e2d4c6f8a0b1c2d3e4f5a6b3f1c5a7e"
        ]

    @pytest.mark.asyncio
    async def test_an_unchanged_environment_says_nothing(self, db) -> None:
        clients, _ = recorded("alb-fargate")
        clock = FakeClock()
        _, row = await record(
            db,
            profile(),
            Resolver(conn=db, clients=clients),
            FakeTransport(BLOCKS, then="hang", clock=clock),
            clock=clock,
        )
        assert not [a for a in store.annotations(db, row.id) if a["code"] == HOST_COUNT_CHANGED]

    @pytest.mark.asyncio
    async def test_a_refresh_that_cannot_reach_aws_costs_the_check_and_not_the_recording(
        self, db
    ) -> None:
        clock = FakeClock()
        resolver = Resolver(conn=db, clients=recorded("alb-fargate")[0])

        import metrix_api.recording as recording_module

        original = recording_module.transport_for
        recording_module.transport_for = lambda endpoint, **kw: FakeTransport(
            BLOCKS, then="hang", clock=clock
        )
        try:
            recorder = await start_observation(
                db, profile(), interval=timedelta(seconds=1), clock=clock, resolver=resolver
            )
            await asyncio.sleep(0.05)
            resolver.clients = clients_from({})[0]  # every call now fails
            row = await recorder.stop()
        finally:
            recording_module.transport_for = original

        assert row.status == store.FINISHED
        assert store.series(db, row.id, row.targets[0], "cpu.user"), "samples survived"


def _shrunk_account() -> dict:
    recording = account("alb-fargate")
    del recording["ecs"]["describe_tasks"][0]["response"]["tasks"][1]
    del recording["ecs"]["list_tasks"][0]["response"]["taskArns"][1]
    del recording["elbv2"]["describe_target_health"][0]["response"]["TargetHealthDescriptions"][1]
    return recording


# ------------------------------------------------------------------------ the routes


@pytest.fixture
def api(tmp_path, monkeypatch):
    """The app, with every AWS session replaced by the recorded Fargate account."""
    from fastapi.testclient import TestClient

    from metrix_api.config import load_config
    from metrix_api.main import create_app
    from metrix_api.profiles import save_profile

    clients, _ = recorded("alb-fargate")
    monkeypatch.setattr(
        "metrix_api.discovery.ecs.Clients.from_config", classmethod(lambda cls, aws: clients)
    )
    # And no collection over the wire either: the discovered addresses are private
    # ones nothing here can reach, and waiting for them to time out would be the
    # slowest part of the suite for no coverage at all.
    monkeypatch.setattr(
        "metrix_api.recording.transport_for",
        lambda endpoint, **kw: FakeTransport(BLOCKS, then="hang"),
    )
    config = load_config(tmp_path).ensure_layout()
    save_profile(config, profile())
    return TestClient(create_app(config))


class TestRoutes:
    def test_a_profile_that_has_never_resolved_shows_no_endpoints(self, api) -> None:
        body = api.get("/api/profiles/staging").json()
        assert body["discover"]["source"] == "api.staging.example.com"
        assert body["endpoints"] == []
        assert body["inventory"] is None

    def test_resolving_stores_the_answer_and_the_page_then_shows_it(self, api) -> None:
        resolved = api.post("/api/profiles/staging/resolve")
        assert resolved.status_code == 200
        body = resolved.json()
        assert body["cached"] is False
        assert body["inventory"]["reached"] == "task"
        assert [e["id"] for e in body["endpoints"]][0].startswith("lb/")

        # Reading the profile back does not walk again: it reads what was stored.
        reopened = api.get("/api/profiles/staging").json()
        assert reopened["inventory"]["id"] == body["inventory"]["id"]
        assert [e["id"] for e in reopened["endpoints"]] == [
            e["id"] for e in body["endpoints"]
        ]

    def test_resolving_a_written_down_profile_says_there_is_nothing_to_resolve(
        self, api, tmp_path
    ) -> None:
        api.post(
            "/api/profiles",
            json={"name": "local", "endpoints": [{"id": "a", "address": "127.0.0.1:8080"}]},
        )
        refused = api.post("/api/profiles/local/resolve")
        assert refused.status_code == 422
        assert "written down" in refused.json()["detail"]

    def test_a_recording_carries_what_it_ran_against(self, api) -> None:
        started = api.post("/api/recordings", json={"profile": "staging", "interval_s": 1})
        assert started.status_code == 201, started.text
        recording_id = started.json()["recording_id"]
        api.post(f"/api/recordings/{recording_id}/stop")

        detail = api.get(f"/api/recordings/{recording_id}").json()
        assert detail["inventory"]["source"] == "api.staging.example.com"
        assert [r["id"] for r in detail["inventory"]["resources"] if r["role"] == "task"]


# ---------------------------------------- verification, and the engine's target list


class TestVerifyAndTargets:
    def test_a_discovered_profile_is_verified_against_what_it_last_resolved_to(
        self, api, monkeypatch
    ) -> None:
        """Not against a fresh walk: 'discovery is broken' and 'the boxes are
        unreachable' are two failures fixed in two different places."""
        api.post("/api/profiles/staging/resolve")

        seen: list[str] = []

        async def answered(endpoint, timeout):
            seen.append(endpoint.id)
            return reachability.Check(
                endpoint=endpoint.id,
                kind=reachability.COLLECT,
                result=reachability.OK,
                address="scrape",
                detail="ip-10-0-11-21",
            )

        monkeypatch.setattr(reachability, "_collect", answered)
        monkeypatch.setattr(reachability, "_load", answered)

        body = api.post("/api/profiles/staging/verify").json()
        assert body["ok"] is True
        assert body["summary"].endswith("reachable")
        # The balancer and both discovered tasks, each asked both questions.
        assert len(seen) == 6
        assert any(s.startswith("task/") for s in seen)

    def test_an_unreachable_box_is_named_and_the_rest_still_report(
        self, api, monkeypatch
    ) -> None:
        api.post("/api/profiles/staging/resolve")

        async def refuse(endpoint, timeout):
            return reachability.Check(
                endpoint=endpoint.id,
                kind=reachability.LOAD,
                result=(
                    reachability.FAILED if endpoint.id.startswith("task/7e") else reachability.OK
                ),
                address=endpoint.address,
                detail="connection refused",
            )

        monkeypatch.setattr(reachability, "_collect", refuse)
        monkeypatch.setattr(reachability, "_load", refuse)

        body = api.post("/api/profiles/staging/verify").json()
        assert body["ok"] is False
        failed = {c["endpoint"] for c in body["checks"] if c["result"] == "failed"}
        assert failed == {"task/7e2d4c6f8a0b1c2d3e4f5a6b3f1c5a7e"}
        assert body["summary"] == "2 of 6 unreachable"

    def test_the_targets_document_points_at_the_balancer(self, api) -> None:
        """Load-balancer addressing: the run points there and watches the tasks."""
        api.post("/api/profiles/staging/resolve")
        document = api.get("/api/profiles/staging/targets").json()

        assert [t["id"] for t in document["list"]] == ["lb/metrix-staging-alb/443"]
        assert document["list"][0]["tls"] == {"enabled": True}
        assert document["list"][0]["host_header"] == "api.staging.example.com"
        # The inventory detail rides along, so a later comparison can explain an outlier.
        assert document["list"][0]["attributes"]["role"] == "lb"

    def test_direct_addressing_points_at_the_tasks_instead(self, api, tmp_path) -> None:
        from metrix_api.config import load_config
        from metrix_api.profiles import save_profile

        save_profile(
            load_config(tmp_path),
            profile(name="staging-direct", addressing="direct"),
        )
        api.post("/api/profiles/staging-direct/resolve")
        document = api.get("/api/profiles/staging-direct/targets").json()

        assert [t["id"] for t in document["list"]] == [
            "task/3f1c5a7e9b2d4c6f8a0b1c2d3e4f5a6b",
            "task/7e2d4c6f8a0b1c2d3e4f5a6b3f1c5a7e",
        ]
        assert all(t["host_header"] == "api.staging.example.com" for t in document["list"])

    def test_a_subset_can_be_asked_for(self, api) -> None:
        api.post("/api/profiles/staging/resolve")
        one = "task/3f1c5a7e9b2d4c6f8a0b1c2d3e4f5a6b"
        document = api.get(f"/api/profiles/staging/targets?only={one}").json()
        assert [t["id"] for t in document["list"]] == [one]

    def test_a_profile_that_has_not_resolved_has_nowhere_to_send_traffic(self, api) -> None:
        refused = api.get("/api/profiles/staging/targets")
        assert refused.status_code == 422
        assert "no endpoints yet" in refused.json()["detail"]
