"""Run series: the same setup over time, and what moved across it.

The primary comparison unit is not "this run against that one" but the series
(design-api 17.2). A two-run diff cannot answer *is this getting better or worse*,
because it has no idea what normal variation looks like; a series measures that from
its own scatter.

The key is a query parameter rather than a path segment. It is a readable tuple with
pipes and an `=` in it, and burying that in a path means every link, every log line
and every bookmark carries a stretch of percent-encoding that hides exactly the thing
the key is readable for.
"""

from __future__ import annotations

import sqlite3
from typing import Any

from fastapi import APIRouter, Depends, HTTPException, Query

from metrix_api import analysis
from metrix_api.deps import get_db
from metrix_api.stats.trend import BAND_WINDOW, MIN_RUNS_FOR_BAND
from metrix_api.store import recordings as store

router = APIRouter(prefix="/api/series", tags=["series"])


def _series(row: store.SeriesRow) -> dict[str, Any]:
    return {
        "key": row.key,
        "kind": row.kind,
        "profile": row.profile,
        "addressing_mode": row.addressing_mode,
        "api_version": row.api_version,
        "runs": row.runs,
        "first_at": row.first_at,
        "last_at": row.last_at,
        "baseline_id": row.baseline_id,
        "invalid_runs": row.invalid_runs,
    }


@router.get("")
def list_series(conn: sqlite3.Connection = Depends(get_db)) -> dict[str, Any]:
    """Every setup that has been recorded against, most recent first.

    `min_runs_for_band` travels with the list so the page can say which series are
    long enough to have a trend worth reading, rather than offering every one of them
    and explaining the short ones only after they are opened.
    """
    return {
        "series": [_series(row) for row in store.series_list(conn)],
        "min_runs_for_band": MIN_RUNS_FOR_BAND,
    }


@router.get("/trend")
def get_trend(
    key: str,
    window: int = Query(default=BAND_WINDOW, ge=2, le=100),
    conn: sqlite3.Connection = Depends(get_db),
) -> dict[str, Any]:
    """One series' whole history: the runs, and every metric across them.

    Everything in one response because the page draws one chart per metric over one
    shared set of runs, and fetching it chart by chart would be a request each for
    data that comes out of a single pass over the series.
    """
    found = [row for row in store.series_list(conn) if row.key == key]
    if not found:
        raise HTTPException(status_code=404, detail=f"no series {key!r}")

    result = analysis.series_trends(conn, key, window=window)
    return {
        "series": _series(found[0]),
        "has_baseline_phase": result.has_baseline_phase,
        "metrics": result.metrics,
        "trends": {metric: t.to_document() for metric, t in result.trends.items()},
        # The runs themselves, in the order the points are drawn in. The chart says
        # which way a metric went; this is what says which run to open next.
        "runs": [
            {
                "id": run.id,
                "started_at": run.started_at,
                "status": run.status,
                "duration_ms": run.duration_ms,
                "is_baseline": run.is_baseline,
                "annotations_by_severity": run.annotations,
                "worst": run.worst,
                "targets": run.targets,
                "note": run.note,
            }
            for run in result.runs
        ],
    }
