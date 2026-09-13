"""N runs side by side, and — where it means anything — merged into one.

design-api 14.4 asks for the stats table with another run's values alongside and a
delta column; 17.5 asks for N of them overlaid and for a run group aggregated into a
single set of statistics. Both are this endpoint.

The aggregate is a **merge, not an average**. The mean of five p95s is not the p95 of
the five windows together and describes nothing; pooling the readings and summarising
once is the only arithmetic that answers a question about the whole. That is the same
property HDR histograms have, which is why 17.5 aggregates a run group by merging
histograms rather than by averaging percentiles — the API side merges raw host
samples today and will merge histograms by the same rule when the engine lands.
"""

from __future__ import annotations

import sqlite3
from typing import Any

from fastapi import APIRouter, Depends, HTTPException, Query

from metrix_api import analysis
from metrix_api.deps import get_db
from metrix_api.store import recordings as store

router = APIRouter(prefix="/api/compare", tags=["compare"])


@router.get("")
def compare(
    run: list[str] = Query(default=[]),
    phase: str | None = None,
    conn: sqlite3.Connection = Depends(get_db),
) -> dict[str, Any]:
    """Compare the named runs, optionally within one phase of each.

    `merged` is present only when the runs share a setup identity. Runs of one setup
    are repeats of one measurement and pool into a better version of it; runs of
    different setups measure different things, and their pooled distribution
    describes nothing that exists while carrying a sample count that makes it look
    authoritative. `differences` names which part of the identity is not shared, so
    the answer is *why*, not just *no* — the columns and the overlay still stand,
    because reading two setups side by side is legitimate and merging them is not.
    """
    if len(run) < 2:
        raise HTTPException(status_code=400, detail="name at least two runs to compare")
    if len(run) > analysis.MAX_COMPARED:
        raise HTTPException(
            status_code=400,
            detail=(
                f"at most {analysis.MAX_COMPARED} runs at once: past that the overlay "
                "stops being readable, and the answer is fewer runs rather than more "
                "colours"
            ),
        )

    missing = [r for r in run if not _exists(conn, r)]
    if missing:
        raise HTTPException(status_code=404, detail=f"no recording {missing[0]!r}")

    result = analysis.compare_runs(conn, run, phase=phase)
    return {
        "phase": result.phase,
        # Offered rather than assumed: only phases every run in the group recorded,
        # because a phase half of them lack makes columns that are empty for no
        # stated reason.
        "phases": analysis.shared_phases(conn, [r.id for r in result.runs]),
        "reference_id": result.reference_id,
        "mergeable": result.mergeable,
        "differences": result.differences,
        "metrics": result.metrics,
        "runs": [
            {
                "id": r.id,
                "started_at": r.started_at,
                "profile": r.profile,
                "kind": r.kind,
                "addressing_mode": r.addressing_mode,
                "series_key": r.series_key,
                "status": r.status,
                "duration_ms": r.duration_ms,
                "is_baseline": r.is_baseline,
                "annotations_by_severity": r.annotations,
                "worst": r.worst,
                "targets": r.targets,
            }
            for r in result.runs
        ],
        "rows": {
            metric: {
                "per_run": {run_id: s.to_document() for run_id, s in by_run.items()},
                "merged": (
                    result.merged[metric].to_document() if metric in result.merged else None
                ),
                "deltas": {
                    run_id: _delta(d) for run_id, d in result.deltas.get(metric, {}).items()
                },
            }
            for metric, by_run in result.per_run.items()
        },
    }


def _exists(conn: sqlite3.Connection, recording_id: str) -> bool:
    try:
        store.get(conn, recording_id)
    except LookupError:
        return False
    return True


def _delta(delta: Any) -> dict[str, Any]:
    """Every delta carries both windows, so the count behind each side travels too.

    design-api 14.4: a table is exactly where a spurious "p99 +18%" gets quoted
    without its caveat, and the caveat is the sample count.
    """
    return {
        "metric": delta.metric,
        "change": delta.change,
        "change_pct": delta.change_pct,
        "band": delta.band,
        "outside_band": delta.outside_band,
        "worse": delta.worse,
        "comparable": delta.comparable,
    }
