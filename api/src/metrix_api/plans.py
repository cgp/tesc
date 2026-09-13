"""Turning a stored plan and a profile into the directory the engine runs.

Exactly two things cross the boundary between the API and the engine, and this is
one of them (design-api-engine-contract C1): a bundle directory holding `mix.json`,
`targets.json` and `calls/`. The engine reads it and needs nothing else — no
database, no API, no cloud credentials. Exporting from here produces exactly that
directory, so what the UI ran and what a person runs by hand are the same artifact.

**A stored plan is the mix and the calls. Targets are never stored with it.** They
come from a profile at assembly time, because a profile is the thing that knows how
to resolve a hostname into the boxes actually behind it (§3.1), and the engine knows
nothing about profiles. One plan against three environments is three bundles that
differ in one file.

**The bytes are deterministic, and that is a correctness requirement rather than a
nicety.** The engine identifies a plan by a SHA-256 over the exact bytes it read, and
that hash is part of a run's series identity (§17.2). If assembling the same plan
twice produced different bytes — a re-ordered object, a different indent — the hash
would move, and every run would start a new series with no history. So documents are
written one way: sorted keys, two-space indent, a trailing newline, UTF-8, LF.

The hash is computed here over the same bytes and by the same rule as the engine's,
so the API can record what a run will be identified by before the run starts. A
contract check runs the real binary over an assembled bundle and compares the two.
"""

from __future__ import annotations

import hashlib
import io
import json
import re
import zipfile
from dataclasses import dataclass, field
from functools import lru_cache
from pathlib import Path
from typing import Any

from jsonschema import Draft202012Validator

from metrix_api.config import Config
from metrix_api.profiles import Profile, to_targets

MIX = "mix.json"
TARGETS = "targets.json"

#: Where the generated JSON Schemas live. Generated from the shared Rust types by
#: `metrix-engine --emit-schemas`, so validating here is validating against the
#: engine's own parser rather than against a second opinion about the format.
SCHEMAS = Path(__file__).resolve().parents[3] / "schema"

#: Plan names are directory names and appear in exports and run metadata, so they
#: are held to the same shape profile names are.
NAME = re.compile(r"^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$")

#: Carried into the bundle untouched: Lua generators and CSV datasets (§4.4). They
#: are not JSON, nothing here validates them, and — matching the engine — they are
#: not part of the plan hash, which covers the three documents it parses.
EXTRA_DIRS = ("gen", "data")


class PlanError(Exception):
    """A plan that cannot be loaded, or cannot be made into a bundle."""


@lru_cache(maxsize=8)
def _validator(name: str) -> Draft202012Validator:
    path = SCHEMAS / name
    if not path.is_file():  # pragma: no cover - a broken checkout, not a user error
        raise PlanError(f"missing schema {name}; run scripts/check-schema.sh")
    return Draft202012Validator(json.loads(path.read_text(encoding="utf-8")))


def _check(document: Any, schema: str, where: str) -> None:
    """Validate against the frozen schema, naming the field rather than the shape.

    A jsonschema message quotes the offending value, which for a header or a query
    parameter can be a secret. Only the path and the rule are kept.
    """
    errors = sorted(_validator(schema).iter_errors(document), key=lambda e: list(e.path))
    if not errors:
        return
    first = errors[0]
    field_path = "/".join(str(part) for part in first.path)
    at = f"{where}/{field_path}" if field_path else where
    raise PlanError(f"{at}: {first.validator} — {first.message[:120]}")


def document_bytes(document: Any) -> bytes:
    """One way to write a document, so the same plan always hashes the same.

    Sorted keys because a dict's order depends on how it was built, and two plans
    that differ only in that are the same plan.
    """
    text = json.dumps(document, indent=2, sort_keys=True, ensure_ascii=False)
    return (text + "\n").encode("utf-8")


@dataclass(frozen=True, slots=True)
class Plan:
    """What is stored: the mixture, the calls it names, and any supporting files."""

    name: str
    mix: dict[str, Any]
    #: Relative path -> document, for every file named in `mix.calls`.
    calls: dict[str, dict[str, Any]] = field(default_factory=dict)
    #: Relative path -> exact bytes, for `gen/` and `data/`.
    extras: dict[str, bytes] = field(default_factory=dict)
    #: Things worth saying about the stored form that are not errors.
    notes: list[str] = field(default_factory=list)

    @property
    def call_names(self) -> list[str]:
        return sorted(name for document in self.calls.values() for name in document)

    @property
    def chains(self) -> list[dict[str, Any]]:
        return list(self.mix.get("chains", []))


def _relative(root: Path, reference: str, *, where: str) -> Path:
    """Resolve a reference inside the plan, refusing anything that leaves it.

    The same rule the engine applies to its bundle root. A plan is a unit of
    transfer, and one that reads a file outside itself is not transferable.
    """
    if not reference or reference.startswith("/") or "\\" in reference:
        raise PlanError(f"{where}: expected a relative path inside the plan")
    candidate = Path(reference)
    if candidate.is_absolute() or any(part in ("..", "") for part in candidate.parts):
        raise PlanError(f"{where}: file references must not leave the plan directory")
    resolved = (root / candidate).resolve()
    if not resolved.is_relative_to(root.resolve()):
        raise PlanError(f"{where}: file references must not leave the plan directory")
    return resolved


def load_plan(config: Config, name: str) -> Plan:
    """Read one stored plan and check it against the frozen schemas.

    Validated on the way in rather than on the way out, so a plan that cannot become
    a runnable bundle says so when it is opened rather than when somebody tries to
    run it.
    """
    if not NAME.match(name):
        raise PlanError(f"plan {name!r}: names are letters, digits, dot, dash, underscore")
    root = config.plans_dir / name
    if not root.is_dir():
        raise PlanError(f"no plan {name!r}")

    mix = _read_json(root / MIX, where=MIX)
    _check(mix, "mix.schema.json", MIX)

    notes = []
    if (root / TARGETS).is_file():
        # Not an error: an exported bundle carries one, and re-importing it should
        # work. But it is never used -- targets come from the profile named at
        # assembly -- and a file that is silently ignored is a file that will be
        # edited by somebody expecting it to matter.
        notes.append(
            f"{TARGETS} in the stored plan is ignored; targets are assembled from the "
            "profile chosen at export"
        )

    calls: dict[str, dict[str, Any]] = {}
    for index, reference in enumerate(mix.get("calls", [])):
        where = f"{MIX}/calls/{index}"
        path = _relative(root, reference, where=where)
        document = _read_json(path, where=reference)
        _check(document, "call.schema.json", reference)
        calls[_posix(reference)] = document

    named = {name for document in calls.values() for name in document}
    for chain_index, chain in enumerate(mix.get("chains", [])):
        for step_index, step in enumerate(chain.get("steps", [])):
            if step.get("call") not in named:
                raise PlanError(
                    f"{MIX}/chains/{chain_index}/steps/{step_index}/call: "
                    f"{step.get('call')!r} is not defined in any calls file"
                )

    extras = {}
    for directory in EXTRA_DIRS:
        for path in sorted((root / directory).rglob("*") if (root / directory).is_dir() else []):
            if path.is_file():
                extras[_posix(path.relative_to(root).as_posix())] = path.read_bytes()

    return Plan(name=name, mix=mix, calls=calls, extras=extras, notes=notes)


def _read_json(path: Path, *, where: str) -> dict[str, Any]:
    if not path.is_file():
        raise PlanError(f"{where}: missing from the plan")
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except UnicodeDecodeError as exc:
        raise PlanError(f"{where}: not UTF-8 text") from exc
    except json.JSONDecodeError as exc:
        # Location, never the offending text: a call document holds headers and
        # query values, and a parse error can quote a token into a log.
        raise PlanError(f"{where}: invalid JSON at line {exc.lineno}, column {exc.colno}") from exc


def _posix(reference: str) -> str:
    return str(Path(reference).as_posix())


def list_plans(config: Config) -> tuple[list[Plan], list[dict[str, str]]]:
    """Every stored plan, and the ones that could not be read.

    Broken plans are returned rather than skipped, for the same reason the profile
    list does it: a plan that vanishes from the list because it has a typo in it is
    a plan nobody can find in order to fix.
    """
    if not config.plans_dir.is_dir():
        return [], []
    found, broken = [], []
    for directory in sorted(p for p in config.plans_dir.iterdir() if p.is_dir()):
        try:
            found.append(load_plan(config, directory.name))
        except PlanError as exc:
            broken.append({"name": directory.name, "error": str(exc)})
    return found, broken


@dataclass(frozen=True, slots=True)
class Bundle:
    """A plan directory, in memory, as the exact bytes that will be written."""

    #: Relative POSIX path -> bytes. Ordered as written; the hash sorts its own.
    files: dict[str, bytes]
    #: The paths the plan hash covers: the three documents the engine parses.
    hashed: tuple[str, ...]

    @property
    def hash(self) -> str:
        """The engine's `plan_hash`, computed by the engine's rule.

        Length-framed so no pair of paths and contents can be concatenated into the
        same byte stream as a different pair — `a`+`bc` and `ab`+`c` must not collide.
        """
        digest = hashlib.sha256()
        for path in sorted(self.hashed):
            content = self.files[path]
            digest.update(len(path).to_bytes(8, "little"))
            digest.update(path.encode("utf-8"))
            digest.update(len(content).to_bytes(8, "little"))
            digest.update(content)
        return f"sha256:{digest.hexdigest()}"

    def write(self, directory: Path) -> Path:
        """Write the bundle out as the directory the engine takes."""
        for relative, content in self.files.items():
            path = directory / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        return directory

    def archive(self) -> bytes:
        """The same directory as a zip, for a browser to download.

        Every entry gets one fixed timestamp. `zipfile` defaults to the clock, which
        would make two exports of an unchanged plan differ byte for byte — and the
        point of exporting is to be able to diff and re-import one.
        """
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
            for relative in sorted(self.files):
                info = zipfile.ZipInfo(relative, date_time=(1980, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = 0o644 << 16
                archive.writestr(info, self.files[relative])
        return buffer.getvalue()


def assemble(plan: Plan, profile: Profile, *, only: list[str] | None = None) -> Bundle:
    """The stored plan plus a profile's boxes: one runnable directory.

    `only` selects a subset of the profile's endpoints, for running against one box
    of an environment rather than all of them.
    """
    targets = to_targets(profile, only=only)
    _check(targets, "targets.schema.json", TARGETS)

    files: dict[str, bytes] = {
        MIX: document_bytes(plan.mix),
        TARGETS: document_bytes(targets),
    }
    for relative, document in plan.calls.items():
        files[relative] = document_bytes(document)
    # Carried verbatim and deliberately outside the hash, matching the engine, which
    # digests only the documents it parses.
    files.update(plan.extras)

    return Bundle(files=files, hashed=(MIX, TARGETS, *plan.calls))
