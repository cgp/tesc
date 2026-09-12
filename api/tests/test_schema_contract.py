"""The cross-language half of the contract.

The Rust types are authoritative and generate `schema/*.json`; these tests prove the
API can validate real documents against them without building Rust. If the engine's
types change and the schemas are regenerated, a break shows up here rather than at
integration time.
"""

import json
from pathlib import Path

import pytest
from jsonschema import Draft202012Validator

REPO = Path(__file__).resolve().parents[2]
SCHEMA = REPO / "schema"
BUNDLE = REPO / "examples" / "plans" / "checkout-mixed"
FIXTURES = REPO / "examples" / "fixtures"


def load(path: Path):
    return json.loads(path.read_text(encoding="utf-8"))


def validator(name: str) -> Draft202012Validator:
    schema = load(SCHEMA / name)
    Draft202012Validator.check_schema(schema)
    return Draft202012Validator(schema)


@pytest.mark.parametrize(
    "schema_name",
    ["call.schema.json", "mix.schema.json", "targets.schema.json", "events.schema.json"],
)
def test_schemas_are_valid_json_schema(schema_name: str) -> None:
    validator(schema_name)


@pytest.mark.parametrize(
    ("schema_name", "document"),
    [
        ("mix.schema.json", "mix.json"),
        ("targets.schema.json", "targets.json"),
        ("call.schema.json", "calls/shop.json"),
    ],
)
def test_example_bundle_validates(schema_name: str, document: str) -> None:
    errors = sorted(validator(schema_name).iter_errors(load(BUNDLE / document)), key=str)
    assert not errors, "\n".join(f"{list(e.absolute_path)}: {e.message}" for e in errors)


def test_a_mix_missing_a_required_field_is_rejected() -> None:
    """The schema has to actually reject things, or validating proves nothing."""
    mix = load(BUNDLE / "mix.json")
    del mix["chains"]
    errors = list(validator("mix.schema.json").iter_errors(mix))
    assert errors, "a mix with no chains should not validate"


def test_a_duration_without_units_is_rejected() -> None:
    """`"30"` is the mistake the Dur pattern exists to catch."""
    mix = load(BUNDLE / "mix.json")
    mix["load"]["duration"] = "30"
    errors = list(validator("mix.schema.json").iter_errors(mix))
    assert errors, 'a bare "30" should not validate as a duration'


def test_fixture_stream_validates_record_by_record() -> None:
    """Every line of the canned stream is a legal record.

    This fixture is how track A builds ingest, the stats table, and the charts before
    an engine exists -- so it must conform to the same schema a real engine will emit.
    """
    events = validator("events.schema.json")
    lines = (FIXTURES / "summary.ndjson").read_text(encoding="utf-8").splitlines()
    assert lines, "fixture stream is empty"

    for number, line in enumerate(lines, start=1):
        record = json.loads(line)
        errors = sorted(events.iter_errors(record), key=str)
        assert not errors, "line {}: {}".format(
            number, "; ".join(f"{list(e.absolute_path)}: {e.message}" for e in errors)
        )


def test_fixture_stream_opens_and_closes_a_run() -> None:
    records = [
        json.loads(line)
        for line in (FIXTURES / "summary.ndjson").read_text(encoding="utf-8").splitlines()
    ]
    assert records[0]["type"] == "run_started"
    assert records[-1]["type"] == "run_finished"

    # Phases arrive in the order the design's timeline defines.
    phases = [r["phase"] for r in records if r["type"] == "phase_changed"]
    assert phases == ["baseline", "warmup", "measure", "drain", "settle"]

    # t_ms never goes backwards: the whole point of a shared monotonic clock.
    times = [r["t_ms"] for r in records]
    assert times == sorted(times)
