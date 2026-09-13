"""Host collection: SSH, scrape.

Given a resolved endpoint list, samples host and process statistics at a fixed
interval and produces one normalized shape regardless of transport.
"""

from metrix_api.observer.collector import Clock, Transport, collect, collect_all
from metrix_api.observer.metrics import ALL_METRICS, GROUPS, Annotation, Gap, Sample
from metrix_api.observer.prometheus import ScrapeTransport
from metrix_api.observer.raw import RawSample, derive
from metrix_api.observer.ssh import SshTransport

__all__ = [
    "ALL_METRICS",
    "GROUPS",
    "Annotation",
    "Clock",
    "Gap",
    "RawSample",
    "Sample",
    "ScrapeTransport",
    "SshTransport",
    "Transport",
    "collect",
    "collect_all",
    "derive",
    "transport_for",
]


def transport_for(endpoint, *, timeout: float = 5.0) -> Transport:
    """Build the transport an endpoint's profile asks for.

    The collector never branches on transport: it is handed one and drives it.
    """
    collection = endpoint.collection()
    if collection.transport == "ssh":
        return SshTransport.from_collection(collection)
    if collection.transport == "scrape":
        return ScrapeTransport.from_collection(collection, timeout=timeout)
    raise ValueError(
        f"endpoint {endpoint.id!r} has transport {collection.transport!r}; "
        "nothing to collect from"
    )
