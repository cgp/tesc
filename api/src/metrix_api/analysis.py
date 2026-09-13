"""Reading a recording back as answers rather than as rows.

Four questions, all of them comparisons, because a host number on its own answers
almost nothing. *68% CPU* means one thing on a box that idles at 60 and another on a
box that idles at 5.

* **per phase** -- what did the load actually change (design-api 15)? An
  observation-only recording has one phase and this is a single column; a phased load
  run will have five, and the shape is the same either way.
* **against the environment baseline** -- is this environment behaving normally
  today, before anything was even asked of it (design-api 10.2)? This is what a saved
  observation-only recording is *for*.
* **recovery** -- did it come back after the traffic stopped, and if not, which way
  did it drift (design-api 9.7)?
* **across the series** -- is this getting better or worse than it was (design-api
  17.3)? The only one of the four that a single recording cannot answer at all.

`stats/` holds the arithmetic and knows nothing about SQLite; this module reads the
store and hands it windows. The sample-count rule lives there, once, so nothing here
can report a number the count will not support.
"""

from __future__ import annotations

import sqlite3
from dataclasses import dataclass, field

from metrix_api.stats import Delta, Recovery, Summary, compare, recovery, summarize
from metrix_api.stats.trend import BAND_WINDOW, Run, Trend, Verdict, trend, verdict_from
from metrix_api.store import recordings as store

#: The two windows a recovery measurement needs. Named here because the pairing is
#: the meaning: "did it come back" is a question about a *later* window relative to
#: an *earlier* one, and either alone answers nothing.
BASELINE_PHASE = "baseline"
SETTLE_PHASE = "settle"

#: Pooling reads across boxes describes "a typical box in this environment", which is
#: the question 10.2 asks. It is the wrong summary when the boxes are not alike --
#: two instance types under one service -- so the per-target view sits beside it
#: rather than being replaced by it.
ENVIRONMENT = "*"


@dataclass(frozen=True, slots=True)
class PhaseWindow:
    """One phase of one target, and what each metric did during it."""

    target_id: str
    phase: str
    from_ms: int
    to_ms: int | None
    metrics: dict[str, Summary] = field(default_factory=dict)


def phase_windows(conn: sqlite3.Connection, recording_id: str) -> list[PhaseWindow]:
    """Every phase of every target, summarised."""
    windows = []
    for row in store.phases(conn, recording_id):
        values = store.window(
            conn,
            recording_id,
            target_id=row["target_id"],
            from_ms=row["from_ms"],
            to_ms=row["to_ms"],
        )
        windows.append(
            PhaseWindow(
                target_id=row["target_id"],
                phase=row["phase"],
                from_ms=row["from_ms"],
                to_ms=row["to_ms"],
                metrics={metric: summarize(metric, vals) for metric, vals in values.items()},
            )
        )
    return windows


def _pooled(conn: sqlite3.Connection, recording_id: str, targets: list[str]) -> dict[str, Summary]:
    """Every target's readings for a metric, in one distribution."""
    gathered: dict[str, list[float]] = {}
    for target in targets:
        for metric, values in store.window(conn, recording_id, target_id=target).items():
            gathered.setdefault(metric, []).extend(values)
    return {metric: summarize(metric, values) for metric, values in gathered.items()}


def summaries(conn: sqlite3.Connection, recording_id: str) -> dict[str, dict[str, Summary]]:
    """Whole-recording summaries, per target plus the pooled environment view."""
    recording = store.get(conn, recording_id)
    per_target = {
        target: {
            metric: summarize(metric, values)
            for metric, values in store.window(conn, recording_id, target_id=target).items()
        }
        for target in recording.targets
    }
    return {**per_target, ENVIRONMENT: _pooled(conn, recording_id, recording.targets)}


@dataclass(frozen=True, slots=True)
class Comparison:
    """One recording measured against the baseline for its series."""

    recording_id: str
    baseline_id: str | None
    #: Keyed by target, plus `ENVIRONMENT` for the pooled view.
    deltas: dict[str, dict[str, Delta]] = field(default_factory=dict)
    #: Targets in one recording and not the other. Not an error: a discovered
    #: environment replaces its tasks on every deployment, so the per-target view is
    #: usually empty and the pooled one carries the answer.
    only_now: list[str] = field(default_factory=list)
    only_baseline: list[str] = field(default_factory=list)

    @property
    def moved(self) -> list[tuple[str, Delta]]:
        """What is worth reading, worst first: (target, delta).

        The pooled view speaks for the environment, and a per-box row is added only
        where that box disagrees with it -- which is the case worth seeing, because
        it means one machine is the odd one out rather than the environment having
        shifted. Listing every box alongside the pooled row would print the same
        finding once per machine and bury the one that differs.
        """
        pooled = self.deltas.get(ENVIRONMENT, {})
        rows = [(ENVIRONMENT, d) for d in pooled.values() if d.outside_band]
        for target, deltas in self.deltas.items():
            if target == ENVIRONMENT:
                continue
            for metric, delta in deltas.items():
                agreed = metric in pooled and pooled[metric].outside_band == delta.outside_band
                # A box earns a row by disagreeing with the pooled verdict -- in
                # either direction, since "everything moved except this one" is as
                # much a finding as the reverse. A metric the pooled view does not
                # carry at all is reported only when it moved.
                if not agreed and (delta.outside_band or metric in pooled):
                    rows.append((target, delta))
        return sorted(rows, key=lambda row: (not row[1].worse, -abs(row[1].change_pct or 0.0)))


def against_baseline(conn: sqlite3.Connection, recording_id: str) -> Comparison:
    """Compare a recording with the baseline recording for its series.

    Same series means same profile, addressing and collection interval (design-api
    17.2) -- the things that have to match for two recordings to be comparable at
    all. Nothing is compared across a change in any of them.
    """
    recording = store.get(conn, recording_id)
    baseline = store.baseline_for(conn, recording.series_key)
    if baseline is None or baseline.id == recording_id:
        return Comparison(recording_id=recording_id, baseline_id=None)

    now = summaries(conn, recording_id)
    was = summaries(conn, baseline.id)
    shared = [t for t in now if t in was]

    return Comparison(
        recording_id=recording_id,
        baseline_id=baseline.id,
        deltas={
            target: {
                metric: compare(was[target][metric], summary)
                for metric, summary in now[target].items()
                if metric in was[target]
            }
            for target in shared
        },
        only_now=sorted(t for t in now if t not in was and t != ENVIRONMENT),
        only_baseline=sorted(t for t in was if t not in now and t != ENVIRONMENT),
    )


def recoveries(conn: sqlite3.Connection, recording_id: str) -> dict[str, dict[str, Recovery]]:
    """Per target, what each metric did after the traffic stopped.

    Empty for a recording with no settle phase, which is every observation-only
    recording: baseline and settle collapse into one window there (design-api 10.2),
    and there is no "after" to measure.
    """
    bounds = {
        (row["target_id"], row["phase"]): (row["from_ms"], row["to_ms"])
        for row in store.phases(conn, recording_id)
    }
    targets = {target for target, _ in bounds}

    found: dict[str, dict[str, Recovery]] = {}
    for target in sorted(targets):
        if (base := bounds.get((target, BASELINE_PHASE))) is None:
            continue
        if (settle := bounds.get((target, SETTLE_PHASE))) is None:
            continue

        baselines = store.window(
            conn, recording_id, target_id=target, from_ms=base[0], to_ms=base[1]
        )
        after = store.window_series(
            conn, recording_id, target_id=target, from_ms=settle[0], to_ms=settle[1]
        )
        measured = {}
        for metric, points in after.items():
            if metric not in baselines:
                continue
            result = recovery(metric, summarize(metric, baselines[metric]), points)
            if result is not None:
                measured[metric] = result
        if measured:
            found[target] = measured
    return found


def leaks(conn: sqlite3.Connection, recording_id: str) -> list[tuple[str, Recovery]]:
    """Metrics that never returned, and drifted the bad way. The leak signal."""
    return [
        (target, r)
        for target, by_metric in recoveries(conn, recording_id).items()
        for r in by_metric.values()
        if r.leaked
    ]


#: How many runs of one series a trend reads. Everything is kept and nothing rolls
#: up (design-api 17.1), so this is a cap on one request rather than on the history:
#: a series with two hundred runs draws its most recent hundred, and the older ones
#: are still there under their own ids.
TREND_RUNS = 100


@dataclass(frozen=True, slots=True)
class SeriesTrends:
    """One series' history: the runs in it, and what each metric did across them."""

    key: str
    runs: list[store.RecordingRow] = field(default_factory=list)
    trends: dict[str, Trend] = field(default_factory=dict)
    #: True when any run in the series recorded a distinct baseline phase, which is
    #: what the environment-drift line is drawn from. False for a series of
    #: observation-only recordings, where baseline and settle collapse into one
    #: window and there is no separate "before anything happened" to trend.
    has_baseline_phase: bool = False

    @property
    def metrics(self) -> list[str]:
        return sorted(self.trends)

    def verdict(self, recording_id: str | None = None) -> Verdict:
        """How one run of this series -- the latest by default -- stands against it."""
        return verdict_from(self.key, self.trends, recording_id=recording_id)


def series_trends(
    conn: sqlite3.Connection,
    key: str,
    *,
    window: int = BAND_WINDOW,
    limit: int = TREND_RUNS,
) -> SeriesTrends:
    """Every metric in one series, oldest run first, with its measured band.

    A run contributes one point per metric: the pooled median across its boxes. Not
    per box, because a discovered environment replaces its tasks on every deployment
    -- a per-box trend would start a fresh line each release and answer nothing
    (design-api 10.2).

    Runs are read in time order and never filtered by status. A run that was aborted
    measured a real, shorter window; its median is a real median and its sample count
    travels with it, so dropping it would hide a fortnight of short runs rather than
    explain them. What *is* held out of the band is any run carrying an `invalid`
    note, because those are the numbers already known not to be trusted.
    """
    runs = sorted(
        store.list_recordings(conn, series=key, limit=limit),
        key=lambda r: (r.started_at, r.id),
    )
    if not runs:
        return SeriesTrends(key=key)

    ids = [r.id for r in runs]
    whole = store.pooled_values(conn, ids)
    at_rest = store.pooled_values(conn, ids, phase=BASELINE_PHASE)

    metrics = sorted({metric for by_metric in whole.values() for metric in by_metric})
    trends = {}
    for metric in metrics:
        trends[metric] = trend(
            metric,
            [
                Run(
                    recording_id=run.id,
                    at=run.started_at,
                    summary=summarize(metric, whole.get(run.id, {}).get(metric, [])),
                    baseline=(
                        summarize(metric, resting)
                        if (resting := at_rest.get(run.id, {}).get(metric))
                        else None
                    ),
                    invalid=bool(run.annotations.get("invalid")),
                    status=run.status,
                )
                for run in runs
            ],
            window=window,
        )

    return SeriesTrends(
        key=key,
        runs=runs,
        trends=trends,
        has_baseline_phase=bool(at_rest),
    )
def series_verdict(
    conn: sqlite3.Connection,
    key: str,
    *,
    recording_id: str | None = None,
    window: int = BAND_WINDOW,
) -> Verdict:
    """The machine-readable answer for one run of one series (design-api 17.4).

    The historical check a pipeline can gate on, and usually the more useful of the
    two it has -- a fixed SLO threshold is a guess made before the data existed, and
    this one is measured from what the setup actually does.
    """
    return series_trends(conn, key, window=window).verdict(recording_id)
