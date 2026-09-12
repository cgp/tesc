# Metrix — Design Outline

Status: design discussion, not yet implemented.
Supersedes the framing in [initial-design.md](initial-design.md) — see [Relationship to the initial design](#18-relationship-to-the-initial-design).

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

### 2.1 Rust engine (`metrix-engine`)

A standalone binary that takes **one self-contained plan bundle** — calls, mix, and targets in a directory (§4.4) — and emits newline-delimited JSON on stdout. It knows nothing about the API, the database, the UI, or AWS. Run it by hand, in CI, from a shell script, or on a machine that has never heard of the rest of this tool.

**The engine owns sequential multi-target execution** (§3.5), not the API. That follows from the portability requirement: if running one plan against eight containers needed an orchestrator, the binary alone would not be enough to do the job on a remote box. One invocation, one bundle, N targets in sequence, one output stream.

**Portability is a design constraint, not a later feature.** Statically linked where the platform allows, no config outside the bundle, no runtime dependency on the API. The intended long-term deployment is `scp` the binary and the bundle to a load box, run it over SSH, collect the stream — and nothing in the engine's design may assume otherwise. That is also what makes it easy to test: the whole load path is exercisable from a shell with a binary and a directory, and a single call can be fired on its own (§4.1).

- Tokio multi-threaded runtime, `hyper` + `rustls` directly rather than `reqwest`, because we need per-phase timing hooks (DNS / connect / TLS / TTFB) and explicit control of the connection pool.
- **Two output streams:**
  - `--events` — per-request records, optionally sampled (`--sample-rate`), for drill-down and the recordings archive.
  - `--summary` — periodic (default 250ms) aggregate snapshots including serialized HDR histograms. This is what the live charts consume.
- Aggregation happens in-process: per-worker histograms and counters, merged on the snapshot tick, so the hot path does no locking and no per-request allocation.
- Exit code reflects SLO evaluation (§16), so CI can gate on it.

**Why Rust for this specifically:** at a few tens of thousands of RPS from one box, the generator's own GC pauses and scheduler jitter become indistinguishable from the target's latency. A load generator that can't hold its own send schedule produces numbers that describe the generator.

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

### 2.4 UI (Tabler, vertical left nav)

Three sections, per the discussion:

| Section | Purpose |
|---|---|
| **Config** | Target/environment profiles, **hostname discovery and the resolved inventory (§3)**, and what to collect from them; plan library with editor (form + raw JSON view), **plan generation from OpenAPI/WSDL/HAR (§8)**, inline validation, auth blocks, mixture weights showing live "this implies N RPS on /foo"; phase durations (§10), engine threading, and SLO thresholds. |
| **Performance › Stats** | **The numbers, as a table** (§14). Per scenario and per step: start, finish, median, standard deviation, counts, errors. Updates once per second during a run. No charts on this page. |
| **Performance › Charts** | The same run drawn (§15) — load and observation on one shared time axis, current-vs-target RPS, host stats, error feed, generator-health strip, phase indicator, stop/abort. No tables on this page. |
| **Recordings** | Archive of everything captured, load runs and observation-only recordings alike. Grouped into **series** by setup identity (§17.2) with trend charts and regression flags; filter by plan/target/tag/mode, mark a recording as **baseline**, overlay N runs, inspect retained error samples, export (JSON / CSV / static HTML report). |

"Recordings" covers both modes deliberately — an observation-only recording and a load run are the same object with different sections populated, so they compare against each other with the same machinery.

**A single static page.** One `index.html`, Tabler's CSS, and JavaScript — all loaded up front, with the API serving JSON and the event stream and nothing else. No server-side templating, no Jinja, no framework, no build step.

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

**IAM: read-only.** `route53:List*`, `elasticloadbalancing:Describe*`, `ecs:List*`, `ecs:Describe*`, `ec2:Describe*`, `autoscaling:Describe*`. The repo ships the policy document, because working this out from permission errors is a bad first hour with a tool.

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

### 3.4 Addressing an individual container

Bypassing the load balancer to hit one container is the main reason discovery exists, and it needs handling the plan author should not have to think about:

- The request goes to the task's private IP and container port.
- **The `Host` header is overridden to the original hostname.** Most services route or vhost on it; a raw IP gets a 404 or a default backend. The profile does this automatically — getting it wrong produces a plausible-looking test of nothing.
- **TLS SNI is set from the original hostname** while connecting to the IP, with certificate verification against that name rather than the address. `insecure_skip_verify` exists, and using it raises a run annotation (§13.1).
- **Reachability is declared and verified at setup.** Private IPs need VPC access: the generator running in-VPC, a VPN, or an SSH tunnel through a bastion. The profile states which, and the API verifies it during setup so the failure surfaces there, with a useful message, rather than as connection-refused at run time.

**Results through the load balancer and results direct to a container are not comparable.** Different network path, no LB queueing, no LB connection reuse, different TLS termination. The profile records which mode it is, and the two never share a run series (§17.2).

### 3.5 Sequential sweep: one plan, many targets

The expected testing pattern: the same plan run against each host or container in turn. **This is not a separate command or a separate mode** — a bundle carries its targets document, and a list of more than one target is a sweep. One target is simply the degenerate case.

```jsonc
// targets.json — produced by the API from a profile, or written directly
{
  "order": "shuffle",           // as_resolved | shuffle
  "gap": "30s",                 // idle between targets
  "list": [
    { "id": "task-a1b2", "address": "10.0.3.41:8080", "host_header": "api.staging.example.com" },
    { "id": "task-c3d4", "address": "10.0.3.77:8080", "host_header": "api.staging.example.com" }
  ]
}
```

The API resolves a profile (§3.1) into that file; by hand, it is written out or exported once and reused. Either way the engine receives concrete addresses and no cloud context. Because targets are a separate document (§4.3), the same mixture runs against a different set of boxes by swapping one file — `--targets other.json`.

**One at a time, never concurrently, across every target in the list.** Sibling containers share a database, a cache, and often a host — run them together and they measure each other. No subset sampling and no parallelism: a sweep is a sequential pass over the full list, and its cost is simply the number of targets times the per-run duration.

- Each target gets its own complete phased run (§10.1), baseline and settle included. Per-target initial conditions are the point: a container that was already hot is visible before its numbers are read.
- Order is as-resolved or randomized. Randomizing decouples results from sweep position, since the first target pays cold-cache costs on shared dependencies that the rest do not.
- An optional inter-run gap lets shared dependencies settle between targets.
- Output is a **sweep**: a set of runs sharing plan, sweep id, and time window, differing only in target.

The comparison this enables is the valuable part — same plan, same conditions, different container. You are looking for the odd one out: a task on a noisy neighbor, an instance of a different type, a container still running an older image digest. The sweep view ranks targets on each headline metric and flags any target outside the sweep's own spread, which is the §17.4 measured-noise-floor logic applied across targets instead of across time.

A sweep deliberately varies the target, so its runs do **not** form a single series under §17.2. They are grouped as a sweep; repeating the whole sweep produces a series *of sweeps*, which gives per-target trends over time and answers "is that one container always the slow one, or was it slow once?"

---

## 4. The plan format

**Three documents, kept deliberately separate**, because they are authored by different parties, change at different rates, and are reused differently:

| Document | Contains | Changes when |
|---|---|---|
| **Calls** — `calls/*.json` | Individual request definitions: method, path, headers, body, assertions, extraction | The service's API changes |
| **Mix** — `mix.json` | Which calls run, in what sequences, at what weights and rates; load shape and phases | The question being asked changes |
| **Targets** — `targets.json` | The boxes to run against (§3) | The environment changes |

The value of the split is that each can move without disturbing the others. A new endpoint adds a call and touches no mixture. Changing "20% writes" to "40% writes" edits one number in one file and leaves every request definition untouched. Pointing the same test at a different set of containers replaces one file. A monolithic plan makes each of those a diff across everything.

It also matches how they are produced: calls are mechanical (derivable from OpenAPI or a codebase, §8), the mix is judgment, and targets come from discovery.

**These are normally not written by hand.** The UI is the authoring surface — edit the mixture, adjust weights, see implied per-endpoint RPS before running. The API exports a complete, runnable bundle for anyone who wants to run or tweak one directly (§4.4), and hand-authoring is fully supported, but it is the exception rather than the assumed workflow.

JSON throughout (YAML accepted and converted), with published JSON Schemas so an LLM can be handed the schema alongside the codebase.

### 4.1 Calls — the individual test call

One file per call, or several grouped in one file. A call knows how to make one request and how to judge the response. It knows nothing about how often it runs or what runs before it.

```jsonc
// calls/products.json
{
  "list-products": {
    "method": "GET",
    "path": "/api/products?page={{ rand(1,50) }}",
    "assert": [
      { "status": 200 },
      { "json": "$.items", "min_length": 1 },
      { "max_latency_ms": 300 }
    ],
    "extract": { "pid": { "json": "$.items[0].id" } }
  },

  "get-product": {
    "method": "GET",
    "path": "/api/products/{{ pid }}",
    "assert": [ { "status": 200 } ]
  },

  "create-order": {
    "method": "POST",
    "path": "/api/orders",
    "headers": { "Content-Type": "application/xml" },
    "body": { "generator": "order-xml", "args": { "user": "{{ users.email }}" } },
    "assert": [ { "status_in": [201, 202] }, { "xpath": "/order/id", "exists": true } ],
    "extract": { "order_id": { "xpath": "/order/id/text()" } }
  }
}
```

Because a call is standalone, it is also the unit you can fire once to check it works — `metrix-engine --call create-order` sends exactly one request and prints the response with assertion results. That is the fastest possible loop for "is this request even right?", and it exists precisely because calls are not tangled into the mixture.

### 4.2 Mix — the load testing mixture

References calls by name, arranges them into scenarios, and sets the shape of the load.

```jsonc
// mix.json
{
  "version": 1,
  "name": "checkout-mixed",
  "calls": ["calls/products.json"],

  "defaults": { "headers": { "Accept": "application/json" }, "timeout_ms": 5000 },
  "auth": { "… §6 …": true },

  "phases": { "baseline": "30s", "settle": "60s" },

  "load": {
    "mode": "fixed",              // "fixed" | "stages" | "breakpoint"  (§11)
    "duration": "60s",            // enforced minimum: duration x rate >= 2250 (§12.1)
    "warmup": "10s",
    "model": "open",              // "open" = fixed arrival rate | "closed" = fixed concurrency
    "rate": 500,
    "max_concurrency": 200        // hitting it annotates the run (§13.1)
  },

  "datasets": {
    "users": { "file": "data/users.csv", "mode": "round_robin" }
  },

  "generators": {
    "order-xml": { "type": "lua", "file": "gen/order.lua", "entry": "generate" }
  },

  "scenarios": [
    { "name": "browse", "weight": 80,
      "steps": [ { "id": "list", "call": "list-products" },
                 { "id": "detail", "call": "get-product" } ] },

    { "name": "submit-order", "weight": 20, "rate": 25,
      "steps": [ { "id": "create", "call": "create-order" },
                 { "id": "poll", "call": "get-order",
                   "repeat_until": { "json": "$.status", "equals": "complete",
                                     "max_attempts": 5, "interval_ms": 200 } } ] }
  ],

  "engine": { "worker_threads": 8, "connections_per_host": 256 },
  "capture": { "error_samples": 10, "body_max_kb": 64, "redact": ["Authorization"] },
  "observe": { "interval_ms": 1000, "collect": ["cpu", "memory", "disk", "net", "process"] },

  "slo": [
    { "metric": "p99_latency_ms", "scenario": "browse", "max": 400 },
    { "metric": "error_rate", "max": 0.001 }
  ]
}
```

A step may override a call's fields inline for the one case where the same endpoint is used differently in two scenarios. Overrides are shallow and discouraged — two genuinely different requests should be two calls.

### 4.3 Targets — the boxes

Produced by discovery from a profile (§3.1) or written directly; the format and the sweep semantics are in §3.5. The mix never names a host, and the targets file never mentions load, which is what lets one mixture run against staging, against production, or against one suspect container with no edit to either file.

### 4.4 The bundle

The three documents plus their supporting files form a directory that is the unit of execution and of transfer:

```
checkout-mixed/
├── mix.json
├── targets.json          # swappable: --targets other.json
├── calls/products.json
├── gen/order.lua         # ← Lua sandbox root is the bundle directory
└── data/users.csv
```

`metrix-engine --plan checkout-mixed/` runs it. Exporting from the API produces exactly this directory, so what the UI ran and what a person runs by hand are the same artifact.

### 4.5 Design notes

- **Weights and explicit rates coexist.** `weight` distributes whatever is left after explicit per-scenario `rate` allocations. Bump one number, everything else redistributes, and the UI shows the resulting per-endpoint RPS *before* you run.
- **`id` on every step**, so chart series, error reports, and SLOs have a stable key independent of which call the step invokes.
- **Assertions are declarative and enumerable.** No expression language. An LLM emits them straight from a schema, and a failure produces a specific message (`$.items expected min_length 1, got 0`) rather than a stack trace.
- **Templating is deliberately tiny** — `{{ var }}`, `{{ dataset.field }}`, and a fixed function set (`rand`, `uuid`, `now`, `seq`, `pick`). Anything more is the generator's job (§7), and a small inline language is what keeps plans statically checkable.
- **Validation is per document and cross-document.** `POST /api/plans/validate` checks each file against its schema *and* checks that every `call` reference resolves, returning JSON Pointer paths. An unresolved call name is the characteristic error of this design, so it gets a specific message naming the step and the missing call.

---

## 5. Chaining

Each scenario is a sequential chain executed by one virtual user with its own variable scope.

- **Extraction:** JSONPath for JSON, XPath for XML, plus header and regex extractors. Response `Content-Type` picks the default parser; an explicit extractor type overrides it.
- **Failure policy:** per-step `on_failure` of `abort` (default — record the chain as failed at this step), `continue`, or `retry: n`. Aborted chains are counted separately from failed requests, so one upstream 500 doesn't inflate the error rate three times over.
- **`repeat_until`** covers the async-job pattern (POST returns 202, poll for completion) without a loop construct. Polling time is recorded separately so it doesn't contaminate request latency.
- **Think time:** optional `delay_ms` between steps, fixed or distribution-based. Off by default — in short windows you usually want the chain tight.
- **Chain latency is reported end-to-end as well as per step.** Per-step numbers find the slow endpoint; end-to-end is what a user actually feels.

**Rate accounting with chains:** a scenario `rate` means *iterations started per second*, not requests per second. A 3-step chain at 25/s is 75 req/s. The Config screen shows both numbers, because this is the single easiest thing to misread in a generated plan.

**Rate vs concurrency:** you cannot independently pin both. In the `open` model `rate` is the control and `max_concurrency` is a safety cap; in `closed` it is the reverse. The UI states which one is binding during the run, and flags when in-flight sits pinned at the cap — that means the cap, not the target, set the result.

---

## 6. Auth

A first-class block rather than a hand-rolled chain step. Token acquisition and refresh are infrastructure for the test, not part of the thing being measured, and conflating the two corrupts both the RPS figure and the latency distribution.

```jsonc
"auth": {
  "mode": "oauth_client_credentials",   // none | basic | bearer | oauth_client_credentials
                                        // | oauth_password | login_request
  "token_url": "https://idp.example.com/oauth2/token",
  "client_id": "{{ env.CLIENT_ID }}",
  "client_secret": "{{ secret.staging_client_secret }}",
  "scope": "orders.write",

  "inject": { "header": "Authorization", "format": "Bearer {{ token }}" },

  "refresh": {
    "strategy": "expires_in_margin",    // refresh at expiry minus margin
    "margin_s": 30,
    "on_401": "refresh_once"            // refresh_once | fail | ignore
  },

  "identity": "shared"                  // shared | per_vu | from_dataset:users
}
```

For services with bespoke auth, `"mode": "login_request"` takes a full request definition and an extractor, reusing the §5 machinery:

```jsonc
"auth": {
  "mode": "login_request",
  "request": { "method": "POST", "path": "/api/session",
               "body": { "user": "{{ users.email }}", "pass": "{{ users.password }}" } },
  "extract": { "token": { "json": "$.token" }, "expires_at": { "json": "$.expiresAt" } },
  "inject": { "header": "X-Session-Token", "format": "{{ token }}" },
  "identity": "from_dataset:users"
}
```

### 6.1 Rules that make the numbers honest

- **Auth traffic is excluded from load metrics by default.** Token endpoint calls are not counted in RPS, not mixed into the latency histogram, and not counted in the error rate. They are reported separately (§9.3). Without this, pointing a test at an IdP-protected service silently inflates your throughput figure with calls to a different system.
- **Tokens are acquired during baseline and warmup**, never first-touched inside the measured window. The token pool is pre-warmed to the size `identity` implies.
- **Refresh is single-flight.** When 200 virtual users hit a 401 at the same instant, exactly one refresh goes out and the rest wait on it. The naive implementation sends 200 refresh requests, DDoSes the IdP, and produces a latency spike that looks like the target degrading. Time spent blocked waiting on a refresh is measured and reported separately from request latency.
- **401s are classified distinctly** from application errors, and a 401 storm mid-run raises an annotation (§13.1) rather than just inflating the error rate.
- **Secrets never live in the plan.** `{{ env.X }}` reads the environment; `{{ secret.X }}` reads a profile-level secret store. Both are redacted in exports, recordings, and error samples. This matters more than usual here because plans are machine-generated and end up committed to repositories.

### 6.2 Identity modes

| Mode | Behavior | Use |
|---|---|---|
| `shared` | One token for all virtual users | Service-account style APIs |
| `per_vu` | Each virtual user holds its own token | Per-user rate limits, per-user cache behavior |
| `from_dataset:<name>` | Token per row of a dataset | Realistic multi-tenant load; pairs with `unique_per_iteration` (§7) |

`shared` is the default because it is cheapest, but it hides per-user rate limiting and per-user cache locality entirely — the Config screen says so when a plan uses it against a target profile flagged as multi-tenant.

### 6.3 Auth metrics

Token acquisitions, refreshes, refresh failures, 401s that triggered a refresh, refresh latency distribution, virtual-user time blocked on refresh, and token-pool utilization.

---

## 7. Request generation

Generation produces **the whole request**, not just the body: path, query parameters, headers, and body. The earlier framing of this as "body generation" was a gap — URL and query generation is at least as common, and a hook that only returns a body cannot express "GET a random product id from the ids this VU has already seen."

### 7.1 The generator contract

One hook, invoked per request, receiving a context and returning any subset of the request to override:

```
context in                        request out (all fields optional)
  vu            virtual user id     path
  iteration     iteration number    query     (map)
  step          step id             headers   (map)
  vars          extracted vars      body      (string or bytes)
  row           current dataset row
  rng           seeded RNG
```

Seeded per virtual user from the run seed, which is recorded in run metadata — so a run can be replayed with identical generated traffic. Without that, comparing two runs means comparing two different workloads.

### 7.2 Tiers

| Tier | Cost per call | Use |
|---|---|---|
| Inline template | ~ns | `{{ var }}` substitution in path, query, headers, body |
| Dataset | ~ns | CSV/JSONL, `round_robin` / `random` / `unique_per_iteration` |
| **Lua (embedded)** | ~µs | **Default for anything dynamic** |
| Rust plugin | ~ns–µs | Heavy generation: large XML, signing, compression |
| Exec sidecar | ~10–100µs + IPC | Escape hatch: a script that already exists |

**Lua is the default answer to "I need real logic here."** Embedded via `mlua`, one VM per worker thread, reused across requests — no process boundary, no serialization, no IPC. It is fast enough to sit on the hot path at the rates in scope, and it keeps generation logic inside the plan's directory rather than in a separate deployable.

```lua
-- gen/order.lua
function generate(ctx)
  local id = ctx.vars.pid or ctx.rng:int(1, 10000)
  return {
    path  = "/api/orders/" .. id,
    query = { region = ctx.row.region, expand = "lines" },
    body  = string.format(
      "<order><user>%s</user><qty>%d</qty></order>",
      ctx.row.email, ctx.rng:int(1, 5))
  }
end
```

```jsonc
"generators": {
  "order": { "type": "lua", "file": "gen/order.lua", "entry": "generate" }
}
```

Sandboxed: no `os.execute`, no network, no `require` outside the plan directory. **Read-only file access within the plan directory is allowed** — a generator that needs a static corpus (sample payloads, a fixture set, a word list) should be able to read it without an exec sidecar.

Corpus files are **loaded once into memory at run start and shared across every Lua VM**, not read per call. They are expected to be small enough for that, and the engine enforces it: a configured size ceiling, checked at load, with a clear failure rather than a slow run. No streaming, no lazy reads, no file handles on the hot path — the whole point of allowing reads is convenience at setup, not I/O during the measured window.

A generator that needs to write, execute, or reach the network wants the exec tier, where the process boundary makes the cost and the risk explicit.

**Rust plugin tier** is a trait implemented in-tree and registered by name, compiled into the engine. For the cases where even Lua's per-call overhead matters — multi-megabyte XML assembly, request signing, on-the-fly compression — or where an existing Rust type can be serialized directly with no intermediate representation.

**Exec sidecar** is retained, unchanged, as the escape hatch: `ndjson` protocol against a pool of long-lived processes (`oneshot` fork-per-call available, rate-capped, flagged in the UI). It is no longer the recommended default, because Lua covers the same need without the IPC cost or the second deployable.

### 7.3 Rules that apply to every tier

- **Generation time is measured and reported separately** (§9.6). If generation is the slow part, that must be visible, never folded into response latency.
- **`prefetch: N`** builds a request buffer during warmup so the measured window pays no generation cost at all. Available to every tier; the default for the exec tier.
- **Generator output is validated** against the request schema before the request is sent. A generator returning a malformed path fails the plan at first use, not at request ten thousand.
- **Generation failure is its own error class** — never counted as a target error.

### 7.4 XML and JSON are symmetric

Content negotiation per request, XPath and JSONPath extractors, optional schema validation (XSD / JSON Schema) as an assertion type. Content-type mismatch — asked for XML, got an HTML error page — is its own error class; it is common, and easy to miss when you only check status codes.

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

**No chain inference.** Guessing chains from repeated id values across a capture is unreliable in exactly the cases that matter, and a wrong chain is worse than none — it yields a plan that runs cleanly while testing a flow the service does not have. Generation emits single-step scenarios only; chaining is the LLM's job (it has the codebase, a far better source than a traffic capture) or the author's.

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

## 9. Metrics to track

Organized by the question each answers. Time series are bucketed at 250ms internally and rolled up for display.

### 9.1 Throughput & volume
- Requests attempted / completed / failed — overall, per scenario, per step
- **Achieved RPS vs target RPS** over time (the gap is the headline number)
- Scenario iterations started / completed / aborted
- Bytes sent and received; payload throughput MB/s — separate from request rate, since large XML bodies saturate links long before they saturate CPU
- Request and response body size distribution

### 9.2 Latency — per step, per scenario, aggregate
- min / mean / p50 / p75 / p90 / p95 / p99 / max, plus stddev. p99.9 is computed and stored but displayed only when the sample count supports it (§12.1) — at the 30s/75 RPS floor it does not.
- Full HDR histogram retained per run, so percentiles can be recomputed over any time slice after the fact and runs can be merged correctly — you cannot average percentiles
- Rolling percentiles over time (p50 / p95 / p99 bands)
- **Phase breakdown:** DNS resolve, TCP connect, TLS handshake, request write, **time-to-first-byte**, body transfer, total. TTFB vs total separates "the server is thinking" from "the response is big or the link is slow."
- **End-to-end chain duration** per scenario
- **Corrected latency** (coordinated-omission adjusted, §10) reported alongside raw, never instead of it
- Latency bucketed by response size — surfaces the "only slow for large accounts" case

### 9.3 Errors & correctness
- HTTP status distribution, by class and exact code, over time
- Transport errors by cause: DNS failure, connection refused, connect timeout, read timeout, TLS failure, connection reset, unexpected EOF
- Assertion failures by assertion id
- **Error samples: the first N errored calls are retained in full** (default 10, `capture.error_samples`), kept **per error class** rather than N overall — otherwise one flood of connection-refused evicts the single 500 that actually explains the problem. Each sample holds the full request as sent (including generated path, query, and body), response headers, and response body truncated at `body_max_kb`, with `redact` patterns applied. First N rather than a random sample, deliberately: the first failures are the ones that show what changed at onset, and they cost nothing to collect — after N, that class only increments a counter.
- Extraction failures (chain broke because a field was missing) — distinct from assertion failures
- Content-type mismatches and schema-validation failures
- Chain abort rate, and **which step chains die at** — a histogram over step ids is often the most diagnostic single chart in a run
- Retry counts and retry success rate
- Time-to-first-error, and error-rate onset relative to the load ramp

### 9.4 Connection behavior
- Connections opened, closed, currently established
- **Connection reuse ratio** — a low ratio usually means a keepalive misconfiguration, and it inflates latency in a way easily misattributed to the app
- TLS handshakes performed vs sessions resumed
- HTTP/2: concurrent streams per connection, server-advertised `SETTINGS_MAX_CONCURRENT_STREAMS`, stream resets, GOAWAY frames
- Socket errors and ephemeral-port exhaustion on the generator

### 9.5 Concurrency & saturation shape
- **In-flight requests over time** vs the configured cap
- Queue depth: requests scheduled but not yet sent
- **Send-schedule drift** — how far behind its own schedule the generator is running. The single most important guard against reporting the generator's limits as the target's.
- **Little's Law check:** in-flight ≈ achieved RPS × mean latency. Divergence means requests are stuck somewhere unaccounted for; the UI flags it.
- Per ramp step: throughput plateau, latency knee, error onset. In `breakpoint` mode these are promoted to named run-level scalars — §11.5 owns their definitions so that "the knee" means one thing across runs.

### 9.6 Load-generator health (self-metrics)
Non-negotiable. Without these a run can't be trusted.
- Generator CPU (per core), RSS, open file descriptors
- Tokio scheduler lag and task queue depth
- Body-generation latency and failure rate per generator; sidecar pool utilization
- NDJSON output backpressure (dropped or sampled event records)
- Clock source and monotonic drift
- **A single green/amber/red "generator healthy" indicator** on the Performance screen, so an invalid run is obvious at a glance rather than after the analysis is written

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

### 9.8 Run metadata (reproducibility & comparison)
Captured every run: plan hash, plan version, engine version, target base URL and profile, target build/commit if discoverable via a health endpoint, start/end timestamps, generator host and hardware, environment tags, free-text note. Without these the Recordings archive becomes a pile of numbers nobody trusts.

### 9.9 Derived / comparative
- Delta vs baseline for every headline metric, with the "worse" direction known per metric
- SLO pass/fail table (§16)
- Cost-per-request proxies: CPU-seconds per request, bytes per request
- Efficiency curve: achieved RPS per unit of target CPU, per ramp step

---

## 10. Run phases and the observation-only mode

### 10.1 The run timeline

Every run — with or without load — is a sequence of named phases on one monotonic clock. A load run never begins the instant you press start, and never ends the instant traffic stops.

```
 t0          t1            t2         t3              t4            t5
 │ baseline  │ warmup      │ measure  │ drain         │ settle      │
 │ observe   │ traffic on, │ the      │ traffic off,  │ observe     │
 │ only,     │ excluded    │ measured │ wait for      │ only,       │
 │ no load   │ from summary│ window   │ in-flight to  │ recovery    │
 │           │             │          │ complete      │             │
 └───────────┴─────────────┴──────────┴───────────────┴─────────────┘
   30s         10s           60s        ≤ timeout       60s
```

| Phase | What happens | Why it exists |
|---|---|---|
| **baseline** | Observer runs, no traffic generated. Start timestamp recorded explicitly and pinned to the run. | Captures **initial conditions**. Without it you cannot tell a CPU at 60% under load from a CPU that was already at 55% before you arrived. This is the reference every load-phase host metric is read against. |
| **warmup** | Traffic at target rate, metrics collected but excluded from the summary | JIT, connection pools, caches, autoscalers |
| **measure** | The window the headline numbers come from | — |
| **drain** | Traffic generation stops; engine waits for in-flight requests to complete or time out | Requests outstanding when the clock stops are otherwise silently dropped or counted as errors, either of which is a lie |
| **settle** | Observer continues, no traffic | Captures **recovery**: how long queues take to drain, GC to catch up, memory to come back, connection counts to return to baseline. A service that recovers in 5s and one that takes 4 minutes look identical at the moment traffic stops. |

Phase boundaries are recorded as timestamped events, drawn as vertical annotations on every chart, and available as filters — any statistic can be computed over any phase. The default comparison the UI presents is **baseline vs measure vs settle** for each host metric, as a three-column table alongside the charts.

Both pauses are configurable, defaulted (30s baseline, 60s settle), and can be set to zero. They are on by default because the cost is a minute of wall clock and the benefit is that the numbers mean something.

**Delta-from-baseline** is a first-class derived series: for every host metric, the observed value minus its baseline-phase median. This is usually the series you actually want to read, and is what the target-side charts plot by default, with absolute values a toggle away.

### 10.2 Observation-only mode

A recording with no plan attached: baseline and settle collapse into one continuous observation window, started and stopped manually or by duration. Everything in §9.7 and §9.8 is captured; §9.1–6.6 are simply absent.

This is the mode for "what does this box normally look like?" — and it is what makes baseline comparison worth anything. A saved observation-only recording can be marked as the **environment baseline** for a profile, and later load runs can be compared against it rather than only against their own baseline phase. That answers a question a single run cannot: is this environment behaving normally *today*, before we even applied load?

Observation-only recordings can also be triggered on a schedule, so an environment accumulates a normal-behavior history over time.

### 10.3 Interaction between the modes

- The load engine does not start until the observer reports it has collected at least one complete sample from every target in the profile. A run that starts before collection is up has no initial conditions, which defeats the point.
- If the observer cannot reach a target at all, the run is flagged at start and the operator chooses whether to proceed load-only. It is not a silent downgrade.
- Clock skew between generator, observer, and each target is measured at run start and recorded; series are aligned on the run's monotonic clock, not on wall-clock timestamps from individual hosts.

---

## 11. Breakpoint search

`"mode": "breakpoint"` ramps load incrementally until something gives, then reports where. This is the "just find the limit" run, and it is a distinct mode rather than a hand-written `stages` list because the stopping logic, the per-step statistics, and the refinement pass all need to be automatic.

```jsonc
"load": {
  "mode": "breakpoint",
  "start_rate": 75,
  "step_rate": 75,              // additive; or "step_factor": 1.5 for geometric
  "step_duration": "30s",       // each step is independently reportable (§12.1)
  "step_recovery": "5s",        // idle between steps so queues drain
  "max_rate": 3000,             // hard ceiling, always required
  "refine": true,               // one bisection pass between last-good and first-bad
  "stop_on": {
    "error_rate": 0.02,
    "p99_latency_ms": 1000,
    "p99_multiple_of_baseline": 5,
    "rate_shortfall_pct": 10    // achieved < 90% of target => saturation
  }
}
```

### 11.1 Each step is its own measured window

A step is a miniature run: its own warmup exclusion, its own histogram, its own host-metric snapshot, its own annotations. Step results are never pooled into one run-wide percentile — that would average a healthy 75 RPS step with a collapsing 900 RPS one and describe neither.

`step_duration` defaults to 30s for the reason in §12.1: below that, a step's p99 is not worth acting on, and a breakpoint run that locates a knee using unsupported percentiles is worse than no run at all.

`step_recovery` matters more than it looks. Without an idle gap, step N+1 inherits step N's queue backlog, and the search finds a false early cliff that is really just accumulated debt. The gap is observed, not slept through — it is a miniature settle phase.

### 11.2 Stop conditions

The search halts at the first triggered condition, records which one fired, and proceeds directly to drain and settle (§10.1) — the recovery curve after a deliberate overload is one of the more useful things this mode produces.

| Condition | Meaning |
|---|---|
| `error_rate` exceeded | The cliff — it is returning failures |
| `p99_latency_ms` / `p99_multiple_of_baseline` | The knee — still correct, no longer acceptable |
| `rate_shortfall_pct` | Saturation — it cannot absorb what is offered |
| `max_rate` reached | No breakpoint in range. Report exactly that; do not imply one was found. |
| generator-limited | *We* broke, not the target — §11.3 |

### 11.3 Distinguishing target saturation from generator saturation

The failure mode that makes a breakpoint run worthless: offered rate stops climbing, latency rises, and the run reports a target limit that is actually the generator's limit.

Before attributing any shortfall to the target, the engine checks its own state (§9.6, §13.2): send-schedule drift, queue depth, generator CPU, body-generation latency, socket and ephemeral-port exhaustion. If any of those are degraded at the same step, the run **aborts with a `generator_limited` annotation and reports no breakpoint**. It does not guess, and it does not publish a number with a caveat attached — a caveated number gets quoted without the caveat.

The pre-run headroom check (§13.2) also caps `max_rate` at the calibrated generator ceiling by default, so most such runs are prevented rather than detected.

### 11.4 Refinement

With `refine: true`, once the cliff is bracketed between the last good rate and the first bad one, the engine runs one bisection pass at the midpoint for a full `step_duration`. One pass, not a full binary search: with 30s steps the added precision stops paying for its wall clock quickly, and run-to-run variance (§12.2) is soon wider than the remaining bracket. The report states the bracket, not a false-precision single number.

### 11.5 The breakpoint report

- **Max sustained rate** — highest step where every SLO held for the full step
- **Knee** — lowest rate where p99 exceeded the configured multiple of its baseline-phase value
- **Cliff** — lowest rate where errors or shortfall crossed the threshold
- **Limiting resource** — which host metric was nearest saturation at the last good step (CPU, iowait, connection count, pool queue depth), from §9.7
- **Failure mode** — what the errors actually were at the cliff: timeouts, refused connections, 5xx, listen-queue drops. "It broke" and "it began refusing connections at the listen queue" are different findings.
- **Recovery** — time to return to baseline after the overload, from the settle phase

Knee, cliff, and max-sustained are stored as named scalars on the run, so a later breakpoint run against the same plan and target compares directly — the single most useful longitudinal number this tool produces.

---

## 12. Short test windows — the statistical problem

Short runs are the explicit goal, and they break several defaults:

### 12.1 Percentiles need samples — the floor is 2250

The working assumption is a **minimum window of 30s at 75+ RPS, so 2250 requests**. That is the validated floor: the engine warns when `duration x rate` falls below it, and refuses to display percentiles the sample count cannot carry.

What 2250 samples buys, by tail count and by the 95% confidence interval on the order statistic:

| Percentile | Samples in tail | 95% CI spans | Verdict at the floor |
|---|---|---|---|
| p50 | 1125 | p47.9 – p52.1 | Solid |
| p95 | 112 | p94.1 – p95.9 | Solid |
| p99 | 22 | p98.6 – p99.4 | **Usable, coarse** — shown with its interval |
| p99.9 | 2 | p99.8 – p100 | **Not reportable** — suppressed |

p99 at the floor is real but blunt: it will not resolve a 10% regression, and it moves run to run on noise alone. The UI therefore **renders p99 with its confidence interval rather than as a bare number**, and suppresses p99.9 below 10,000 samples rather than printing a figure derived from two requests.

The general rule the engine applies: a percentile needs roughly 10 samples beyond it to be crude and 100 to be stable. That is 1,000 samples for a crude p99 and 10,000 for a stable one — 133s at 75 RPS, or 30s at 333 RPS. The run header states which side of that the run sits on.

**Consequence for regression detection:** at the floor, compare **p95** when the question is "did this get worse?" and reserve p99 for "is the tail catastrophic?". The comparison view leads with the metric the sample count can defend rather than always leading with p99.

### 12.2 Everything else short windows break

1. **Warmup contaminates everything.** JIT, connection pools, caches, autoscalers. The `warmup` window is measured and charted but excluded from the summary, so you can *see* the warmup effect instead of having it silently averaged into your p99.
2. **Coordinated omission.** In a closed model a slow response delays the next request, so the worst latencies never get sampled. The engine defaults to the open model (requests issued on schedule regardless of outstanding ones) and reports raw and schedule-corrected latency side by side. Raw is what happened; corrected is what someone queued behind it would have experienced.
3. **Run-to-run variance.** A single 30s run is a sample, not a measurement — and at 2250 samples the p99 noise floor is wide enough that this matters more, not less. Recordings supports **run groups**: the same plan N times, with the spread shown and the observed noise floor stated, so a "regression" smaller than the spread is labelled as one. This is the cheapest available correction to a short-window methodology.
4. **Time alignment.** Every series is stamped against the run's monotonic start so load and target-side charts overlay exactly. Generator-to-target clock skew is measured at run start and recorded.

---

## 13. Run annotations, validity, and our own limits

Two related problems: a run can be invalidated by the generator rather than the target, and the person reading the chart three weeks later will not remember which. Both are solved by attaching structured, automatic annotations to the run itself.

### 13.1 Run annotations

Detectors run continuously during a run and attach records of the form:

```jsonc
{
  "code": "concurrency_cap_reached",
  "severity": "warn",              // "info" | "warn" | "invalid"
  "phase": "measure",
  "from_ms": 14200, "to_ms": 30000,
  "message": "In-flight pinned at max_concurrency=200 for 15.8s (53% of the measured window). Peak in-flight 200. Achieved RPS and latency for this window are attributable to the concurrency cap, not to the target.",
  "detail": { "cap": 200, "duration_ms": 15800, "window_pct": 53, "peak_in_flight": 200 }
}
```

Annotations are shown on the run header, drawn as shaded regions on the affected charts, carried into every export, and surfaced in comparisons. **A run carrying an `invalid` annotation cannot be set as a baseline without an explicit override**, which is the point of the whole mechanism.

The detector set, at minimum:

| Code | Severity | Fires when |
|---|---|---|
| `concurrency_cap_reached` | warn, or **invalid** above 25% of the measured window | In-flight requests sit at `max_concurrency`. Records duration at cap, percentage of window, peak in-flight. |
| `rate_not_achieved` | warn | Achieved RPS below target by more than the configured tolerance |
| `send_schedule_drift` | invalid | The generator is falling behind its own send schedule |
| `generator_saturated` | invalid | Generator CPU, FD, or ephemeral-port exhaustion |
| `body_generation_slow` | warn | Generator sidecar latency is a material fraction of request latency |
| `generator_limited` | invalid | Composite: a shortfall coincides with any generator-side degradation (§11.3) |
| `sample_count_low` | warn | The reported percentiles are not supported by the sample count (§12.1) |
| `events_dropped` | warn | NDJSON backpressure caused per-request event sampling |
| `clock_skew` | warn | Generator-to-target skew beyond tolerance |
| `collection_gap` | warn | Observer lost a target for an interval |
| `host_count_changed` | info | Target instance count changed mid-run — the autoscaling case (§18) |
| `target_unreachable` | invalid | Observer could not reach a target at all |
| `warmup_effect_detected` | info | Measure-phase early latency materially above its late value |
| `not_returned_to_baseline` | warn | A host metric failed to return to baseline during settle — leak signal |

Free-text operator notes attach to the same list, so machine and human annotations read together.

**On `concurrency_cap_reached` specifically:** hitting the cap is not inherently a failure — it is often exactly the closed-model test you intended. What matters is that the headline numbers stop describing the target and start describing the cap, and nothing in a chart shows that. Hence the explicit note, the shaded chart region, and the escalation to `invalid` when the cap dominates the window.

### 13.2 Tracking our own limits

The generator is measuring instrument and load source at once, so its capability has to be a known quantity rather than an assumption.

**Calibration.** `metrix-engine --calibrate` (a mode of the same binary, so a load box calibrates itself) ramps against a built-in in-process null target to find this machine's ceiling, and against a loopback echo server to find the ceiling including the real socket and TLS path. The difference between the two is itself informative. Calibration is per plan *shape*, not per plan — body size, TLS on/off, chain depth, and body-generation mode are what move the number, so a small matrix is measured and stored as a **machine profile** with the hardware it was measured on.

**Worker threads.** `engine.worker_threads` (default: physical cores − 1) sets the Tokio runtime's thread count, with `connections_per_host` and optional core pinning alongside. Raising it is the first lever for generator headroom, and calibration is per thread count — so the machine profile records a ceiling curve across thread counts rather than a single number, and the headroom check below knows what raising it would buy.

Worth stating plainly: **the services in scope are expected to cap out well below the generator's ceiling**, which is the comfortable case — it means the measurement is of the target throughout. The threading knob exists for the exception, and the calibration curve is what tells you which case you are in *before* the run rather than after. If a target genuinely outruns a tuned single box, the honest output is the `generator_limited` annotation, not a bigger number.

**Headroom check, before the run starts.** Demanded RPS (accounting for chain multiplication — §5) is compared against the calibrated ceiling:

| Headroom | Behavior |
|---|---|
| < 50% of ceiling | Proceed |
| 50–70% | Proceed, `info` annotation recording the ratio |
| 70–90% | Warn before start; run carries a `warn` annotation |
| > 90% | Refuse by default; requires explicit override, and the run is annotated `invalid` |

In breakpoint mode this is also what caps `max_rate` by default (§11.3) — the search stops at the point where the tool would begin measuring itself.

**During the run,** the §9.6 self-metrics feed the detectors above. The Performance screen's generator-health strip shows current headroom as a live gauge beside the target's numbers, so the two are read together rather than the client's limits being discovered afterwards in a log.

**In run metadata,** the calibrated ceiling, the machine profile id, and the observed peak headroom are recorded (§9.8). Without this, a comparison across a generator hardware change silently attributes a generator improvement to the target.

**Known limitation, stated rather than hidden:** a single-box generator has a ceiling, and for a sufficiently fast target the honest answer is "this tool cannot generate enough load to find your limit." The design's job is to say that clearly instead of reporting the ceiling it hit as though it were the target's. Distributed generation (§19) is the eventual answer; the annotation is the answer today.

---

## 14. The stats table

A page of its own, separate from the charts. The two answer different questions and are read at different moments — the table is what gets exact numbers read off it, quoted in a ticket, and scanned for the row that looks wrong; the charts are for shape and timing. Putting both on one page makes each worse, so the Performance section has two pages and neither borrows from the other.

The table is live during a run, updating once per second (§2.5), and is the same view for a finished run.

### 14.1 Rows

One row per step, grouped under its scenario, with a run-total row at the top. Scenario rows aggregate their steps; chains additionally get an end-to-end row, since chain duration is not the sum of step medians. Sortable on any column, and the grouping collapses so a 40-step plan stays readable.

### 14.2 Columns

Default visible:

| Column | Notes |
|---|---|
| Name | Scenario / step id, indented by grouping |
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
| Target vs achieved RPS over time, stacked by scenario | Did we apply the load we asked for? |
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

## 16. SLOs and CI

Plans carry SLO assertions. The engine evaluates them at end of run, sets its exit code accordingly, and the API exposes a machine-readable verdict. This closes the loop for the machine-authored use case: an agent can generate a plan, run it, and read a pass/fail with the specific violated thresholds — without parsing charts.

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
- Baseline-phase stats compared across targets, which catches the case that otherwise wastes an afternoon — one container was already loaded before the test started.
- Repeated sweeps form a series of sweeps, giving per-target trends: is that container reliably slow, or was it unlucky once?

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
| Plan discovery | **`POST /api/plans/generate`** (§8) — deterministic skeleton from OpenAPI/WSDL/HAR/access log. **No chain inference.** |
| Dynamic generation | **Embedded Lua as the default tier** (§7.2), generating the whole request. Rust plugin for heavy cases, exec sidecar as the escape hatch. |
| Lua file access | **Read-only within the plan directory**; corpus loaded once into memory at run start under a size ceiling. No I/O on the hot path. |
| Error retention | **First N errored calls per error class**, default 10, secrets redacted (§9.3). |
| Generator headroom | **Configurable worker threads**; calibration measured as a curve across thread counts (§13.2). |
| Cross-run comparison | **Run series** (§17.2–16.6) grouped by setup identity, with measured noise floors. |
| Series segmentation | **None.** The identity tuple defines the series; a setup change is simply a different series (§17.2). |
| Python tooling | **uv** — `pyproject.toml` + committed `uv.lock`, `uv sync` / `uv run`. No pip, no hand-managed venv (§2.2). |
| Plan structure | **Three separate documents** — calls, mix, targets (§4) — composed into a bundle. Authored in the UI by default; exportable and hand-editable. |
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
