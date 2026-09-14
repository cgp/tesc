"""One plan, many boxes, one window: which of them is the odd one out.

A sweep runs the same work against every target in turn (design-engine §3.5), so the
thing that varies is the machine. That makes it the one comparison in this tool where
the reference is not history: **the sweep's own spread is the band**, which is §17.4's
measured-noise-floor logic turned sideways — across targets instead of across time.

Three things are kept apart here, exactly as they are in the trend view:

- **`outside` is geometry.** It says a value sits beyond the band and nothing else.
- **`supported`** says the box collected enough samples to have a median at all.
- **`flagged`** is the verdict, and needs all three: outside, supported, and not
  already known to be untrustworthy.

A box is judged against **the other boxes, never including itself**. The trend view
refuses to measure a band over a window containing the point it is judging, because
such a window widens to swallow the very movement it is meant to detect; one slow
container inflating the spread it is then compared against is the same mistake with
the axes swapped.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from statistics import median

from metrix_api.stats.summary import NOISE_MULTIPLE, Summary, quantile, worse_direction

#: How many *other* boxes a target needs before its distance from them means
#: anything. Five, matching the run count a trend band needs, and for the same
#: reason: an interquartile range over three numbers is not a description of spread.
#: A sweep of four containers is still ranked, still shows its attributes and its
#: baselines, and simply does not accuse anybody.
MIN_PEERS_FOR_BAND = 5

#: The narrowest the band may get, as a fraction of what the sweep's middle looks
#: like. This is the third distance this tool floors, and it gets its own number for
#: the same reason the other two do: 2% describes how far a sample strays from its
#: own window's median, 5% how far one run's median strays from the last one's, and
#: this one has to cover how far one box strays from its siblings. Two containers of
#: the same image on the same cluster differ by more than a couple of percent as a
#: matter of course -- a different host, a different neighbour, a different half of
#: the rack -- and a floor that denies it turns ordinary variation into a finding.
MIN_BAND_FRACTION = 0.05


@dataclass(frozen=True, slots=True)
class Box:
    """One target's contribution to a sweep, as the caller reads it out of the store."""

    target_id: str
    #: What discovery found: instance type, AZ, image digest, task definition
    #: revision. Carried because the explanation for an outlier is usually sitting in
    #: this row rather than in the measurement (§17.6).
    attributes: dict[str, str] = field(default_factory=dict)
    #: The metric over the window being compared.
    summary: Summary | None = None
    #: The same metric over this box's baseline phase. The question it answers is the
    #: one that otherwise sends somebody chasing a phantom: was this container
    #: already loaded before the test started?
    baseline: Summary | None = None
    #: Carries an `invalid` note of its own. Still ranked -- knowing which box was
    #: not to be trusted is part of reading the sweep -- but kept out of every band
    #: and never flagged.
    invalid: bool = False


@dataclass(frozen=True, slots=True)
class Standing:
    """Where one box came out on one metric, and whether that means anything."""

    target_id: str
    #: The box's median over the window. `None` when its sample count could not
    #: support one, which is a withheld figure rather than a zero.
    value: float | None
    n: int
    invalid: bool
    #: Best first, from 1. Ordered by what is good for this metric, not by size.
    rank: int
    baseline_value: float | None = None
    baseline_n: int = 0
    #: What the other boxes looked like, and how far from them counts as a move.
    #: Both `None` until enough peers have a supported figure.
    centre: float | None = None
    band: float | None = None
    peers: int = 0
    #: Beyond the band, and if so whether in the bad direction for this metric.
    #: Geometry on its own: see `flagged`.
    outside: bool = False
    worse: bool = False

    @property
    def supported(self) -> bool:
        return self.value is not None

    @property
    def judged(self) -> bool:
        """Whether this box was measured against anything at all."""
        return self.band is not None and self.supported and not self.invalid

    @property
    def flagged(self) -> bool:
        """The odd one out: outside the spread, sample-supported, and trustworthy."""
        return self.judged and self.outside

    @property
    def unjudged_because(self) -> str | None:
        """Why no verdict was reached, in the words a reader needs.

        A blank where an answer should be is the thing that gets misread as a pass,
        so every box that was not judged says which of the three conditions it missed.
        """
        if not self.supported:
            return "too few samples on this box to have a median"
        if self.invalid:
            return "this box carries an invalid note, so it is ranked but not judged"
        if self.band is None:
            missing = MIN_PEERS_FOR_BAND - self.peers
            return (
                f"{self.peers} other box(es) with a figure; {missing} more would give "
                "the sweep a spread to measure against"
            )
        return None


@dataclass(frozen=True, slots=True)
class MetricSweep:
    """Every box ranked on one metric."""

    metric: str
    #: Which direction is the bad news, so a rank means "best" rather than "smallest".
    worse: str
    standings: list[Standing] = field(default_factory=list)

    @property
    def odd_ones_out(self) -> list[Standing]:
        return [s for s in self.standings if s.flagged]

    @property
    def judged(self) -> bool:
        return any(s.judged for s in self.standings)


def band_from(values: list[float]) -> tuple[float, float] | None:
    """The middle of the other boxes, and how far from it is still ordinary.

    The same shape the trend and the within-window comparison use -- median, twice
    the interquartile range, then a floor -- so that "outside the band" means one
    thing across this whole tool. Only the floor differs, and only because the thing
    being floored is a different distance.
    """
    if len(values) < MIN_PEERS_FOR_BAND:
        return None
    ordered = sorted(values)
    centre = median(ordered)
    iqr = quantile(ordered, 0.75) - quantile(ordered, 0.25)
    return centre, max(NOISE_MULTIPLE * iqr, MIN_BAND_FRACTION * abs(centre))


def rank(metric: str, boxes: list[Box]) -> MetricSweep:
    """Rank the boxes on one metric, each judged against the others."""
    worse_up = worse_direction(metric) == "up"

    #: Only boxes that have a figure and are not already known to be untrustworthy
    #: can describe what normal looks like for the rest.
    usable = {
        box.target_id: box.summary.p50
        for box in boxes
        if box.summary is not None and box.summary.p50 is not None and not box.invalid
    }

    ordered = sorted(
        boxes,
        key=lambda box: _order(box, worse_up),
    )

    standings = []
    for position, box in enumerate(ordered, start=1):
        value = box.summary.p50 if box.summary else None
        peers = [v for target, v in usable.items() if target != box.target_id]
        measured = band_from(peers)
        centre, half = measured if measured else (None, None)
        outside = bool(measured and value is not None and abs(value - centre) > half)
        standings.append(
            Standing(
                target_id=box.target_id,
                value=value,
                n=box.summary.n if box.summary else 0,
                invalid=box.invalid,
                rank=position,
                baseline_value=box.baseline.p50 if box.baseline else None,
                baseline_n=box.baseline.n if box.baseline else 0,
                centre=centre,
                band=half,
                peers=len(peers),
                outside=outside,
                worse=bool(outside and value is not None and (value > centre) == worse_up),
            )
        )
    return MetricSweep(metric=metric, worse="up" if worse_up else "down", standings=standings)


def _order(box: Box, worse_up: bool) -> tuple[int, float, str]:
    """Best first. A box with no figure sorts last rather than as a zero."""
    value = box.summary.p50 if box.summary else None
    if value is None:
        return (1, 0.0, box.target_id)
    return (0, value if worse_up else -value, box.target_id)
