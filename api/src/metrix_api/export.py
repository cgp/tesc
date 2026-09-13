"""Taking a recording somewhere else: JSON, CSV, and a report that stands alone.

design-api 17.1 asks for three, and they are three because they answer three
different questions. JSON is *give me everything, I will process it*. CSV is *give me
the series, I am putting it in a spreadsheet*. The HTML report is *give me something I
can send to somebody who does not have this tool*, which is the only one of the three
with a reader who cannot ask a follow-up question — so it is also the one that has to
carry its own explanations.

**Self-contained means self-contained.** The report has no stylesheet link, no script
tag and no image URL: it is one file that renders identically on a laptop with no
network, which is the same reason the application itself vendors everything it draws
with. The charts are inline SVG, generated here, for want of any way to draw a line
without a plotting library that would have to be fetched.

The sample-count rule holds in all three. A figure the count cannot support is absent
from the JSON, is an empty cell in the CSV, and is an em dash carrying its count in
the report — never a zero anywhere, and never a bare number.
"""

from __future__ import annotations

import csv
import io
import sqlite3
from html import escape
from typing import Any

from metrix_api import analysis
from metrix_api.stats import Summary
from metrix_api.store import inventories
from metrix_api.store import recordings as store

#: Roughly the proportions of a chart on a page, and small enough that six of them do
#: not make a file nobody opens. The viewBox does the scaling, so these are ratios.
CHART_WIDTH = 900
CHART_HEIGHT = 180
CHART_PAD = 28


def document(conn: sqlite3.Connection, recording_id: str) -> dict[str, Any]:
    """Everything known about one recording, in one JSON-serialisable object.

    The full export: what it was, what it ran against, every sample, every note, and
    the summaries — so a consumer never has to recompute a percentile and get a
    different answer from the one on screen.
    """
    recording = store.get(conn, recording_id)
    summaries = analysis.summaries(conn, recording_id)
    stored = inventories.for_recording(conn, recording_id)

    return {
        "recording": {
            "id": recording.id,
            "kind": recording.kind,
            "status": recording.status,
            "profile": recording.profile,
            "addressing_mode": recording.addressing_mode,
            "api_version": recording.api_version,
            "series_key": recording.series_key,
            "started_at": recording.started_at,
            "finished_at": recording.finished_at,
            "duration_ms": recording.duration_ms,
            "is_baseline": recording.is_baseline,
            "purged_at": recording.purged_at,
            "note": recording.note,
            "targets": recording.targets,
        },
        "summaries": {
            target: {metric: s.to_document() for metric, s in by_metric.items()}
            for target, by_metric in summaries.items()
        },
        "phases": [
            {"target_id": p["target_id"], "phase": p["phase"],
             "from_ms": p["from_ms"], "to_ms": p["to_ms"]}
            for p in store.phases(conn, recording_id)
        ],
        "annotations": [
            {"code": a["code"], "severity": a["severity"], "target_id": a["target_id"],
             "from_ms": a["from_ms"], "to_ms": a["to_ms"], "message": a["message"]}
            for a in store.annotations(conn, recording_id)
        ],
        "gaps": [
            {"target_id": g["target_id"], "from_ms": g["from_ms"],
             "to_ms": g["to_ms"], "reason": g["reason"]}
            for g in store.gaps(conn, recording_id)
        ],
        "identity": store.identities(conn, recording_id),
        "filesystems": store.filesystem_usage(conn, recording_id),
        "inventory": stored.inventory.to_document() if stored else None,
        # Last, and the only large part: a reader streaming this can stop before it.
        "samples": [
            {"target_id": t, "t_ms": ms, "metric": m, "value": v}
            for t, ms, m, v in store.samples(conn, recording_id)
        ],
    }


def series_csv(conn: sqlite3.Connection, recording_id: str) -> str:
    """Every sample, one per row, in the shape a spreadsheet expects.

    Long rather than wide: a column per metric would need a column set fixed before
    the first row is written, and the collectors deliberately report whatever a host
    exposes. Long also survives a target that started answering halfway through,
    which wide turns into a block of blanks.
    """
    out = io.StringIO()
    writer = csv.writer(out, lineterminator="\n")
    writer.writerow(["recording_id", "target_id", "t_ms", "metric", "value"])
    for target, t_ms, metric, value in store.samples(conn, recording_id):
        writer.writerow([recording_id, target, t_ms, metric, value])
    return out.getvalue()


def summary_csv(conn: sqlite3.Connection, recording_id: str) -> str:
    """The table as it reads on screen, with the count beside every figure.

    A second CSV rather than a flag on the first, because these go to different
    places: the series goes into a chart somebody is building, and this goes into a
    ticket. `n` is a column and not a footnote — a spreadsheet is exactly where a
    figure gets separated from its caveat.
    """
    out = io.StringIO()
    writer = csv.writer(out, lineterminator="\n")
    writer.writerow(["target", "metric", "n", "min", "median", "p95", "max", "stddev"])
    for target, by_metric in sorted(analysis.summaries(conn, recording_id).items()):
        for metric, s in sorted(by_metric.items()):
            writer.writerow(
                [
                    target,
                    metric,
                    s.n,
                    _csv_number(s.minimum),
                    _csv_number(s.p50),
                    _csv_number(s.p95),
                    _csv_number(s.maximum),
                    _csv_number(s.stddev),
                ]
            )
    return out.getvalue()


def _csv_number(value: float | None) -> str:
    """Empty for a figure the count could not support.

    Empty rather than zero, and empty rather than the string "None": a spreadsheet
    averages a zero and chokes on a word, and both are worse than a blank cell that
    a person can see is blank.
    """
    return "" if value is None else f"{value:.6g}"


# ------------------------------------------------------------------- the report


def report_html(conn: sqlite3.Connection, recording_id: str) -> str:
    """One file, no network, readable by somebody without this tool."""
    recording = store.get(conn, recording_id)
    summaries = analysis.summaries(conn, recording_id)
    comparison = analysis.against_baseline(conn, recording_id)
    annotations = store.annotations(conn, recording_id)
    gaps = store.gaps(conn, recording_id)
    spans = store.spans(conn, recording_id)

    metrics = sorted({m for by_metric in summaries.values() for m in by_metric})
    charts = "".join(
        _chart(conn, recording_id, metric, recording.targets, gaps) for metric in metrics
    )

    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{escape(recording.id)} — Metrix</title>
<style>{_CSS}</style>
</head>
<body>
<h1>{escape(recording.id)}</h1>
<p class="lede">{escape(recording.kind)} recording of
  <strong>{escape(recording.profile or "—")}</strong> via
  {escape(recording.addressing_mode)}, started {escape(recording.started_at)}.</p>

{_worst_banner(annotations, recording)}

<h2>What it was</h2>
{_facts(recording, spans)}

<h2>The numbers</h2>
<p class="note">Every figure carries the sample count behind it. One the count cannot
  support is an em dash with that count, never a zero: a median over eleven readings
  and one over six hundred are different claims.</p>
{_summary_table(summaries, metrics)}

{_comparison_section(comparison)}

<h2>Over time</h2>
<p class="note">Gaps in collection are drawn as breaks in the line and never joined
  across. A straight segment through a window nothing was collected in is a picture
  of data that does not exist.</p>
{charts or '<p class="note">No samples were collected.</p>'}

<h2>Notes</h2>
{_annotations(annotations)}

<h2>Collection gaps</h2>
{_gaps(gaps)}

<footer>Generated by Metrix {escape(recording.api_version)} from recording
  <code>{escape(recording.id)}</code>. Series
  <code>{escape(recording.series_key)}</code>.
  {"The request-level data for this recording was purged on "
   + escape(recording.purged_at) + "; every figure above is unaffected."
   if recording.purged_at else ""}</footer>
</body>
</html>
"""


_CSS = """
:root { color-scheme: light; }
body { font: 15px/1.55 system-ui, -apple-system, Segoe UI, sans-serif;
       color: #1c2333; background: #fff; margin: 0 auto; padding: 2rem 1.5rem 4rem;
       max-width: 62rem; }
h1 { font-size: 1.5rem; margin: 0 0 .25rem; }
h2 { font-size: 1rem; text-transform: uppercase; letter-spacing: .04em;
     color: #667382; margin: 2.25rem 0 .5rem; border-bottom: 1px solid #e6e7e9;
     padding-bottom: .35rem; }
.lede { color: #667382; margin: 0 0 1.5rem; }
.note { color: #667382; font-size: .875rem; max-width: 70ch; margin: .25rem 0 .75rem; }
table { border-collapse: collapse; width: 100%; font-size: .875rem; }
th, td { text-align: left; padding: .3rem .5rem; border-bottom: 1px solid #f0f1f3; }
th { color: #667382; font-weight: 600; }
td.num, th.num { text-align: right; font-variant-numeric: tabular-nums; }
td.name { font-weight: 600; }
small { color: #667382; }
.dash { color: #98a2b3; }
.grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(11rem, 1fr));
        gap: .75rem 1.5rem; margin-bottom: .5rem; }
.grid dt { color: #667382; font-size: .75rem; text-transform: uppercase;
           letter-spacing: .03em; }
.grid dd { margin: 0; }
.banner { padding: .75rem 1rem; border-radius: .375rem; margin: 0 0 1.5rem;
          font-size: .875rem; }
.banner.invalid { background: #fdeaea; color: #8a1f1f; }
.banner.warn { background: #fdf3e7; color: #8a4b12; }
.banner.clean { background: #eaf6ec; color: #1c6b2c; }
.chart { margin: 0 0 1.25rem; }
.chart figcaption { color: #667382; font-size: .8125rem; margin-bottom: .25rem; }
svg { display: block; width: 100%; height: auto; }
code { font: .8125rem ui-monospace, SFMono-Regular, Menlo, monospace; }
footer { margin-top: 3rem; padding-top: .75rem; border-top: 1px solid #e6e7e9;
         color: #667382; font-size: .8125rem; }
"""


def _worst_banner(annotations: list[sqlite3.Row], recording: store.RecordingRow) -> str:
    """Whether these numbers can be trusted, before any of them are read.

    At the top, because the reader of a report is the one person who cannot open the
    recording and check — and an `invalid` note found after the figures have been
    quoted is a note that arrived too late.
    """
    severities = {a["severity"] for a in annotations}
    if "invalid" in severities:
        codes = sorted({a["code"] for a in annotations if a["severity"] == "invalid"})
        return (
            '<p class="banner invalid"><strong>These numbers cannot be trusted.</strong> '
            f"This recording carries {escape(', '.join(codes))}, which is why it cannot "
            "be a baseline. Read the notes below before quoting anything here.</p>"
        )
    if "warn" in severities:
        return (
            '<p class="banner warn"><strong>Read the notes before quoting this.</strong> '
            "Something happened during this recording that affects how its figures "
            "should be read.</p>"
        )
    return (
        '<p class="banner clean">No warnings. Every interval was collected and nothing '
        "was flagged during this recording.</p>"
    )


def _facts(recording: store.RecordingRow, spans: dict[str, dict[str, int]]) -> str:
    items = {
        "Profile": recording.profile or "—",
        "Addressing": recording.addressing_mode,
        "Status": recording.status,
        "Length": _duration(recording.duration_ms),
        "Started": recording.started_at,
        "Finished": recording.finished_at or "—",
        "Targets": str(len(recording.targets)),
        "Baseline": "yes" if recording.is_baseline else "no",
    }
    grid = "".join(
        f"<div><dt>{escape(k)}</dt><dd>{escape(v)}</dd></div>" for k, v in items.items()
    )

    # A target whose last sample is well before the end stopped answering, and a
    # column of medians will never say so.
    rows = "".join(
        f"<tr><td class='name'>{escape(target)}</td>"
        f"<td class='num'>{span['n']}</td>"
        f"<td class='num'>{span['first_ms'] / 1000:.0f}s</td>"
        f"<td class='num'>{span['last_ms'] / 1000:.0f}s</td></tr>"
        for target, span in sorted(spans.items())
    )
    table = (
        "<table><thead><tr><th>Target</th><th class='num'>Samples</th>"
        "<th class='num'>First</th><th class='num'>Last</th></tr></thead>"
        f"<tbody>{rows}</tbody></table>"
        if rows
        else ""
    )
    return f"<dl class='grid'>{grid}</dl>{table}"


def _summary_table(summaries: dict[str, dict[str, Summary]], metrics: list[str]) -> str:
    targets = sorted(t for t in summaries if t != analysis.ENVIRONMENT)
    order = [*targets, analysis.ENVIRONMENT] if analysis.ENVIRONMENT in summaries else targets

    rows = []
    for target in order:
        label = "every box pooled" if target == analysis.ENVIRONMENT else target
        for metric in metrics:
            summary = summaries[target].get(metric)
            if summary is None:
                continue
            rows.append(
                f"<tr><td class='name'>{escape(label)}</td><td>{escape(metric)}</td>"
                f"<td class='num'>{summary.n}</td>"
                f"<td class='num'>{_figure(summary, summary.minimum)}</td>"
                f"<td class='num'>{_figure(summary, summary.p50)}</td>"
                f"<td class='num'>{_figure(summary, summary.p95)}</td>"
                f"<td class='num'>{_figure(summary, summary.maximum)}</td>"
                f"<td class='num'>{_figure(summary, summary.stddev)}</td></tr>"
            )

    return (
        "<table><thead><tr><th>Target</th><th>Metric</th><th class='num'>n</th>"
        "<th class='num'>Min</th><th class='num'>Median</th><th class='num'>p95</th>"
        "<th class='num'>Max</th><th class='num'>Std dev</th></tr></thead>"
        f"<tbody>{''.join(rows)}</tbody></table>"
    )


def _figure(summary: Summary, value: float | None) -> str:
    if value is None:
        return f"<span class='dash'>—<small> n={summary.n}</small></span>"
    return _number(summary.metric, value)


def _comparison_section(comparison: analysis.Comparison) -> str:
    if not comparison.baseline_id:
        return ""
    moved = comparison.moved
    if not moved:
        return (
            "<h2>Against normal</h2><p class='note'>Nothing moved beyond the band the "
            f"baseline's own spread supports. Compared with "
            f"<code>{escape(comparison.baseline_id)}</code>.</p>"
        )
    rows = "".join(
        f"<tr><td class='name'>{escape(d.metric)}</td>"
        f"<td>{escape('every box' if target == analysis.ENVIRONMENT else target)}</td>"
        f"<td class='num'>{_figure(d.baseline, d.baseline.p50)}</td>"
        f"<td class='num'>{_figure(d.current, d.current.p50)}</td>"
        f"<td class='num'>{'+' if d.change > 0 else ''}{_number(d.metric, d.change)}"
        f" <small>({'+' if d.change > 0 else ''}{d.change_pct:.1f}%)</small></td></tr>"
        for target, d in moved
    )
    return (
        "<h2>Against normal</h2>"
        "<p class='note'>Compared with the baseline for this series, "
        f"<code>{escape(comparison.baseline_id)}</code>. A metric is listed only where "
        "it moved further than the baseline's own spread — a band measured, not "
        "chosen.</p>"
        "<table><thead><tr><th>Metric</th><th>Where</th><th class='num'>Normally</th>"
        "<th class='num'>This time</th><th class='num'>Change</th></tr></thead>"
        f"<tbody>{rows}</tbody></table>"
    )


def _annotations(annotations: list[sqlite3.Row]) -> str:
    if not annotations:
        return "<p class='note'>None.</p>"
    rows = "".join(
        f"<tr><td class='name'>{escape(a['severity'])}</td>"
        f"<td><code>{escape(a['code'])}</code></td>"
        f"<td>{escape(a['message'])}</td></tr>"
        for a in annotations
    )
    return (
        "<table><thead><tr><th>Severity</th><th>Code</th><th>What happened</th>"
        f"</tr></thead><tbody>{rows}</tbody></table>"
    )


def _gaps(gaps: list[sqlite3.Row]) -> str:
    if not gaps:
        return "<p class='note'>None. Every interval was collected.</p>"
    rows = "".join(
        f"<tr><td class='name'>{escape(g['target_id'])}</td>"
        f"<td class='num'>{g['from_ms'] / 1000:.0f}s</td>"
        f"<td class='num'>{g['to_ms'] / 1000:.0f}s</td>"
        f"<td>{escape(g['reason'] or '')}</td></tr>"
        for g in gaps
    )
    return (
        "<table><thead><tr><th>Target</th><th class='num'>From</th>"
        f"<th class='num'>To</th><th>Why</th></tr></thead><tbody>{rows}</tbody></table>"
    )


def _chart(
    conn: sqlite3.Connection,
    recording_id: str,
    metric: str,
    targets: list[str],
    gaps: list[sqlite3.Row],
) -> str:
    """One metric, every target, as inline SVG.

    Hand-drawn because the report must render with no network and no script, and a
    plotting library would be neither. It gets one thing right that matters more than
    anything else it could do: a gap is a break in the line. Each target's points are
    split into runs wherever a recorded gap falls, and each run is its own polyline —
    so a window nothing was collected in is empty rather than spanned.
    """
    series = {
        target: store.series(conn, recording_id, target, metric) for target in targets
    }
    series = {t: points for t, points in series.items() if points}
    if not series:
        return ""

    xs = [t for points in series.values() for t, _ in points]
    ys = [v for points in series.values() for _, v in points]
    x_min, x_max = min(xs), max(xs)
    y_min, y_max = min(ys), max(ys)
    if y_max == y_min:
        y_min, y_max = y_min - 1, y_max + 1
    x_span = max(1, x_max - x_min)

    def place(t: float, v: float) -> tuple[float, float]:
        x = CHART_PAD + (t - x_min) / x_span * (CHART_WIDTH - 2 * CHART_PAD)
        y = CHART_HEIGHT - CHART_PAD - (v - y_min) / (y_max - y_min) * (
            CHART_HEIGHT - 2 * CHART_PAD
        )
        return x, y

    lines = []
    for i, (target, points) in enumerate(sorted(series.items())):
        colour = _COLORS[i % len(_COLORS)]
        for run in _unbroken(points, [g for g in gaps if g["target_id"] == target]):
            if len(run) == 1:
                x, y = place(*run[0])
                lines.append(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="2" fill="{colour}"/>')
                continue
            path = " ".join(f"{x:.1f},{y:.1f}" for x, y in (place(t, v) for t, v in run))
            lines.append(
                f'<polyline points="{path}" fill="none" stroke="{colour}" '
                'stroke-width="1.5" stroke-linejoin="round"/>'
            )

    legend = " ".join(
        f'<tspan fill="{_COLORS[i % len(_COLORS)]}">■</tspan> {escape(target)}'
        for i, target in enumerate(sorted(series))
    )
    return f"""<figure class="chart">
  <figcaption>{escape(metric)} — {escape(_caption(metric))}</figcaption>
  <svg viewBox="0 0 {CHART_WIDTH} {CHART_HEIGHT}" role="img"
       aria-label="{escape(metric)} over the recording">
    <rect x="{CHART_PAD}" y="{CHART_PAD}" width="{CHART_WIDTH - 2 * CHART_PAD}"
          height="{CHART_HEIGHT - 2 * CHART_PAD}" fill="none" stroke="#f0f1f3"/>
    <text x="{CHART_PAD}" y="{CHART_PAD - 8}" font-size="11" fill="#667382"
      >{escape(_number(metric, y_max))}</text>
    <text x="{CHART_PAD}" y="{CHART_HEIGHT - 8}" font-size="11" fill="#667382"
      >{escape(_number(metric, y_min))} · {x_min / 1000:.0f}s to {x_max / 1000:.0f}s</text>
    <text x="{CHART_WIDTH - CHART_PAD}" y="{CHART_HEIGHT - 8}" font-size="11"
          text-anchor="end">{legend}</text>
    {"".join(lines)}
  </svg>
</figure>"""


def _unbroken(
    points: list[tuple[int, float]], gaps: list[sqlite3.Row]
) -> list[list[tuple[int, float]]]:
    """Split a target's points wherever a recorded gap falls.

    The whole reason this function exists: a polyline drawn straight through a
    collection gap is indistinguishable from a flat healthy stretch, at exactly the
    moment somebody is looking for the opposite.
    """
    if not gaps:
        return [points] if points else []

    runs: list[list[tuple[int, float]]] = [[]]
    for point in points:
        previous = runs[-1][-1] if runs[-1] else None
        if previous is not None and any(
            previous[0] <= g["from_ms"] and point[0] >= g["to_ms"] for g in gaps
        ):
            runs.append([])
        runs[-1].append(point)
    return [run for run in runs if run]


_COLORS = ["#206bc4", "#d63939", "#2fb344", "#f76707", "#ae3ec9", "#0ca678"]

_CAPTIONS = {
    "cpu.busy": "how hard the machine was working, ignoring idle and steal",
    "cpu.user": "time in application code rather than the kernel",
    "cpu.system": "time in the kernel — high with heavy IO or syscall traffic",
    "cpu.iowait": "time blocked on storage; latency tracking this is a disk problem",
    "mem.used_bytes": "memory in use; a line that only ever climbs is the leak signal",
    "mem.available_bytes": "what is left before the workload starts swapping",
    "conn.established": "open connections — whether keepalive is doing its job",
    "proc.count": "processes on the box; a count that climbs and never falls is a leak",
    "fd.open": "open file descriptors, which leak and then fail hard",
}


def _caption(metric: str) -> str:
    return _CAPTIONS.get(metric, "over the life of the recording")


def _number(metric: str, value: float) -> str:
    """Units are never implied, and the rule matches what the page shows."""
    if metric.endswith("_bytes"):
        size, unit = abs(value), "B"
        for candidate in ("KB", "MB", "GB", "TB"):
            if size < 1024:
                break
            size /= 1024
            unit = candidate
        sign = "-" if value < 0 else ""
        return f"{sign}{size:.1f} {unit}" if unit != "B" else f"{sign}{size:.0f} B"
    if metric.startswith("cpu.") or metric.endswith("_pct"):
        return f"{value:.1f}%"
    return f"{value:.2f}"


def _duration(ms: int | None) -> str:
    if ms is None:
        return "—"
    seconds = round(ms / 1000)
    return f"{seconds}s" if seconds < 60 else f"{seconds // 60}m {seconds % 60:02d}s"
