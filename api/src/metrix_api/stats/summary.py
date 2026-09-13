"""Summarising a window of host samples, and comparing two of them.

One rule runs through this module and it is the reason the module exists at all:
**every number carries the count behind it, and a number the count cannot support is
not reported.** A p95 over twelve samples is the maximum wearing a hat, and once it
reaches a table someone quotes it. So `Summary` has no bare floats -- a percentile
that is not supported comes back `None`, and the count travels with the value rather
than being available somewhere nearby.

The comparison half exists because an absolute host number rarely answers anything.
*68% CPU* is only meaningful next to what that box does when nothing is happening,
which is what a baseline window is (design-api 9.7) -- and whether a change is worth
looking at depends on how much that metric moves on its own, which is what the
baseline's own spread says.
"""

from __future__ import annotations

import math
from dataclasses import dataclass
from statistics import median, stdev

#: A quantile needs enough samples that it is not just the extreme value. The rule is
#: `n * (1 - q) >= 1` -- at least one whole sample in the tail being described -- with
#: a floor of 20 for p95, which is where that arithmetic lands anyway. Below it the
#: percentile is withheld rather than shown with a caveat nobody reads.
MIN_FOR_MEDIAN = 3
MIN_FOR_P95 = 20

#: Which direction is bad, per metric family. Deltas are coloured by whether the
#: change is good or bad rather than by sign (design-api 14.4): more free memory and
#: more CPU are both "up", and only one of them is a problem.
#:
#: Matched longest-prefix-first, so `mem.available_bytes` beats `mem.`.
WORSE_WHEN = {
    "mem.available_bytes": "down",
    "disk.free_bytes": "down",
    "cpu.idle": "down",
    "": "up",
}


def worse_direction(metric: str) -> str:
    """Whether an increase or a decrease in this metric is the bad news."""
    for prefix in sorted(WORSE_WHEN, key=len, reverse=True):
        if metric.startswith(prefix):
            return WORSE_WHEN[prefix]
    return "up"


def quantile(values: list[float], q: float) -> float:
    """Linear interpolation between order statistics, as `statistics.quantiles` does.

    Written out rather than called because `quantiles` returns cut points for a
    partition, and asking it for one arbitrary quantile reads as a puzzle at the call
    site.
    """
    if not values:
        raise ValueError("no values")
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = q * (len(ordered) - 1)
    low = math.floor(position)
    high = math.ceil(position)
    if low == high:
        return ordered[low]
    return ordered[low] + (ordered[high] - ordered[low]) * (position - low)


@dataclass(frozen=True, slots=True)
class Summary:
    """One metric, on one target, over one window of time."""

    metric: str
    #: The count behind every other field here. Never absent, and never implied.
    n: int
    minimum: float | None = None
    maximum: float | None = None
    mean: float | None = None
    #: Withheld below `MIN_FOR_MEDIAN` and `MIN_FOR_P95` respectively.
    p50: float | None = None
    p95: float | None = None
    #: Interquartile range: how much this metric moves on its own over this window.
    #: It is what makes "different from baseline" a claim rather than an observation.
    iqr: float | None = None
    #: Standard deviation. It earns its column (design-api 14.2): a median of 40 beside
    #: a deviation of 300 says "bimodal or unstable" faster than any percentile. It is
    #: also the wrong thing to set a threshold on, because these distributions are
    #: right-skewed and it overstates the typical spread -- which is why median and p95
    #: sit next to it rather than behind a picker.
    stddev: float | None = None

    @property
    def supported(self) -> bool:
        return self.p50 is not None

    def to_document(self) -> dict[str, float | int | str | bool | None]:
        """The wire shape, written once. The live stream and the REST endpoint serve
        the same table, so they must serve it in the same words."""
        return {
            "metric": self.metric,
            "n": self.n,
            "min": self.minimum,
            "max": self.maximum,
            "mean": self.mean,
            "p50": self.p50,
            "p95": self.p95,
            "iqr": self.iqr,
            "stddev": self.stddev,
            "supported": self.supported,
        }


def summarize(metric: str, values: list[float]) -> Summary:
    """Describe a window. Withholds what the sample count cannot support."""
    n = len(values)
    if n == 0:
        return Summary(metric=metric, n=0)

    ordered = sorted(values)
    return Summary(
        metric=metric,
        n=n,
        minimum=ordered[0],
        maximum=ordered[-1],
        mean=sum(ordered) / n,
        p50=median(ordered) if n >= MIN_FOR_MEDIAN else None,
        p95=quantile(ordered, 0.95) if n >= MIN_FOR_P95 else None,
        # Two points define a deviation, so this needs a lower floor than the median;
        # one point has none at all rather than a deviation of zero.
        stddev=stdev(ordered) if n >= 2 else None,
        # The IQR of two points is the gap between them, which says nothing about
        # spread; the median threshold is the right floor for it too.
        iqr=(
            quantile(ordered, 0.75) - quantile(ordered, 0.25) if n >= MIN_FOR_MEDIAN else None
        ),
    )


#: How far a metric may sit from its baseline before it is worth looking at, as a
#: multiple of the baseline window's own IQR. Two is the same default the trend view
#: uses for regressions (design-api 17.4), for the same reason: a band drawn from the
#: measured noise, rather than a percentage somebody picked.
NOISE_MULTIPLE = 2.0

#: When a metric barely moves, its IQR collapses towards zero and any change at all
#: reads as significant. The band never narrows below this fraction of the baseline
#: median -- 2% of a value that is genuinely steady is still noise.
MIN_BAND_FRACTION = 0.02


@dataclass(frozen=True, slots=True)
class Delta:
    """One metric, now versus its baseline."""

    metric: str
    baseline: Summary
    current: Summary
    #: The comparison is of medians: a window's median is what a person means by
    #: "what it normally sits at", and it does not move because one sample spiked.
    change: float | None = None
    change_pct: float | None = None
    #: Half-width of the band the baseline's own spread supports.
    band: float | None = None
    #: Whether the move is larger than that band -- and if so, whether it is the bad
    #: direction for this metric.
    outside_band: bool = False
    worse: bool = False

    @property
    def comparable(self) -> bool:
        """Both windows carried enough samples to be compared at all."""
        return self.baseline.supported and self.current.supported


def compare(baseline: Summary, current: Summary) -> Delta:
    """Now against normal, with the band the baseline's own spread supports.

    A delta neither window can support comes back with `comparable` false and no
    numbers, rather than with a figure that would be quoted without its caveat.
    """
    if not (baseline.supported and current.supported):
        return Delta(metric=current.metric, baseline=baseline, current=current)

    change = current.p50 - baseline.p50
    band = max(
        NOISE_MULTIPLE * (baseline.iqr or 0.0),
        MIN_BAND_FRACTION * abs(baseline.p50),
    )
    outside = abs(change) > band
    return Delta(
        metric=current.metric,
        baseline=baseline,
        current=current,
        change=change,
        change_pct=(100.0 * change / baseline.p50) if baseline.p50 else None,
        band=band,
        outside_band=outside,
        worse=outside and (change > 0) == (worse_direction(current.metric) == "up"),
    )
