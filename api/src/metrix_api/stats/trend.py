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

`outside` is geometry and nothing more -- the point sits beyond the band. It is one
of the three conditions design-api 17.4 requires before the word *regression* is
earned; `Point.flagged` is where all three are put together, and the reason they are
written out separately is that any one of them alone produces false positives at a
rate that trains people to ignore the flag.
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
    #: Geometry, not a verdict on its own: see `flagged`.
    outside: bool = False
    worse: bool = False

    @property
    def supported(self) -> bool:
        """The run carried enough samples to have a median at all.

        `stats/summary.py` withholds one below its floor, so an unsupported point has
        no value rather than a value with a caveat -- which is why this reads as a
        `None` check and not as a second sample-count rule.
        """
        return self.value is not None

    @property
    def flagged(self) -> bool:
        """The three conditions of design-api 17.4, together.

        It moved beyond the band the series' own history supports, **and** its sample
        count supports the claim, **and** the run is not one whose numbers are already
        known to be untrustworthy. Any one of the three on its own produces false
        positives at a rate that teaches people to ignore the flag, which costs more
        than never having flagged anything.
        """
        return self.outside and self.supported and not self.invalid

    @property
    def regressed(self) -> bool:
        """Flagged, and in the direction that is bad for this metric.

        A flag in the good direction is still worth surfacing -- an unexplained
        improvement usually means the test stopped doing some of the work -- but it
        is not what a pipeline should fail on, so the two are named apart.
        """
        return self.flagged and self.worse

    @property
    def judged(self) -> bool:
        """Whether this point could be checked at all."""
        return self.band is not None and self.supported and not self.invalid

    @property
    def unjudged_because(self) -> str | None:
        """Why it could not be, in the order that matters.

        Kept as a reason rather than folded into a pass, because "this is fine" and
        "this could not be checked" are different answers and a pipeline acting on
        the first when it was given the second is the whole failure mode here.
        """
        if self.invalid:
            return "invalid"
        if not self.supported:
            return "unsupported"
        if self.band is None:
            return "no_band"
        return None


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

    def point_for(self, recording_id: str) -> Point | None:
        return next((p for p in self.points if p.recording_id == recording_id), None)

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
                    "flagged": p.flagged,
                    "regressed": p.regressed,
                    "judged": p.judged,
                    "unjudged_because": p.unjudged_because,
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
# --------------------------------------------------------------- the CI verdict

#: Nothing in this series could be checked -- too short a history, a run whose
#: sample count says nothing, or a run already marked invalid. Deliberately not
#: `OK`: a pipeline that treats "could not check" as "passed" gets exactly one
#: useful signal out of this endpoint, the wrong one.
UNKNOWN = "unknown"
#: Everything that could be checked sat inside its band.
OK = "ok"
#: Something moved beyond its band, but only in the direction that is good for it.
#: Worth a look -- an unexplained improvement usually means the test stopped doing
#: part of the work -- but not something to fail a build on.
CHANGED = "changed"
#: Something moved beyond its band the bad way, with the count to support it, on a
#: run with no invalid note. This is the one to fail on.
REGRESSED = "regressed"


@dataclass(frozen=True, slots=True)
class Finding:
    """One metric that moved, and by how much against what."""

    metric: str
    value: float
    center: float
    band: float
    n: int
    worse: bool

    @property
    def change(self) -> float:
        return self.value - self.center

    @property
    def change_pct(self) -> float | None:
        return (100.0 * self.change / self.center) if self.center else None

    def to_document(self) -> dict[str, object]:
        return {
            "metric": self.metric,
            "value": self.value,
            "center": self.center,
            "band": self.band,
            "change": self.change,
            "change_pct": self.change_pct,
            "n": self.n,
            "worse": self.worse,
        }


@dataclass(frozen=True, slots=True)
class Verdict:
    """One run against the history of its series, in a shape a pipeline can read.

    design-api 17.4: the historical check is usually more useful than a fixed SLO
    threshold, because fixed thresholds are guesses made before the data existed.
    What makes it safe to gate on is that it says when it cannot answer, per metric
    and with the reason, rather than reporting silence as a pass.
    """

    series_key: str
    recording_id: str | None = None
    at: str | None = None
    findings: list[Finding] = field(default_factory=list)
    #: Metrics that were checked and sat inside their band.
    judged: list[str] = field(default_factory=list)
    #: Metrics that could not be checked, and why: metric -> reason.
    unjudged: dict[str, str] = field(default_factory=dict)

    @property
    def status(self) -> str:
        if any(f.worse for f in self.findings):
            return REGRESSED
        if self.findings:
            return CHANGED
        return OK if self.judged else UNKNOWN

    def to_document(self) -> dict[str, object]:
        return {
            "series_key": self.series_key,
            "recording_id": self.recording_id,
            "at": self.at,
            "status": self.status,
            # Worst first: a pipeline prints the first line of this and stops.
            "findings": [
                f.to_document()
                for f in sorted(
                    self.findings, key=lambda f: (not f.worse, -abs(f.change_pct or 0.0))
                )
            ],
            "judged": sorted(self.judged),
            "unjudged": dict(sorted(self.unjudged.items())),
        }


def verdict_from(
    series_key: str, trends: dict[str, Trend], *, recording_id: str | None = None
) -> Verdict:
    """Judge one run -- the latest by default -- across every metric in a series."""
    points = {}
    for metric, one in trends.items():
        point = one.point_for(recording_id) if recording_id else one.latest
        if point is not None:
            points[metric] = point
    if not points:
        return Verdict(series_key=series_key, recording_id=recording_id)

    any_point = next(iter(points.values()))
    return Verdict(
        series_key=series_key,
        recording_id=any_point.recording_id,
        at=any_point.at,
        findings=[
            Finding(
                metric=metric,
                value=p.value,
                center=p.center,
                band=p.band,
                n=p.n,
                worse=p.worse,
            )
            for metric, p in points.items()
            if p.flagged
        ],
        judged=[metric for metric, p in points.items() if p.judged and not p.flagged],
        unjudged={
            metric: reason
            for metric, p in points.items()
            if (reason := p.unjudged_because) is not None
        },
    )
