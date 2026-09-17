"""Target profiles, and the point where one becomes an engine targets document."""

from __future__ import annotations

import json
import subprocess
from datetime import timedelta
from pathlib import Path

import pytest
from jsonschema import Draft202012Validator

from metrix_api.config import load_config
from metrix_api.profiles import (
    ProfileError,
    list_profiles,
    load_profile,
    parse_profile,
    save_profile,
    to_document,
    to_targets,
)

REPO = Path(__file__).resolve().parents[2]
EXAMPLES = REPO / "examples" / "profiles"
EXAMPLE = EXAMPLES / "local.json"
TARGETS_SCHEMA = REPO / "schema" / "targets.schema.json"

def shipped_examples() -> list[Path]:
    """The example profiles the repo ships -- the tracked ones, not every file here.

    `examples/profiles/` is a convenient place to keep a real profile while working,
    and an ignored file of one's own must not fail the suite: this asserts something
    about what Metrix publishes, not about what is on this disk.
    """
    listed = subprocess.run(
        ["git", "ls-files", "--", str(EXAMPLES)],
        capture_output=True,
        text=True,
        cwd=REPO,
        check=False,
    )
    if listed.returncode != 0:  # not a checkout: judge every file rather than none
        return sorted(EXAMPLES.glob("*.json"))
    return sorted(REPO / line for line in listed.stdout.split() if line.endswith(".json"))


STAGING = {
    "name": "staging",
    "description": "two tasks behind the load balancer",
    "addressing": "load_balancer",
    "order": "shuffle",
    "gap": "30s",
    "observe": {"interval": "1s", "collect": ["cpu", "memory"]},
    "endpoints": [
        {
            "id": "task-a1b2c3",
            "address": "10.0.3.41:8080",
            "host_header": "api.staging.example.com",
            "attributes": {"availability_zone": "us-east-1a", "image_digest": "sha256:9f2c1e"},
            "collect": {"transport": "ssh", "user": "ec2-user"},
        },
        {
            "id": "task-d4e5f6",
            "address": "10.0.3.77:8080",
            "host_header": "api.staging.example.com",
            "tls": {"enabled": True},
            "collect": {"transport": "none"},
        },
    ],
}


def profile(**overrides):
    return parse_profile({**STAGING, **overrides})


class TestParsing:
    def test_a_profile_round_trips_through_disk(self, tmp_path: Path) -> None:
        config = load_config(tmp_path).ensure_layout()
        original = profile()
        save_profile(config, original)
        assert load_profile(config, "staging") == original

    def test_the_document_form_is_stable(self) -> None:
        once = to_document(profile())
        assert parse_profile(once) == profile()
        assert to_document(parse_profile(once)) == once

    def test_an_observation_username_round_trips(self) -> None:
        parsed = profile(
            observe={"interval": "1s", "ssh_user": " ubuntu ", "collect": ["cpu"]}
        )
        assert parsed.ssh_user == "ubuntu"
        assert parse_profile(to_document(parsed)) == parsed

    def test_a_resolved_host_is_kept_even_when_collection_is_disabled(self) -> None:
        parsed = profile(
            endpoints=[
                {
                    "id": "alb",
                    "addressing": "alb",
                    "address": "api.example.com:443",
                    "collect": {"transport": "none", "host": "10.0.11.21"},
                }
            ]
        )
        document = to_document(parsed)
        assert document["endpoints"][0]["collect"] == {
            "transport": "none",
            "host": "10.0.11.21",
        }
        assert parse_profile(document) == parsed

    def test_every_shipped_example_parses(self) -> None:
        """These are what people copy. A broken example is worse than none, and
        nothing else in the suite reads this directory."""
        found = shipped_examples()
        assert found, "the examples people copy from have gone missing"
        for path in found:
            parsed = parse_profile(
                json.loads(path.read_text(encoding="utf-8")), name=path.stem
            )
            assert parsed.name == path.stem
            # One or the other: an example either lists its machines or says where
            # to find them. Neither is a profile that points at nothing.
            assert parsed.endpoints or parsed.discover, (
                f"{path.name}: an example with no endpoints and no discovery"
            )

    def test_the_local_example_scrapes(self) -> None:
        parsed = parse_profile(json.loads(EXAMPLE.read_text(encoding="utf-8")), name="local")
        assert parsed.name == "local"
        assert parsed.endpoints[0].collect.transport == "scrape"

    def test_listing_and_missing_profiles(self, tmp_path: Path) -> None:
        config = load_config(tmp_path).ensure_layout()
        assert list_profiles(config) == []
        save_profile(config, profile())
        assert list_profiles(config) == ["staging"]
        with pytest.raises(ProfileError, match="no profile named 'prod'"):
            load_profile(config, "prod")


class TestValidation:
    def test_a_typo_is_named(self) -> None:
        with pytest.raises(ProfileError, match="endpoitns"):
            parse_profile({**STAGING, "endpoitns": []})

    def test_the_name_must_match_the_filename(self, tmp_path: Path) -> None:
        config = load_config(tmp_path).ensure_layout()
        (config.profiles_dir / "prod.json").write_text(json.dumps(STAGING), encoding="utf-8")
        with pytest.raises(ProfileError, match="does not match the filename"):
            load_profile(config, "prod")

    def test_duplicate_endpoint_ids_are_refused(self) -> None:
        doubled = [STAGING["endpoints"][0], dict(STAGING["endpoints"][0])]
        with pytest.raises(ProfileError, match="duplicate endpoint id"):
            profile(endpoints=doubled)

    def test_an_address_without_a_port_is_refused(self) -> None:
        with pytest.raises(ProfileError, match="must be host:port"):
            profile(endpoints=[{"id": "a", "address": "10.0.3.41"}])

    def test_no_endpoints_is_refused(self) -> None:
        with pytest.raises(ProfileError, match="non-empty"):
            profile(endpoints=[])

    def test_direct_addressing_requires_a_host_header(self) -> None:
        """Without it the request reaches a default backend and the test measures nothing."""
        with pytest.raises(ProfileError, match="host_header is required"):
            profile(
                addressing="direct",
                endpoints=[{"id": "task-a", "address": "10.0.3.41:8080"}],
            )

    def test_direct_addressing_with_a_host_header_is_fine(self) -> None:
        parsed = profile(
            addressing="direct",
            endpoints=[
                {"id": "task-a", "address": "10.0.3.41:8080", "host_header": "api.example.com"}
            ],
        )
        assert parsed.addressing == "direct"
        assert parsed.endpoints[0].addressing == "ip"

    @pytest.mark.parametrize("addressing", ["ip", "alb", "elb", "ecs", "fargate"])
    def test_every_endpoint_addressing_kind_round_trips(self, addressing: str) -> None:
        endpoint = {
            "id": "target",
            "addressing": addressing,
            "address": "10.0.3.41:8080",
            **({"host_header": "api.example.com"} if addressing == "ip" else {}),
        }
        parsed = profile(endpoints=[endpoint])
        assert parsed.endpoints[0].addressing == addressing
        assert parse_profile(to_document(parsed)) == parsed

    def test_legacy_profile_addressing_is_mapped_onto_explicit_endpoints(self) -> None:
        direct = profile(
            addressing="direct",
            endpoints=[
                {"id": "target", "address": "10.0.3.41:8080", "host_header": "api.example.com"}
            ],
        )
        balanced = profile(
            addressing="load_balancer",
            endpoints=[{"id": "target", "address": "lb.example.com:443"}],
        )
        assert direct.endpoints[0].addressing == "ip"
        assert balanced.endpoints[0].addressing == "alb"
        assert "addressing" not in to_document(direct)

    def test_an_observation_only_endpoint_keeps_its_path_class(self) -> None:
        parsed = profile(
            endpoints=[
                {
                    "id": "balancer",
                    "addressing": "alb",
                    "address": "lb.example.com:443",
                    "load": False,
                    "collect": {"transport": "ssh"},
                }
            ]
        )
        assert parsed.addressing == "load_balancer"

    def test_an_unknown_transport_is_refused(self) -> None:
        with pytest.raises(ProfileError, match="transport"):
            profile(
                endpoints=[
                    {"id": "a", "address": "10.0.3.41:8080", "collect": {"transport": "telnet"}}
                ]
            )


class TestEndpoints:
    def test_host_and_port_are_derived_from_the_address(self) -> None:
        endpoint = profile().endpoints[0]
        assert (endpoint.host, endpoint.port) == ("10.0.3.41", 8080)

    def test_ipv6_addresses_keep_their_brackets_out_of_the_host(self) -> None:
        parsed = profile(
            endpoints=[{"id": "v6", "address": "[2001:db8::1]:8080"}],
        )
        assert parsed.endpoints[0].host == "2001:db8::1"
        assert parsed.endpoints[0].port == 8080

    def test_collection_defaults_to_the_endpoint_host(self) -> None:
        collection = profile().endpoints[0].collection()
        assert collection.host == "10.0.3.41"
        assert collection.user == "ec2-user"

    def test_the_observation_username_applies_to_every_ssh_endpoint(self) -> None:
        prof = profile(
            observe={"interval": "1s", "ssh_user": "ubuntu", "collect": ["cpu"]}
        )
        observed = prof.with_observation_defaults()
        assert observed.endpoints[0].collect.user == "ubuntu"
        assert observed.endpoints[1].collect.transport == "none"

    def test_only_collectable_endpoints_are_observed(self) -> None:
        parsed = profile()
        assert [e.id for e in parsed.observed] == ["task-a1b2c3"]


class TestToTargets:
    @pytest.fixture
    def validator(self) -> Draft202012Validator:
        return Draft202012Validator(json.loads(TARGETS_SCHEMA.read_text(encoding="utf-8")))

    def test_the_result_satisfies_the_engines_schema(self, validator) -> None:
        """The handoff point: a profile becomes something the engine understands."""
        targets = to_targets(profile())
        errors = sorted(validator.iter_errors(targets), key=str)
        assert not errors, "\n".join(f"{list(e.absolute_path)}: {e.message}" for e in errors)

    def test_it_carries_what_explains_an_outlier(self) -> None:
        first = to_targets(profile())["list"][0]
        assert first["host_header"] == "api.staging.example.com"
        assert first["attributes"]["image_digest"] == "sha256:9f2c1e"

    def test_sweep_settings_come_through(self) -> None:
        targets = to_targets(profile())
        assert targets["order"] == "shuffle"
        assert targets["gap"] == "30s"

    def test_a_subset_can_be_selected(self, validator) -> None:
        targets = to_targets(profile(), only=["task-d4e5f6"])
        assert [t["id"] for t in targets["list"]] == ["task-d4e5f6"]
        assert not list(validator.iter_errors(targets))

    def test_selecting_an_unknown_endpoint_says_what_exists(self) -> None:
        with pytest.raises(ProfileError, match="task-a1b2c3"):
            to_targets(profile(), only=["task-nope"])

    def test_selecting_nothing_is_refused(self) -> None:
        with pytest.raises(ProfileError, match="no targets selected"):
            to_targets(profile(), only=[])

    def test_tls_is_only_emitted_when_it_says_something(self) -> None:
        targets = to_targets(profile())
        assert "tls" not in targets["list"][0], "a plain endpoint should not carry empty tls"
        assert targets["list"][1]["tls"] == {"enabled": True}


def test_gap_survives_the_duration_round_trip() -> None:
    assert parse_profile(to_document(profile(gap="2m"))).gap == timedelta(minutes=2)
