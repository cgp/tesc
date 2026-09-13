"""What happened after the traffic stopped.

These are the settle-phase measurements of design-api 9.7, and they are meaningless
without a settle phase -- which is the point of having one. Three questions, and each
answers something the load window cannot:

* **time-to-recover** -- how long until the metric is back inside a tolerance band of
  where it started. A box that recovers in four seconds and one that takes four
  minutes look identical in a measure-phase summary.
* **peak after stop** -- queues drain, garbage collects and buffers flush *after* the
  last request, so the worst value a box sees is often not under load at all. A run
  that reports only its measure phase misses it entirely.
* **did it come back** -- a metric still above its band when the settle window ends is
  the leak signal: memory, file descriptors, threads, connections. It does not prove a
  leak, and it is the only cheap evidence there is.

The band is the baseline window's own spread, for the reason in `summary.py`: a
tolerance somebody picked would flag a steady metric and miss a noisy one.
"""

from __future__ import annotations

from dataclasses import dataclass

from metrix_api.stats.summary import Summary, worse_direction

#: How close counts as recovered, as a multiple of the baseline window's IQR. Wider
#: than the comparison band (design-api 17.4 uses 2x for "is this different"): the
#: question here is *has it come back*, and demanding the metric return to exactly
#: its median would report a permanent leak on every box that is merely busy.
RECOVERY_MULTIPLE = 3.0

#: Same reasoning as `MIN_BAND_FRACTION` in summary.py -- a metric that barely moves
#: has almost no IQR, and would otherwise never be judged recovered.
MIN_BAND_FRACTION = 0.05


@dataclass(frozen=True, slots=True)
class Recovery:
    """One metric's behaviour after traffic stopped."""

    metric: str
    #: Milliseconds from the start of the settle window until the metric first sits
    #: inside the band and stays there. None when it never does.
    recovered_ms: int | None = None
    #: The worst value seen after the traffic stopped, and when.
    peak: float | None = None
    peak_at_ms: int | None = None
    #: Where the metric finished, and the band it was judged against.
    final: float | None = None
    band: float | None = None
    baseline: float | None = None
    #: How many samples the settle window held. Nothing here is worth reading without
    #: it: "recovered in 2s" from two samples is a coin toss.
    n: int = 0

    @property
    def returned(self) -> bool:
        return self.recovered_ms is not None

    @property
    def leaked(self) -> bool:
        """Never came back, and drifted the bad way for this metric.

        Both halves matter. A metric that ends outside its band on the *good* side --
        more free memory than it started with -- is not a leak, and calling it one
        teaches people to ignore the flag.
        """
        if self.returned or self.final is None or self.baseline is None:
            return False
        return (self.final > self.baseline) == (worse_direction(self.metric) == "up")


def band_for(baseline: Summary) -> float | None:
    """The tolerance band around a baseline window, or None if it cannot be judged."""
    if not baseline.supported or baseline.p50 is None:
        return None
    return max(
        RECOVERY_MULTIPLE * (baseline.iqr or 0.0),
        MIN_BAND_FRACTION * abs(baseline.p50),
    )


def recovery(
    metric: str, baseline: Summary, settle: list[tuple[int, float]]
) -> Recovery | None:
    """Measure one metric's settle window against its baseline.

    `settle` is (t_ms, value) in time order, as the store returns a series. None when
    there is nothing to judge against -- an unsupported baseline or an empty settle
    window is a missing measurement, not a recovery of zero.
    """
    band = band_for(baseline)
    if band is None or not settle:
        return None

    start = settle[0][0]
    target = baseline.p50 or 0.0
    inside = [abs(value - target) <= band for _, value in settle]

    # The first point from which it stays inside, not the first point that touches:
    # a metric that dips into the band and climbs back out has not recovered, and
    # reporting the first touch would be the most flattering possible reading.
    recovered_ms = None
    for i, ok in enumerate(inside):
        if ok and all(inside[i:]):
            recovered_ms = settle[i][0] - start
            break

    worst = max(settle, key=lambda point: point[1] * (1 if worse_direction(metric) == "up" else -1))
    return Recovery(
        metric=metric,
        recovered_ms=recovered_ms,
        peak=worst[1],
        peak_at_ms=worst[0] - start,
        final=settle[-1][1],
        band=band,
        baseline=target,
        n=len(settle),
    )
