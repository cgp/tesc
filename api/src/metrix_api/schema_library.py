"""Uploaded service descriptions, parsed once and kept under ``METRIX_HOME``.

The library owns storage, not interpretation.  Every upload goes through the plan
generator's source dispatch before this module writes anything, so Plans and Schemas
cannot disagree about what OpenAPI 3, Swagger 2, WSDL, HAR, an access log or a route
list means.
"""

from __future__ import annotations

import json
import re
import shutil
import tempfile
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from metrix_api import generate
from metrix_api.config import Config

METADATA = "metadata.json"
SOURCE = "source.txt"
VERSION = 1
KINDS = ("openapi", "swagger", "wadl", "wsdl", "har", "access_log", "routes")
ID = re.compile(r"^[a-z0-9][a-z0-9._-]*$")


class SchemaLibraryError(Exception):
    """A library entry could not be named, stored or read."""


class SchemaExistsError(SchemaLibraryError):
    """An upload resolves to an id already in the library."""


@dataclass(frozen=True, slots=True)
class StoredSchema:
    id: str
    filename: str
    source: str
    uploaded_at: str
    calls: list[dict[str, str]]
    content: str | None = None

    def summary(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "filename": self.filename,
            "source": self.source,
            "uploaded_at": self.uploaded_at,
            "call_count": len(self.calls),
        }

    def document(self) -> dict[str, Any]:
        return {**self.summary(), "calls": self.calls, "content": self.content}


def schema_id(filename: str, source: str) -> str:
    """A stable, readable id from the uploaded name and selected parser."""
    clean = _filename(filename)
    if source not in KINDS:
        raise SchemaLibraryError(f"unknown source {source!r}; expected one of {', '.join(KINDS)}")
    return f"{generate.slug(Path(clean).stem)}-{source}"


def store(config: Config, filename: str, source: str, content: str) -> StoredSchema:
    """Parse, then atomically add one source document to the library."""
    entry_id = schema_id(filename, source)
    try:
        draft = generate.generate(source, content, name=generate.slug(Path(filename).stem))
    except generate.GenerationError as exc:
        raise SchemaLibraryError(str(exc)) from exc

    calls = [
        {
            "name": name,
            "method": str(call.get("method", "")),
            "path": str(call.get("path", "")),
            **({"description": str(call["description"])} if call.get("description") else {}),
        }
        for name, call in draft.calls.items()
    ]
    uploaded_at = datetime.now(UTC).isoformat()
    metadata = {
        "version": VERSION,
        "id": entry_id,
        "filename": filename,
        "source": source,
        "uploaded_at": uploaded_at,
        "calls": calls,
    }

    root = config.schemas_dir
    target = root / entry_id
    if target.exists():
        raise SchemaExistsError(f"a schema with id {entry_id!r} already exists")

    temporary = Path(tempfile.mkdtemp(prefix=".upload-", dir=root))
    try:
        (temporary / SOURCE).write_text(content, encoding="utf-8")
        (temporary / METADATA).write_text(
            json.dumps(metadata, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        try:
            temporary.rename(target)
        except FileExistsError as exc:
            raise SchemaExistsError(f"a schema with id {entry_id!r} already exists") from exc
    finally:
        if temporary.exists():
            shutil.rmtree(temporary)

    return StoredSchema(entry_id, filename, source, uploaded_at, calls, content)


def list_schemas(config: Config) -> list[StoredSchema]:
    entries = [
        _load(path, include_content=False)
        for path in config.schemas_dir.iterdir()
        if path.is_dir() and not path.name.startswith(".") and (path / METADATA).is_file()
    ]
    return sorted(entries, key=lambda entry: entry.id)


def load(config: Config, entry_id: str) -> StoredSchema:
    _validate_id(entry_id)
    root = config.schemas_dir / entry_id
    if not root.is_dir():
        raise FileNotFoundError(entry_id)
    return _load(root, include_content=True)


def delete(config: Config, entry_id: str) -> None:
    _validate_id(entry_id)
    root = config.schemas_dir / entry_id
    if not root.is_dir():
        raise FileNotFoundError(entry_id)
    shutil.rmtree(root)


def _load(root: Path, *, include_content: bool) -> StoredSchema:
    try:
        raw = json.loads((root / METADATA).read_text(encoding="utf-8"))
        if raw.get("version") != VERSION or raw.get("id") != root.name:
            raise ValueError("metadata identity or version does not match its directory")
        calls = raw["calls"]
        if not isinstance(calls, list):
            raise ValueError("calls must be a list")
        content = (root / SOURCE).read_text(encoding="utf-8") if include_content else None
        return StoredSchema(
            id=root.name,
            filename=str(raw["filename"]),
            source=str(raw["source"]),
            uploaded_at=str(raw["uploaded_at"]),
            calls=calls,
            content=content,
        )
    except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as exc:
        raise SchemaLibraryError(f"schema {root.name!r}: {exc}") from exc


def _filename(filename: str) -> str:
    name = filename.strip()
    if not name or name in {".", ".."} or Path(name).name != name or "/" in name or "\\" in name:
        raise SchemaLibraryError("filename must be one file name, without a directory")
    return name


def _validate_id(entry_id: str) -> None:
    if not ID.fullmatch(entry_id):
        raise FileNotFoundError(entry_id)
