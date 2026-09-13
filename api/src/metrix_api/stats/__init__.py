"""Percentiles, windows, deltas, and the settle-phase measurements.

One rule holds this package together and the UI cannot bypass it: **every number
carries the count behind it, and a number the count cannot support is not reported.**
That is why summarising lives here rather than in whatever view happens to need it.

`summary.py` describes a window and compares two of them; `recovery.py` answers what
happened after the traffic stopped; `trend.py` carries the same band arithmetic across
a whole series of runs.
"""

from metrix_api.stats.recovery import (
    MIN_BAND_FRACTION as RECOVERY_MIN_BAND_FRACTION,
)
from metrix_api.stats.recovery import (
    RECOVERY_MULTIPLE,
    Recovery,
    band_for,
    recovery,
)
from metrix_api.stats.summary import (
    MIN_BAND_FRACTION,
    MIN_FOR_MEDIAN,
    MIN_FOR_P95,
    NOISE_MULTIPLE,
    Delta,
    Summary,
    compare,
    quantile,
    summarize,
    worse_direction,
)
from metrix_api.stats.trend import (
    BAND_WINDOW,
    MIN_RUNS_FOR_BAND,
    Point,
    Run,
    Trend,
    band_from,
    trend,
)
from metrix_api.stats.trend import (
    MIN_BAND_FRACTION as TREND_MIN_BAND_FRACTION,
)

__all__ = [
    "BAND_WINDOW",
    "MIN_BAND_FRACTION",
    "MIN_FOR_MEDIAN",
    "MIN_FOR_P95",
    "MIN_RUNS_FOR_BAND",
    "NOISE_MULTIPLE",
    "RECOVERY_MIN_BAND_FRACTION",
    "RECOVERY_MULTIPLE",
    "TREND_MIN_BAND_FRACTION",
    "Delta",
    "Point",
    "Recovery",
    "Run",
    "Summary",
    "Trend",
    "band_for",
    "band_from",
    "compare",
    "quantile",
    "recovery",
    "summarize",
    "trend",
    "worse_direction",
]
