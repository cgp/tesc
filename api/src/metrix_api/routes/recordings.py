"""The archive: what was recorded, and what it saw."""

from __future__ import annotations

import json
import sqlite3
from typing import Any

from fastapi import APIRouter, Depends, HTTPException, Query

from metrix_api.deps import get_db
from metrix_api.store import recordings as store

router = APIRouter(prefix="/api/recordings", tags=["recordings"])


def _row(recording: store.RecordingRow) -> dict[str, Any]:
    return {
        "id": recording.id,
        "kind": recording.kind,
        "status": recording.status,
        "profile": recording.profile,
        "addressing_mode": recording.addressing_mode,
        "series_key": recording.series_key,
        "started_at": recording.started_at,
        "finished_at": recording.finished_at,
        "duration_ms": recording.duration_ms,
        "is_baseline": recording.is_baseline,
        "note": recording.note,
        "targets": recording.targets,
    }


@router.get("")
def list_recordings(
    kind: str | None = None,
    profile: str | None = None,
    series: str | None = None,
    limit: int = Query(default=100, ge=1, le=1000),
    conn: sqlite3.Connection = Depends(get_db),
) -> dict[str, Any]:
    rows = store.list_recordings(conn, kind=kind, profile=profile, series=series, limit=limit)
    return {"recordings": [_row(r) for r in rows]}


@router.get("/{recording_id}")
def get_recording(
    recording_id: str, conn: sqlite3.Connection = Depends(get_db)
) -> dict[str, Any]:
    try:
        recording = store.get(conn, recording_id)
    except LookupError as exc:
        raise HTTPException(status_code=404, detail=str(exc)) from exc

    metrics = [
        r["metric"]
        for r in conn.execute(
            "SELECT DISTINCT metric FROM host_sample WHERE recording_id = ? ORDER BY metric",
            (recording_id,),
        )
    ]
    return {
        **_row(recording),
        "metrics": metrics,
        "phases": [
            {"target_id": p["target_id"], "phase": p["phase"],
             "from_ms": p["from_ms"], "to_ms": p["to_ms"]}
            for p in store.phases(conn, recording_id)
        ],
        # Gaps travel with the recording so a chart can draw holes rather than lines.
        "gaps": [
            {"target_id": g["target_id"], "from_ms": g["from_ms"],
             "to_ms": g["to_ms"], "reason": g["reason"]}
            for g in store.gaps(conn, recording_id)
        ],
        "annotations": [
            {"code": a["code"], "severity": a["severity"], "target_id": a["target_id"],
             "from_ms": a["from_ms"], "to_ms": a["to_ms"], "message": a["message"],
             "detail": json.loads(a["detail"]) if a["detail"] else None}
            for a in store.annotations(conn, recording_id)
        ],
        # What each box was, and what the run did to its disks. Neither is a series:
        # identity does not change, and filesystem usage is read once at each end.
        "identity": store.identities(conn, recording_id),
        "filesystems": store.filesystem_usage(conn, recording_id),
    }


@router.get("/{recording_id}/series")
def get_series(
    recording_id: str,
    metric: str,
    target: str | None = None,
    conn: sqlite3.Connection = Depends(get_db),
) -> dict[str, Any]:
    """One metric over time, per target: the shape a chart consumes."""
    rows = store.samples(conn, recording_id, target_id=target, metric=metric)
    by_target: dict[str, list[list[float]]] = {}
    for target_id, t_ms, _, value in rows:
        by_target.setdefault(target_id, []).append([t_ms, value])
    return {"metric": metric, "series": by_target}
