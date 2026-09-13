"""Validate the genuine standalone streams produced by the Rust output test."""

import json
from pathlib import Path

from jsonschema import Draft202012Validator, FormatChecker

root = Path(__file__).resolve().parents[1]
schema = json.loads((root / "schema/events.schema.json").read_text(encoding="utf-8"))
validator = Draft202012Validator(schema, format_checker=FormatChecker())
for name in ("summary", "events"):
    path = root / "engine/target/output-contract" / f"{name}.ndjson"
    lines = path.read_text(encoding="utf-8").splitlines()
    if not lines:
        raise ValueError(f"empty {name} output")
    for number, line in enumerate(lines, 1):
        try:
            validator.validate(json.loads(line))
        except Exception as error:
            raise ValueError(f"{name} line {number} violates the event contract") from error
    print(f"{name}: {len(lines)} records match the frozen schema")
