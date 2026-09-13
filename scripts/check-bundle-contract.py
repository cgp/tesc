"""The API assembles a bundle; the real engine reads it and agrees what it is.

Two things are checked, and both are about the same promise — that what the UI runs
and what a person runs by hand are the same artifact.

**The engine accepts what the API produces.** The bundle is assembled here and handed
to the actual binary. If the API ever emits a document the engine's parser rejects,
this fails at the boundary rather than the first time somebody launches a run.

**The two agree on the plan hash.** The engine identifies a plan by a SHA-256 over
the bytes it read, and that hash is part of a run's series identity: if the API's
idea of it drifted from the engine's, every run would be filed under a name the
engine never used. The rule is implemented twice, in two languages, so it is checked
against the thing itself rather than against a copy of the same assumption.

The target address points nowhere on purpose. The engine emits `run_started` — hash
included — before it tries to connect, so no mock server is needed and nothing here
depends on the network.
"""

import json
import subprocess
import tempfile
from pathlib import Path

from metrix_api import plans
from metrix_api.config import load_config
from metrix_api.profiles import load_profile, parse_profile, save_profile

root = Path(__file__).resolve().parents[1]

BINARY = next(
    (
        candidate
        for candidate in (
            root / "engine/target/debug/metrix-engine.exe",
            root / "engine/target/debug/metrix-engine",
        )
        if candidate.is_file()
    ),
    None,
)
if BINARY is None:
    raise SystemExit(
        "engine binary not built; this check runs after `check.sh engine` in the full suite"
    )

# Nothing listens here. The engine reports the plan it loaded before it finds that out.
PROFILE = {
    "name": "contract",
    "endpoints": [{"id": "nowhere", "address": "127.0.0.1:1", "collect": {"transport": "none"}}],
}

with tempfile.TemporaryDirectory() as workspace:
    home = load_config(Path(workspace) / "home").ensure_layout()
    save_profile(home, parse_profile(PROFILE))

    # The shipped example, stored the way a plan is stored: mix and calls, no targets.
    source = root / "examples/plans/mock-fixed"
    stored = home.plans_dir / "mock-fixed"
    (stored / "calls").mkdir(parents=True)
    (stored / "mix.json").write_bytes((source / "mix.json").read_bytes())
    (stored / "calls/ping.json").write_bytes((source / "calls/ping.json").read_bytes())

    bundle = plans.assemble(
        plans.load_plan(home, "mock-fixed"), load_profile(home, "contract")
    )
    exported = bundle.write(Path(workspace) / "bundle")

    result = subprocess.run(
        [str(BINARY), "--plan", str(exported)],
        capture_output=True,
        text=True,
        timeout=60,
        # A nonzero exit is expected: the target is unreachable on purpose, and what
        # is being read is the record the engine emits before it finds that out.
        check=False,
    )
    first = result.stdout.splitlines()[0] if result.stdout.strip() else ""
    if not first:
        raise SystemExit(
            "the engine produced no records for an API-assembled bundle:\n"
            f"{result.stderr.strip()[:800]}"
        )

    started = json.loads(first)
    if started["type"] != "run_started":
        raise SystemExit(f"expected run_started first, got {started['type']!r}")
    if started["plan_hash"] != bundle.hash:
        raise SystemExit(
            "plan hash disagrees across the boundary: a run would be filed under a "
            "name the engine never used:\n"
            f"  api    {bundle.hash}\n"
            f"  engine {started['plan_hash']}"
        )

print(f"engine accepted the assembled bundle and agrees on {bundle.hash}")
