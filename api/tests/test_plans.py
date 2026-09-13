"""Bundle assembly: what crosses the boundary, and why its bytes cannot drift.

The engine identifies a plan by a SHA-256 over the exact bytes it read, and that
hash is part of a run's series identity. So the thing being defended here is not
"the export works" but "the export is the same every time" — a bundle whose bytes
move re-identifies the plan, and every run starts a fresh series with no history.

The cross-language agreement between this hash and the engine's is checked by
`scripts/check-bundle-contract.py` in the full suite, against the real binary.
"""

from __future__ import annotations

import hashlib
import io
import json
import zipfile
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from metrix_api import plans
from metrix_api.config import load_config
from metrix_api.main import create_app
from metrix_api.profiles import parse_profile, save_profile

PROFILE = {
    "name": "staging",
    "endpoints": [
        {"id": "app-1", "address": "10.0.3.41:8080", "collect": {"transport": "ssh"}},
        {"id": "app-2", "address": "10.0.3.42:8080", "collect": {"transport": "ssh"}},
        {"id": "watch-only", "address": "10.0.3.9:80", "load": False,
         "collect": {"transport": "ssh"}},
    ],
}

MIX = {
    "version": 1,
    "name": "checkout",
    "calls": ["calls/ping.json"],
    "phases": {"baseline": "0s", "settle": "0s"},
    "load": {"mode": "fixed", "model": "open", "rate": 75, "duration": "30s"},
    "chains": [
        {"name": "ping", "percent": 100, "session": "fresh",
         "steps": [{"id": "get", "call": "ping"}]}
    ],
}

CALLS = {"ping": {"method": "GET", "path": "/ping"}}


@pytest.fixture
def home(tmp_path: Path):
    config = load_config(tmp_path).ensure_layout()
    save_profile(config, parse_profile(PROFILE))
    return config


@pytest.fixture
def client(home):
    return TestClient(create_app(home))


def write_plan(config, name="checkout", *, mix=None, calls=None, extra=None):
    root = config.plans_dir / name
    (root / "calls").mkdir(parents=True, exist_ok=True)
    (root / "mix.json").write_text(json.dumps(mix or MIX), encoding="utf-8")
    (root / "calls" / "ping.json").write_text(json.dumps(calls or CALLS), encoding="utf-8")
    for relative, content in (extra or {}).items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
    return root


def bundle_of(config, name="checkout", **kw):
    from metrix_api.profiles import load_profile

    return plans.assemble(plans.load_plan(config, name), load_profile(config, "staging"), **kw)


# --------------------------------------------------------------------- assembly


class TestAssembly:
    def test_the_bundle_is_the_directory_the_engine_takes(self, home) -> None:
        write_plan(home)
        bundle = bundle_of(home)
        assert set(bundle.files) == {"mix.json", "targets.json", "calls/ping.json"}

    def test_targets_come_from_the_profile_not_from_the_plan(self, home) -> None:
        """The only thing the API adds. The engine knows nothing about profiles."""
        write_plan(home)
        targets = json.loads(bundle_of(home).files["targets.json"])
        assert [t["id"] for t in targets["list"]] == ["app-1", "app-2"]
        assert targets["list"][0]["address"] == "10.0.3.41:8080"

    def test_an_endpoint_that_takes_no_traffic_is_not_a_target(self, home) -> None:
        write_plan(home)
        targets = json.loads(bundle_of(home).files["targets.json"])
        assert "watch-only" not in [t["id"] for t in targets["list"]]

    def test_one_box_of_an_environment_can_be_singled_out(self, home) -> None:
        write_plan(home)
        targets = json.loads(bundle_of(home, only=["app-2"]).files["targets.json"])
        assert [t["id"] for t in targets["list"]] == ["app-2"]

    def test_one_plan_against_two_profiles_differs_in_exactly_one_file(self, home) -> None:
        write_plan(home)
        save_profile(home, parse_profile({**PROFILE, "name": "prod"}))
        from metrix_api.profiles import load_profile

        plan = plans.load_plan(home, "checkout")
        staging = plans.assemble(plan, load_profile(home, "staging"))
        prod = plans.assemble(plan, load_profile(home, "prod"))

        differ = {p for p in staging.files if staging.files[p] != prod.files.get(p)}
        assert differ == set(), "these two profiles describe the same boxes"
        assert staging.hash == prod.hash

    def test_generators_and_datasets_are_carried_verbatim(self, home) -> None:
        write_plan(home, extra={"gen/order.lua": b"-- lua\n", "data/users.csv": b"id\n1\n"})
        bundle = bundle_of(home)
        assert bundle.files["gen/order.lua"] == b"-- lua\n"
        assert bundle.files["data/users.csv"] == b"id\n1\n"

    def test_supporting_files_are_outside_the_hash_as_they_are_for_the_engine(
        self, home
    ) -> None:
        """The engine digests the documents it parses. Hashing more here would make
        two implementations of one identity that disagree."""
        write_plan(home)
        without = bundle_of(home).hash
        write_plan(home, extra={"data/users.csv": b"id\n1\n"})
        assert bundle_of(home).hash == without
        assert "data/users.csv" not in bundle_of(home).hashed


# ------------------------------------------------------------------- the bytes


class TestDeterminism:
    def test_assembling_twice_produces_identical_bytes(self, home) -> None:
        write_plan(home)
        assert bundle_of(home).files == bundle_of(home).files

    def test_key_order_in_the_stored_file_does_not_change_the_hash(self, home) -> None:
        """A plan written by a different tool, or edited by hand, is the same plan.

        Without this the hash follows whatever order the JSON happened to be in, and
        a reformat in an editor would split a series in two.
        """
        write_plan(home)
        first = bundle_of(home).hash

        reversed_mix = dict(reversed(list(MIX.items())))
        write_plan(home, mix=reversed_mix)
        assert bundle_of(home).hash == first

    def test_a_real_change_does_move_the_hash(self, home) -> None:
        write_plan(home)
        before = bundle_of(home).hash
        write_plan(home, mix={**MIX, "load": {**MIX["load"], "rate": 150}})
        assert bundle_of(home).hash != before

    def test_the_hash_is_length_framed_so_paths_cannot_be_confused(self) -> None:
        """`a` + `bc` and `ab` + `c` must not digest the same."""
        one = plans.Bundle(files={"a": b"bc", "ab": b""}, hashed=("a", "ab"))
        two = plans.Bundle(files={"a": b"", "ab": b"bc"}, hashed=("a", "ab"))
        assert one.hash != two.hash

    def test_the_hash_matches_a_digest_computed_by_hand(self, home) -> None:
        """The engine's rule, written out independently: for each path in sorted
        order, its length as eight little-endian bytes, the path, the content length
        the same way, then the content."""
        write_plan(home)
        bundle = bundle_of(home)

        expected = hashlib.sha256()
        for path in sorted(bundle.hashed):
            content = bundle.files[path]
            expected.update(len(path).to_bytes(8, "little"))
            expected.update(path.encode("utf-8"))
            expected.update(len(content).to_bytes(8, "little"))
            expected.update(content)
        assert bundle.hash == f"sha256:{expected.hexdigest()}"

    def test_the_zip_does_not_carry_the_clock(self, home) -> None:
        """Two exports of an unchanged plan have to be byte-identical, or the thing
        you diff before re-importing is always different."""
        write_plan(home)
        assert bundle_of(home).archive() == bundle_of(home).archive()

    def test_the_zip_unpacks_to_the_bundle(self, home) -> None:
        write_plan(home, extra={"gen/order.lua": b"-- lua\n"})
        bundle = bundle_of(home)
        with zipfile.ZipFile(io.BytesIO(bundle.archive())) as archive:
            assert sorted(archive.namelist()) == sorted(bundle.files)
            assert archive.read("mix.json") == bundle.files["mix.json"]

    def test_writing_it_out_produces_a_directory_the_engine_would_read(
        self, home, tmp_path
    ) -> None:
        write_plan(home, extra={"gen/order.lua": b"-- lua\n"})
        out = bundle_of(home).write(tmp_path / "exported")
        assert (out / "mix.json").is_file()
        assert (out / "calls" / "ping.json").is_file()
        assert (out / "gen" / "order.lua").read_bytes() == b"-- lua\n"


# -------------------------------------------------------------------- refusals


class TestLoading:
    def test_a_mix_the_engine_would_reject_is_refused_here(self, home) -> None:
        """Validated against the schema generated from the engine's own types, so a
        bundle this produces cannot be one the engine fails to parse."""
        write_plan(home, mix={**MIX, "load": {"mode": "nonsense", "model": "open"}})
        with pytest.raises(plans.PlanError, match="mix.json"):
            plans.load_plan(home, "checkout")

    def test_a_step_naming_a_call_that_does_not_exist_is_refused(self, home) -> None:
        broken = {**MIX, "chains": [{**MIX["chains"][0],
                                     "steps": [{"id": "get", "call": "absent"}]}]}
        write_plan(home, mix=broken)
        with pytest.raises(plans.PlanError, match="not defined in any calls file"):
            plans.load_plan(home, "checkout")

    def test_a_call_reference_outside_the_plan_is_refused(self, home) -> None:
        """A plan is a unit of transfer; one that reads a file outside itself is not."""
        for escape in ("../secrets/token.json", "/etc/passwd"):
            write_plan(home, mix={**MIX, "calls": [escape]})
            with pytest.raises(plans.PlanError, match="plan directory|inside the plan"):
                plans.load_plan(home, "checkout")

    def test_invalid_json_reports_where_and_never_what(self, home) -> None:
        """A call document holds headers and query values; a parse error that quoted
        the offending text could put a token in a log."""
        write_plan(home)
        (home.plans_dir / "checkout" / "calls" / "ping.json").write_text(
            '{"ping": {"method": "GET", "path": "/p", "headers": {"authorization": "Bearer s3cret"',
            encoding="utf-8",
        )
        with pytest.raises(plans.PlanError) as caught:
            plans.load_plan(home, "checkout")
        assert "line 1" in str(caught.value)
        assert "s3cret" not in str(caught.value)

    def test_a_stored_targets_file_is_noted_rather_than_obeyed_or_refused(
        self, home
    ) -> None:
        """An exported bundle carries one, so re-importing has to work — but it is
        never used, and a file that is silently ignored gets edited by somebody who
        thinks it matters."""
        root = write_plan(home)
        (root / "targets.json").write_text(json.dumps({"list": [{"id": "x", "address": "h:1"}]}))

        plan = plans.load_plan(home, "checkout")
        assert any("ignored" in note for note in plan.notes)
        targets = json.loads(bundle_of(home).files["targets.json"])
        assert [t["id"] for t in targets["list"]] == ["app-1", "app-2"]

    def test_a_broken_plan_stays_in_the_list_with_its_reason(self, home) -> None:
        write_plan(home, name="good")
        write_plan(home, name="bad", mix={"version": 99})
        found, broken = plans.list_plans(home)
        assert [p.name for p in found] == ["good"]
        assert broken[0]["name"] == "bad" and broken[0]["error"]


# ---------------------------------------------------------------------- routes


class TestRoutes:
    def test_the_zip_carries_the_hash_of_what_is_inside(self, home, client) -> None:
        write_plan(home)
        response = client.get("/api/plans/checkout/bundle", params={"profile": "staging"})

        assert response.status_code == 200
        assert response.headers["content-type"] == "application/zip"
        assert "checkout-staging.zip" in response.headers["content-disposition"]
        assert response.headers["x-metrix-plan-hash"] == bundle_of(home).hash

        with zipfile.ZipFile(io.BytesIO(response.content)) as archive:
            assert "mix.json" in archive.namelist()

    def test_the_json_form_shows_what_the_zip_would_contain(self, home, client) -> None:
        write_plan(home)
        body = client.get(
            "/api/plans/checkout/bundle", params={"profile": "staging", "format": "json"}
        ).json()

        assert body["plan_hash"] == bundle_of(home).hash
        assert set(body["files"]) == {"mix.json", "targets.json", "calls/ping.json"}
        assert body["hashed"] == ["calls/ping.json", "mix.json", "targets.json"]
        assert json.loads(body["files"]["targets.json"])["list"]

    def test_a_bundle_needs_a_profile_to_be_a_bundle(self, home, client) -> None:
        write_plan(home)
        assert client.get("/api/plans/checkout/bundle").status_code == 422

    def test_an_unknown_plan_or_profile_is_a_404(self, home, client) -> None:
        write_plan(home)
        assert client.get(
            "/api/plans/nope/bundle", params={"profile": "staging"}
        ).status_code == 404
        assert client.get(
            "/api/plans/checkout/bundle", params={"profile": "nope"}
        ).status_code == 404

    def test_a_plan_that_exists_but_does_not_validate_is_not_a_404(self, home, client) -> None:
        """"Typo in the name" and "fix your mix" are different problems."""
        write_plan(home, mix={"version": 99})
        assert client.get("/api/plans/checkout").status_code == 422

    def test_a_profile_with_nowhere_to_send_traffic_says_so(self, home, client) -> None:
        write_plan(home)
        save_profile(
            home,
            parse_profile(
                {
                    "name": "watched",
                    "endpoints": [
                        {"id": "a", "address": "10.0.0.1:80", "load": False,
                         "collect": {"transport": "ssh"}}
                    ],
                }
            ),
        )
        response = client.get("/api/plans/checkout/bundle", params={"profile": "watched"})
        assert response.status_code == 422
        assert "observation-only" in response.json()["detail"]

    def test_the_list_summarises_without_making_anyone_open_the_files(
        self, home, client
    ) -> None:
        write_plan(home)
        body = client.get("/api/plans").json()
        plan = body["plans"][0]
        assert plan["name"] == "checkout"
        assert plan["calls"] == ["ping"]
        assert plan["chains"] == [{"name": "ping", "percent": 100.0, "steps": 1}]
        assert plan["load"]["rate"] == 75
