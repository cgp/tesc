"""Can this profile actually be used, right now?

A profile is a set of claims about an environment: traffic goes *here*, statistics
come from *there*. Both are easy to get subtly wrong -- a security group that admits
the load balancer and not this machine, an exporter that is not running, an SSH key
that works for one box and not its replacement -- and every one of those mistakes is
invisible until a recording produces a page of gaps.

So the two claims are checked separately and reported separately, because they fail
for different reasons and are fixed by different people:

* **the load target** -- open a connection to the address, and complete the TLS
  handshake when TLS is on. Nothing is sent and nothing is read: this asks whether
  the socket accepts, not what is listening on it.
* **the collector** -- one real probe over the transport the recording will use. Not
  a port check: an SSH login that succeeds and then cannot run the stats script, or
  an exporter answering 404 on the configured path, are exactly the failures a port
  check misses and a recording then hits.

Verification is on demand. It is never run as a side effect of loading a page, and a
failure is reported rather than raised -- a profile with one unreachable box is still
worth having, and knowing *which* box is the point.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import ssl
import time
from dataclasses import dataclass, field

from metrix_api.observer import transport_for
from metrix_api.observer.facts import IDENTITY_KEYS
from metrix_api.profiles import Endpoint, Profile

log = logging.getLogger(__name__)

#: What a check was of. Both are per endpoint, and either can fail on its own.
LOAD = "load"
COLLECT = "collect"

#: A check that could not be attempted is not a check that failed. An endpoint with
#: no collector is not broken; it was never going to be observed.
SKIPPED = "skipped"
OK = "ok"
FAILED = "failed"


@dataclass(frozen=True, slots=True)
class Check:
    """One question asked of one endpoint, and the answer."""

    endpoint: str
    kind: str
    result: str
    #: Where it went, in the same spelling the page shows elsewhere.
    address: str
    #: What happened, in a sentence someone can act on.
    detail: str
    #: Round trip, when there was one. A slow success is worth seeing.
    ms: float | None = None

    @property
    def ok(self) -> bool:
        return self.result == OK


@dataclass(frozen=True, slots=True)
class Report:
    profile: str
    checks: list[Check] = field(default_factory=list)

    @property
    def failures(self) -> list[Check]:
        return [c for c in self.checks if c.result == FAILED]

    @property
    def ok(self) -> bool:
        return not self.failures

    def summary(self) -> str:
        attempted = [c for c in self.checks if c.result != SKIPPED]
        if not attempted:
            return "nothing to check"
        if self.ok:
            return f"{len(attempted)} of {len(attempted)} reachable"
        return f"{len(self.failures)} of {len(attempted)} unreachable"


async def verify(
    profile: Profile, *, timeout: float = 5.0, endpoints: list[Endpoint] | None = None
) -> Report:
    """Check every endpoint, in parallel.

    In parallel because these are timeouts, not work: a profile with eight boxes and
    one firewall problem would otherwise take eight times the timeout to tell you
    about it, in series, while nothing happens.
    """
    chosen = profile.endpoints if endpoints is None else endpoints
    checks = await asyncio.gather(
        *[_load(endpoint, timeout) for endpoint in chosen],
        *[_collect(endpoint, timeout) for endpoint in chosen],
    )
    # Grouped by endpoint rather than by kind, because that is how they are read: one
    # box at a time, both of its answers together.
    order = {endpoint.id: i for i, endpoint in enumerate(chosen)}
    return Report(
        profile=profile.name,
        checks=sorted(checks, key=lambda c: (order[c.endpoint], c.kind != LOAD)),
    )


async def _load(endpoint: Endpoint, timeout: float) -> Check:
    """Open a connection to the load target. Nothing is sent."""
    if not endpoint.load:
        return Check(
            endpoint=endpoint.id,
            kind=LOAD,
            result=SKIPPED,
            address=endpoint.address,
            detail="observed only — traffic is not sent here",
        )

    context = _tls_context(endpoint)
    started = time.perf_counter()
    try:
        reader, writer = await asyncio.wait_for(
            asyncio.open_connection(
                endpoint.host,
                endpoint.port,
                ssl=context,
                # The name the certificate is checked against, and what SNI carries.
                # A container addressed by IP presents the service's certificate, so
                # verifying against the IP would fail on a correctly configured box.
                server_hostname=_sni(endpoint) if context else None,
            ),
            timeout,
        )
    except TimeoutError:
        return _failed(endpoint, LOAD, f"no answer within {timeout:g}s", started)
    except ssl.SSLCertVerificationError as exc:
        # Distinguished because it is the one failure that means the socket is fine:
        # something answered, and it is not who it said it would be.
        return _failed(
            endpoint,
            LOAD,
            f"connected, but the certificate did not verify: {exc.verify_message or exc}",
            started,
        )
    except (OSError, ssl.SSLError) as exc:
        return _failed(endpoint, LOAD, _reason(exc), started)

    writer.close()
    # Best-effort: a peer that resets rather than closing politely has still answered.
    with contextlib.suppress(OSError):
        await writer.wait_closed()
    del reader

    how = "TLS handshake completed" if context else "connected"
    return Check(
        endpoint=endpoint.id,
        kind=LOAD,
        result=OK,
        address=endpoint.address,
        detail=how,
        ms=_elapsed(started),
    )


async def _collect(endpoint: Endpoint, timeout: float) -> Check:
    """One real probe over the transport a recording would use."""
    if endpoint.collect.transport == "none":
        return Check(
            endpoint=endpoint.id,
            kind=COLLECT,
            result=SKIPPED,
            address="—",
            detail="no collector configured — this endpoint contributes no host series",
        )

    try:
        transport = transport_for(endpoint, timeout=timeout)
    except Exception as exc:  # noqa: BLE001 - an unusable collect block
        return Check(
            endpoint=endpoint.id,
            kind=COLLECT,
            result=FAILED,
            address="—",
            detail=f"this collector cannot be built: {exc}",
        )

    address = transport.describe()
    diagnostics = (
        transport.diagnostics() if hasattr(transport, "diagnostics") else {"address": address}
    )
    log.info("collector probe start endpoint=%s diagnostics=%s", endpoint.id, diagnostics)
    started = time.perf_counter()
    try:
        facts = await asyncio.wait_for(transport.probe(), timeout + 1.0)
    except TimeoutError:
        detail = f"no answer within {timeout:g}s; {diagnostic_text(diagnostics)}"
        log.error("collector probe timeout endpoint=%s diagnostics=%s", endpoint.id, diagnostics)
        return _failed(endpoint, COLLECT, detail, started, address)
    except Exception as exc:  # noqa: BLE001 - every transport fails differently
        detail = f"{_reason(exc)}; {diagnostic_text(diagnostics)}"
        log.error(
            "collector probe failed endpoint=%s diagnostics=%s error=%s",
            endpoint.id,
            diagnostics,
            exc,
            exc_info=True,
        )
        return _failed(endpoint, COLLECT, detail, started, address)

    # A probe that answers but says nothing means the connection works and the thing
    # on the other end is not what we think it is -- an exporter with the wrong
    # collectors on, or a shell that swallowed the script.
    if not facts:
        return _failed(
            endpoint,
            COLLECT,
            "answered, but reported nothing this tool can read",
            started,
            address,
        )

    named = ", ".join(
        str(facts.identity[key]) for key in IDENTITY_KEYS if facts.identity.get(key)
    )
    return Check(
        endpoint=endpoint.id,
        kind=COLLECT,
        result=OK,
        address=address,
        detail=named or f"answered with {len(facts.filesystems)} filesystem(s)",
        ms=_elapsed(started),
    )


def _tls_context(endpoint: Endpoint) -> ssl.SSLContext | None:
    if not endpoint.tls.enabled:
        return None
    context = ssl.create_default_context()
    if endpoint.tls.insecure_skip_verify:
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
    return context


def _sni(endpoint: Endpoint) -> str:
    return endpoint.tls.sni or endpoint.host_header or endpoint.host


def _failed(
    endpoint: Endpoint, kind: str, detail: str, started: float, address: str | None = None
) -> Check:
    return Check(
        endpoint=endpoint.id,
        kind=kind,
        result=FAILED,
        address=address if address is not None else endpoint.address,
        detail=detail,
        ms=_elapsed(started),
    )


def _reason(exc: BaseException) -> str:
    """The message without the class name, unless the message is empty.

    `ConnectionRefusedError` says everything by its type and nothing by its text on
    some platforms, and "[Errno 111]" on others; showing both would be noise on one
    and useless on the other.
    """
    text = str(exc).strip()
    return text or type(exc).__name__


def diagnostic_text(details: dict[str, object]) -> str:
    """A safe, compact explanation of the SSH options attempted."""
    ordered = (
        "requested_host",
        "requested_port",
        "host",
        "port",
        "username",
        "ssh_config",
        "identity_files",
        "loaded_client_keys",
        "explicit_key",
        "diagnostics_error",
    )
    return "; ".join(
        f"{key}={details[key]}" for key in ordered if key in details and details[key] is not None
    )


def _elapsed(started: float) -> float:
    return round((time.perf_counter() - started) * 1000, 1)
