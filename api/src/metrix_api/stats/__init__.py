"""Percentiles, windows, deltas, and the settle-phase measurements.

One rule holds this package together and the UI cannot bypass it: **every number
carries the count behind it, and a number the count cannot support is not reported.**
That is why summarising lives here rather than in whatever view happens to need it.

`summary.py` describes a window and compares two of them; `recovery.py` answers what
happened after the traffic stopped.
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

__all__ = [
    "MIN_BAND_FRACTION",
    "MIN_FOR_MEDIAN",
    "MIN_FOR_P95",
    "NOISE_MULTIPLE",
    "RECOVERY_MIN_BAND_FRACTION",
    "RECOVERY_MULTIPLE",
    "Delta",
    "Recovery",
    "Summary",
    "band_for",
    "compare",
    "quantile",
    "recovery",
    "summarize",
    "worse_direction",
]
