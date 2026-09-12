# Metrix — API and Front End Design

The control plane: target discovery, host observation, storage and analysis, and the browser UI. Pairs with [design-engine.md](design-engine.md); the boundary between them is in [design-api-engine-contract.md](design-api-engine-contract.md).

Everything through §17 is buildable and useful with no engine installed — observation-only recordings are a first-class mode, not a degraded one.

*Section numbers are preserved from the original combined outline, so cross-references between these three documents remain valid. Numbers are therefore not contiguous within any one file.*

---

## 2. Architecture — control plane

### 2.2 Python API (FastAPI, managed with uv)

Owns everything except the hot path: **target profiles and ECS discovery (§3)**, plan storage and validation, run lifecycle (spawn engine, tail NDJSON, write to SQLite), the live stream to the browser (§2.5), comparison/reporting, and serving the UI.

Deliberately kept off the request path. The engine is the only thing that touches the target under load.

**The API is the only Python entry point, and the primary authoring surface.** Plans are normally built and edited in the UI, not in a text editor; the API exports a runnable bundle (§4.4) for anyone who wants to run or tweak one by hand. There are no `metrix run` / `observe` / `sweep` CLI wrappers: load tests are launched from the front end or by invoking the engine directly, and observation is driven through the API. A Python wrapper around the engine would undercut the portability rule above by making the useful path depend on the control plane.

**Tooling: uv.** `pyproject.toml` plus a committed `uv.lock`, `uv sync` to set up, `uv run` to launch — no manually managed virtualenv, no `requirements.txt`, no `pip` in any instruction or script. Resolution and install are fast enough that a clean environment is not a thing anyone avoids doing, which matters for a tool whose results depend on knowing exactly what was running.

The lockfile is part of run reproducibility for the same reason the engine version is (§9.8): the API computes the statistics that runs are compared on, and a silently upgraded dependency that changes a percentile calculation would be invisible without it. The API's own version and lock hash are recorded in run metadata.

### 2.3 Observer (collector)

Owns target-side collection, and has no dependency on the load engine. Given a resolved inventory from §3 it samples host and process statistics at 1s and writes them into the same run timeline the engine writes to, keyed on the same target identities the load metrics use.

- Transports: SSH-exec of a small stats script (works day one, nothing to install), a node-exporter-style HTTP scrape, or the Docker/ECS API for container targets. Same normalized output shape from all three, so charts and comparisons don't care which was used.
- Runs as part of a load run, or on its own from the front end — observation-only recordings (§10.2) are started and stopped in the UI, not from a command line.
- Target discovery is per-profile and re-resolved at each phase boundary, so a host appearing or disappearing mid-run is recorded as an event rather than a gap in a series.
- A collection failure degrades rather than aborts: the affected series is marked unavailable for that interval and the run continues, with the gap drawn explicitly on the chart rather than interpolated.

An endpoint carries two addresses for two purposes: `address` is the load target, `collect` is a separate connection host statistics are read from. Neither is derived from the other, nothing is inferred from what listens on the load target, and what is collected is whole-machine rather than per-process. Written up for users in [profiles.md](profiles.md).

### 2.4 UI (Tabler, vertical left nav)

Three sections, per the discussion:

| Section | Purpose |
|---|---|
| **Config** | What this process is and where it keeps things: the resolved `$METRIX_HOME`, which rule chose it, the database. Plan library with editor (form + raw JSON view), **plan generation from OpenAPI/WSDL/HAR (§8)**, inline validation, auth blocks, mixture weights showing live "this implies N RPS on /foo"; phase durations (§10), engine threading, and SLO thresholds. |
| **Profiles** | The environments a run can be pointed at: create, edit and delete them, endpoint by endpoint, plus **hostname discovery and the resolved inventory (§3)**. The editor submits a whole document and the server runs the same validation a hand-written file goes through, so a form cannot save what the loader would reject. A profile's name is fixed after creation — it is part of a recording's series identity (§17.2), so renaming one through the editor would split its history in two. |
| **Performance › Stats** | **The numbers, as a table** (§14). Per chain and per step: start, finish, median, standard deviation, counts, errors. Updates once per second during a run. No charts on this page. |
| **Performance › Charts** | The same run drawn (§15) — load and observation on one shared time axis, current-vs-target RPS, host stats, error feed, generator-health strip, phase indicator, stop/abort. No tables on this page. |
| **Recordings** | Archive of everything captured, load runs and observation-only recordings alike. Grouped into **series** by setup identity (§17.2) with trend charts and regression flags; filter by plan/target/tag/mode, mark a recording as **baseline**, overlay N runs, inspect retained error samples, export (JSON / CSV / static HTML report). |

"Recordings" covers both modes deliberately — an observation-only recording and a load run are the same object with different sections populated, so they compare against each other with the same machinery.

**A single static page.** One `index.html`, Tabler's CSS, and JavaScript — all loaded up front, with the API serving JSON and the event stream and nothing else. No server-side templating, no Jinja, no framework, no build step.

**Fullscreen and fluid.** The page fills the window: no centred max-width column, just a small even inset at the edges. This is a measurement tool read a table at a time, so horizontal space goes to target columns rather than to whitespace. The inset is one variable (`--tblr-gutter-x` on the page containers).

**Nothing on the page is fetched at render time.** Tabler is vendored under `web/vendor/` and the icons are inline SVG rather than a font or a sprite sheet: a tool that watches a private network must draw itself without the public one, and a menu whose icons arrive on a second request arrives late. `scripts/check.sh` fails if a CDN URL reappears.

The JavaScript is split into ES modules along clear seams so it stays maintainable without tooling:

| Module | Responsibility |
|---|---|
| `api.js` | Every fetch call; the only place a URL appears |
| `stream.js` | SSE connection, reconnect, `Last-Event-ID` replay |
| `state.js` | Current run state; the single source the views read |
| `table.js` | The stats table (§14) |
| `charts.js` | uPlot setup and updates (§15) |
| `config.js` | Profiles, plans, validation display |
| `recordings.js` | Archive, series, comparison views |
| `ui.js` | Icons and empty states — the markup the views share |

Views subscribe to `state.js` and re-render from it; nothing else talks to the network. Charts use uPlot, which handles thousands of points at 60fps without fighting us.

**Desktop only.** No mobile layout, no responsive breakpoints, no touch affordances — this is a wide-screen tool for reading dense tables and multi-series charts side by side, and narrowing it would cost exactly the density that makes it useful. Where "responsive" appears in this document it means *does not lag* (§2.5), never *reflows for small screens*.

### 2.5 The live view

A running test updates **once per second**, on both the stats table and the charts.

**Transport: Server-Sent Events, not WebSocket.** Nothing here streams client-to-server — the view is one-way, and control actions (stop, abort, annotate) are ordinary POSTs, simpler to authorize and log than messages multiplexed into a socket. SSE gives exactly what this needs for free: automatic reconnection, plain HTTP/2 with no upgrade or heartbeat plumbing, and **`Last-Event-ID` replay**, so a dropped connection resumes where it left off instead of leaving a hole in the chart. WebSocket earns its place when the browser needs to stream something back; until then it is a second protocol for no gain.

- **Cadence is decoupled from the engine.** The engine emits 250ms snapshots (§2.1); the API aggregates and pushes once per second. Engine cadence can change without touching the UI.
- **Payloads are deltas**, with a full snapshot as the first event after connect or reconnect, so a late-joining viewer is immediately correct. Each event carries counters since the last, current percentiles, phase and elapsed time, generator health, and any annotations raised in the interval.
- **Slow clients coalesce rather than queue** — a view thirty seconds behind is worse than one that skipped thirty seconds. Events fan out from a single engine stream, so viewer count adds no per-viewer load.
- **The stream is never on the measurement path.** The engine writes to a pipe and never blocks on a consumer; if the API stalls, `events_dropped` is annotated (§13.1) and the run continues unaffected. Nothing a browser does can perturb a test in progress.

---

## 3. Target profiles and discovery

A **target profile** is a named, versioned description of where to send traffic and what to observe. Plans reference it by name (`target.profile`), so a plan stays portable across environments and the same plan can be pointed at staging, at production, or at one specific container without editing the plan.

Discovery is what turns a hostname into that list. It is the step between "we have a service" and "we can test and observe it", and in AWS it is several hops.

### 3.1 Discovery from a hostname

```
api.staging.example.com
  ↓  DNS / Route 53
ALB or NLB DNS name
  ↓  elasticloadbalancing: describe_load_balancers → listeners → rules
target group(s)
  ↓  elasticloadbalancing: describe_target_health
registered targets (instance ids, or IP:port for awsvpc)
  ↓  ecs: list_services → describe_services  (matched on target group ARN)
ECS service + cluster
  ↓  ecs: list_tasks → describe_tasks
tasks → containers, ENIs, private IPs, task definition revision, image digest
  ↓  ec2: describe_network_interfaces / describe_instances
EC2 instance id, type, AZ, private DNS
  ↓  autoscaling: describe_auto_scaling_groups   (EC2 launch type)
ASG name, desired / min / max
```

Every hop is optional. A hostname that resolves straight to an instance, or an NLB with IP targets and no ECS behind it, stops early and yields what it found. **Partial resolution is the normal case and is reported as a result, not a failure** — the profile records how far it got and what it could not determine, which is far more useful than an error.

**Scope: ECS only.** No EKS/Kubernetes, no plain-Docker discovery, and no provider-neutral abstraction layer written in advance of a second provider. Two entry points cover everything needed:

1. **Discovery** — the chain above, from a hostname or from an ECS cluster + service named directly (skipping DNS and the load balancer).
2. **An explicit endpoint list** — hostnames or IPs, no discovery at all. This is how the tool works against a local dev server, and it is the day-one path before any AWS wiring exists.

The inventory shape (§3.3) happens to be generic enough that another provider could populate it later. That is a convenience if it ever happens, not a design commitment, and nothing is abstracted for it now.

### 3.2 This belongs to the Python API, not the engine

Discovery is boto3 in the API, run at profile setup and refresh time. **The Rust engine never links an AWS SDK, never holds credentials, and never makes a control-plane call.** It is handed a flat list of resolved endpoints and nothing else.

Three reasons, all of which would be violated by pushing it down:

1. **Credentials and IAM stay in one place.** The engine runs in CI, on a laptop, by hand against a bare URL — giving it a cloud identity to do so would be absurd.
2. **Control-plane calls are slow, rate-limited, and retry-prone.** Those characteristics are acceptable during setup and completely unacceptable anywhere near a send loop that is holding a schedule to the millisecond (§13.2).
3. **The engine stays independently runnable.** A design where the load generator needs AWS to function cannot be tested, debugged, or reused against a local dev server.

Resolution is cached with a TTL, refreshed on demand, at run start, and at phase boundaries (§10.1) so an instance-count change mid-run is detected rather than inferred.

**IAM: read-only.** `route53:List*`, `elasticloadbalancing:Describe*`, `ecs:List*`, `ecs:Describe*`, `ec2:Describe*`, `autoscaling:Describe*`. The repo ships the policy document, because working this out from permission errors is a poor introduction to a tool.

### 3.3 The resolved inventory

Discovery produces a versioned snapshot, stored with the profile and **pinned into every run that uses it**. Per entry:

| Field | Why it is kept |
|---|---|
| Role (lb / instance / task / container) | Determines how it can be addressed and observed |
| Address + port | The endpoint |
| Instance id, type, AZ | Hardware and placement differences explain outliers |
| Task ARN, **task definition revision** | Identifies the deployment |
| Container name, **image digest** | Identifies the build |
| ASG name, desired/min/max | Autoscaling context (§18) |
| Health status, discovered_at | Freshness and eligibility |

The inventory serves both subsystems: it is the engine's endpoint list *and* the observer's collection target list (§2.3), so load metrics and host metrics attach to the same identities and can be joined without guesswork.

Task definition revision and image digest earn their place in cross-run comparison: they are what lets a later analysis say *this is a different build*, which is the difference between a regression and a deployment.

---

## 8. Plan generation from an endpoint

`POST /api/plans/generate` takes a source and returns a draft plan. Yes to the question — with one deliberate boundary.

### 8.1 What the tool does and does not do

**The tool does the mechanical part — which is exactly the calls document** (§4.1). Given a service description it emits one call per operation, parameters filled from schema types and examples, assertions derived from declared response codes and schemas, plus a starter mix with flat weights. No LLM inside the tool — this step is deterministic, so the same input gives the same skeleton, and a diff between two generated plans means the service changed.

**The LLM does the judgment part — which is the mix** (§4.2). Realistic weights, which calls chain into which, what a meaningful assertion is beyond "returned 200", which endpoints matter. The document split falls out of this division naturally: the deterministic half and the judgment half are separate files, so regenerating calls after an API change does not touch a tuned mixture. The generated skeleton is the LLM's starting point rather than its output, which is a much easier task than authoring from nothing and produces far less drift between runs.

### 8.2 Sources

| Source | What it yields |
|---|---|
| **OpenAPI** (URL or file) | Operations, parameter types, request/response schemas, declared status codes. The richest structural source. |
| **WSDL / XSD** | Same for SOAP and XML services; bodies generated from the schema. |
| **HAR capture** | **Realistic mixtures** — observed call frequencies become weights. Chains are *not* inferred (see below). |
| **Access log** | Path frequencies and status distribution; weights grounded in production traffic. |
| **Route list** | Bare minimum: a list of method+path lines, for when nothing else exists. |

HAR and access-log sources are the valuable ones, because they answer the question a schema cannot: what does this service *actually* get asked for? A plan whose mixture comes from observed traffic is worth considerably more than one whose weights were guessed.

### 8.3 Draft semantics

**No chain inference.** Guessing chains from repeated id values across a capture is unreliable in exactly the cases that matter, and a wrong chain is worse than none — it yields a plan that runs cleanly while testing a flow the service does not have. Generation emits single-step chains only; chaining is the LLM's job (it has the codebase, a far better source than a traffic capture) or the author's.

Generated plans come back marked `"draft": true` with `todo` annotations on fields needing human or LLM attention — guessed weights, placeholder values, unchained operations that probably belong in a sequence. The UI surfaces these as a checklist on the Config screen, and `draft: true` plans are flagged when run, so a provisional mixture is never mistaken for a reviewed one.

The intended loop, end to end:

```
codebase / OpenAPI / HAR
        ↓  POST /api/plans/generate
   draft plan (deterministic skeleton)
        ↓  LLM fills mixture, chains, assertions
   candidate plan
        ↓  POST /api/plans/validate
   errors with JSON Pointer paths → LLM repairs → revalidate
        ↓
   run

---

## 9. Metrics — target-side and derived

*The load-side metric categories (§9.1–9.6, §9.8) are produced by the engine; see design-engine.md.*

### 9.7 Target-side metrics (the observation capability)
Collected by the observer (§2.3) over SSH, HTTP scrape, or container API, sampled at 1s and aligned to the run's monotonic clock. Captured in every phase — including the baseline and settle windows — and captured on its own in observation-only mode (§10.2). Each metric below is reported as an absolute value, as a **delta from its baseline-phase median**, and as a per-phase summary:
- CPU (user / sys / iowait / steal), load average
- Memory used / available / cached, swap activity
- Disk IOPS, throughput, await, queue depth, free space
- Network throughput, packet drops, retransmits, conntrack table usage
- Established connection count, listen-queue overflows / SYN backlog drops
- Process-level: RSS, thread count, open FDs, restarts
- Runtime-specific where available: JVM GC pause time/frequency and heap-after-GC; Go goroutine count and GC pause; Python GIL contention
- Application pool stats where exposed: thread-pool queue depth, DB connection-pool wait time

Recovery metrics, derived from the settle phase and meaningless without it:
- Time for each metric to return within a tolerance band of its baseline value (**time-to-recover**)
- Peak value reached *after* traffic stopped — queues, GC, and flushes often peak during drain, not under load
- Whether a metric fails to return to baseline at all within the settle window (leak signal: memory, FDs, threads, connections)

This channel answers *why*: p99 climbing exactly as iowait spikes is a different bug from p99 climbing while the box is idle — and a box that was already at 55% CPU before you sent a single request is a different story from one that started clean, which is what the baseline phase is there to tell you.

### 9.9 Derived / comparative
- Delta vs baseline for every headline metric, with the "worse" direction known per metric
- SLO pass/fail table (§16)
- Cost-per-request proxies: CPU-seconds per request, bytes per request
- Efficiency curve: achieved RPS per unit of target CPU, per ramp step

---

## 10. Observation-only mode

*The phased run timeline (§10.1) is engine-side; see design-engine.md.*

### 10.2 Observation-only mode

A recording with no plan attached: baseline and settle collapse into one continuous observation window, started and stopped manually or by duration. Everything in §9.7 and §9.8 is captured; §9.1–6.6 are simply absent.

This is the mode for "what does this box normally look like?" — and it is what makes baseline comparison worth anything. A saved observation-only recording can be marked as the **environment baseline** for a profile, and later load runs can be compared against it rather than only against their own baseline phase. That answers a question a single run cannot: is this environment behaving normally *today*, before we even applied load?

Observation-only recordings can also be triggered on a schedule, so an environment accumulates a normal-behavior history over time.

---

## 14. The stats table

A page of its own, separate from the charts. The two answer different questions and are read at different moments — the table is what gets exact numbers read off it, quoted in a ticket, and scanned for the row that looks wrong; the charts are for shape and timing. Putting both on one page makes each worse, so the Performance section has two pages and neither borrows from the other.

The table is live during a run, updating once per second (§2.5), and is the same view for a finished run.

### 14.1 Rows

One row per step, grouped under its chain, with a run-total row at the top. Chain rows aggregate their steps; chains additionally get an end-to-end row, since chain duration is not the sum of step medians. Sortable on any column, and the grouping collapses so a 40-step plan stays readable.

### 14.2 Columns

Default visible:

| Column | Notes |
|---|---|
| Name | Chain / step id, indented by grouping |
| **Start** | Timestamp of the row's first completed request, relative to run start. Not the run's start — a chain's third step begins whenever its chain reaches it, and a step that never started at all is diagnostic. |
| **Finish** | Timestamp of the row's last completed request, relative to run start |
| Count | Completed requests |
| **Median** | p50 latency, ms |
| **Std dev** | Latency standard deviation, ms |
| Errors | Count and percentage |
| RPS | Achieved, averaged over the row's active window |

Available from the column picker: min, max, p75, p90, p95, p99 (with its confidence interval, §12.1), TTFB median, bytes sent/received, connection reuse, retries, chain aborts. Absolute wall-clock timestamps are on hover; relative figures are the default because those are what compare across runs.

**On standard deviation:** it is here because it was asked for, and it genuinely earns a column — a median of 40ms beside a standard deviation of 300ms tells you at a glance that a step is bimodal or unstable, which no percentile shows as quickly. Worth knowing that latency distributions are right-skewed, so std dev overstates the typical spread and is a poor thing to set a threshold on; median and p95 describe the experience better, and sit adjacent in the table for that reason. Nothing hidden — just don't let std dev be the number that decides something.

### 14.3 Live behavior

- Updates once per second from the same stream as the charts (§2.5).
- **Tabular numerals and fixed column widths**, so figures update in place without rows reflowing. A table that jitters every second is unreadable, and this is a one-line CSS decision that makes the difference between a live view people watch and one they wait out.
- Rows carrying errors or annotations are marked in place, and the marker links to the retained error samples (§9.3).
- Current phase and elapsed time sit in the table header, so numbers are never read without knowing which phase produced them.
- Copy-to-clipboard as TSV and export as CSV, both reflecting current filtering and sort — the common case is pasting these numbers somewhere else.

### 14.4 Comparison mode

The same table with a second run's values alongside and a delta column — used for run-vs-baseline and for sweep comparison (§17.6). Deltas are colored by whether the direction is good or bad for that metric rather than by sign, and are suppressed where the sample count cannot support the claim (§12.1). A table is exactly where a spurious "p99 +18%" gets quoted without its caveat.

---

## 15. Charts

The charts page carries no tables; exact figures live in §14. Principles: every axis labeled with units; every series named as it appears in the plan; shared x-axis and synchronized crosshair across all time series, load and host alike; log scale available on latency; **phase boundaries and ramp-stage boundaries drawn as vertical annotations with shaded phase bands on every chart**; gaps in collection drawn as gaps, never interpolated; **run annotations (§13.1) drawn as shaded regions on the charts they affect**; and no chart ships without a one-line caption stating what it answers.

| Chart | Answers |
|---|---|
| Target vs achieved RPS over time, stacked by chain | Did we apply the load we asked for? |
| Latency percentile bands over time (p50/p95/p99, log y) | When did it degrade? |
| Latency histogram + CDF for the run | What shape — long tail or bimodal? |
| Error rate + status-code stacked area | What broke, and when? |
| Chain failure histogram by step id | Where in the flow does it break? |
| In-flight concurrency vs cap, with cap-reached regions shaded (§13.1) | Are we measuring the target or our own limiter? |
| Generator headroom vs demanded rate over time (§13.2) | How close are we to measuring ourselves? |
| Phase-breakdown stacked bar per endpoint (DNS/connect/TLS/TTFB/transfer) | Which part of the request is slow? |
| Throughput and latency vs offered load, one point per step, knee and cliff marked (§11.5) | Where's the knee, where's the ceiling? |
| Per-step small multiples: latency distribution at each rate | How does the shape change as it loads up? |
| Error composition by step — timeouts vs refused vs 5xx | What kind of failure is the cliff? |
| Connection reuse ratio over time | Is keepalive working? |
| Target CPU / memory / IO overlaid on the latency chart, phase bands shaded | Why did it degrade? |
| Host metrics across the full timeline, baseline median drawn as a horizontal reference line | How far from normal did it get? |
| Per-phase summary table: baseline vs measure vs settle, per host metric | What did the load actually change, and did it come back? |
| Recovery curve — settle phase only, time-to-baseline per metric | How long does it take to recover? |
| Generator health strip (always visible during a run) | Is this run valid? |
| Baseline comparison: overlaid percentile bands + delta table | Better or worse than last time? |
| Observation-only recording vs environment baseline | Is this environment behaving normally today? |
| **Trend: metric per run across a series, with noise band (§17.3)** | Is this getting better or worse over time? |
| **Trend: knee / cliff / max sustained rate per run** | Is our capacity moving? |
| Baseline-phase host stats across runs | Is the test environment itself drifting? |
| **Sweep ranking: one bar per target per headline metric, spread band drawn (§17.6)** | Which host or container is the odd one out? |
| Per-target trend across repeated sweeps | Is it always that one? |

---

## 17. Recordings & comparison

### 17.1 Storage

- Every run persisted: metadata, rolled-up metric series, HDR histograms, error samples (§9.3), and optionally sampled per-request events.
- Mark any run as **baseline** for a plan + target pair — blocked, absent explicit override, for runs carrying an `invalid` annotation (§13.1).
- Breakpoint runs additionally store knee / cliff / max-sustained as named scalars.
- Export: JSON (full), CSV (series), and a self-contained static HTML report.
- **Everything is kept by default, and nothing rolls up.** Summaries, histograms, inventories, and annotations are small — a full series loads into memory comfortably — so there is no retention tiering, no aged rollup, and no reduced-fidelity historical points. Trend charts read the real numbers at every age.
- **Underlying call data is purged by a button, not by a policy.** Per-request event records and retained error-sample bodies (§9.3) are the only bulky things stored; a single action drops them for a run, a sweep, or a whole series, leaving the summaries, histograms, and annotations intact. Manual and explicit beats a retention policy that quietly deletes the evidence for the one run somebody needed.

### 17.2 Run series — comparing across runs of the same setup

The primary comparison unit is not "this run vs that run" but **the series**: every run sharing the same setup, ordered in time. This is the view that answers the question actually worth asking — *is this getting better or worse?* — and a two-run diff cannot answer it, because a two-run diff has no idea what normal variation looks like.

**Setup identity** is the tuple that has to match for runs to belong to the same series:

```
plan hash + target profile + addressing mode + load mode/rate + engine version + machine profile
```

**Addressing mode** (through the load balancer vs direct to a container, §3.4) is part of the tuple because the two measure different network paths. Task definition revision and image digest from the inventory (§3.3) are recorded on the run but deliberately *not* part of the identity — a new build is exactly what you want to see as a movement within a series, not as a reason to start a new one. A build change is drawn on the trend chart as a marker.

All five matter. Comparing across a changed plan, a rebuilt generator, or different generator hardware and attributing the difference to the target is the most expensive mistake this view can make, so the tuple is computed automatically and runs group themselves — no manual tagging, which would be skipped exactly when it mattered.

**When any element of the tuple changes, the runs simply belong to a different series.** There is no segmentation machinery, no break markers, and no bridging — the identity tuple already does the whole job. Change the plan and you have started a new history; the old one is still there, under its own identity. Simple, and it fails safe: the only way to compare across a setup change is to open the two series deliberately and side by side.

### 17.3 The trend view

For any metric, a point per run against time, with:

- **The noise band.** Rolling median and spread (IQR) of the last N runs in the series, drawn behind the points. This is the series' own noise floor, measured rather than assumed, and it is what makes "regression" a meaningful word: a movement inside the band is not one.
- **Confidence-aware plotting.** Metrics are drawn with the interval their sample count supports (§12.1). At the 2250-request floor the p99 trend line carries a visibly wide band, which is honest and stops a noisy p99 from generating a bug hunt every third run.
- **Annotation markers** on any run carrying warnings, and `invalid` runs excluded from the band by default (shown as hollow points, since knowing a run failed validity is itself part of the history).
- **Environment drift, from the baseline phase.** Baseline-phase host stats trended across runs answer a question no single run can: is the test environment itself changing under us? A p95 that has crept up 30% alongside a baseline CPU that crept up 30% is not an application regression.

Trendable metrics include p50/p95/p99 with intervals, error rate, achieved vs target RPS, chain completion rate, connection reuse, per-phase host stats, recovery time from settle, and — for breakpoint runs — **knee, cliff, and max sustained rate**. That last set is the most valuable longitudinal output the tool produces: capacity over time, one point per run.

### 17.4 Regression detection

A metric is flagged when it moves beyond a configured multiple of the series' noise floor (default 2× IQR), **and** its sample count supports the claim, **and** the run carries no `invalid` annotation. All three conditions, because any one alone produces false positives at a rate that trains people to ignore the flag.

Flags are advisory in the UI and available as a machine-readable verdict for the same CI path as §16, so a pipeline can fail on "outside the historical band" as well as on a fixed SLO threshold. The historical check is usually the more useful of the two — fixed thresholds are guesses made before the data existed.

### 17.5 Multi-run comparison

- Select any N runs from a series and overlay their latency bands, histograms, and CDFs.
- **Aggregate a run group** (§12.2) into a single set of statistics by merging HDR histograms — valid because the histograms are mergeable, unlike percentiles. Five 30s runs merged give 11,250 samples and a p99 with roughly the precision a single 150s run would have had, which is the cheapest route past the short-window sample-count limit.
- Per-phase comparison across runs: baseline vs baseline (environment drift), measure vs measure (the actual question), settle vs settle (is recovery degrading?).
- Step-aligned comparison for breakpoint runs: the same rate step across two runs, side by side.

### 17.6 Sweep comparison

A sweep (§3.5) is its own comparison view: one plan, many targets, one time window.

- Targets ranked on each headline metric, with the sweep's own spread as the reference — the outlier logic of §17.4 applied across targets rather than across time.
- Per-target attributes from the inventory shown alongside (instance type, AZ, image digest, task definition revision), because the explanation for an outlier is usually sitting in that row: an older digest, a different instance type, a lone task in another AZ.
- Baseline-phase stats compared across targets, which catches the case that otherwise sends you chasing a phantom regression — one container was already loaded before the test started.
- Repeated sweeps form a series of sweeps, giving per-target trends: is that container reliably slow, or was it unlucky once?

---

## 20. Plan editing in the UI

The front end is the expected authoring surface (§4), but the three documents are not equally editable — the split of §4 is what makes differentiated permissions natural rather than arbitrary.

| Document | In the UI | Why |
|---|---|---|
| **Mix** (§4.2) | **Full editing** | This is the knob that gets turned: percentages, total rate, duration, phases, session policy per chain |
| **Targets** (§4.3) | **Full editing** | Pick a profile, re-resolve it, select or deselect individual hosts and containers |
| **Calls** (§4.1) | **Read-only inspection** | Request definitions are authored by an LLM from the codebase or generated from a schema; editing them by hand in a browser invites drift from the service they describe |

### 20.1 Editing the mix

The mixture editor is the primary screen. Chains listed with their percentages, edited either as a percentage or as a target RPS (converted and both shown), with a **live total that must reach 100** before the plan can run — the error names the shortfall or excess rather than silently renormalizing.

Alongside each chain: implied iterations/s, implied req/s given its step count, session policy, and the calls it invokes. Adding a chain means selecting from the calls already defined; reordering steps and setting `repeat_until` are in scope. Everything in `load` — rate, duration, warmup, model, concurrency cap, breakpoint parameters — is editable here, with the §12.1 sample-count consequence shown live: a duration and rate that fall below the 2250 floor say so before the run, not after.

### 20.2 Editing targets

Choose a profile and re-resolve it (§3.1), then include or exclude individual entries from the inventory. Each row shows what discovery found — instance type, AZ, task definition revision, image digest, health — because that is what a person needs to decide whether a box belongs in the run. Addressing mode (through the load balancer or direct to a container, §3.4) is a choice here, with its incomparability warning attached.

### 20.3 Inspecting calls

A read-only list, sufficient to understand what a chain does without opening a file:

- **Name** and **description** — a free-text `description` field on every call, which exists for this view. Generated plans populate it from the OpenAPI summary; LLM-authored plans should write one.
- **Method and path template**, with the variables it consumes and extracts made visible, so the dependencies between steps in a chain are legible.
- **Generation strategy** — inline template, dataset, Lua, Rust plugin, or exec sidecar (§7.2), with the generator's name and, for Lua, the file and entry point.
- **Assertions**, listed plainly — this is where a chain like `login-fail` explains itself as expecting a 401.

No editing at this time. The affordance offered instead is **export the bundle** (§4.4): change the call in the file, with the tooling and review a code change gets, and re-import. If hand-editing calls in the browser turns out to be needed, it should arrive as a deliberate decision with validation behind it, not as a text box.
