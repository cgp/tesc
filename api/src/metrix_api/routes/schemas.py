"""Stored source schemas: upload, inspect and remove."""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, HTTPException, Request, Response
from pydantic import BaseModel, Field

from metrix_api import schema_library
from metrix_api.config import Config

router = APIRouter(prefix="/api/schemas", tags=["schemas"])


def _config(request: Request) -> Config:
    return request.app.state.config


class Upload(BaseModel):
    filename: str = Field(description="The uploaded file name, without a directory")
    source: str = Field(description="openapi, swagger, wadl, wsdl, har, access_log or routes")
    content: str = Field(description="The UTF-8 source document")


@router.get("")
def list_schemas(request: Request) -> dict[str, Any]:
    try:
        entries = schema_library.list_schemas(_config(request))
    except schema_library.SchemaLibraryError as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc
    return {"schemas": [entry.summary() for entry in entries]}


@router.post("", status_code=201)
def upload_schema(request: Request, body: Upload) -> dict[str, Any]:
    try:
        entry = schema_library.store(_config(request), body.filename, body.source, body.content)
    except schema_library.SchemaExistsError as exc:
        raise HTTPException(status_code=409, detail=str(exc)) from exc
    except schema_library.SchemaLibraryError as exc:
        raise HTTPException(status_code=422, detail=str(exc)) from exc
    return entry.document()


@router.get("/{entry_id}")
def get_schema(entry_id: str, request: Request) -> dict[str, Any]:
    try:
        return schema_library.load(_config(request), entry_id).document()
    except FileNotFoundError as exc:
        raise HTTPException(status_code=404, detail=f"no schema {entry_id!r}") from exc
    except schema_library.SchemaLibraryError as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc


@router.delete("/{entry_id}", status_code=204)
def delete_schema(entry_id: str, request: Request) -> Response:
    try:
        schema_library.delete(_config(request), entry_id)
    except FileNotFoundError as exc:
        raise HTTPException(status_code=404, detail=f"no schema {entry_id!r}") from exc
    return Response(status_code=204)
