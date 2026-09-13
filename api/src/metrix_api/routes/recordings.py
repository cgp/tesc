"""The archive: what was recorded, and what it saw."""

from __future__ import annotations

import json
import sqlite3
from typing import Any

from fastapi import APIRouter, Depends, HTTPException, Query

from metrix_api import analysis
from metrix_api.deps import get_db
from metrix_api.stats import Delta, Recovery, Summary
from metrix_api.store import inventories
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
        # Exactly what this ran against, pinned at the time. Present only for a
        # profile that discovers; a written-down endpoint list is already in the file.
        "inventory": _inventory(conn, recording_id),
    }


def _inventory(conn: sqlite3.Connection, recording_id: str) -> dict[str, Any] | None:
    """The snapshot this recording was pinned to.

    Read from the pin rather than resolved again: the point of storing it is that the
    tasks it names have very likely been replaced since, and the answer to "what did
    this measure" must not change when the environment does.
    """
    stored = inventories.for_recording(conn, recording_id)
    if stored is None:
        return None
    return {
        "id": stored.id,
        "source": stored.inventory.source,
        "reached": stored.inventory.reached,
        "discovered_at": stored.discovered_at,
        "confirmed_at": stored.confirmed_at,
        **{k: v for k, v in stored.inventory.to_document().items() if k in ("resources", "notes")},
    }


def _summary(summary: Summary) -> dict[str, Any]:
    """Every field carries `n`, because that is the rule this is here to enforce."""
    return summary.to_document()


def _delta(delta: Delta) -> dict[str, Any]:
    return {
        "metric": delta.metric,
        "baseline": _summary(delta.baseline),
        "current": _summary(delta.current),
        "change": delta.change,
        "change_pct": delta.change_pct,
        "band": delta.band,
        "outside_band": delta.outside_band,
        "worse": delta.worse,
        "comparable": delta.comparable,
    }


def _recovery(result: Recovery) -> dict[str, Any]:
    return {
        "metric": result.metric,
        "recovered_ms": result.recovered_ms,
        "returned": result.returned,
        "leaked": result.leaked,
        "peak": result.peak,
        "peak_at_ms": result.peak_at_ms,
        "final": result.final,
        "baseline": result.baseline,
        "band": result.band,
        "n": result.n,
    }


@router.get("/{recording_id}/summary")
def get_summary(
    recording_id: str, conn: sqlite3.Connection = Depends(get_db)
) -> dict[str, Any]:
    """What each metric did, per target and per phase.

    `*` among the targets is the pooled view: every box's readings in one
    distribution, which is what "how does this environment behave" asks. It is the
    wrong summary when the boxes are not alike, which is why the per-target rows sit
    beside it rather than being replaced by it.
    """
    try:
        store.get(conn, recording_id)
    except LookupError as exc:
        raise HTTPException(status_code=404, detail=str(exc)) from exc

    return {
        "recording_id": recording_id,
        "targets": {
            target: {metric: _summary(s) for metric, s in metrics.items()}
            for target, metrics in analysis.summaries(conn, recording_id).items()
        },
        # Per target rather than per metric: every metric on one box is collected in
        # the same sample, so they share a span, and a column of identical values
        # repeated once per row is noise.
        "spans": store.spans(conn, recording_id),
        "phases": [
            {
                "target_id": w.target_id,
                "phase": w.phase,
                "from_ms": w.from_ms,
                "to_ms": w.to_ms,
                "metrics": {metric: _summary(s) for metric, s in w.metrics.items()},
            }
            for w in analysis.phase_windows(conn, recording_id)
        ],
    }


@router.get("/{recording_id}/comparison")
def get_comparison(
    recording_id: str, conn: sqlite3.Connection = Depends(get_db)
) -> dict[str, Any]:
    """This recording against the baseline for its series.

    Is this environment behaving normally today? Only recordings of the same setup
    are compared -- same profile, addressing and interval -- because those are what
    have to match for two of them to mean the same thing.
    """
    try:
        store.get(conn, recording_id)
    except LookupError as exc:
        raise HTTPException(status_code=404, detail=str(exc)) from exc

    comparison = analysis.against_baseline(conn, recording_id)
    return {
        "recording_id": comparison.recording_id,
        "baseline_id": comparison.baseline_id,
        "targets": {
            target: {metric: _delta(d) for metric, d in deltas.items()}
            for target, deltas in comparison.deltas.items()
        },
        "moved": [{"target": target, **_delta(d)} for target, d in comparison.moved],
        "only_now": comparison.only_now,
        "only_baseline": comparison.only_baseline,
    }


@router.get("/{recording_id}/recovery")
def get_recovery(
    recording_id: str, conn: sqlite3.Connection = Depends(get_db)
) -> dict[str, Any]:
    """What happened after the traffic stopped. Empty without a settle phase."""
    found = analysis.recoveries(conn, recording_id)
    return {
        "recording_id": recording_id,
        "targets": {
            target: {metric: _recovery(r) for metric, r in by_metric.items()}
            for target, by_metric in found.items()
        },
    }


@router.post("/{recording_id}/baseline")
def set_baseline(
    recording_id: str,
    override: bool = False,
    conn: sqlite3.Connection = Depends(get_db),
) -> dict[str, Any]:
    """Make this the baseline its series is compared against.

    Refused for a recording carrying an `invalid` annotation unless overridden
    deliberately: a baseline is what everything later is measured against, so one
    taken while a target was unreachable would quietly poison every comparison
    instead of failing one.
    """
    try:
        row = store.mark_baseline(conn, recording_id, override=override)
    except LookupError as exc:
        raise HTTPException(status_code=404, detail=str(exc)) from exc
    except store.BaselineRefused as exc:
        # 409: the request is well formed and the recording is real; it is the state
        # of that recording that says no. Retrying with `override` is the way past.
        raise HTTPException(status_code=409, detail=str(exc)) from exc
    return _row(row)


@router.delete("/{recording_id}/baseline")
def unset_baseline(
    recording_id: str, conn: sqlite3.Connection = Depends(get_db)
) -> dict[str, Any]:
    try:
        return _row(store.clear_baseline(conn, recording_id))
    except LookupError as exc:
        raise HTTPException(status_code=404, detail=str(exc)) from exc


@router.get("/{recording_id}/series")
def get_series(
    recording_id: str,
    metric: str | None = None,
    target: str | None = None,
    conn: sqlite3.Connection = Depends(get_db),
) -> dict[str, Any]:
    """Metrics over time, per target: the shape a chart consumes.

    Every metric at once unless one is named. The charts page draws one chart per
    metric on a shared axis, and fetching them one at a time would be a request per
    chart for data that comes out of a single table scan.
    """
    rows = store.samples(conn, recording_id, target_id=target, metric=metric)
    series: dict[str, dict[str, list[list[float]]]] = {}
    for target_id, t_ms, name, value in rows:
        series.setdefault(name, {}).setdefault(target_id, []).append([t_ms, value])

    return {
        "recording_id": recording_id,
        "metrics": sorted(series),
        "series": series,
        # What a chart needs behind the lines: the bands to shade, the holes to leave,
        # and the line to draw across for "normal". All of it is per recording, so
        # sending it here saves the page three more round trips.
        "phases": [
            {
                "target_id": p["target_id"],
                "phase": p["phase"],
                "from_ms": p["from_ms"],
                "to_ms": p["to_ms"],
            }
            for p in store.phases(conn, recording_id)
        ],
        "gaps": [
            {
                "target_id": g["target_id"],
                "from_ms": g["from_ms"],
                "to_ms": g["to_ms"],
                "reason": g["reason"],
            }
            for g in store.gaps(conn, recording_id)
        ],
        "annotations": [
            {
                "code": a["code"],
                "severity": a["severity"],
                "target_id": a["target_id"],
                "from_ms": a["from_ms"],
                "to_ms": a["to_ms"],
                "message": a["message"],
            }
            for a in store.annotations(conn, recording_id)
        ],
        # The reference line: what this environment normally sits at, per metric.
        "baseline": _baseline_medians(conn, recording_id),
    }


def _baseline_medians(conn: sqlite3.Connection, recording_id: str) -> dict[str, float]:
    """The pooled median of each metric in this series' baseline recording.

    Pooled across boxes rather than per box, because it is drawn as one horizontal
    line across a chart that overlays every target -- and because a discovered
    environment's boxes are replaced between recordings anyway (design-api 10.2).
    """
    try:
        recording = store.get(conn, recording_id)
    except LookupError:
        return {}
    baseline = store.baseline_for(conn, recording.series_key)
    if baseline is None or baseline.id == recording_id:
        return {}
    pooled = analysis.summaries(conn, baseline.id).get(analysis.ENVIRONMENT, {})
    return {metric: s.p50 for metric, s in pooled.items() if s.p50 is not None}
