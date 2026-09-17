# Metrix — API / Engine Contract

The short document. What the two halves are, how they hand off to each other, and what is settled.

Companions: [design-api.md](design-api.md) — control plane, observation, front end. [design-engine.md](design-engine.md) — load generation and measurement.

*Section numbers are preserved from the original combined outline, so cross-references between these three documents remain valid. Numbers are therefore not contiguous within any one file.*

---

## 1. What this is

Two capabilities that work hand in hand, either of which is useful alone:

- **Observation** — record and compare statistics about processes and hosts running a service. Usable entirely on its own: point it at an environment, record, and compare against a previous recording. No load test required.
- **Load generation** — apply a defined synthetic load, with test definitions **authored by machines**. An LLM reads a codebase, finds the routes, infers the request shapes and the chains between them, and emits a test plan. A human reviews and tweaks the mixture.

Run together, they are the interesting case: the load series and the host series share one clock and one timeline (§10), so "p99 climbed" and "iowait spiked" can be read against each other instead of guessed at.

Three properties drive every decision below:

1. **Machine-authored plans.** The plan format is the primary API surface. It has to be writable without a feedback loop — declarative, flat, statically validatable, with errors that point at the offending field.
2. **Short test windows.** Runs measured in tens of seconds to a few minutes, run often, during development. This is a statistics problem as much as an engineering one (§11).
3. **Easy-to-modify mixtures.** Changing "20% writes" to "40% writes" is a one-line edit that does not invalidate comparison against previous runs.

**Protocol scope: HTTP/1.1 and HTTP/2 only.** (There is no HTTP/1.2; reading that as 1.1.) No gRPC, no WebSocket — decided, not deferred. The transport layer is written directly against `hyper` with no abstraction trait, because a speculative trait for protocols we have ruled out costs clarity now and buys nothing later.

Non-goals: gRPC and WebSocket targets, distributed multi-node generation, long-running soak tests, synthetic monitoring, a general-purpose scripting runtime.

---

## 2. Architecture

```
  browser (static page, SSE)
        │
        ▼
  ┌────────────────────────────┐
  │  Python API (FastAPI, uv)    │   SQLite: plans, profiles, runs, series
  │  discovery │ observer │ store │   files:  summaries, events, error samples
  └────┬──────────────────┬─────┘
       │ plan bundle           │ SSH / scrape, 1s samples
       │ (calls + mix + targets)│
       ▼                       │
  ┌──────────────────┐  HTTP  │
  │  metrix-engine     │───────┼────▶ target hosts / containers
  │  (Rust, portable)  │◀───────┘
  └──────────────────┘
       NDJSON back to the API — or to a file, if run by hand
```

The engine is detachable: it takes a plan bundle and produces a stream. The API is a convenient way to produce that spec and consume that stream, not a prerequisite for either. Long term, the same binary and plan bundle are copied to a dedicated load box and driven over SSH; nothing in the architecture treats that as a special case.

The observer is an independent subsystem. A recording can be started with no plan attached at all — that is the monitoring-only mode described in §10.

Target discovery (§3) sits in the Python API and feeds both the engine and the observer a resolved endpoint list. The engine holds no cloud credentials and makes no control-plane calls.

---

## C1. The handoff

Exactly two things cross the boundary. Everything else is private to one side.

**Into the engine: a plan bundle.** A directory containing `mix.json`, `targets.json`, `calls/`, and any generators and datasets (§4.4). The API assembles and exports one; a person can write one by hand. The engine reads it and needs nothing else — no database, no API, no cloud credentials.

**A stored plan is the mix and the calls; `targets.json` is never stored with it.** It is written at assembly time from a named profile, because the profile is the thing that knows how to resolve a hostname into the boxes actually behind it (§3.1) and the engine knows nothing about profiles. One plan against three environments is three bundles differing in one file. A `targets.json` found in a stored plan — an exported bundle that was edited and re-imported — is reported and then ignored rather than obeyed or refused: silently honouring it would give two sources of truth, and refusing it would break the export-edit-reimport loop the export exists for.

**The assembled bytes are deterministic, and that is a correctness requirement.** The engine identifies a plan by a SHA-256 over the exact bytes it read, and that hash is part of a run's series identity (§17.2). If assembling the same plan twice produced different bytes — a reordered object, a different indent, a zip carrying the clock — the hash would move and every run would start a fresh series with no history. So documents are written one way: sorted keys, two-space indent, trailing newline, UTF-8, LF; and zip entries take a fixed timestamp.

The hash rule is implemented on both sides — length-framed path, path, length-framed content, content, in sorted path order — so the API can record what a run will be identified by before the run starts. Two implementations of one identity can disagree, so `scripts/check-bundle-contract.py` assembles a bundle, hands it to the real binary, and compares the API's hash with the `plan_hash` the engine reports. It is checked against the thing itself rather than against a copy of the same assumption.

**Out of the engine: NDJSON.** Two streams with different volumes and consumers:

| Stream | Cadence | Consumer |
|---|---|---|
| `--summary` | 250ms aggregate snapshots incl. serialized HDR histograms | Live view, stats table, charts |
| `--events` | One record per request, optionally sampled | Drill-down, error samples, archive |

Record shapes are frozen in `schema/events.schema.json` before either side implements them, and the API ingests a recorded fixture stream from the start. That is what keeps integration a wiring exercise rather than a negotiation.

### C1.1 Launching a run

1. The API resolves a profile into `targets.json` (§3.1) and assembles the bundle.
2. It spawns `metrix-engine --plan <bundle>` and reads `--summary` from the pipe.
3. The engine runs its own phased timeline (§10.1) across every target in sequence, emitting snapshots.
4. The API interleaves those with its observer's host samples on one clock, streams to the browser (§2.5), and persists.
5. The engine exits with a code reflecting SLO evaluation (§16); the API records the verdict.

The engine never calls back. It has no address for the API, no notion that one exists, and no behavior that changes when one is attached. If the reader stalls, the engine annotates `events_dropped` and keeps going — nothing downstream can perturb a measurement in progress.

**The engine's phase timeline becomes the recording's.** The engine owns the phases (design-engine §10.1) and the observer is watching whatever the run is pointed at, so a `baseline` the engine declares is the window every host sample inside it was collected during. The transitions are written against the recording's own boxes rather than the engine's target: traffic goes to one socket and statistics come from another, usually on a different machine (§3.4), and a phase filed under a target with no samples is a window nothing can be read over. One timeline, because the engine runs one target; a sequential sweep will give each target its own, and this will have to say which timeline a host sample belongs to rather than assuming there is only one.

**What produced a run is pinned from `run_started`.** Plan name, plan hash, engine version, seed and machine profile are the engine's to state, and the recording has columns waiting for them. The plan hash matters most: it is what a run is identified by, so without it a stored run cannot be told apart from a run of a plan that has since been edited.

**The two clocks are resolved into one at ingest.** The engine counts from its own start; the observer has been collecting since before the engine was spawned, because that is what a baseline phase is. A load figure and a host figure carrying the same `t_ms` must describe the same moment, or every chart that overlays them misrepresents cause and effect. The offset is taken from the wall-clock instant each side says its own clock started — the engine's `run_started.started_at` against the recording's — and applied to every record. Not the moment a record arrived: that is delayed by pipe buffering and by whatever the reader was busy with, and it would fold the API's own latency into the alignment of a measurement. Both are the same machine's clock, because the engine is a child process, which is the only reason this is sound. The engine's original `t_ms` is stored beside the shifted one, so the alignment can be audited rather than believed.

**A shared crosshair is not a shared axis.** Every chart on the page is drawn over the recording's whole span rather than its own metric's, because load data begins wherever the run was launched and host data begins at zero. Left to fit their own data, two charts put the same moment at different pixels — and reading one against the other is the entire purpose of putting them on one page.

**Reading the stream must never stall the engine.** A full pipe is backpressure, and the engine's answer to backpressure is to drop records and annotate, so a slow reader silently costs measurement data. The summary stream is drained continuously into the store rather than gathered and processed at the end, and stderr is drained on its own task so a diagnostic large enough to fill its buffer cannot deadlock the run. Cancellation asks the engine to finish rather than killing it: a killed engine loses whatever is still in its output queue, including the record that says how the run went.

**What the engine says about itself is stored only where it is measured.** `self_metrics_unavailable` covers exactly the OS resource probes it names — `cpu_pct`, `rss_bytes`, `open_fds` — which the frozen v1 schema requires as numbers and the engine sends as zero until their collectors exist. Those are dropped rather than stored, because a chart drawing `cpu_pct: 0` reports a generator loafing along at nothing, which is the most reassuring wrong answer this tool could give. The scheduler's own gauges — `in_flight`, `queue_depth`, `drift_ms` — are measured and are kept, including their zeros: nothing in flight is a reading, not an absence. Treating the annotation as a blanket over everything self-reported throws away three real numbers a run is judged by.

### C1.2 What each side owns

| API owns | Engine owns |
|---|---|
| Profiles, ECS discovery, inventories | The load path, end to end |
| Host observation (SSH / scrape) | Phases, rates, chains, sessions, auth |
| Storage, series, comparison, purge | Statistics: histograms, percentiles, CIs |
| Front end, live stream | Generator self-metrics and calibration |
| Bundle assembly and export | Sequential multi-target execution |
| Plan generation from OpenAPI/HAR | SLO evaluation and exit codes |

### C1.3 Build order

The shared foundation comes first:

- [x] **F0.1** — Repo skeleton, Rust workspace, `uv init`, committed lockfiles
- [x] **F0.2** — `metrix-plan` types: call, mix, targets — separate from the start
- [x] **F0.3** — Generated schemas + `scripts/check-schema.sh`; NDJSON shapes frozen in `schema/events.schema.json`
- [x] **F0.4** — CI: `cargo test`, `cargo clippy -D warnings`, `uv run pytest`, schema drift check

After that the tracks are independent: [implementation-api.md](implementation-api.md) and [implementation-engine.md](implementation-engine.md). Track A through A3 is a working host-observation tool with no engine installed; track B runs bundles from a shell with no API.


---

## 18. Relationship to the initial design

[initial-design.md](initial-design.md) described a server-stats recorder that could *trigger* load tests. This outline keeps that recorder as a first-class, independently usable capability (§2.3, §9.7, §10.2) and adds the machine-authored load generator beside it. Neither is subordinate: you can record an environment without ever generating load, and the load engine runs without an observer attached — but the phased timeline in §10 is built so that running them together is the default and the most informative case.

Three items from the initial doc are kept deliberately: **hostname-to-instance tracing, which grew into §3** — the initial doc listed "traces from hostname to ECS instances as well as just accepting IP addresses" and that turned out to be foundational rather than a convenience, since it is what makes per-container testing and per-container observation address the same identities; SSH-based direct collection (no agent install needed to get value on day one); and the explicit autoscaling gap. That gap is worth restating in the new framing: **a short test window against an autoscaling target measures pre-scale capacity plus scaling delay, and reports them as one number.** Detecting instance-count changes mid-run and annotating the charts is the minimum viable handling, and §3.1 now supplies what that needs — the ASG name with its desired/min/max, re-resolved at each phase boundary, surfacing as the `host_count_changed` annotation (§13.1). Reading the *configured* scaling trigger and evaluating how long it actually takes to fire remains out of scope, as the initial doc concluded.

---

## 19. Decisions taken, and what is out of scope

### 19.1 Decided

| Question | Decision |
|---|---|
| Protocols | **HTTP/1.1 and HTTP/2 only.** No gRPC, no WebSocket, no transport abstraction (§1). |
| Auth | **First-class `auth` block** (§6); auth traffic excluded from load metrics, single-flight refresh. |
| Target discovery | **ECS only** (§3), in the Python API via boto3, read-only IAM. The engine holds no credentials and makes no control-plane calls. Explicit endpoint lists cover everything else. |
| Multi-target testing | **Sequential sweep over every resolved target** (§3.5). No concurrency, no subset sampling. |
| Plan discovery | **`POST /api/plans/generate`** (§8) — deterministic skeleton from OpenAPI 3/Swagger 2/WADL/WSDL/HAR/access log. **No chain inference.** |
| Dynamic generation | **Embedded Lua as the default tier** (§7.2), generating the whole request. Rust plugin for heavy cases, exec sidecar as the escape hatch. |
| Lua file access | **Read-only within the plan directory**; corpus loaded once into memory at run start under a size ceiling. No I/O on the hot path. |
| Error retention | **First N errored calls per error class**, default 10, secrets redacted (§9.3). |
| Generator headroom | **Configurable worker threads**; calibration measured as a curve across thread counts (§13.2). |
| Cross-run comparison | **Run series** (§17.2–16.6) grouped by setup identity, with measured noise floors. |
| Series segmentation | **None.** The identity tuple defines the series; a setup change is simply a different series (§17.2). |
| Python tooling | **uv** — `pyproject.toml` + committed `uv.lock`, `uv sync` / `uv run`. No pip, no hand-managed venv (§2.2). |
| Plan structure | **Three separate documents** — calls, mix, targets (§4) — composed into a bundle. Authored in the UI by default; exportable and hand-editable. |
| Mixture unit | **Named chains with percentages of one total rate, summing to 100** (§4.5). A percentage buys chain *iterations*, not requests. `--chain <name>` takes the same name the mix uses. |
| Sessions | **Declared per chain**: `fresh` / `reuse` / `pool` (§4.2), binding cookie jar and auth identity together. |
| Engine independence | **The engine takes one self-contained plan bundle and owns sequential multi-target execution** (§2.1, §3.5). No Python wrapper commands; portable to a load box by copying a binary and a directory. |
| Front end | **One static page** — `index.html`, Tabler CSS, ES modules, all loaded up front. No Jinja, no framework, no build step (§2.4). |
| Live view transport | **SSE, not WebSocket** (§2.5). One-way stream, 1s cadence, `Last-Event-ID` replay; control actions are ordinary POSTs. |
| UI layout | **Desktop only** (§2.4). Stats table and charts are separate pages (§14, §15). |
| Data retention | **Keep everything, roll up nothing.** A button purges per-request events and error-sample bodies on demand (§17.1). |

### 19.2 Out of scope

Deliberately excluded, recorded here so they are not rediscovered later as gaps:

- **gRPC and WebSocket targets** — HTTP/1.1 and HTTP/2 only.
- **Kubernetes and other non-ECS discovery** — ECS plus explicit endpoint lists. The inventory shape (§3.3) would accommodate another provider, but nothing is abstracted in advance of one.
- **Distributed multi-box generation** — a single box is the design point. The summary format stays mergeable so it remains possible, and no attempt is made to predict when it would be needed: if the ceiling is ever reached, the `generator_limited` annotation (§13.1) reports it honestly, which is the whole requirement.
- **Chain inference from captured traffic** (§8.3).
- **Series segmentation and bridging** (§17.2).
- **Mobile and responsive layouts** — desktop only (§2.4).
- **Long-running soak tests, synthetic monitoring, distributed tracing integration.**
- **Evaluating configured autoscaling triggers** — instance-count changes are detected and annotated (§18); reading a scaling policy and timing how long it takes to fire is not attempted, as the initial design concluded.
