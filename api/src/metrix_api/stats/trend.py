"""Trends across a run series: is this getting better or worse than it was?

A single run answers almost nothing on its own, and a two-run diff answers almost
nothing either -- it has no idea what normal variation looks like. The series does
(design-api 17.2): every run of the same setup, in time order, with a band drawn from
the scatter those runs actually show.

Two rules shape everything here.

**The band is measured from the runs before each point, never from a window
containing it.** A window that includes the point it is judging widens to swallow
exactly the movement it exists to detect: a jump twice the size of normal noise drags
the median and the IQR up with it and lands comfortably inside its own band. Trailing
means the band states what was normal *before* this run -- which is the claim anyone
reading a trend is making anyway.

**A band the run count cannot support is not drawn.** The same rule as the rest of
`stats/`, counting runs rather than samples. An IQR over three runs is the gap between
two of them, and a band that narrow flags the fourth run for being a Tuesday, which is
how a flag gets trained into noise (design-api 17.4).

What this module deliberately does not do is decide that a point *is* a regression.
`outside` is geometry -- the point sits beyond the band -- and that is one of the
three conditions design-api 17.4 requires. The verdict is assembled elsewhere.
"""

from __future__ import annotations

from collections.abc import Iterable, Sequence
from dataclasses import dataclass, field
from statistics import median

from metrix_api.stats.summary import (
    NOISE_MULTIPLE,
    Summary,
    quantile,
    worse_direction,
)

#: How many previous runs the band is measured over. Long enough for an IQR to
#: describe something, short enough that a setup which drifted six months ago is not
#: still setting today's idea of normal.
BAND_WINDOW = 10

#: Fewer previous runs than this and there is no band at all. Withholding it is the
#: honest answer to a short history, and it fails safe: no band means no point is
#: ever outside one.
MIN_RUNS_FOR_BAND = 5

#: The narrowest the band may get, as a fraction of what normal is. Wider than the
#: 2% `compare()` uses within a window, and deliberately so: that floor describes how
#: far a *sample* strays from its own median over one recording, and this one has to
#: cover how far one run's median strays from the last one's. Two runs of the same
#: setup on the same hardware differ by more than two percent as a matter of course --
#: a different hour, a different neighbour on the host -- and a floor that denies it
#: turns an ordinary Tuesday into a finding.
MIN_BAND_FRACTION = 0.05


@dataclass(frozen=True, slots=True)
class Run:
    """One run's contribution to a trend, as the caller reads it out of the store."""

    recording_id: str
    at: str
    #: The whole-recording summary for this metric, pooled across boxes. Pooled
    #: because a discovered environment replaces its tasks between runs, so a
    #: per-box trend would be a new line every deployment (design-api 10.2).
    summary: Summary
    #: The same metric over the baseline phase alone, where the run had one. This is
    #: the environment-drift question: a p95 that crept up 30% alongside a baseline
    #: CPU that crept up 30% is not an application regression (design-api 17.3).
    baseline: Summary | None = None
    #: Carries an `invalid` annotation. Still drawn -- knowing a run failed validity
    #: is part of the history -- but kept out of the band.
    invalid: bool = False
    status: str = "finished"


@dataclass(frozen=True, slots=True)
class Point:
    """One run on the chart, and where it sits relative to what came before."""

    recording_id: str
    at: str
    #: The median of the run's window. `None` when the sample count could not
    #: support one, which is a gap in the line rather than a zero.
    value: float | None
    n: int
    invalid: bool
    status: str
    baseline_value: float | None = None
    baseline_n: int = 0
    #: What normal looked like before this run, and how far from it counts as a
    #: move. Both `None` until the series is long enough to say.
    center: float | None = None
    band: float | None = None
    #: Runs behind the band. Carried so a reader can see the band widen as the
    #: history fills rather than wondering why it appeared.
    band_runs: int = 0
    #: Beyond the band, and if so whether it is the bad direction for this metric.
    #: Geometry, not a verdict: design-api 17.4 also requires the count to support
    #: the claim and the run to be valid.
    outside: bool = False
    worse: bool = False


@dataclass(frozen=True, slots=True)
class Trend:
    """One metric across one series, oldest first."""

    metric: str
    points: list[Point] = field(default_factory=list)
    window: int = BAND_WINDOW

    @property
    def latest(self) -> Point | None:
        return self.points[-1] if self.points else None

    @property
    def banded(self) -> bool:
        """Whether any point has a band. False for a series too short to have one."""
        return any(p.band is not None for p in self.points)

    def to_document(self) -> dict[str, object]:
        return {
            "metric": self.metric,
            "window": self.window,
            "min_runs_for_band": MIN_RUNS_FOR_BAND,
            "banded": self.banded,
            "points": [
                {
                    "recording_id": p.recording_id,
                    "at": p.at,
                    "value": p.value,
                    "n": p.n,
                    "invalid": p.invalid,
                    "status": p.status,
                    "baseline_value": p.baseline_value,
                    "baseline_n": p.baseline_n,
                    "center": p.center,
                    "band": p.band,
                    "band_runs": p.band_runs,
                    "outside": p.outside,
                    "worse": p.worse,
                }
                for p in self.points
            ],
        }


def band_from(values: Sequence[float]) -> tuple[float, float] | None:
    """The centre and half-width the given history supports, or nothing.

    The shape `compare()` uses against a baseline window -- median, then twice the
    IQR, then a floor -- applied to run medians instead of samples, so "outside the
    band" means one thing in this tool. Only the floor differs, and only because the
    thing being floored is a different distance.
    """
    if len(values) < MIN_RUNS_FOR_BAND:
        return None
    ordered = sorted(values)
    centre = median(ordered)
    iqr = quantile(ordered, 0.75) - quantile(ordered, 0.25)
    return centre, max(NOISE_MULTIPLE * iqr, MIN_BAND_FRACTION * abs(centre))


def trend(metric: str, runs: Iterable[Run], *, window: int = BAND_WINDOW) -> Trend:
    """Turn a series' runs into points, each judged against the ones before it."""
    worse_up = worse_direction(metric) == "up"
    history: list[float] = []
    points: list[Point] = []

    for run in sorted(runs, key=lambda r: (r.at, r.recording_id)):
        value = run.summary.p50
        measured = band_from(history[-window:])
        centre, half = measured if measured else (None, None)
        outside = (
            value is not None and centre is not None and abs(value - centre) > half
        )
        points.append(
            Point(
                recording_id=run.recording_id,
                at=run.at,
                value=value,
                n=run.summary.n,
                invalid=run.invalid,
                status=run.status,
                baseline_value=run.baseline.p50 if run.baseline else None,
                baseline_n=run.baseline.n if run.baseline else 0,
                center=centre,
                band=half,
                band_runs=len(history[-window:]) if measured else 0,
                outside=outside,
                worse=outside and (value > centre) == worse_up,
            )
        )
        # An invalid run is history nobody should measure against: its numbers are
        # the ones we already know not to trust, and letting them set the band would
        # spread that distrust over every run that follows.
        if value is not None and not run.invalid:
            history.append(value)

    return Trend(metric=metric, points=points, window=window)
