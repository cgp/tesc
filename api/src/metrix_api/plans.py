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

from metrix_api.config import Config, ConfigError, parse_duration
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

#: Written beside a generated plan and never into the bundle: it records that the
#: plan is a skeleton nobody has reviewed, and what is still guesswork in it (§8.3).
#: Deliberately not a field in `mix.json` -- that document's shape belongs to the
#: engine, the plan hash covers its bytes, and a control-plane review state has no
#: business moving a run's identity.
DRAFT = "draft.json"

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
    #: The `draft.json` sidecar, when the plan was generated and not yet reviewed.
    draft: dict[str, Any] | None = None

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
    return _plan_from(root, name, mix)


def _plan_from(root: Path, name: str, mix: dict[str, Any]) -> Plan:
    """One mixture, its calls read off disk beside it, checked as a whole.

    Shared by the loader and by the editor's save path, so a document submitted from
    a form goes through exactly what a hand-written file goes through -- including
    the reference check, which the schema cannot express.
    """
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

    draft = None
    if (root / DRAFT).is_file():
        try:
            draft = json.loads((root / DRAFT).read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            # A marker that will not parse must not make the plan unreadable. The
            # plan is what matters; losing the todo list is a smaller loss than
            # losing the mixture, and the note says which happened.
            notes.append(f"{DRAFT} could not be read, so this plan's todo list is missing")

    extras = {}
    for directory in EXTRA_DIRS:
        for path in sorted((root / directory).rglob("*") if (root / directory).is_dir() else []):
            if path.is_file():
                extras[_posix(path.relative_to(root).as_posix())] = path.read_bytes()

    return Plan(name=name, mix=mix, calls=calls, extras=extras, notes=notes, draft=draft)


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


# --------------------------------------------------------- what a mixture implies

#: The statistical floor, matching the engine's `metrix_plan::MIN_SAMPLES`: 30s at
#: 75 RPS. Below it a p99 is an order statistic with a 95% CI spanning p98.6-p99.4,
#: and p99.9 is not measurable at all (§12.1). The engine warns rather than refuses,
#: and so does this: a short run is a legitimate smoke test, it just cannot carry a
#: tail percentile.
MIN_SAMPLES = 2250

#: Chain percentages must total this, within `PERCENT_EPSILON` -- so `16.67 x 6` is
#: not a validation failure. Both match `metrix_plan`.
PERCENT_TOTAL = 100.0
PERCENT_EPSILON = 0.01


@dataclass(frozen=True, slots=True)
class Problem:
    """One thing wrong with a mixture, or one thing worth knowing about it.

    `severity` separates the two, and they are not the same question. An **error**
    stops the plan being assembled into a bundle, because the run it describes is not
    the run somebody means. A **warning** is a run that will happen and will produce
    numbers that cannot be quoted -- which is worse than a failure if it is not said
    out loud beforehand.
    """

    severity: str
    #: A path into `mix.json`, so the editor can point at the field rather than at
    #: the document.
    where: str
    message: str

    def to_document(self) -> dict[str, str]:
        return {"severity": self.severity, "where": self.where, "message": self.message}


def _seconds(value: Any) -> float | None:
    """A duration string as seconds, or None when it is not one.

    Never raises: this runs over documents that have passed the schema and over
    drafts on their way to it. A field that cannot be read is withheld, not guessed.
    """
    if not isinstance(value, str) or not value:
        return None
    try:
        return parse_duration(value).total_seconds()
    except ConfigError:
        return None


def _rate(load: dict[str, Any]) -> float | None:
    rate = load.get("rate")
    return float(rate) if isinstance(rate, (int, float)) and rate > 0 else None


def figures(plan: Plan) -> dict[str, Any]:
    """The arithmetic behind the mixture: what each percentage actually buys.

    A percentage is not a quantity anybody can judge. 20% of a run is a number of
    requests, and whether that number supports a p99 is the question the editor
    exists to answer before the run rather than after it. So the conversion is done
    here, once, and the browser displays the answer -- the same rule every percentile
    in this product follows (§12.1): the figure and the sample count behind it are
    computed in one place.

    **A percentage buys chain iterations, not requests.** A two-step chain at 20% of
    150/s is 30 iterations a second and 60 requests a second, and conflating the two
    under-counts the load by the length of the chain.

    Everything rate-derived is withheld when there is no rate to derive it from. A
    breakpoint run searches for its rate, so no honest number exists here before it
    runs; `withheld` says which case this is rather than showing zeros.
    """
    load = plan.mix.get("load", {})
    mode = load.get("mode", "fixed")
    duration_s = _seconds(load.get("duration"))
    warmup_s = _seconds(load.get("warmup")) or 0.0
    phases = plan.mix.get("phases", {})

    rate = _rate(load)
    withheld = None
    if mode == "breakpoint":
        # The rate is the thing the run is looking for. Any figure here would be an
        # invention, and an invented sample count is the one number this tool must
        # never print.
        rate, withheld = None, "a breakpoint run searches for its rate"
    elif mode == "stages":
        stages = [s for s in load.get("stages", []) if _seconds(s.get("duration"))]
        served = sum(float(s.get("rate", 0)) * (_seconds(s["duration"]) or 0) for s in stages)
        staged = sum(_seconds(s["duration"]) or 0 for s in stages)
        duration_s = staged or duration_s
        # A ramp has no single rate, and its average is what the sample count rests
        # on. Stated as an average rather than passed off as the rate.
        rate = (served / staged) if staged else None
        withheld = None if rate else "the stages carry no duration"
    elif rate is None:
        withheld = "the mixture sets no rate"

    measured_s = None if duration_s is None else max(duration_s - warmup_s, 0.0)
    percent_total = sum(
        float(c["percent"]) for c in plan.chains if isinstance(c.get("percent"), (int, float))
    )

    chains = []
    for chain in plan.chains:
        percent = chain.get("percent")
        steps = len(chain.get("steps", []))
        share = None
        if rate is not None and isinstance(percent, (int, float)):
            share = rate * float(percent) / 100.0
        requests_per_s = None if share is None else share * steps
        requests = (
            None
            if requests_per_s is None or measured_s is None
            else int(round(requests_per_s * measured_s))
        )
        chains.append(
            {
                "name": chain.get("name"),
                "percent": percent,
                "session": chain.get("session", "reuse"),
                "steps": steps,
                "iterations_per_s": share,
                "requests_per_s": requests_per_s,
                "requests": requests,
                # Whether this chain's own percentiles will mean anything, which is a
                # different question from whether the run is long enough overall.
                "supported": None if requests is None else requests >= MIN_SAMPLES,
            }
        )

    total_requests = (
        None
        if not chains or any(c["requests"] is None for c in chains)
        else sum(c["requests"] for c in chains)
    )
    return {
        "mode": mode,
        "model": load.get("model", "open"),
        "rate": rate,
        "duration_s": duration_s,
        "warmup_s": warmup_s,
        "measured_s": measured_s,
        "baseline_s": _seconds(phases.get("baseline")),
        "settle_s": _seconds(phases.get("settle")),
        "percent_total": percent_total,
        "floor": MIN_SAMPLES,
        "iterations": (
            None if rate is None or measured_s is None else int(round(rate * measured_s))
        ),
        "requests": total_requests,
        "supported": None if total_requests is None else total_requests >= MIN_SAMPLES,
        "withheld": withheld,
        "chains": chains,
    }


def check(plan: Plan) -> list[Problem]:
    """Everything that stops this mixture running, and everything worth knowing.

    The schema has already had its say by the time this runs; these are the rules a
    schema cannot express -- ones about the document as a whole, or about what the
    numbers in it will produce. This is the only place they are written: the browser
    renders the list and computes none of it, so a form cannot save what a
    hand-written file would be rejected for.
    """
    problems: list[Problem] = []
    load = plan.mix.get("load", {})
    numbers = figures(plan)

    if not plan.chains:
        problems.append(Problem("error", "chains", "a mixture with no chains sends no traffic"))

    total = numbers["percent_total"]
    if plan.chains and abs(total - PERCENT_TOTAL) > PERCENT_EPSILON:
        # Named as a shortfall or an excess, never renormalized. Renormalizing would
        # silently change every other chain's share to accommodate a typo in one of
        # them, and the run would measure a mixture nobody chose.
        gap = PERCENT_TOTAL - total
        direction = f"{gap:.4g} short of" if gap > 0 else f"{-gap:.4g} over"
        problems.append(
            Problem(
                "error",
                "chains",
                f"the chain percentages total {total:.4g}, which is {direction} 100",
            )
        )

    seen: set[str] = set()
    for index, chain in enumerate(plan.chains):
        name = chain.get("name", "")
        at = f"chains/{index}"
        if name in seen:
            problems.append(
                Problem(
                    "error",
                    f"{at}/name",
                    f"two chains are named {name!r}; the name keys every chart series, "
                    "error report and SLO, so it has to pick one of them out",
                )
            )
        seen.add(name)

        steps = chain.get("steps", [])
        if not steps:
            problems.append(Problem("error", f"{at}/steps", f"chain {name!r} has no steps to send"))
        ids: set[str] = set()
        for step_index, step in enumerate(steps):
            step_id = step.get("id", "")
            if step_id in ids:
                problems.append(
                    Problem(
                        "error",
                        f"{at}/steps/{step_index}/id",
                        f"chain {name!r} uses the step id {step_id!r} twice; a step id is "
                        "how one step's own latency is reported",
                    )
                )
            ids.add(step_id)

        session = chain.get("session", "reuse")
        if session == "pool" and not chain.get("pool_size"):
            problems.append(
                Problem(
                    "error",
                    f"{at}/pool_size",
                    f"chain {name!r} cycles a pool of sessions but does not say how many",
                )
            )
        if session != "pool" and chain.get("pool_size"):
            problems.append(
                Problem(
                    "warning",
                    f"{at}/pool_size",
                    f"chain {name!r} sets a pool size but its session policy is "
                    f"{session!r}, so it is ignored",
                )
            )

    mode = load.get("mode", "fixed")
    if mode == "fixed" and _rate(load) is None:
        problems.append(Problem("error", "load/rate", "a fixed-rate run needs a rate"))
    if mode == "stages" and not load.get("stages"):
        problems.append(Problem("error", "load/stages", "a staged run needs its stages"))
    if mode == "breakpoint" and not load.get("breakpoint"):
        problems.append(
            Problem("error", "load/breakpoint", "a breakpoint run needs its search parameters")
        )
    if _seconds(load.get("duration")) is None:
        problems.append(
            Problem("error", "load/duration", 'a duration with units, like "60s" or "5m"')
        )
    elif numbers["measured_s"] == 0:
        problems.append(
            Problem(
                "error",
                "load/warmup",
                "the warmup is as long as the run, so nothing would be measured",
            )
        )

    problems.extend(_sample_count_problems(numbers))
    problems.extend(_unbound_problems(plan))
    if plan.draft:
        # A warning, never an error. A generated skeleton runs -- that is the point of
        # generating one -- and what it cannot do is be mistaken for a mixture somebody
        # chose. Blocking it would only teach people to delete the marker.
        outstanding = len(plan.draft.get("todos", []))
        problems.append(
            Problem(
                "warning",
                DRAFT,
                f"generated from {plan.draft.get('source', 'a description')} and not yet "
                f"reviewed, with {outstanding} thing(s) still to decide; the weights and "
                "the volume are this tool's, not anybody's",
            )
        )

    used = {step.get("call") for chain in plan.chains for step in chain.get("steps", [])}
    for unused in sorted(set(plan.call_names) - used):
        problems.append(
            Problem("warning", "calls", f"the call {unused!r} is defined but no chain invokes it")
        )
    return problems


def _sample_count_problems(numbers: dict[str, Any]) -> list[Problem]:
    """The §12.1 floor, said before the run instead of after it.

    Two separate statements, because they fail independently: a run can be long
    enough overall while a 5% chain inside it is nowhere near -- and it is the
    chain's percentiles that get quoted, not the run's.
    """
    problems = []
    if numbers["supported"] is False:
        problems.append(
            Problem(
                "warning",
                "load/duration",
                f"this run makes about {numbers['requests']} requests, below the "
                f"{MIN_SAMPLES} a tail percentile needs: a p99 measured here has a "
                "confidence interval wide enough to hide a regression",
            )
        )
    thin = [c for c in numbers["chains"] if c["supported"] is False]
    # Only when the run as a whole clears the floor. Otherwise this repeats the line
    # above once per chain, and a warning said six times is a warning nobody reads.
    if thin and numbers["supported"]:
        named = ", ".join(f"{c['name']} ({c['requests']})" for c in thin)
        problems.append(
            Problem(
                "warning",
                "chains",
                f"these chains stay below {MIN_SAMPLES} requests, so their percentiles "
                f"will be withheld: {named}",
            )
        )
    return problems


def ready(problems: list[Problem]) -> bool:
    """Whether this plan can be run. Errors stop it; warnings do not."""
    return not any(problem.severity == "error" for problem in problems)


# ------------------------------------------------------------ reading the calls

#: A template reference in a call: `{{ users.email }}`, `{{ pid }}`, `{{ rand(1,20) }}`.
TEMPLATE = re.compile(r"\{\{\s*(.+?)\s*\}\}")

#: Prefixes that resolve outside the chain. `env` and `secret` come from the
#: environment the engine runs in, `token` from the auth block.
AMBIENT = ("env.", "secret.")


def _references(value: Any) -> list[str]:
    """Every `{{ ... }}` in a call, wherever it is written.

    A call's variables are spread across its path, query, headers and body, and a
    reader trying to work out what a step depends on should not have to find them.
    """
    found: list[str] = []
    if isinstance(value, str):
        found.extend(match.group(1) for match in TEMPLATE.finditer(value))
    elif isinstance(value, dict):
        for item in value.values():
            found.extend(_references(item))
    elif isinstance(value, list):
        for item in value:
            found.extend(_references(item))
    return found


def _body_kind(call: dict[str, Any]) -> str:
    """How the request is produced (§7.2), in the words the plan uses.

    The generator is named on the call rather than inside its body, because the hook
    returns the whole request -- path, query, headers and body -- and a `body` block
    that could set the path would be a field lying about what it does.
    """
    generate = call.get("generate")
    if isinstance(generate, dict) and generate.get("generator"):
        return f"generator {generate['generator']}"
    if call.get("body") is None:
        return "none"
    return "inline template"


def call_details(plan: Plan) -> list[dict[str, Any]]:
    """Every call the plan defines, as much as is needed to read a chain.

    Calls are read-only in the UI on purpose (§20.3): they are authored from the
    codebase or generated from a schema, and hand-editing a request definition in a
    browser is how a plan drifts from the service it describes. What a reader needs
    instead is what each one does and what it depends on -- so the variables it
    consumes and the ones it extracts are pulled out here rather than left for
    someone to find by reading JSON.
    """
    details = []
    for reference, document in sorted(plan.calls.items()):
        for name, call in sorted(document.items()):
            uses = sorted(set(_references(call)))
            details.append(
                {
                    "name": name,
                    "file": reference,
                    "description": call.get("description"),
                    "method": call.get("method", "GET"),
                    "path": call.get("path", ""),
                    "headers": dict(sorted((call.get("headers") or {}).items())),
                    "query": sorted(call.get("query") or {}),
                    "body": _body_kind(call),
                    "uses": uses,
                    "extracts": sorted(call.get("extract") or {}),
                    "assert": call.get("assert") or [],
                }
            )
    return details


def _bindings(plan: Plan) -> set[str]:
    """Names that resolve without an earlier step having produced them.

    Datasets and not generators: a dotted name is a dataset field, and a generator
    produces request parts rather than template variables. Treating a generator name
    as a prefix here would call a plan ready that the engine refuses to load, which
    is the one thing this check must never do.
    """
    provided = {f"{name}." for name in plan.mix.get("datasets", {})}
    provided.update(AMBIENT)
    return provided


def _unbound_problems(plan: Plan) -> list[Problem]:
    """Steps that read a variable nothing before them writes.

    The most common way a hand-edited chain breaks, and the least visible: reordering
    two steps or deleting one leaves a `{{ order_id }}` that resolves to nothing, and
    the run reports it as an assertion failure against the service -- a bug hunt in
    the wrong codebase.

    A warning rather than an error, because this reads templates rather than
    evaluating them: an expression is skipped, an ambient prefix is trusted, and
    anything left is reported as a question rather than a verdict.
    """
    by_name = {detail["name"]: detail for detail in call_details(plan)}
    ambient = _bindings(plan)
    problems = []
    for index, chain in enumerate(plan.chains):
        available: set[str] = set()
        if plan.mix.get("auth"):
            available.add("token")
        for step_index, step in enumerate(chain.get("steps", [])):
            detail = by_name.get(step.get("call"))
            if detail is None:  # an undefined call is already a load error
                continue
            for used in detail["uses"]:
                if "(" in used or used in available:
                    continue
                if any(used.startswith(prefix) for prefix in ambient):
                    continue
                problems.append(
                    Problem(
                        "warning",
                        f"chains/{index}/steps/{step_index}",
                        f"step {step.get('id')!r} of chain {chain.get('name')!r} uses "
                        f"{{{{ {used} }}}}, which no earlier step extracts and no dataset "
                        "provides",
                    )
                )
            available.update(detail["extracts"])
    return problems


# --------------------------------------------------------------- writing one back


def plan_path(config: Config, name: str) -> Path:
    return config.plans_dir / name


def accept_draft(config: Config, name: str) -> bool:
    """Say that a generated plan has been read by somebody. Removes the marker.

    A separate act from saving it. Editing one percentage is not a review, and a flag
    that cleared itself on the first edit would mark every generated plan reviewed
    within a minute of being opened.
    """
    marker = plan_path(config, name) / DRAFT
    if not marker.is_file():
        return False
    marker.unlink()
    return True


def parse_mix(config: Config, name: str, mix: dict[str, Any]) -> Plan:
    """A submitted mixture read as if it had been the file, without writing it.

    The editor validates against this while it is being typed in, so what the form
    is told is what the loader would say about the same document saved.
    """
    root = plan_path(config, name)
    if not root.is_dir():
        raise PlanError(f"no plan {name!r}")
    return _plan_from(root, name, mix)


def save_mix(config: Config, name: str, mix: dict[str, Any]) -> Plan:
    """Replace one plan's mixture with a submitted document.

    The whole document, never a patch, and validated by the loader's own path: the
    editor cannot save something a hand-written file would be rejected for, and there
    is no second opinion about the format living in JavaScript.

    Written with `document_bytes`, the rule the bundle is hashed under. A save that
    reformatted the file -- a different indent, a different key order -- would move
    the plan hash and start a fresh series with no history, for a plan nobody had
    actually changed.
    """
    if not NAME.match(name):
        raise PlanError(f"plan {name!r}: names are letters, digits, dot, dash, underscore")
    root = plan_path(config, name)
    if not root.is_dir():
        raise PlanError(f"no plan {name!r}")
    if mix.get("name") != name:
        # The name is inside the bytes the plan hash covers, so renaming a plan
        # renames the series every run of it belongs to. That is a deliberate act,
        # not a side effect of editing a percentage.
        raise PlanError(
            f"{MIX}/name: a plan's name is fixed at {name!r}; it is part of what a run is "
            "identified by, so renaming one would split its history in two"
        )
    plan = _plan_from(root, name, mix)
    (root / MIX).write_bytes(document_bytes(mix))
    return plan
