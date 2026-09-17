"""METRIX_HOME resolution and config parsing."""

from __future__ import annotations

from datetime import timedelta
from pathlib import Path

import pytest

from metrix_api.config import (
    ENV_HOME,
    LOCAL_HOME_NAME,
    ConfigError,
    describe_home_source,
    format_duration,
    load_config,
    parse_duration,
    resolve_home,
)


class TestHomeResolution:
    """Four rules in a fixed order. Every test pins the working directory: two of
    the rules read state outside the process, so a test that does not would pass or
    fail depending on where pytest was started."""

    def test_explicit_argument_wins(self, tmp_path: Path, monkeypatch) -> None:
        monkeypatch.chdir(tmp_path)
        monkeypatch.setenv(ENV_HOME, str(tmp_path / "from-env"))
        (tmp_path / LOCAL_HOME_NAME).mkdir()
        resolved = resolve_home(tmp_path / "explicit")
        assert resolved.path == tmp_path / "explicit"
        assert resolved.source == "argument"

    def test_environment_is_next(self, tmp_path: Path, monkeypatch) -> None:
        monkeypatch.chdir(tmp_path)
        monkeypatch.setenv(ENV_HOME, str(tmp_path / "from-env"))
        (tmp_path / LOCAL_HOME_NAME).mkdir()
        resolved = resolve_home()
        assert resolved.path == tmp_path / "from-env"
        assert resolved.source == "environment"

    def test_an_existing_local_dot_metrix_is_used(self, tmp_path: Path, monkeypatch) -> None:
        """A checkout opts in by creating the directory."""
        monkeypatch.chdir(tmp_path)
        monkeypatch.delenv(ENV_HOME, raising=False)
        (tmp_path / LOCAL_HOME_NAME).mkdir()
        resolved = resolve_home()
        assert resolved.path == tmp_path / LOCAL_HOME_NAME
        assert resolved.source == "project"

    def test_a_local_dot_metrix_is_never_created_by_being_in_a_directory(
        self, tmp_path: Path, monkeypatch
    ) -> None:
        """Without this the tool would scatter a database wherever it was started."""
        monkeypatch.chdir(tmp_path)
        monkeypatch.delenv(ENV_HOME, raising=False)
        resolved = resolve_home()
        assert resolved.path == Path.home() / ".metrix"
        assert resolved.source == "default"
        assert not (tmp_path / LOCAL_HOME_NAME).exists()

    def test_a_local_dot_metrix_that_is_a_file_is_not_a_home(
        self, tmp_path: Path, monkeypatch
    ) -> None:
        monkeypatch.chdir(tmp_path)
        monkeypatch.delenv(ENV_HOME, raising=False)
        (tmp_path / LOCAL_HOME_NAME).write_text("not a directory")
        assert resolve_home().source == "default"

    def test_default_when_nothing_is_set(self, tmp_path: Path, monkeypatch) -> None:
        monkeypatch.chdir(tmp_path)
        monkeypatch.delenv(ENV_HOME, raising=False)
        assert resolve_home().path == Path.home() / ".metrix"

    def test_an_empty_environment_variable_is_not_a_home(
        self, tmp_path: Path, monkeypatch
    ) -> None:
        monkeypatch.chdir(tmp_path)
        monkeypatch.setenv(ENV_HOME, "")
        assert resolve_home().path == Path.home() / ".metrix"

    def test_every_source_has_an_explanation_the_page_can_show(
        self, tmp_path: Path, monkeypatch
    ) -> None:
        """The Config page prints this string; an unmapped source would KeyError."""
        monkeypatch.chdir(tmp_path)
        monkeypatch.delenv(ENV_HOME, raising=False)
        for source in ("argument", "environment", "project", "default"):
            assert describe_home_source(source)

        monkeypatch.setenv(ENV_HOME, str(tmp_path / "env"))
        assert ENV_HOME in resolve_home().explanation


class TestLayout:
    def test_every_path_hangs_off_the_one_root(self, tmp_path: Path) -> None:
        config = load_config(tmp_path)
        for path in (
            config.config_file,
            config.profiles_dir,
            config.schemas_dir,
            config.plans_dir,
            config.secrets_dir,
            config.runs_dir,
            config.database,
            config.run_dir("2026-09-12T14-03-11Z_a3f9"),
        ):
            assert tmp_path in path.parents or path.parent == tmp_path

    def test_ensure_layout_is_repeatable(self, tmp_path: Path) -> None:
        config = load_config(tmp_path / "home")
        config.ensure_layout()
        config.ensure_layout()  # must not raise on an existing tree
        assert config.profiles_dir.is_dir()
        assert config.schemas_dir.is_dir()
        assert config.runs_dir.is_dir()


class TestDurations:
    @pytest.mark.parametrize(
        ("text", "seconds"),
        [("30s", 30), ("10m", 600), ("1h", 3600), ("1h30m", 5400), ("90s", 90)],
    )
    def test_parsing(self, text: str, seconds: int) -> None:
        assert parse_duration(text) == timedelta(seconds=seconds)

    def test_a_bare_number_is_rejected_with_advice(self) -> None:
        with pytest.raises(ConfigError, match="write 30s, not 30"):
            parse_duration("30")

    @pytest.mark.parametrize("text", ["", "s", "30x", "m30", "thirty"])
    def test_nonsense_is_rejected(self, text: str) -> None:
        with pytest.raises(ConfigError):
            parse_duration(text)

    @pytest.mark.parametrize(("seconds", "text"), [(90, "90s"), (120, "2m"), (3600, "1h")])
    def test_formatting_matches_the_engine(self, seconds: int, text: str) -> None:
        assert format_duration(timedelta(seconds=seconds)) == text


class TestConfigFile:
    def test_a_missing_file_means_defaults(self, tmp_path: Path) -> None:
        config = load_config(tmp_path)
        assert config.server.port == 8080
        assert config.observe.interval == timedelta(seconds=1)
        assert config.phases.baseline == timedelta(seconds=30)
        assert config.phases.settle == timedelta(seconds=60)

    def test_values_are_read(self, tmp_path: Path) -> None:
        (tmp_path / "config.toml").write_text(
            """
            [server]
            host = "0.0.0.0"
            port = 9000

            [aws]
            profile = "staging"
            region = "us-east-1"

            [observe]
            interval = "2s"
            ssh_user = "ec2-user"

            [phases]
            baseline = "1m"
            settle = "90s"
            """,
            encoding="utf-8",
        )
        config = load_config(tmp_path)
        assert (config.server.host, config.server.port) == ("0.0.0.0", 9000)
        assert config.aws.profile == "staging"
        assert config.observe.interval == timedelta(seconds=2)
        assert config.observe.ssh_user == "ec2-user"
        assert config.phases.baseline == timedelta(seconds=60)
        assert config.phases.settle == timedelta(seconds=90)

    def test_a_typo_is_named_rather_than_ignored(self, tmp_path: Path) -> None:
        (tmp_path / "config.toml").write_text("[observe]\nintervals = '2s'\n", encoding="utf-8")
        with pytest.raises(ConfigError, match="intervals"):
            load_config(tmp_path)

    def test_an_unknown_section_is_named(self, tmp_path: Path) -> None:
        (tmp_path / "config.toml").write_text("[oberve]\ninterval = '2s'\n", encoding="utf-8")
        with pytest.raises(ConfigError, match="oberve"):
            load_config(tmp_path)

    def test_a_bad_port_is_rejected(self, tmp_path: Path) -> None:
        (tmp_path / "config.toml").write_text("[server]\nport = 99999\n", encoding="utf-8")
        with pytest.raises(ConfigError, match="port"):
            load_config(tmp_path)

    def test_malformed_toml_names_the_file(self, tmp_path: Path) -> None:
        (tmp_path / "config.toml").write_text("[server\n", encoding="utf-8")
        with pytest.raises(ConfigError, match="config.toml"):
            load_config(tmp_path)
