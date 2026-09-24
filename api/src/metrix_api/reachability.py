"""Can this profile actually be used, right now?

A profile is a set of claims about an environment: traffic goes *here*, statistics
come from *there*. Both are easy to get subtly wrong -- a security group that admits
the load balancer and not this machine, an exporter that is not running, an SSH key
that works for one box and not its replacement -- and every one of those mistakes is
invisible until a recording produces a page of gaps.

So the two claims are checked separately and reported separately, because they fail
for different reasons and are fixed by different people:

* **the load target** -- open a connection to the address, complete the TLS
  handshake when TLS is on, send `GET /` and read the status line. Any status
  answers the question; what the application thinks of `/` does not.
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
import socket
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
    """Check every endpoint, with its load target before its collector.

    Different endpoints are checked in parallel because these are timeouts, not work,
    but each endpoint's front-end connection is completed before its SSH probe starts.
    That avoids doubling the connection burst against one host and makes a profile
    check answer the front-end question before asking the box for observations.
    """
    chosen = profile.endpoints if endpoints is None else endpoints
    async def check_endpoint(endpoint: Endpoint) -> list[Check]:
        load = await _load(endpoint, timeout)
        collect = await _collect(endpoint, timeout)
        return [load, collect]

    started = time.perf_counter()
    log.info("verify start profile=%s endpoints=%d", profile.name, len(chosen))
    grouped = await asyncio.gather(*(check_endpoint(endpoint) for endpoint in chosen))
    checks = [check for pair in grouped for check in pair]
    # Grouped by endpoint rather than by kind, because that is how they are read: one
    # box at a time, both of its answers together.
    order = {endpoint.id: i for i, endpoint in enumerate(chosen)}
    report = Report(
        profile=profile.name,
        checks=sorted(checks, key=lambda c: (order[c.endpoint], c.kind != LOAD)),
    )
    log.log(
        logging.INFO if report.ok else logging.WARNING,
        "verify done profile=%s result=%s elapsed=%sms",
        profile.name,
        report.summary(),
        _elapsed(started),
    )
    return report


async def _load(endpoint: Endpoint, timeout: float) -> Check:
    """Request the root resource from the load target.

    Any well-formed HTTP response answers the front-end question. A 404 is useful
    evidence that the service answered, even though it is not an application success.
    """
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
    connected: float | None = None
    answered: float | None = None
    writer: asyncio.StreamWriter | None = None

    async def request_root() -> int:
        nonlocal connected, answered, writer
        reader, writer = await _dial(
            endpoint.host,
            endpoint.port,
            context,
            # The name the certificate is checked against, and what SNI carries.
            # A container addressed by IP presents the service's certificate, so
            # verifying against the IP would fail on a correctly configured box.
            _sni(endpoint) if context else None,
        )
        connected = _elapsed(started)
        host = endpoint.host_header or endpoint.host
        writer.write(f"GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n".encode())
        await writer.drain()
        line = await reader.readline()
        answered = _elapsed(started)
        parts = line.decode("latin-1", errors="replace").strip().split(maxsplit=2)
        if len(parts) < 2 or not parts[0].startswith("HTTP/") or not parts[1].isdigit():
            raise OSError("front end returned an invalid HTTP response")
        return int(parts[1])

    log.info("front end check start endpoint=%s address=%s", endpoint.id, endpoint.address)
    try:
        status = await asyncio.wait_for(request_root(), timeout)
    except TimeoutError:
        stage = "connecting" if connected is None else f"a response (connected in {connected}ms)"
        return _load_failed(endpoint, f"no answer within {timeout:g}s", started, stage)
    except ssl.SSLCertVerificationError as exc:
        # Distinguished because it is the one failure that means the socket is fine:
        # something answered, and it is not who it said it would be.
        return _load_failed(
            endpoint,
            f"connected, but the certificate did not verify: {exc.verify_message or exc}",
            started,
        )
    except (OSError, ssl.SSLError) as exc:
        return _load_failed(endpoint, _reason(exc), started)
    finally:
        await _close(writer)

    # Timed to the status line: the answer is in, and how long closing takes (a TLS
    # shutdown waits for the peer) is not a property of the front end.
    ms = answered if answered is not None else _elapsed(started)
    log.info(
        "front end check ok endpoint=%s address=%s status=%d connect=%sms response=%sms",
        endpoint.id,
        endpoint.address,
        status,
        connected,
        round(ms - (connected or 0.0), 1),
    )
    return Check(
        endpoint=endpoint.id,
        kind=LOAD,
        result=OK,
        address=endpoint.address,
        detail=f"HTTP {status}",
        ms=ms,
    )


async def _dial(
    host: str, port: int, context: ssl.SSLContext | None, server_hostname: str | None
) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
    """Connect to every address the name resolves to at once; the first wins.

    In turn, a name whose first answer is an unreachable IPv6 address -- `localhost`
    on Windows, a dual-stack balancer seen from an IPv4-only network -- costs a
    refused-connection retry or the whole timeout before IPv4 is tried.

    Raced here rather than with `create_connection(happy_eyeballs_delay=...)`, because
    uvicorn runs on uvloop wherever it is installed and uvloop does not take that
    argument. When every address fails, the first address's error is the one raised:
    it is the one an ordinary client would have reported.
    """
    loop = asyncio.get_running_loop()
    infos = await loop.getaddrinfo(host, port, type=socket.SOCK_STREAM)
    addresses = list(dict.fromkeys(info[4][0] for info in infos))

    def attempt(address: str):
        return asyncio.open_connection(
            address, port, ssl=context, server_hostname=server_hostname
        )

    if len(addresses) == 1:
        return await attempt(addresses[0])

    tasks = [asyncio.ensure_future(attempt(address)) for address in addresses]
    winner = None
    try:
        for finished in asyncio.as_completed(tasks):
            with contextlib.suppress(OSError, ssl.SSLError):
                winner = await finished
                return winner
        # Every attempt failed; re-raise the first address's reason.
        return tasks[0].result()
    finally:
        for task in tasks:
            task.cancel()
        # Reap the losers, closing any that connected in the same instant.
        for outcome in await asyncio.gather(*tasks, return_exceptions=True):
            if isinstance(outcome, tuple) and outcome is not winner:
                outcome[1].close()


#: How long a finished check waits for its connection to close. A plain socket
#: closes at once; a TLS one waits for the peer's close_notify, which a front end
#: that has already answered owes us nothing for.
CLOSE_GRACE = 0.5


async def _close(writer: asyncio.StreamWriter | None) -> None:
    if writer is None:
        return
    writer.close()
    with contextlib.suppress(OSError, ssl.SSLError, TimeoutError):
        await asyncio.wait_for(writer.wait_closed(), CLOSE_GRACE)


def _load_failed(endpoint: Endpoint, detail: str, started: float, stage: str = "") -> Check:
    check = _failed(endpoint, LOAD, detail, started)
    log.warning(
        "front end check failed endpoint=%s address=%s after=%sms%s error=%s",
        endpoint.id,
        endpoint.address,
        check.ms,
        f" waiting_for={stage!r}" if stage else "",
        detail,
    )
    return check


async def verify_load(endpoint: Endpoint, timeout: float) -> Check:
    """Run only the lightweight front-end check for a draft endpoint."""
    return await _load(endpoint, timeout)


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
    log.info("collector probe start endpoint=%s address=%s", endpoint.id, address)
    started = time.perf_counter()
    try:
        facts = await asyncio.wait_for(transport.probe(), timeout + 1.0)
    except TimeoutError:
        diagnostics = await _diagnostics(transport, address)
        detail = f"no answer within {timeout:g}s; {diagnostic_text(diagnostics)}"
        log.error("collector probe timeout endpoint=%s diagnostics=%s", endpoint.id, diagnostics)
        return _failed(endpoint, COLLECT, detail, started, address)
    except Exception as exc:  # noqa: BLE001 - every transport fails differently
        diagnostics = await _diagnostics(transport, address)
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
    check = Check(
        endpoint=endpoint.id,
        kind=COLLECT,
        result=OK,
        address=address,
        detail=named or f"answered with {len(facts.filesystems)} filesystem(s)",
        ms=_elapsed(started),
    )
    log.info(
        "collector probe ok endpoint=%s address=%s elapsed=%sms identity=%s",
        endpoint.id,
        address,
        check.ms,
        check.detail,
    )
    return check


async def _diagnostics(transport: object, address: str) -> dict[str, object]:
    """The effective connection settings, built only once a probe has failed.

    Building them loads private keys and parses SSH config -- a hundred milliseconds
    or more of synchronous work. On the event loop that would be charged to every
    other check in flight, and on success nobody reads the answer. So: afterwards,
    and in a thread.
    """
    explain = getattr(transport, "diagnostics", None)
    if explain is None:
        return {"address": address}
    return await asyncio.to_thread(explain)


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
