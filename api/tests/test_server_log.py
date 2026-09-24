"""The server log the Logs page reads: recent records, newest sequence, no repeats."""

from __future__ import annotations

import logging
from pathlib import Path

from fastapi.testclient import TestClient

from metrix_api import server_log
from metrix_api.config import load_config
from metrix_api.main import create_app


def record(message: str, level: int = logging.INFO, exc_info=None) -> logging.LogRecord:
    return logging.LogRecord(
        "metrix_api.reachability", level, __file__, 1, message, None, exc_info
    )


class TestBuffer:
    def test_a_reader_gets_only_what_it_has_not_seen(self) -> None:
        buffer = server_log.Buffer()
        for message in ("one", "two", "three"):
            buffer.emit(record(message))
        first = buffer.since(0)
        assert [e["message"] for e in first["entries"]] == ["one", "two", "three"]
        assert first["latest"] == 3

        buffer.emit(record("four"))
        second = buffer.since(first["latest"])
        assert [e["message"] for e in second["entries"]] == ["four"]
        assert buffer.since(second["latest"])["entries"] == []

    def test_the_source_drops_the_package_prefix(self) -> None:
        buffer = server_log.Buffer()
        buffer.emit(record("x", logging.WARNING))
        entry = buffer.since(0)["entries"][0]
        assert entry["source"] == "reachability"
        assert entry["level"] == "warning"
        assert entry["time"].endswith("Z")

    def test_it_is_bounded_and_says_when_a_reader_missed_records(self) -> None:
        buffer = server_log.Buffer(capacity=3)
        for i in range(5):
            buffer.emit(record(str(i)))
        page = buffer.since(0)
        assert [e["message"] for e in page["entries"]] == ["2", "3", "4"]
        assert page["truncated"], "records 1 and 2 were evicted before this reader saw them"
        assert not buffer.since(2)["truncated"]

    def test_an_exception_keeps_its_trace(self) -> None:
        buffer = server_log.Buffer()
        try:
            raise ValueError("boom")
        except ValueError:
            import sys

            buffer.emit(record("failed", logging.ERROR, sys.exc_info()))
        entry = buffer.since(0)["entries"][0]
        assert "ValueError: boom" in entry["trace"]


def test_the_route_serves_what_the_package_logs(tmp_path: Path) -> None:
    client = TestClient(create_app(load_config(tmp_path).ensure_layout()))
    before = client.get("/api/logs").json()["latest"]
    logging.getLogger("metrix_api.discovery.ecs").info("discovery start hostname=%r", "x")
    logging.getLogger("botocore").warning("not ours")
    page = client.get("/api/logs", params={"after": before}).json()
    assert [e["message"] for e in page["entries"]] == ["discovery start hostname='x'"]
    assert page["entries"][0]["source"] == "discovery.ecs"


def test_an_unhandled_error_is_logged_with_its_trace(tmp_path: Path, monkeypatch) -> None:
    """A 500 whose traceback went only to uvicorn's console is a 500 nobody can read."""
    from metrix_api import profiles

    def broken(config):
        raise RuntimeError("the profile directory exploded")

    monkeypatch.setattr(profiles, "list_profiles", broken)
    client = TestClient(
        create_app(load_config(tmp_path).ensure_layout()), raise_server_exceptions=False
    )
    before = client.get("/api/logs").json()["latest"]
    response = client.get("/api/profiles")
    assert response.status_code == 500
    assert "the profile directory exploded" in response.json()["detail"]

    [entry] = client.get("/api/logs", params={"after": before}).json()["entries"]
    assert entry["level"] == "error"
    assert entry["message"] == "unhandled error on GET /api/profiles"
    assert "RuntimeError: the profile directory exploded" in entry["trace"]
