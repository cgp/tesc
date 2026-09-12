"""Host collection: SSH, scrape.

Given a resolved endpoint list, samples host and process statistics at a fixed
interval and produces one normalized shape regardless of transport.
"""

from metrix_api.observer.collector import Clock, Transport, collect, collect_all
from metrix_api.observer.metrics import ALL_METRICS, GROUPS, Gap, Sample
from metrix_api.observer.ssh import SshTransport

__all__ = [
    "ALL_METRICS",
    "GROUPS",
    "Clock",
    "Gap",
    "Sample",
    "SshTransport",
    "Transport",
    "collect",
    "collect_all",
]
