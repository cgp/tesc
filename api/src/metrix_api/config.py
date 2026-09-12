"""METRIX_HOME resolution and settings.

Everything the API keeps on disk lives under one root so a person can find it, back
it up, or delete it without guessing. The root is resolved once, at startup, and
passed down -- nothing else reads the environment.

Layout (docs/implementation-api.md 2):

    $METRIX_HOME/
        config.toml     this file's source
        profiles/       target profiles
        plans/          one bundle directory per plan
        secrets/        optional local store for {{ secret.* }}
        metrix.db       SQLite: everything queryable
        runs/           per-run directories: streams, samples, snapshots
"""

from __future__ import annotations

import os
import re
import tomllib
from dataclasses import dataclass, field, replace
from datetime import timedelta
from pathlib import Path
from typing import Any

ENV_HOME = "METRIX_HOME"
DEFAULT_HOME = Path.home() / ".metrix"
CONFIG_NAME = "config.toml"

#: Same spelling the plan format uses, so a duration means one thing across the tool.
#: Mirrors the ``Dur`` pattern in schema/mix.schema.json.
_DURATION = re.compile(r"^(?:\d+[smh])+$")
_DURATION_PART = re.compile(r"(\d+)([smh])")
_UNIT_SECONDS = {"s": 1, "m": 60, "h": 3600}


class ConfigError(Exception):
    """Configuration that cannot be used. The message says which key and why."""


def parse_duration(value: str, *, key: str = "duration") -> timedelta:
    """Read ``"30s"``, ``"10m"``, ``"1h30m"``.

    Units are required: a bare ``"30"`` is ambiguous and is rejected rather than
    guessed at, exactly as the engine rejects it.
    """
    text = value.strip()
    if not _DURATION.match(text):
        raise ConfigError(
            f'{key}: expected a duration like "30s", "10m" or "1h30m", got {value!r}'
            + ("; write 30s, not 30" if text.isdigit() else "")
        )
    seconds = sum(int(n) * _UNIT_SECONDS[u] for n, u in _DURATION_PART.findall(text))
    return timedelta(seconds=seconds)


def format_duration(value: timedelta) -> str:
    """Render the largest whole unit that divides evenly, matching the engine."""
    seconds = int(value.total_seconds())
    if seconds and seconds % 3600 == 0:
        return f"{seconds // 3600}h"
    if seconds and seconds % 60 == 0:
        return f"{seconds // 60}m"
    return f"{seconds}s"


@dataclass(frozen=True, slots=True)
class ServerConfig:
    host: str = "127.0.0.1"
    port: int = 8080


@dataclass(frozen=True, slots=True)
class AwsConfig:
    """Read-only discovery credentials. Used only under ``discovery/``."""

    profile: str | None = None
    region: str | None = None


@dataclass(frozen=True, slots=True)
class ObserveConfig:
    """Defaults for host collection. A profile may override any of these."""

    interval: timedelta = timedelta(seconds=1)
    ssh_user: str | None = None
    ssh_key: Path | None = None
    #: How long a collector may take before the interval is recorded as a gap.
    timeout: timedelta = timedelta(seconds=5)


@dataclass(frozen=True, slots=True)
class PhaseConfig:
    """On by default: the cost is wall clock, the benefit is numbers that mean something."""

    baseline: timedelta = timedelta(seconds=30)
    settle: timedelta = timedelta(seconds=60)


@dataclass(frozen=True, slots=True)
class Config:
    home: Path
    server: ServerConfig = field(default_factory=ServerConfig)
    aws: AwsConfig = field(default_factory=AwsConfig)
    observe: ObserveConfig = field(default_factory=ObserveConfig)
    phases: PhaseConfig = field(default_factory=PhaseConfig)

    # Paths are derived, never configured separately: one root, no surprises.
    @property
    def config_file(self) -> Path:
        return self.home / CONFIG_NAME

    @property
    def profiles_dir(self) -> Path:
        return self.home / "profiles"

    @property
    def plans_dir(self) -> Path:
        return self.home / "plans"

    @property
    def secrets_dir(self) -> Path:
        return self.home / "secrets"

    @property
    def runs_dir(self) -> Path:
        return self.home / "runs"

    @property
    def database(self) -> Path:
        return self.home / "metrix.db"

    def run_dir(self, recording_id: str) -> Path:
        return self.runs_dir / recording_id

    def ensure_layout(self) -> Config:
        """Create the directories. Safe to call repeatedly."""
        for path in (self.home, self.profiles_dir, self.plans_dir, self.secrets_dir, self.runs_dir):
            path.mkdir(parents=True, exist_ok=True)
        return self


def resolve_home(explicit: Path | str | None = None) -> Path:
    """Explicit argument, then ``$METRIX_HOME``, then ``~/.metrix``."""
    if explicit is not None:
        return Path(explicit).expanduser()
    from_env = os.environ.get(ENV_HOME)
    if from_env:
        return Path(from_env).expanduser()
    return DEFAULT_HOME


def load_config(home: Path | str | None = None) -> Config:
    """Build the configuration. A missing config.toml means defaults, not an error."""
    root = resolve_home(home)
    config = Config(home=root)

    path = config.config_file
    if not path.is_file():
        return config

    try:
        raw = tomllib.loads(path.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as exc:
        raise ConfigError(f"{path}: {exc}") from exc

    known = {"server", "aws", "observe", "phases"}
    if unknown := set(raw) - known:
        raise ConfigError(
            f"{path}: unknown section(s) {', '.join(sorted(unknown))}; expected "
            f"{', '.join(sorted(known))}"
        )

    return replace(
        config,
        server=_server(raw.get("server", {}), path),
        aws=_aws(raw.get("aws", {}), path),
        observe=_observe(raw.get("observe", {}), path),
        phases=_phases(raw.get("phases", {}), path),
    )


def _section(raw: dict[str, Any], allowed: set[str], name: str, path: Path) -> None:
    if unknown := set(raw) - allowed:
        raise ConfigError(
            f"{path}: unknown key(s) in [{name}]: {', '.join(sorted(unknown))}; expected "
            f"{', '.join(sorted(allowed))}"
        )


def _server(raw: dict[str, Any], path: Path) -> ServerConfig:
    _section(raw, {"host", "port"}, "server", path)
    # Defaults come from an instance: with slots=True the class attributes are slot
    # descriptors, not values.
    defaults = ServerConfig()
    port = raw.get("port", defaults.port)
    if not isinstance(port, int) or isinstance(port, bool) or not 1 <= port <= 65535:
        raise ConfigError(f"{path}: server.port must be a port number, got {port!r}")
    return ServerConfig(host=str(raw.get("host", defaults.host)), port=port)


def _aws(raw: dict[str, Any], path: Path) -> AwsConfig:
    _section(raw, {"profile", "region"}, "aws", path)
    return AwsConfig(profile=raw.get("profile"), region=raw.get("region"))


def _observe(raw: dict[str, Any], path: Path) -> ObserveConfig:
    _section(raw, {"interval", "ssh_user", "ssh_key", "timeout"}, "observe", path)
    defaults = ObserveConfig()
    key = raw.get("ssh_key")
    return ObserveConfig(
        interval=(
            parse_duration(raw["interval"], key="observe.interval")
            if "interval" in raw
            else defaults.interval
        ),
        ssh_user=raw.get("ssh_user"),
        ssh_key=Path(key).expanduser() if key else None,
        timeout=(
            parse_duration(raw["timeout"], key="observe.timeout")
            if "timeout" in raw
            else defaults.timeout
        ),
    )


def _phases(raw: dict[str, Any], path: Path) -> PhaseConfig:
    _section(raw, {"baseline", "settle"}, "phases", path)
    defaults = PhaseConfig()
    return PhaseConfig(
        baseline=(
            parse_duration(raw["baseline"], key="phases.baseline")
            if "baseline" in raw
            else defaults.baseline
        ),
        settle=(
            parse_duration(raw["settle"], key="phases.settle")
            if "settle" in raw
            else defaults.settle
        ),
    )
