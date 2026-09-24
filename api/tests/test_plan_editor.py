"""Editing a mixture: what it implies, what stops it running, and what a save costs.

Two things are being defended here. The first is that **the arithmetic behind a
percentage is computed once, on the server** — a share is not a quantity anybody can
judge, and the number of requests it buys decides whether the run's percentiles mean
anything (§12.1). The second is that **a save cannot move the plan hash**: the bytes
are the plan's identity, so reformatting a file nobody changed would start a fresh
series with no history.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from metrix_api import plans
from metrix_api.config import load_config
from metrix_api.main import create_app
from metrix_api.profiles import parse_profile, save_profile

PROFILE = {
    "name": "staging",
    "endpoints": [{"id": "app-1", "address": "10.0.3.41:8080", "collect": {"transport": "ssh"}}],
}

CALLS = {
    "search": {
        "description": "Full-text search",
        "method": "GET",
        "path": "/api/search",
        "query": {"q": "{{ users.term }}"},
        "assert": [{"status": 200}],
        "extract": {"pid": {"json": "$.items[0].id"}},
    },
    "add": {
        "method": "POST",
        "path": "/api/cart/{{ pid }}",
        "generate": {"generator": "order-xml"},
        "assert": [{"status": 201}],
    },
    "spare": {"method": "GET", "path": "/api/spare"},
}


def mix(**overrides):
    document = {
        "version": 1,
        "name": "shop",
        "calls": ["calls/shop.json"],
        "datasets": {"users": {"file": "data/users.csv", "mode": "round_robin"}},
        "generators": {"order-xml": {"type": "lua", "file": "gen/order.lua", "entry": "go"}},
        "phases": {"baseline": "0s", "settle": "0s"},
        "load": {"mode": "fixed", "model": "open", "rate": 100, "duration": "60s"},
        "chains": [
            {
                "name": "browse",
                "percent": 100,
                "session": "reuse",
                "steps": [{"id": "find", "call": "search"}, {"id": "buy", "call": "add"}],
            }
        ],
    }
    document.update(overrides)
    return document


@pytest.fixture
def home(tmp_path: Path):
    config = load_config(tmp_path).ensure_layout()
    save_profile(config, parse_profile(PROFILE))
    root = config.plans_dir / "shop"
    (root / "calls").mkdir(parents=True)
    (root / "mix.json").write_bytes(plans.document_bytes(mix()))
    (root / "calls" / "shop.json").write_bytes(plans.document_bytes(CALLS))
    return config


@pytest.fixture
def client(home):
    return TestClient(create_app(home))


def planned(home, **overrides):
    return plans.parse_mix(home, "shop", mix(**overrides))


def messages(problems, severity=None):
    return [p.message for p in problems if severity is None or p.severity == severity]


class TestWhatAPercentageBuys:
    def test_a_share_buys_iterations_and_a_chain_multiplies_them_into_requests(
        self, home
    ) -> None:
        # The distinction the editor exists to make: 100% of 100/s over a two-step
        # chain is 100 iterations a second and 200 requests a second, and reporting
        # the first as the load under-counts it by the length of the chain.
        numbers = plans.figures(planned(home))
        chain = numbers["chains"][0]
        assert chain["iterations_per_s"] == 100
        assert chain["requests_per_s"] == 200
        assert chain["requests"] == 12000
        assert numbers["iterations"] == 6000

    def test_a_warmup_is_not_measured_and_does_not_count_towards_the_sample_floor(
        self, home
    ) -> None:
        numbers = plans.figures(
            planned(home, load={"mode": "fixed", "rate": 100, "duration": "60s", "warmup": "50s"})
        )
        assert numbers["measured_s"] == 10
        assert numbers["requests"] == 2000

    def test_shares_are_split_across_chains_rather_than_applied_to_each(self, home) -> None:
        numbers = plans.figures(
            planned(
                home,
                chains=[
                    {"name": "a", "percent": 75, "steps": [{"id": "s", "call": "search"}]},
                    {"name": "b", "percent": 25, "steps": [{"id": "s", "call": "spare"}]},
                ],
            )
        )
        assert [c["iterations_per_s"] for c in numbers["chains"]] == [75, 25]

    def test_a_breakpoint_run_reports_no_request_count_at_all(self, home) -> None:
        # The rate is the thing the run is searching for. An estimate here would be
        # an invented sample count, which is the one number this tool must not print.
        numbers = plans.figures(
            planned(
                home,
                load={
                    "mode": "breakpoint",
                    "duration": "60s",
                    "breakpoint": {"start_rate": 10, "step_duration": "30s", "max_rate": 500},
                },
            )
        )
        assert numbers["rate"] is None
        assert numbers["requests"] is None
        assert "searches for its rate" in numbers["withheld"]

    def test_a_staged_ramp_is_counted_from_its_stages(self, home) -> None:
        numbers = plans.figures(
            planned(
                home,
                load={
                    "mode": "stages",
                    "duration": "60s",
                    "stages": [
                        {"rate": 50, "duration": "10s"},
                        {"rate": 150, "duration": "10s"},
                    ],
                },
            )
        )
        # 500 + 1500 iterations over 20s: an average rate of 100, not the last stage's.
        assert numbers["rate"] == 100
        assert numbers["iterations"] == 2000


class TestWhatStopsARun:
    def test_a_shortfall_is_named_rather_than_renormalized(self, home) -> None:
        plan = planned(
            home,
            chains=[
                {"name": "a", "percent": 60, "steps": [{"id": "s", "call": "search"}]},
                {"name": "b", "percent": 35, "steps": [{"id": "s", "call": "spare"}]},
            ],
        )
        problems = plans.check(plan)
        assert not plans.ready(problems)
        assert "5 short of 100" in " ".join(messages(problems, "error"))
        # Nothing was adjusted to make it fit: adjusting one chain to accommodate a
        # typo in another measures a mixture nobody chose.
        assert [c["percent"] for c in plan.chains] == [60, 35]

    def test_an_excess_is_named_as_an_excess(self, home) -> None:
        problems = plans.check(
            planned(
                home,
                chains=[
                    {"name": "a", "percent": 60, "steps": [{"id": "s", "call": "search"}]},
                    {"name": "b", "percent": 50, "steps": [{"id": "s", "call": "spare"}]},
                ],
            )
        )
        assert "10 over 100" in " ".join(messages(problems, "error"))

    def test_a_share_that_does_not_divide_evenly_is_tolerated(self, home) -> None:
        chains = [
            {"name": f"c{i}", "percent": 16.666, "steps": [{"id": "s", "call": "spare"}]}
            for i in range(6)
        ]
        # 99.996: inside the epsilon, which exists so that a share nobody can write
        # exactly is not a validation failure. The tolerance is the engine's own
        # PERCENT_EPSILON, so what passes here passes there.
        assert plans.ready(plans.check(planned(home, chains=chains)))

    def test_the_tolerance_is_the_engines_and_is_not_widened_here(self, home) -> None:
        # 16.67 x 6 is 100.02, which is outside it. Pinned rather than papered over:
        # the constant is shared with the engine, and a mixture the API accepted and
        # the engine rejected would be the worst of both.
        chains = [
            {"name": f"c{i}", "percent": 16.67, "steps": [{"id": "s", "call": "spare"}]}
            for i in range(6)
        ]
        assert not plans.ready(plans.check(planned(home, chains=chains)))

    def test_two_chains_with_one_name_cannot_be_told_apart_later(self, home) -> None:
        problems = plans.check(
            planned(
                home,
                chains=[
                    {"name": "a", "percent": 50, "steps": [{"id": "s", "call": "search"}]},
                    {"name": "a", "percent": 50, "steps": [{"id": "s", "call": "spare"}]},
                ],
            )
        )
        assert any("two chains are named" in m for m in messages(problems, "error"))

    def test_a_step_id_used_twice_makes_one_latency_unreportable(self, home) -> None:
        problems = plans.check(
            planned(
                home,
                chains=[
                    {
                        "name": "a",
                        "percent": 100,
                        "steps": [{"id": "s", "call": "search"}, {"id": "s", "call": "spare"}],
                    }
                ],
            )
        )
        assert any("twice" in m for m in messages(problems, "error"))

    def test_a_pooled_chain_without_a_pool_size_does_not_run(self, home) -> None:
        problems = plans.check(
            planned(
                home,
                chains=[
                    {
                        "name": "a",
                        "percent": 100,
                        "session": "pool",
                        "steps": [{"id": "s", "call": "spare"}],
                    }
                ],
            )
        )
        assert not plans.ready(problems)

    def test_a_pool_size_on_a_chain_that_does_not_pool_is_a_warning_not_a_failure(
        self, home
    ) -> None:
        problems = plans.check(
            planned(
                home,
                chains=[
                    {
                        "name": "a",
                        "percent": 100,
                        "session": "reuse",
                        "pool_size": 50,
                        "steps": [{"id": "s", "call": "spare"}],
                    }
                ],
            )
        )
        assert plans.ready(problems)
        assert any("is ignored" in m for m in messages(problems, "warning"))

    def test_a_warmup_as_long_as_the_run_measures_nothing(self, home) -> None:
        problems = plans.check(
            planned(home, load={"mode": "fixed", "rate": 100, "duration": "30s", "warmup": "30s"})
        )
        assert any("nothing would be measured" in m for m in messages(problems, "error"))

    def test_a_fixed_run_without_a_rate_is_an_error_rather_than_a_zero(self, home) -> None:
        problems = plans.check(planned(home, load={"mode": "fixed", "duration": "30s"}))
        assert "load/rate" in [p.where for p in problems if p.severity == "error"]


class TestSampleCounts:
    def test_a_short_run_is_warned_about_before_it_is_made_not_after(self, home) -> None:
        problems = plans.check(
            planned(home, load={"mode": "fixed", "rate": 5, "duration": "30s"})
        )
        warning = " ".join(messages(problems, "warning"))
        assert "2250" in warning
        # A warning, not an error: a short smoke test is a legitimate thing to run.
        # What it cannot do is carry a tail percentile.
        assert plans.ready(problems)

    def test_a_thin_chain_inside_a_long_enough_run_is_named_on_its_own(self, home) -> None:
        problems = plans.check(
            planned(
                home,
                chains=[
                    {"name": "bulk", "percent": 98, "steps": [{"id": "s", "call": "spare"}]},
                    {"name": "rare", "percent": 2, "steps": [{"id": "s", "call": "search"}]},
                ],
            )
        )
        warning = " ".join(messages(problems, "warning"))
        assert "rare (120)" in warning
        assert "bulk" not in warning

    def test_a_run_below_the_floor_says_so_once_rather_than_once_per_chain(self, home) -> None:
        problems = plans.check(
            planned(
                home,
                load={"mode": "fixed", "rate": 5, "duration": "30s"},
                chains=[
                    {"name": "a", "percent": 50, "steps": [{"id": "s", "call": "search"}]},
                    {"name": "b", "percent": 50, "steps": [{"id": "s", "call": "spare"}]},
                ],
            )
        )
        assert len([m for m in messages(problems, "warning") if "2250" in m]) == 1


class TestVariablesBetweenSteps:
    def test_a_step_reading_what_an_earlier_step_extracted_is_not_flagged(self, home) -> None:
        # `add` reads {{ pid }}, which `search` extracts. In this order, it resolves.
        assert not [m for m in messages(plans.check(planned(home))) if "pid" in m]

    def test_reordering_two_steps_breaks_the_variable_between_them(self, home) -> None:
        problems = plans.check(
            planned(
                home,
                chains=[
                    {
                        "name": "browse",
                        "percent": 100,
                        "steps": [{"id": "buy", "call": "add"}, {"id": "find", "call": "search"}],
                    }
                ],
            )
        )
        assert any("pid" in m for m in messages(problems, "warning"))
        # A warning: this reads templates rather than evaluating them, and a guess
        # dressed as a verdict would stop a run that was fine.
        assert plans.ready(problems)

    def test_a_dataset_column_is_provided_by_the_dataset_not_by_a_step(self, home) -> None:
        assert not [m for m in messages(plans.check(planned(home))) if "users.term" in m]

    def test_a_call_no_chain_invokes_is_worth_saying_out_loud(self, home) -> None:
        assert any("'spare'" in m for m in messages(plans.check(planned(home)), "warning"))


class TestSaving:
    def test_new_plan_can_start_from_a_typed_endpoint_without_a_schema(self, client) -> None:
        document = {
            "version": 1, "name": "typed", "calls": ["calls/generated.json"],
            "load": {"mode": "fixed", "rate": 75, "duration": "60s"},
            "chains": [{"name": "display", "percent": 100,
                        "steps": [{"id": "display", "call": "get-display"}]}],
        }
        response = client.post("/api/plans", json={
            "name": "typed", "mix": document,
            "basic_calls": {"get-display": {"method": "GET", "path": "/display/{{uuid()}}"}},
        })
        assert response.status_code == 201
        assert response.json()["ready"] is True
        details = client.get("/api/plans/typed").json()["call_details"]
        assert details[0]["path"] == "/display/{{uuid()}}"

    def test_basic_can_validate_and_save_a_templated_endpoint(self, home, client) -> None:
        edited = mix(
            calls=["calls/shop.json", "calls/basic.json"],
            chains=[{"name": "display", "percent": 100,
                     "steps": [{"id": "display", "call": "get-display-id"}]}],
        )
        body = {"mix": edited, "calls": {"get-display-id": {
            "method": "GET", "path": "/display/{{id}}"}}}
        before = (home.plans_dir / "shop" / "mix.json").read_bytes()
        checked = client.post("/api/plans/shop/validate-basic", json=body)
        assert checked.status_code == 200
        assert (home.plans_dir / "shop" / "mix.json").read_bytes() == before
        saved = client.put("/api/plans/shop/basic", json=body)
        assert saved.status_code == 200
        assert next(c for c in saved.json()["call_details"] if c["name"] == "get-display-id")[
            "path"
        ] == "/display/{{id}}"
        written = plans.load_plan(home, "shop").calls["calls/basic.json"]["get-display-id"]
        assert written == body["calls"]["get-display-id"]

    def test_basic_rejects_a_non_path_without_writing(self, home, client) -> None:
        before = (home.plans_dir / "shop" / "mix.json").read_bytes()
        response = client.put("/api/plans/shop/basic", json={"mix": mix(), "calls": {
            "bad": {"method": "GET", "path": "https://example.com/display"}}})
        assert response.status_code == 422
        assert (home.plans_dir / "shop" / "mix.json").read_bytes() == before

    def test_a_save_that_changes_nothing_leaves_the_bytes_and_the_hash_alone(
        self, home, client
    ) -> None:
        before = (home.plans_dir / "shop" / "mix.json").read_bytes()
        hash_before = plans.assemble(
            plans.load_plan(home, "shop"), parse_profile(PROFILE)
        ).hash

        # Submitted with its keys in a different order, as a form would build it.
        shuffled = dict(reversed(list(mix().items())))
        assert client.put("/api/plans/shop", json=shuffled).status_code == 200

        assert (home.plans_dir / "shop" / "mix.json").read_bytes() == before
        assert (
            plans.assemble(plans.load_plan(home, "shop"), parse_profile(PROFILE)).hash
            == hash_before
        )

    def test_a_rename_is_refused_because_it_would_split_the_history(self, home, client) -> None:
        response = client.put("/api/plans/shop", json=mix(name="shop-v2"))
        assert response.status_code == 422
        assert "fixed" in response.json()["detail"]
        assert json.loads((home.plans_dir / "shop" / "mix.json").read_text())["name"] == "shop"

    def test_a_half_finished_mixture_still_saves(self, home, client) -> None:
        # An editor that refuses to save until the percentages total 100 is an editor
        # that loses an afternoon's work. What errors stop is running, not saving.
        broken = mix(chains=[{"name": "a", "percent": 40, "steps": [{"id": "s", "call": "spare"}]}])
        response = client.put("/api/plans/shop", json=broken)
        assert response.status_code == 200
        assert response.json()["ready"] is False
        assert json.loads((home.plans_dir / "shop" / "mix.json").read_text())["chains"][0][
            "percent"
        ] == 40

    def test_a_step_naming_a_call_that_does_not_exist_is_not_written(self, home, client) -> None:
        before = (home.plans_dir / "shop" / "mix.json").read_bytes()
        response = client.put(
            "/api/plans/shop",
            json=mix(
                chains=[{"name": "a", "percent": 100, "steps": [{"id": "s", "call": "nope"}]}]
            ),
        )
        assert response.status_code == 422
        assert "not defined in any calls file" in response.json()["detail"]
        assert (home.plans_dir / "shop" / "mix.json").read_bytes() == before

    def test_validating_answers_without_writing_anything(self, home, client) -> None:
        before = (home.plans_dir / "shop" / "mix.json").read_bytes()
        response = client.post(
            "/api/plans/shop/validate",
            json=mix(
                chains=[{"name": "a", "percent": 40, "steps": [{"id": "s", "call": "spare"}]}]
            ),
        )
        assert response.status_code == 200
        assert response.json()["ready"] is False
        assert (home.plans_dir / "shop" / "mix.json").read_bytes() == before

    def test_the_document_is_returned_whole_so_a_round_trip_loses_nothing(
        self, home, client
    ) -> None:
        # The summary drops everything the form does not draw. Editing from it would
        # delete a dataset on the first save; this is what the editor loads instead.
        document = client.get("/api/plans/shop/document").json()
        assert document["datasets"]["users"]["file"] == "data/users.csv"
        assert client.put("/api/plans/shop", json=document).status_code == 200
        assert client.get("/api/plans/shop/document").json() == document


class TestTheGateBetweenEditingAndRunning:
    def test_a_plan_with_errors_does_not_become_a_bundle(self, home, client) -> None:
        client.put(
            "/api/plans/shop",
            json=mix(
                chains=[{"name": "a", "percent": 40, "steps": [{"id": "s", "call": "spare"}]}]
            ),
        )
        response = client.get("/api/plans/shop/bundle", params={"profile": "staging"})
        assert response.status_code == 422
        # The reasons, not a count: the point of refusing is to get them fixed.
        assert "60 short of 100" in response.json()["detail"]

    def test_warnings_do_not_stop_a_bundle(self, home, client) -> None:
        response = client.get("/api/plans/shop/bundle", params={"profile": "staging"})
        assert response.status_code == 200
        assert response.headers["X-Metrix-Plan-Hash"].startswith("sha256:")

    def test_the_list_says_which_plans_can_run_without_opening_each_one(
        self, home, client
    ) -> None:
        body = client.get("/api/plans").json()["plans"][0]
        assert body["ready"] is True
        assert body["figures"]["chains"][0]["requests_per_s"] == 200
        assert [p["severity"] for p in body["problems"]] == ["warning"]


class TestReadingTheCalls:
    def test_a_call_says_what_it_reads_and_what_it_provides(self, home, client) -> None:
        details = {c["name"]: c for c in client.get("/api/plans/shop").json()["call_details"]}
        assert details["search"]["uses"] == ["users.term"]
        assert details["search"]["extracts"] == ["pid"]
        assert details["search"]["description"] == "Full-text search"

    def test_a_variable_in_a_path_counts_as_much_as_one_in_a_body(self, home, client) -> None:
        details = {c["name"]: c for c in client.get("/api/plans/shop").json()["call_details"]}
        assert details["add"]["uses"] == ["pid"]
        assert details["add"]["body"] == "generator order-xml"

    def test_assertions_travel_as_written_so_a_401_can_explain_itself(
        self, home, client
    ) -> None:
        details = {c["name"]: c for c in client.get("/api/plans/shop").json()["call_details"]}
        assert details["add"]["assert"] == [{"status": 201}]
