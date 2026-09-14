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
from dataclasses import dataclass, field, replace
from typing import Any

from metrix_api.stats import (
    Delta,
    Recovery,
    Summary,
    compare,
    merge,
    recovery,
    summarize,
)
from metrix_api.stats import sweep as sweep_stats
from metrix_api.stats.trend import BAND_WINDOW, Run, Trend, Verdict, trend, verdict_from
from metrix_api.store import recordings as store

#: The two windows a recovery measurement needs. Named here because the pairing is
#: the meaning: "did it come back" is a question about a *later* window relative to
#: an *earlier* one, and either alone answers nothing.
BASELINE_PHASE = "baseline"
SETTLE_PHASE = "settle"

#: The window a sweep compares over unless asked for another. An observation-only
#: recording records its single window under this name too, so the sweep view reads a
#: watched environment and a load run the same way.
MEASURE_PHASE = "measure"

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
#: How many runs one comparison may hold. Not a storage limit -- everything is kept
#: (design-api 17.1) -- but an overlay of more lines than there are distinguishable
#: colours stops being readable, and the answer to that is fewer runs rather than
#: more hues. The same bargain the chart palette already makes across targets.
MAX_COMPARED = 6

#: What the identity tuple is made of, in words a person can read off a row. Used to
#: say *which part* differs when two runs are not comparable, because "different
#: setup" without naming the difference is the same as no answer.
IDENTITY = {
    "kind": lambda r: r.kind,
    "profile": lambda r: r.profile,
    "addressing": lambda r: r.addressing_mode,
    "api version": lambda r: r.api_version,
}


@dataclass(frozen=True, slots=True)
class RunComparison:
    """N runs side by side, and -- where it means anything -- merged into one."""

    runs: list[store.RecordingRow] = field(default_factory=list)
    phase: str | None = None
    #: Per metric, per run. A run missing a metric is absent rather than zero.
    per_run: dict[str, dict[str, Summary]] = field(default_factory=dict)
    #: Per metric, every run's readings in one distribution. Empty when the runs do
    #: not share a setup -- see `mergeable`.
    merged: dict[str, Summary] = field(default_factory=dict)
    #: Each run against the reference, per metric. The reference is the oldest run,
    #: because a comparison reads as "what changed since", and what changed since is
    #: measured from the earlier thing.
    deltas: dict[str, dict[str, Delta]] = field(default_factory=dict)
    reference_id: str | None = None
    #: Which parts of the identity tuple are not shared: name -> the values seen.
    differences: dict[str, list[str]] = field(default_factory=dict)

    @property
    def metrics(self) -> list[str]:
        return sorted(self.per_run)

    @property
    def mergeable(self) -> bool:
        """Whether pooling these runs describes anything real.

        Runs of one setup are repeats of one measurement and merge into a better
        version of it. Runs of *different* setups are measurements of different
        things, and their pooled distribution describes nothing that exists -- it is
        the average of an apple and a Tuesday, with a sample count that makes it look
        authoritative. Side by side is still a legitimate way to read them
        (design-api 17.2 says opening two setups deliberately is the only way to
        compare across a change), so the overlay and the columns stay; only the
        merged column is withheld.
        """
        return not self.differences and len(self.runs) > 1


def compare_runs(
    conn: sqlite3.Connection,
    recording_ids: list[str],
    *,
    phase: str | None = None,
) -> RunComparison:
    """Read N runs as one table: each on its own, merged, and against the first.

    `phase` restricts every run to that phase of its own timeline, which is what
    design-api 17.5 asks for: baseline against baseline is environment drift, measure
    against measure is the actual question, and settle against settle says whether
    recovery is degrading. Without it the whole recording is the window.
    """
    runs = []
    for recording_id in recording_ids[:MAX_COMPARED]:
        try:
            runs.append(store.get(conn, recording_id))
        except LookupError:
            continue
    if not runs:
        return RunComparison(phase=phase)

    runs.sort(key=lambda r: (r.started_at, r.id))
    ids = [r.id for r in runs]
    values = store.pooled_values(conn, ids, phase=phase)

    per_run: dict[str, dict[str, Summary]] = {}
    for run in runs:
        for metric, readings in values.get(run.id, {}).items():
            per_run.setdefault(metric, {})[run.id] = summarize(metric, readings)

    differences = {
        name: sorted({str(read(run)) for run in runs})
        for name, read in IDENTITY.items()
        if len({read(run) for run in runs}) > 1
    }

    reference = runs[0]
    comparison = RunComparison(
        runs=runs,
        phase=phase,
        per_run=per_run,
        deltas={
            metric: {
                run.id: compare(by_run[reference.id], by_run[run.id])
                for run in runs[1:]
                if run.id in by_run and reference.id in by_run
            }
            for metric, by_run in per_run.items()
        },
        reference_id=reference.id,
        differences=differences,
    )
    if not comparison.mergeable:
        return comparison

    return replace(
        comparison,
        merged={
            metric: merge(metric, [values.get(i, {}).get(metric, []) for i in ids])
            for metric in per_run
        },
    )


def shared_phases(conn: sqlite3.Connection, recording_ids: list[str]) -> list[str]:
    """Phases every one of these runs recorded, in the order they happen.

    Only the shared ones: offering a phase that half the runs do not have would make
    a comparison whose columns are missing for no stated reason. An observation-only
    run has one phase, so a group containing one offers only that.
    """
    order = ["baseline", "warmup", "measure", "drain", "settle"]
    seen: list[set[str]] = []
    for recording_id in recording_ids:
        seen.append({row["phase"] for row in store.phases(conn, recording_id)})
    if not seen:
        return []
    shared = set.intersection(*seen)
    return [phase for phase in order if phase in shared] + sorted(shared - set(order))


# ------------------------------------------------------------------- sweeps (17.6)

#: How many earlier sweeps a flagged box is read back across. The same window the
#: trend band uses: long enough to tell "always this one" from "unlucky once",
#: short enough that a container replaced six weeks ago is not still being quoted.
SWEEP_HISTORY = BAND_WINDOW


@dataclass(frozen=True, slots=True)
class Appearance:
    """One box's showing in one earlier sweep of the same series."""

    recording_id: str
    at: str
    value: float | None
    n: int
    rank: int
    targets: int
    flagged: bool


@dataclass(frozen=True, slots=True)
class Finding:
    """A box that came out beyond the sweep's own spread, and its record."""

    metric: str
    target_id: str
    standing: sweep_stats.Standing
    #: The same box on the same metric in the sweeps before this one, oldest first.
    #: Empty when the series has no earlier sweep this box appeared in -- which is
    #: the usual case for ephemeral tasks, and is an answer rather than a gap.
    history: list[Appearance] = field(default_factory=list)

    @property
    def previously_flagged(self) -> int:
        return sum(1 for appearance in self.history if appearance.flagged)


@dataclass(frozen=True, slots=True)
class Sweep:
    """Every box in one recording, ranked against the others on every metric."""

    recording_id: str
    phase: str
    phases: list[str] = field(default_factory=list)
    boxes: list[dict[str, Any]] = field(default_factory=list)
    metrics: list[sweep_stats.MetricSweep] = field(default_factory=list)
    findings: list[Finding] = field(default_factory=list)
    #: True once the sweep has enough boxes to describe its own spread. A smaller
    #: sweep is still ranked and still shows its attributes; it just does not accuse.
    judged: bool = False


def _boxes(
    conn: sqlite3.Connection,
    recording_id: str,
    *,
    phase: str,
    targets: list[dict[str, Any]],
) -> tuple[dict[str, dict[str, Summary]], dict[str, dict[str, Summary]]]:
    """Per box: the metrics over the compared phase, and over its own baseline."""
    measured: dict[str, dict[str, Summary]] = {}
    resting: dict[str, dict[str, Summary]] = {}
    windows = {
        (window.target_id, window.phase): window for window in phase_windows(conn, recording_id)
    }
    for target in targets:
        box = target["target_id"]
        found = windows.get((box, phase))
        measured[box] = dict(found.metrics) if found else {}
        at_rest = windows.get((box, BASELINE_PHASE))
        resting[box] = dict(at_rest.metrics) if at_rest else {}
    return measured, resting


def sweep(
    conn: sqlite3.Connection,
    recording_id: str,
    *,
    phase: str | None = None,
    history: int = SWEEP_HISTORY,
) -> Sweep:
    """Rank the boxes of one recording against each other (design-api 17.6).

    A sweep varies the target and holds everything else still, so the reference is
    not history but the sweep's own spread -- §17.4's measured band applied across
    boxes instead of across time. What is being looked for is the odd one out: a task
    on a noisy neighbour, an instance of a different type, a container still running
    an older image digest.

    Each box's baseline phase travels with its measurement, because the alternative
    is a phantom: a container that was already loaded before the run started looks
    exactly like one that buckled under the load, right up until somebody reads the
    two numbers side by side.
    """
    recording = store.get(conn, recording_id)
    targets = store.target_details(conn, recording_id)
    invalid = store.invalid_targets(conn, recording_id)
    available = sorted({row["phase"] for row in store.phases(conn, recording_id)})
    chosen = phase or (MEASURE_PHASE if MEASURE_PHASE in available else None)
    if chosen is None:
        chosen = available[0] if available else MEASURE_PHASE

    measured, resting = _boxes(conn, recording_id, phase=chosen, targets=targets)
    names = sorted({metric for by_metric in measured.values() for metric in by_metric})

    ranked = []
    for metric in names:
        ranked.append(
            sweep_stats.rank(
                metric,
                [
                    sweep_stats.Box(
                        target_id=target["target_id"],
                        attributes=target["attributes"],
                        summary=measured[target["target_id"]].get(metric),
                        baseline=resting[target["target_id"]].get(metric),
                        invalid=target["target_id"] in invalid,
                    )
                    for target in targets
                ],
            )
        )

    findings = [
        Finding(metric=one.metric, target_id=standing.target_id, standing=standing)
        for one in ranked
        for standing in one.odd_ones_out
    ]
    findings = _with_history(conn, recording, findings, phase=chosen, limit=history)

    return Sweep(
        recording_id=recording_id,
        phase=chosen,
        phases=available,
        boxes=targets,
        metrics=ranked,
        findings=sorted(
            findings,
            key=lambda f: (not f.standing.worse, -f.previously_flagged, f.metric),
        ),
        judged=any(one.judged for one in ranked),
    )


def _with_history(
    conn: sqlite3.Connection,
    recording: store.RecordingRow,
    findings: list[Finding],
    *,
    phase: str,
    limit: int,
) -> list[Finding]:
    """Read each flagged box back across the sweeps before this one.

    The question §17.6 ends on: is that container reliably the slow one, or was it
    unlucky once? Asked only of boxes something was actually flagged on -- reading
    every box's whole history to draw a page nobody is looking at is how a list view
    becomes slow exactly as the archive becomes worth having.
    """
    if not findings:
        return findings

    earlier = [
        run
        for run in store.list_recordings(conn, series=recording.series_key, limit=limit + 1)
        if (run.started_at, run.id) < (recording.started_at, recording.id)
    ]
    earlier = sorted(earlier, key=lambda r: (r.started_at, r.id))[-limit:]
    if not earlier:
        return findings

    ids = [run.id for run in earlier]
    with_history = []
    for metric in sorted({finding.metric for finding in findings}):
        readings = store.target_values(conn, ids, metric=metric, phase=phase)
        # Each earlier sweep is ranked again by the same rule, so "flagged before"
        # means what it means now rather than being a second, looser idea of odd.
        ranked = {}
        for run in earlier:
            boxes = readings.get(run.id, {})
            invalid = store.invalid_targets(conn, run.id)
            ranked[run.id] = sweep_stats.rank(
                metric,
                [
                    sweep_stats.Box(
                        target_id=target,
                        summary=summarize(metric, values),
                        invalid=target in invalid,
                    )
                    for target, values in sorted(boxes.items())
                ],
            )
        for finding in findings:
            if finding.metric != metric:
                continue
            appearances = []
            for run in earlier:
                standing = next(
                    (s for s in ranked[run.id].standings if s.target_id == finding.target_id),
                    None,
                )
                if standing is None:
                    continue
                appearances.append(
                    Appearance(
                        recording_id=run.id,
                        at=run.started_at,
                        value=standing.value,
                        n=standing.n,
                        rank=standing.rank,
                        targets=len(ranked[run.id].standings),
                        flagged=standing.flagged,
                    )
                )
            with_history.append(
                Finding(
                    metric=finding.metric,
                    target_id=finding.target_id,
                    standing=finding.standing,
                    history=appearances,
                )
            )
    return with_history
