"""Reading the engine's NDJSON into the store, on the recording's clock.

The engine counts milliseconds from its own start. The observer has been collecting
since before the engine was spawned — that is what a baseline phase is — so the two
clocks have different zeros, and a load figure and a host figure carrying the same
`t_ms` would describe different moments. **Resolving that offset once, and applying
it to every record, is what this module is for.** Everything else here is shape.

The offset comes from the wall-clock instant each side says its own clock started:
the engine's `run_started.started_at` against the recording's `started_at`. Not the
moment the record arrived — that is delayed by pipe buffering and by however long
the reader was busy, and it would fold the API's own latency into the alignment of a
measurement. Both are the same machine's clock, because the engine is a child
process, which is the only reason this is sound.

Records are applied as they arrive rather than buffered to the end. A run interrupted
by a crash is still worth having up to the point it stopped — the same reasoning that
draws a gap rather than hiding it.
"""

from __future__ import annotations

import json
import sqlite3
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any

from metrix_api.observer.metrics import Annotation
from metrix_api.store import recordings as store
from metrix_api.store.db import transaction

#: The scalar series a chart can draw straight onto the host axis. Named so they
#: cannot collide with a host metric, and chosen because each answers a question
#: design-api 15 asks of this page: did we apply the load we asked for, and are we
#: measuring the target or ourselves.
SCALARS = {
    "load.target_rate": "target_rate",
    "load.achieved_rate": "achieved_rate",
    "load.in_flight": "in_flight",
    "load.queue_depth": "queue_depth",
    "load.drift_ms": "drift_ms",
}

#: The engine's terminal record carries these. `stopped_because` is null on a clean
#: finish and is the reason otherwise.
RUN_FINISHED = "run_finished"

#: The engine raises this when some of what it reports about itself is a
#: placeholder rather than a reading.
UNAVAILABLE_MARKER = "self_metrics_unavailable"

#: Exactly which fields that annotation covers: the OS resource probes inside the
#: `generator` document, which the frozen v1 schema requires as numbers and the
#: engine sends as zero until their collectors exist. Named explicitly because the
#: annotation does *not* cover the scheduler's own gauges -- `in_flight`,
#: `queue_depth` and `drift_ms` are measured (design-engine B1.5) and dropping them
#: would throw away real numbers. Reading a placeholder zero as health is the
#: expensive mistake; refusing a measured one is merely a silent loss, and both are
#: avoided by saying which is which rather than by treating the annotation as a
#: blanket.
UNAVAILABLE_FIELDS = ("cpu_pct", "rss_bytes", "open_fds")


class IngestError(Exception):
    """A stream that cannot be read onto this recording's clock."""


def _parsed(stamp: str) -> datetime:
    return datetime.fromisoformat(stamp.replace("Z", "+00:00"))


@dataclass(slots=True)
class Ingest:
    """One engine stream, landing in one recording.

    Stateful on purpose: the clock offset is learned from `run_started` and every
    later record depends on it, so this is a thing with a beginning rather than a
    function called per line.
    """

    conn: sqlite3.Connection
    recording_id: str
    #: When the recording's own clock started, in wall time.
    started_at: datetime

    offset_ms: int | None = None
    windows: int = 0
    records: int = 0
    #: Records that arrived before `run_started` and so could not be placed on the
    #: recording's clock. Counted rather than guessed at.
    unplaced: int = 0
    #: True once the engine has said the generator's own numbers are not real.
    self_metrics_unavailable: bool = False
    exit_code: int | None = None
    stopped_because: str | None = None
    #: Chains and steps seen, so a caller can describe the run without a query.
    chains: set[str] = field(default_factory=set)

    def line(self, text: str) -> None:
        """Apply one NDJSON line. Blank lines are skipped, not an error."""
        text = text.strip()
        if not text:
            return
        try:
            record = json.loads(text)
        except json.JSONDecodeError as exc:
            # Location, never the line: a request record carries a URL and headers.
            raise IngestError(
                f"engine stream: invalid JSON at column {exc.colno}"
            ) from exc
        if not isinstance(record, dict) or "type" not in record:
            raise IngestError("engine stream: a record with no type")
        self.record(record)

    def record(self, record: dict[str, Any]) -> None:
        self.records += 1
        kind = record["type"]
        if kind == "run_started":
            self._start(record)
        elif kind == "summary":
            self._summary(record)
        elif kind == "annotation":
            self._annotation(record)
        elif kind == RUN_FINISHED:
            self._finish(record)
        # phase_changed, target_started/finished and request records are read by
        # later steps; they are counted here and deliberately not invented into
        # tables nothing yet reads.

    # ------------------------------------------------------------------ the clock

    def _start(self, record: dict[str, Any]) -> None:
        """Learn the offset between the engine's zero and the recording's.

        `run_started` carries `t_ms: 0` by definition, so its `started_at` is the
        wall-clock instant of the engine's zero. The difference from the recording's
        own start is the number every later `t_ms` is shifted by.
        """
        try:
            engine_start = _parsed(record["started_at"])
        except (KeyError, ValueError) as exc:
            raise IngestError("engine stream: run_started carries no usable start time") from exc

        offset = (engine_start - self.started_at).total_seconds() * 1000.0
        if offset < 0:
            # The engine cannot have started before the recording did. Clamping
            # rather than failing: a clock that moved is worth a note, not a lost run.
            offset = 0.0
        self.offset_ms = round(offset)

    def at(self, engine_t_ms: int) -> int | None:
        """One engine timestamp on the recording's clock."""
        if self.offset_ms is None:
            return None
        return engine_t_ms + self.offset_ms

    # ----------------------------------------------------------------- the shapes

    def _summary(self, record: dict[str, Any]) -> None:
        engine_t = int(record.get("t_ms", 0))
        t_ms = self.at(engine_t)
        if t_ms is None:
            self.unplaced += 1
            return

        target = record["target_id"]
        with transaction(self.conn):
            self.conn.execute(
                """
                INSERT OR REPLACE INTO load_window
                    (recording_id, target_id, t_ms, engine_t_ms, phase, window_ms,
                     target_rate, achieved_rate, in_flight, queue_depth, drift_ms,
                     bytes_sent, bytes_received, connections_opened, connections_reused,
                     generator)
                VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
                """,
                (
                    self.recording_id,
                    target,
                    t_ms,
                    engine_t,
                    record.get("phase", "measure"),
                    int(record.get("window_ms", 0)),
                    record.get("target_rate"),
                    record.get("achieved_rate"),
                    # Measured by the scheduler itself, and kept as reported. A zero
                    # here is a real reading: nothing in flight is worth knowing.
                    record.get("in_flight"),
                    record.get("queue_depth"),
                    record.get("drift_ms"),
                    int(record.get("bytes_sent", 0)),
                    int(record.get("bytes_received", 0)),
                    int(record.get("connections_opened", 0)),
                    int(record.get("connections_reused", 0)),
                    self._generator(record.get("generator")),
                ),
            )
            for chain, body in (record.get("chains") or {}).items():
                self.chains.add(chain)
                self._chain(target, t_ms, chain, body)
        self.windows += 1

    def _generator(self, generator: dict[str, Any] | None) -> str | None:
        """What the engine says about itself, minus anything it has said is not real.

        The placeholder fields are removed rather than stored as zero. A chart that
        drew `cpu_pct: 0` would report a generator loafing along at nothing, which is
        the single most reassuring wrong answer this tool could give -- §13.2 exists
        to ask whether the generator is the thing being measured, and a confident
        zero answers "no" when the truth is "nobody looked".
        """
        if generator is None:
            return None
        if self.self_metrics_unavailable:
            generator = {
                key: value for key, value in generator.items() if key not in UNAVAILABLE_FIELDS
            }
        return json.dumps(generator, sort_keys=True)

    def _chain(self, target: str, t_ms: int, chain: str, body: dict[str, Any]) -> None:
        counters = (
            int(body.get("iterations_started", 0)),
            int(body.get("iterations_completed", 0)),
            int(body.get("iterations_aborted", 0)),
        )
        self._row(target, t_ms, chain, None, "duration", counters, body.get("duration"), None)
        for step, detail in (body.get("steps") or {}).items():
            counts = (
                int(detail.get("attempted", 0)),
                int(detail.get("completed", 0)),
                int(detail.get("failed", 0)),
            )
            statuses = detail.get("statuses")
            self._row(target, t_ms, chain, step, "total", counts, detail.get("total"), statuses)
            if detail.get("ttfb") is not None:
                self._row(target, t_ms, chain, step, "ttfb", counts, detail["ttfb"], None)

    def _row(
        self,
        target: str,
        t_ms: int,
        chain: str,
        step: str | None,
        kind: str,
        counters: tuple[int, int, int],
        histogram: dict[str, Any] | None,
        statuses: dict[str, int] | None,
    ) -> None:
        histogram = histogram or {}
        self.conn.execute(
            """
            INSERT OR REPLACE INTO load_row
                (recording_id, target_id, t_ms, chain, step, kind,
                 attempted, completed, failed,
                 count, min_us, max_us, mean_us, hdr, statuses)
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
            """,
            (
                self.recording_id,
                target,
                t_ms,
                chain,
                # '' is the chain's own row; the schema's CHECK ties it to `kind`.
                step or "",
                kind,
                *counters,
                int(histogram.get("count", 0)),
                histogram.get("min_us"),
                histogram.get("max_us"),
                histogram.get("mean_us"),
                histogram.get("hdr"),
                json.dumps(statuses, sort_keys=True) if statuses else None,
            ),
        )

    def _annotation(self, record: dict[str, Any]) -> None:
        """The engine's notes become the recording's notes.

        One stream of annotations, whatever produced them: a collection gap from the
        observer and dropped output from the generator are both things a reader must
        know before quoting a number, and two lists would mean two places to look.
        """
        code = record.get("code", "engine_annotation")
        if code == UNAVAILABLE_MARKER:
            # Everything this run says about the generator's own health is a
            # placeholder. Remember it so no gauge is stored as though measured.
            self.self_metrics_unavailable = True

        from_ms = self.at(int(record.get("from_ms", record.get("t_ms", 0))))
        if from_ms is None:
            self.unplaced += 1
            return
        to = record.get("to_ms")
        store.add_annotation(
            self.conn,
            self.recording_id,
            Annotation(
                code=code,
                severity=record.get("severity", "info"),
                target_id=record.get("target_id"),
                phase=record.get("phase"),
                from_ms=from_ms,
                to_ms=self.at(int(to)) if to is not None else None,
                message=record.get("message", ""),
                detail=record.get("detail") or {},
                source="engine",
            ),
        )

    def _finish(self, record: dict[str, Any]) -> None:
        self.exit_code = record.get("exit_code")
        self.stopped_because = record.get("stopped_because")
        with transaction(self.conn):
            self.conn.execute(
                "UPDATE recording SET engine_exit_code = ?, stopped_because = ? WHERE id = ?",
                (self.exit_code, self.stopped_because, self.recording_id),
            )
