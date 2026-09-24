# Metrix — API and Front End Design

The control plane: target discovery, host observation, storage and analysis, and the browser UI. Pairs with [design-engine.md](design-engine.md); the boundary between them is in [design-api-engine-contract.md](design-api-engine-contract.md).

Everything through §17 is buildable and useful with no engine installed — observation-only recordings are a first-class mode, not a degraded one.

Profile verification is also available per SSH host from the endpoint row. It reports the exact effective host, port, username, SSH config files, identity files, and loaded-key count used by the probe, so authentication and routing failures can be distinguished from a genuinely unreachable host. SSH host-key checking is intentionally disabled for these ephemeral hosts: the presented key is accepted, with change warnings deferred to a later policy.

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
| **Config** | What this process is and where it keeps things: the resolved `$METRIX_HOME`, which rule chose it, the database. |
| **Schemas** | Uploaded service descriptions kept under `$METRIX_HOME/schemas/`: OpenAPI 3, Swagger 2.0, WADL, WSDL 1.1, HAR captures, access logs and route lists. A substantial drop target beside the picker accepts one or more files and parses them immediately through the same deterministic source reader Plans uses before anything is stored; the table shows the source identity and resulting call count, and each entry can be opened or deleted. Links beside the target name the accepted source formats. The browser checks drag-and-drop primitives at startup and shows a picker-only explanation with local diagnostics if they are absent or a drop arrives without readable files. |
| **Plans** | The plan library and the mixture editor (§20): chains and their shares, each one shown as the iterations and requests it actually buys, phase durations (§10), engine threading and SLO thresholds. Validation is displayed, never decided here — the same server-side check gates the bundle. **Plan generation from OpenAPI/Swagger/WADL/WSDL/HAR (§8)** lands here too. |
| **Profiles** | The environments a run can be pointed at: create, edit, rename and delete them, endpoint by endpoint, plus **hostname discovery and the resolved inventory (§3)**. The editor submits a whole document and the server runs the same validation a hand-written file goes through, so a form cannot save what the loader would reject. Renaming moves the profile file; existing recordings keep the name they were made with, so the new name begins a new series identity (§17.2). |
| **Performance › Stats** | **The numbers, as a table** (§14). Per chain and per step: start, finish, median, standard deviation, counts, errors. Updates once per second during a run. No charts on this page. |
| **Performance › Charts** | The same run drawn (§15) — load and observation on one shared time axis, current-vs-target RPS, host stats, error feed, generator-health strip, phase indicator, stop/abort. No tables on this page. |
| **Archive › Sweep** | One recording's boxes ranked against each other (§17.6). Reached from a recording rather than from the menu: it is a question about one recording, and a standing menu entry would be empty on arrival. |
| **Archive › Recordings** | Everything captured, load runs and observation-only recordings alike: filter by kind/profile/status/severity/baseline, mark a recording as **baseline**, overlay N runs, inspect retained error samples, export (JSON / CSV / static HTML report), **purge** the request-level bulk or **delete** the recording outright (§17.1 — two different actions, kept apart). |
| **Archive › Series** | The same list grouped by setup identity (§17.2), with the trend view (§17.3) and regression flags (§17.4). Its own page rather than a section of a recording, because it is the only view that is not about one recording: a run's own page answers *what happened*, and this answers *is that better or worse than the last ten*. Each recording links to its series and back. |
| **Server › Logs** | What this process has been doing (§2.6): discovery walks, verification checks with their timings, recordings starting and failing. Newest first, filterable to warnings and errors, and polled only while the page is open. |

"Recordings" covers both modes deliberately — an observation-only recording and a load run are the same object with different sections populated, so they compare against each other with the same machinery.

**A single static page.** One `index.html`, Tabler's CSS, and JavaScript — all loaded up front, with the API serving JSON and the event stream and nothing else. No server-side templating, no Jinja, no framework, no build step.

**Fullscreen and fluid.** The page fills the window: no centred max-width column, just a small even inset at the edges. This is a measurement tool read a table at a time, so horizontal space goes to target columns rather than to whitespace. The inset is one variable (`--tblr-gutter-x` on the page containers).

**Nothing on the page is fetched at render time.** Tabler is vendored under `web/vendor/` and the icons are inline SVG rather than a font or a sprite sheet: a tool that watches a private network must draw itself without the public one, and a menu whose icons arrive on a second request arrives late. `scripts/check.sh` fails if a CDN URL reappears.

The JavaScript is split into ES modules along clear seams so it stays maintainable without tooling:

| Module | Responsibility |
|---|---|
| `api.js` | Every fetch call; the only place a URL appears |
| `stream.js` | SSE connection, reconnect, `Last-Event-ID` replay |
| `state.js` | Shared state; subscriptions select the values each view reads |
| `table.js` | The stats table (§14) |
| `charts.js` | uPlot setup and updates (§15) |
| `config.js` | Profiles, plans, validation display |
| `recordings.js` | Archive, series, comparison views |
| `ui.js` | Icons and empty states — the markup the views share |

Views declare selectors for the state they read; subscriptions compare selected values by identity and render only when those values change. Updates replace state objects rather than mutate them in place. The shell, health badge, error banner and active view subscribe separately: polling, live samples and background list responses must preserve an open editor's fields, focus and selection. An open profile editor selects only its draft; intentional editor actions capture the form before replacing that draft. Views make no network calls. Charts use uPlot, which handles thousands of points at 60fps without fighting us.

**Desktop only.** No mobile layout, no responsive breakpoints, no touch affordances — this is a wide-screen tool for reading dense tables and multi-series charts side by side, and narrowing it would cost exactly the density that makes it useful. Where "responsive" appears in this document it means *does not lag* (§2.5), never *reflows for small screens*.

### 2.5 The live view

A running test updates **once per second**, on both the stats table and the charts.

**Transport: Server-Sent Events, not WebSocket.** Nothing here streams client-to-server — the view is one-way, and control actions (stop, abort, annotate) are ordinary POSTs, simpler to authorize and log than messages multiplexed into a socket. SSE gives exactly what this needs for free: automatic reconnection, plain HTTP/2 with no upgrade or heartbeat plumbing, and **`Last-Event-ID` replay**, so a dropped connection resumes where it left off instead of leaving a hole in the chart. WebSocket earns its place when the browser needs to stream something back; until then it is a second protocol for no gain.

- **Cadence is decoupled from the engine.** The engine emits 250ms snapshots (§2.1); the API aggregates and pushes once per second. Engine cadence can change without touching the UI.
- **Payloads are deltas**, with a full snapshot as the first event after connect or reconnect, so a late-joining viewer is immediately correct. Each event carries counters since the last, current percentiles, phase and elapsed time, generator health, and any annotations raised in the interval.
- **Slow clients coalesce rather than queue** — a view thirty seconds behind is worse than one that skipped thirty seconds. Events fan out from a single engine stream, so viewer count adds no per-viewer load.
- **The stream is never on the measurement path.** The engine writes to a pipe and never blocks on a consumer; if the API stalls, `events_dropped` is annotated (§13.1) and the run continues unaffected. Nothing a browser does can perturb a test in progress.

### 2.6 The server log

Operations on the server fail in ways the page that started them only summarises: an SSH probe's effective config, a discovery hop's note, an engine that exited. Every `metrix_api` logger at INFO and above is also written into a bounded in-memory buffer (the most recent 2,000 records) and served by `GET /api/logs?after=<seq>`, which returns only records newer than the sequence number the page already holds.

An error no route handled is logged there too, with its traceback, and the 500 carries the exception's message rather than a bare status line.

In memory, not on disk: it answers *what just happened*, and a restart is a fair place for that to end. Nothing enters the log that the rule on secrets forbids — messages are written by this code, which logs addresses, usernames and key paths, never key material or header values. Third-party loggers (botocore, asyncssh, uvicorn's access log) are left out: they are noise at this level, and the page's own polling would fill the access log with itself.

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

Resolution is cached with a TTL, refreshed on demand, at run start, and at phase boundaries (§10.1) so an instance-count change mid-run is detected rather than inferred. A profile carries the *question* — a `discover` block naming a hostname, or a cluster and service — and never the resolved endpoints: writing those back would freeze one walk into a document whose purpose is to ask for a fresh one. The answer is stored, and an unchanged re-resolution extends the stored row rather than adding a copy of it, so the table is a history of when the environment changed rather than of when it was checked.

A change found at a boundary raises `host_count_changed` — **warn, not invalid**, because an environment that scaled under load may be exactly what was being measured. Collection stays with the set pinned at the start: a host series that begins halfway through a recording is worse than an absent one, and every average over "the environment" would change meaning mid-chart.

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
| Health status | Eligibility, and whether a target was mid-deregistration |

Per snapshot rather than per entry: the source it was resolved from, one `discovered_at` — a walk is a single instant, and per-entry times would differ only by the latency of the call that found each one — **the furthest hop the walk reached**, and the notes saying what it could not determine. Those last two are how partial resolution is reported as a result rather than an error (§3.1).

The inventory serves both subsystems: it is the engine's endpoint list *and* the observer's collection target list (§2.3), so load metrics and host metrics attach to the same identities and can be joined without guesswork.

Task definition revision and image digest earn their place in cross-run comparison: they are what lets a later analysis say *this is a different build*, which is the difference between a regression and a deployment.

### 3.4 Addressing, and the two roles an endpoint plays

An endpoint is a machine, and a machine can play either of two parts in a run: **traffic goes there**, or **statistics come from there**. They are separate flags (`load`, and a `collect` block) because they are usually not the same set. Each endpoint also says how its address is reached: `ip`, `alb`, `elb`, `ecs`, or `fargate`. The choice is kept per endpoint because a profile can point traffic at a balancer while observing concrete tasks behind it. These labels classify the network path; the engine still receives one concrete `host:port` address and makes no cloud calls.

Automatic discovery currently resolves ALB/NLB through ELBv2, ECS services, and both EC2- and Fargate-launched tasks. The `elb` label covers non-ALB elastic load balancers: NLB is discovered, while a classic ELB is accepted as an explicit endpoint but is not discovered. The editor keeps every choice visible and says when part of a selected kind is explicit-only rather than pretending the discovery implementation covers it.

An explicitly entered ALB can use that same walk to choose its observation host without turning the profile into a discovered profile. **Resolve hosts** follows the ALB hostname through the AWS chain and presents the resulting instance addresses, or task addresses when there is no instance, as read-only candidates. The profile format still has one `collect.host`, so the editor permits one selected candidate and saves only that address; the inventory and the other candidates are transient setup evidence. Enabling collection and testing the selected SSH probe are separate, explicit actions. A test is never run by selecting a host, and its result is not persisted.

Every endpoint carries an address regardless, because that is where the box *is*; `load` says whether it is also where the load goes. This is what makes `targets.json` derivable: the default selection is the endpoints that take traffic, and the observer's list is the ones with a collector.

Observation defaults include an optional profile-level SSH username. It is applied to every SSH collector when an observation starts, while an endpoint-level username remains the fallback when the profile field is empty. The default is applied to the in-memory observation profile, not copied into endpoint files.

**The endpoint modes derive the addressing class stored in series identity** (§17.2): `alb` and `elb` are through-the-balancer, while `ip`, `ecs`, and `fargate` are direct. Those two paths are never compared against each other. A legacy profile-wide `load_balancer` or `direct` value is still accepted and mapped onto its endpoints when the file is read.

### 3.5 Verifying a profile

A profile is a set of claims, and every one of them fails silently: a security group that admits the balancer and not this machine, an exporter that is not running, a key that works for one box and not its replacement. Left unchecked, the first evidence is a recording full of gaps.

Verification asks both questions of every endpoint, on demand, and reports them separately because they are fixed by different people. The front-end check makes one `GET /` request and treats any valid HTTP status, including 404, as evidence that the service answered. Different endpoints may be checked in parallel, but each endpoint's front-end request is completed before its collector probe begins, avoiding a duplicate connection burst against one host:

- **the load target** — open a connection, complete the TLS handshake when TLS is on, send `GET /` and read only the status line.
- **the collector** — one real probe over the transport a recording would use. Not a port check, because an SSH login that succeeds and then cannot run the stats script, and an exporter answering 404 on the configured path, are exactly the failures a port check passes and a recording then hits.

A successful check costs round trips, not seconds: a front end on the same network answers in milliseconds, and an SSH probe in a few round trips plus the remote script. Three things used to add seconds and no longer may:

- **Dual-stack names are raced, not tried in turn.** Every resolved address is dialled at once and the first to connect is used. Tried in order, a name whose first answer is an unreachable IPv6 address (`localhost` on Windows, a dual-stack balancer from an IPv4-only network) pays a refused-connection retry, or the whole timeout, before IPv4 is tried.
- **Nothing blocks the event loop.** Checks run concurrently on one loop, so synchronous work in one — importing the SSH stack, loading private keys to explain a failure — is charged to every other check in flight. SSH diagnostics are built only after a probe fails, and off the loop.
- **The clock stops at the status line.** A check's time is to the first response line; closing the connection (a TLS shutdown waits for the peer) is bounded and not charged to it.

An SSH probe is traced step by step (`observer/ssh_trace.py`): config and keys, DNS, TCP, key exchange, authentication, session, script and close are each timestamped as they complete, and AsyncSSH's own debug account of that one connection is kept beside them. The report goes to the server log, and a failure names the step it interrupted. AsyncSSH's debug level is raised only while a probe runs, and what it adds is captured by a logger filter and dropped, so a recording's streams are not narrated and no console fills with it.

What remains slow is slow for a reason worth seeing: a filtered address costs the full observation timeout, and Windows retries a refused connection for about two seconds before reporting it. Each check writes its timing to the server log (§2.6), split into connect and response for the front end, so a slow application root is distinguishable from a slow network.

Never automatic. A failure is a timeout against someone else's network, so it happens when someone asks, and the result is held for that sitting rather than stored — reachability is true of a moment, and a green tick from yesterday presented as current is worse than no tick.

The run-time counterpart is the `target_unreachable` annotation: a target that produced *no* samples at all for a whole recording. It is `invalid` rather than `warn` because the box was named in the profile, so a reader counts it among what was measured, and an average over "the environment" that quietly omits one of its machines is worse than no average.

---

## 8. Plan generation from an endpoint

### 8.0 Stored source schemas

Service descriptions may be uploaded once into a small source library instead of
being pasted directly into the plan generator every time. Each entry lives at
`$METRIX_HOME/schemas/<id>/`: the original UTF-8 content is kept as `source.txt` and
`metadata.json` records its uploaded filename, source type and the call summary
produced at upload. For OpenAPI 3 and Swagger 2.0, the id uses `info.title`, then
`info.description` if the title is absent or unusable, then the filename stem. Other
sources use the filename stem. Metadata text is slugged and capped at 64 characters
before the source type is appended, so a document titled `Shop API` becomes `shop-api-openapi`;
an existing id is a conflict rather than an implicit overwrite. Stored ids do not
change when this naming rule changes.

Upload is transactional at the feature boundary: the server first runs the content
through the same `generate.generate` dispatch used by `POST /api/plans/generate`, and
only a source that defines at least one call is written. The schema library therefore
does not grow a second definition of OpenAPI 3, Swagger 2, WADL, WSDL, HAR, access-log or route-list
validity. A failed parse leaves no library entry. The stored call names, methods and
paths are an upload-time summary for the list and detail views; they do not become a
fourth plan document and are never handed to the engine.

`POST /api/plans/generate` takes a source and returns a draft plan. The Plans editor
also opens as a blank mixture: it lists the stored schemas, lets the author choose
which endpoints become calls, and creates the plan from those selected definitions.
Generation is no longer a prerequisite for opening the editor; the stored schema is
the source of the selected call documents.

### 8.1 What the tool does and does not do

**The tool does the mechanical part — which is exactly the calls document** (§4.1). Given a service description it emits one call per operation, parameters filled from schema types and examples, assertions derived from declared response codes and schemas, plus a starter mix with flat weights. No LLM inside the tool — this step is deterministic, so the same input gives the same skeleton, and a diff between two generated plans means the service changed.

**The LLM does the judgment part — which is the mix** (§4.2). Realistic weights, which calls chain into which, what a meaningful assertion is beyond "returned 200", which endpoints matter. The document split falls out of this division naturally: the deterministic half and the judgment half are separate files, so regenerating calls after an API change does not touch a tuned mixture. The generated skeleton is the LLM's starting point rather than its output, which is a much easier task than authoring from nothing and produces far less drift between runs.

### 8.2 Sources

| Source | What it yields |
|---|---|
| **OpenAPI 3** (JSON or YAML) | Operations, parameter types, request/response schemas, declared status codes. The richest structural source. |
| **Swagger 2.0** (JSON or YAML) | The same structural input, read locally into the generator's operation model. Its `definitions`, body and form parameters, `basePath`, response codes and security declarations are handled directly; Metrix does not upload a document to a converter or construct an intermediate OpenAPI 3 file. |
| **WADL** | HTTP resources and methods from the WADL 2009/02 XML application tree. Nested resource paths, required template/query/header parameters, request media types and successful response codes become calls; external grammars and references are not fetched. |
| **WSDL / XSD** | Same for SOAP and XML services; bodies generated from the schema. |
| **HAR capture** | **Realistic mixtures** — observed call frequencies become weights. Chains are *not* inferred (see below). |
| **Access log** | Path frequencies and status distribution; weights grounded in production traffic. |
| **Route list** | Bare minimum: a list of method+path lines, for when nothing else exists. |

HAR and access-log sources are the valuable ones, because they answer the question a schema cannot: what does this service *actually* get asked for? A plan whose mixture comes from observed traffic is worth considerably more than one whose weights were guessed.

**The document is posted, never fetched from a URL.** A control plane that retrieves whatever address it is handed is a request forwarder sitting inside the network it exists to observe, which is a larger thing than a plan generator and not a decision anybody makes by asking for a skeleton. The same rule applies inside a document: a local `$ref` is resolved, a remote one is left where it is.

**Nothing is copied out of a capture except the shape of the request.** A HAR from a browser session carries session cookies, bearer tokens and whatever was typed into the login form, and a plan is a file that ends up in a repository. Method, path and status are read; bodies and headers are not, and the todo list says how many were left behind. Authentication belongs in the mix's `auth` block, which knows how to refresh a credential and keeps it out of the documents.

**The one guess a traffic source makes is which path segments are identifiers.** `/api/orders/12345` and `/api/orders/67890` are one operation, and left as literals a week of logs becomes four thousand calls. Numeric, UUID and long-hex segments become templates — and every call that had one carries a todo saying so, because a service whose routes really are numeric deserves to see the guess rather than find it later.

### 8.3 Draft semantics

**No chain inference.** Guessing chains from repeated id values across a capture is unreliable in exactly the cases that matter, and a wrong chain is worse than none — it yields a plan that runs cleanly while testing a flow the service does not have. Generation emits single-step chains only; chaining is the LLM's job (it has the codebase, a far better source than a traffic capture) or the author's.

Generated plans come back marked as drafts, with `todo` entries on the fields needing human or LLM attention — guessed weights, placeholder values, unchained operations that probably belong in a sequence. The Plans page surfaces these as a checklist, and a draft is marked as one everywhere it appears, so a provisional mixture is never mistaken for a reviewed one.

**The marker is a `draft.json` sidecar, not a field in `mix.json`.** That document's shape belongs to the engine, and the plan hash covers its bytes: a control-plane review state has no business appearing in a schema the load generator parses, or moving the identity of a run. Clearing the marker is its own action rather than a side effect of saving — editing one percentage is not a review, and a flag that cleared itself on the first edit would mark every generated plan reviewed a minute after it was opened.

**A draft warns; it does not refuse.** A generated skeleton runs, which is the point of generating one. What it cannot do is pass for a mixture somebody chose. Blocking it outright would only teach people to delete the marker.

**Regenerating replaces the calls and leaves the mixture alone.** This is what the document split is for: the mechanical half goes stale when the service changes and the judgment half does not, so `POST /api/plans/{name}/regenerate` rewrites `calls/generated.json` and does not touch `mix.json`. A regeneration that would leave a chain naming a call the service no longer has is refused rather than written — the plan would still load, right up until somebody tried to run it.

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

**Sample counts apply here too, with their own floor.** §12.1 governs request counts; a host series sampled at 1s has far fewer points, so the rule is stated separately: a median needs at least three samples, a p95 at least twenty — below which the 95th percentile is the maximum wearing a hat. An unsupported figure is **withheld rather than shown with a caveat**, because a caveat in a table is a caveat nobody quotes. Every figure carries its `n`.

**"Different from baseline" is measured against the baseline's own spread**, not a percentage: 2× the baseline window's IQR, the same band the trend view uses for regressions (§17.4), floored at 2% of the baseline median so a metric that barely moves does not flag on rounding. Recovery uses a wider band (3×, floored at 5%) because *has it come back* is a different question from *is it different*, and demanding an exact return would report a permanent leak on every box that is merely busy.

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

Comparison is **per box and pooled across boxes**, and the pooled view is the one that answers the question. A discovered environment replaces its tasks on every deployment, so matching this recording's targets against the baseline's usually matches nothing; pooling every box's readings into one distribution describes *a typical box in this environment*, which survives the tasks being replaced. The per-box rows sit beside it for the case pooling hides — two instance types under one service — and a box is listed only where its verdict differs from the pooled one, since that is what "one machine is the odd one out" looks like.

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

**With no engine, the rows are host metrics grouped under their target**, and the run-total row at the top is every box's readings pooled into one distribution — the same pooled view the baseline comparison uses (§10.2). The columns become Count, Min, Median, p95, Max and Std dev, and a figure the sample count cannot support is drawn as an em dash carrying that count rather than as a blank or a zero. Sorting is within a group: sorting across groups would dissolve the grouping into a flat list and lose which box a row belongs to. The chain-and-step rows of §14.1 arrive with the engine and slot into the same shape.

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

**With no engine, the page draws the host charts of this table**: one per metric, every target overlaid, sharing one x-axis and one crosshair. The load-side charts arrive with the engine and slot in beside them.

**A gap is a break in the line, and that needs nulls in the data rather than absent points** — a plotting library joins across a missing x and draws a straight segment through the window nothing was collected in, which is a picture of data that does not exist, indistinguishable from a flat healthy stretch. So the x-axis is the union of every target's sample times, a target missing one gets a null there, and each recorded gap contributes an x of its own for the case where every target stopped at once. Under the crosshair such a point reads as a dash, not a number.

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

That bulk lives as files under `runs/<id>/` rather than in SQLite, which is what makes a purge a directory delete instead of a transaction that then has to vacuum a database. Host samples are not bulk — they are the rolled-up series this tool is built to read — and are never dropped, so a purged recording keeps every figure, every note, and its place in every trend. **The confirmation names a measured size and lists what survives as well as what goes**: a dialog that guesses at the number teaches people to stop reading dialogs, and one that enumerates only losses reads as though everything is being lost. Purging is recorded as a timestamp, because *purged and empty* and *never written* are different facts about a recording and only one of them is a decision somebody made.

- **Deleting a recording is a different action, and it is also a button.** Purging answers *we are done with the request-level evidence*; deleting answers *this should not be in the history at all* — a bad run, a misconfigured target, an experiment nobody is interested in any more. The two are kept apart rather than folded into one control with a checkbox, because the thing at stake is different: a purge cannot cost you a number and a delete takes all of them, and a person reaching for the first should never be one misread away from the second.

Deletion removes the row and every row that hangs off it — targets, phases, samples, gaps, annotations, host identity, filesystem readings — along with the run directory. SQLite enforces those cascades only when `PRAGMA foreign_keys` is on, so the outcome is asserted by a test rather than trusted to the declaration: orphaned samples would be invisible, would never be read again, and would grow the database forever. The pinned inventory is deliberately *not* cascaded; one snapshot is commonly shared by every run made against an environment that did not change, and it outlives them.

**The confirmation names what a count cannot.** It says which action this is, in the same breath as the other one — *a purge keeps the figures and drops only the request-level data; this keeps nothing* — and it calls out any **baseline** in the selection by id, because deleting one leaves every later recording in that series with nothing to be compared against, and that consequence lands somewhere the person deleting is not looking. Deleting several is one action for the same reason purging a series is: doing it one row at a time is how somebody gives up halfway and leaves an archive in two states. A selection is cleared whenever the list beneath it is reloaded, so narrowing a filter can never turn *delete the three I picked* into *delete three I can no longer see*.

### 17.7 Exports

Three shapes, because they have three readers.

- **JSON** is everything: metadata, summaries, phases, notes, gaps, inventory, and every sample. The summaries travel with it so a consumer never recomputes a percentile and gets a different answer from the one on screen.
- **CSV** comes two ways. The *series* is one sample per row — long rather than wide, because the collectors report whatever a host exposes and a fixed column set would have to be decided before the first row. The *summary* is the table, with `n` as a column rather than a footnote: a spreadsheet is exactly where a figure gets separated from its caveat. A figure the count cannot support is an empty cell, never a zero.
- **A static HTML report** is one self-contained file: no stylesheet link, no script tag, no image URL, and charts drawn as inline SVG. Its reader is the only one who cannot open the recording and ask a follow-up question, which is why it carries its own explanations and **leads with whether the numbers can be trusted at all** — an `invalid` note discovered after the figures have been quoted is a note that arrived too late. Gaps break the polyline there too; that rule holds in every place this tool draws a line.

**Filtering is server-side, and the list says what it is hiding.** The archive is capped per page, so filtering the rows a page happens to hold would answer “nothing matches” for a recording two pages down — a filter that lies is worse than no filter. The header states the count against the size of the whole archive, and states it as a *match* whenever a filter is set, because “12 captured” under an active filter reads as though nothing were filtered. The choices offered come from every recording rather than from the filtered page, so narrowing never removes the way back out.

**Every row says whether it can be trusted before it is opened**: the worst annotation severity it carries, or *clean*. Scanning an archive for the run that went wrong is the thing this list is for, and a row that looks identical to a good one until you open it makes that impossible.

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

**The band is measured from the runs *before* each point, never from a window containing it.** A window that includes the point it is judging widens to swallow exactly the movement it exists to detect: a jump twice the size of normal noise drags the median and the IQR up with it and lands comfortably inside its own band. Trailing also makes the band a claim with a date on it — *this is what normal was before this run* — which is the claim anyone reading a trend is making anyway. It is drawn as a step, holding from each run until the next one re-measures it, because a smooth ribbon would show a band that was never in force at any moment on the chart.

**A band the run count cannot support is not drawn**, the sample-count rule of §12.1 applied to runs instead of samples. Below five usable runs there is no band, and the list of series says how many more each one needs rather than making that discoverable one click at a time. The floor on band width is wider between runs than within one — 5% against 2% — because the two floor different distances: how far a sample strays from its window's median, and how far one run's median strays from the last one's. Two runs of the same setup differ by more than two percent as a matter of course, and a floor that denies it turns an ordinary Tuesday into a finding.

**A run with no supported median is a break in the line, and an invalid run breaks it too.** The first because a withheld number must not be drawn as a zero; the second because a line drawn through a run whose numbers cannot be trusted is a claim the data does not support. The invalid run still gets a hollow point — it happened, and that it failed validity is part of the history.

**The x axis is real time, not run index.** Runs are not evenly spaced, and a chart that pretended they were would hide the fortnight nobody recorded anything in — which is often the explanation for the step everyone is staring at.

Trendable metrics include p50/p95/p99 with intervals, error rate, achieved vs target RPS, chain completion rate, connection reuse, per-phase host stats, recovery time from settle, and — for breakpoint runs — **knee, cliff, and max sustained rate**. That last set is the most valuable longitudinal output the tool produces: capacity over time, one point per run.

### 17.4 Regression detection

A metric is flagged when it moves beyond a configured multiple of the series' noise floor (default 2× IQR), **and** its sample count supports the claim, **and** the run carries no `invalid` annotation. All three conditions, because any one alone produces false positives at a rate that trains people to ignore the flag.

The three are kept apart in the code. §17.3 computes only the geometry — *this point sits outside the band* — and the word *regression* is not used until all three are assembled. A view that called the first condition by the name of the verdict would be wrong on precisely the runs the other two exist to catch.

**A move in the good direction meets all three conditions and is still not a regression.** It is reported as a *change*: worth reading, because an unexplained improvement usually means the test stopped doing part of the work, but not something to fail a build on. The two are named apart rather than merged into "flagged", because the one thing a pipeline does with this is decide whether to stop.

**Not being able to check is a first-class answer, and never a pass.** A series too short to have a band, a run whose sample count cannot support a median, and a run already carrying an `invalid` note are three different reasons the check has nothing to say, and the verdict names the metric and the reason for each. Reporting silence as success is the one failure this check must not have — a pipeline that treats *could not check* as *passed* gets exactly one useful signal out of it, the wrong one, and it gets it on the runs that went most wrong.

The verdict is served at `GET /api/series/verdict?key=…`, for the latest run or for one named with `&recording=…`, with `status` one of `regressed`, `changed`, `ok` and `unknown`. The trend response carries the same document, so the page and the pipeline read one judgement rather than two implementations of it.

Flags are advisory in the UI and available as a machine-readable verdict for the same CI path as §16, so a pipeline can fail on "outside the historical band" as well as on a fixed SLO threshold. The historical check is usually the more useful of the two — fixed thresholds are guesses made before the data existed.

### 17.5 Multi-run comparison

- Select any N runs from a series and overlay their latency bands, histograms, and CDFs.
- **Aggregate a run group** (§12.2) into a single set of statistics by merging HDR histograms — valid because the histograms are mergeable, unlike percentiles. Five 30s runs merged give 11,250 samples and a p99 with roughly the precision a single 150s run would have had, which is the cheapest route past the short-window sample-count limit.

**Distributions merge; percentiles do not**, and that is the whole reason the aggregate is computed the way it is. The mean of five p95s is not the p95 of the five windows together and has no interpretation at all — it is an average of five order statistics, each describing a different set. So an aggregate pools the readings and describes the pooled set once. With no engine that means merging raw host samples; with one it means merging histograms, by the same rule and for the same reason.

**Merging is refused across runs of different setups.** Runs of one setup are repeats of a single measurement and pool into a better version of it. Runs of *different* setups measure different things, and their combined distribution describes nothing that exists while carrying a sample count that would make it look authoritative. The side-by-side columns and the overlay stay — reading two setups against each other deliberately is the sanctioned way to compare across a setup change (§17.2) — and only the merged column is withheld, with the differing part of the identity named rather than the column silently dropped.

**A comparison holds at most six runs.** Not a storage limit; an overlay with more lines than there are distinguishable colours stops being readable, and the answer to that is fewer runs rather than more hues — the same bargain the per-target charts already make.

The overlay draws seconds since each run started rather than wall clock, which is what makes two runs of the same plan lie on top of each other, and pools each run's boxes into one line: six runs across three boxes is eighteen lines, which is not a chart. The per-box view is §15, one run at a time.
- Per-phase comparison across runs: baseline vs baseline (environment drift), measure vs measure (the actual question), settle vs settle (is recovery degrading?).
- Step-aligned comparison for breakpoint runs: the same rate step across two runs, side by side.

### 17.6 Sweep comparison

A sweep (§3.5) is its own comparison view: one plan, many targets, one time window. A recording covering more than one box **is** the sweep — the targets, their phases and their attributes are already stored per box, so nothing has to be tagged or grouped for this view to exist.

- Targets ranked on each headline metric, with the sweep's own spread as the reference — the outlier logic of §17.4 applied across targets rather than across time.
- Per-target attributes from the inventory shown alongside (instance type, AZ, image digest, task definition revision), because the explanation for an outlier is usually sitting in that row: an older digest, a different instance type, a lone task in another AZ.
- Baseline-phase stats compared across targets, which catches the case that otherwise sends you chasing a phantom regression — one container was already loaded before the test started.
- Repeated sweeps form a series of sweeps, giving per-target trends: is that container reliably slow, or was it unlucky once?

**Every box is judged against the others and never against a set containing itself.** The trend view refuses to measure a band over a window holding the point it is judging, because such a window widens to swallow the movement it exists to detect; one slow container inflating the spread it is then compared against is the same mistake with the axes swapped. Each row therefore carries its own band, and the bar behind each row draws that one rather than a shared average.

**Outside the band is not the verdict**, exactly as in §17.4. A box is called the odd one out only when it is outside, its sample count supports the figure, and it is not already carrying an `invalid` note — and a box carrying one is kept out of everybody else's band, since a reading nobody trusts cannot define normal. A note naming no target belongs to the whole recording and so covers every box in it.

**A sweep too small to describe its own spread is ranked and not judged.** An interquartile range over three numbers is not a description of spread, so below the peer floor the ordering, the attributes and the baselines are all still shown and no accusation is made. Each unjudged row says which of the three conditions it missed, because a blank in a ranked table reads as a pass.

**The history is read only for boxes something was flagged on**, and each earlier sweep is re-ranked by the same rule, so "flagged before" means what it means now. A box no earlier sweep carried — the normal case for ephemeral tasks, which the environment replaces between runs — says so rather than showing an empty trend.

---

## 20. Plan editing in the UI

The front end is the expected authoring surface (§4), but the three documents are not equally editable — the split of §4 is what makes differentiated permissions natural rather than arbitrary.

| Document | In the UI | Why |
|---|---|---|
| **Mix** (§4.2) | **Full editing** | This is the knob that gets turned: percentages, total rate, duration, phases, session policy per chain |
| **Targets** (§4.3) | **Full editing** | Pick a profile, re-resolve it, select or deselect individual hosts and containers |
| **Calls** (§4.1) | **Read-only inspection** | Request definitions are authored by an LLM from the codebase or generated from a schema; editing them by hand in a browser invites drift from the service they describe |

### 20.1 Editing the mix

The mixture editor shows a table where every row is one single-call chain and its RPS. The table derives the total load rate and normalizes the stored percentages, so it never exposes a half-valid percentage total; lowering the run below the 2250-sample floor (75 RPS for 30 seconds) remains a warning, not an invalid plan. Plans with multi-step chains or another shape the table cannot represent keep their chain definitions unchanged. Their Load controls remain editable, and the page explains that the richer chain definitions are carried into the exported bundle.

Rows show chain name, call, RPS, expected status and request type. Call and request type are selectors over the existing read-only call definitions: choosing a request type selects among calls of that type rather than mutating a call. A final row cannot be deleted, blank names are repaired to unique names, and an empty or zero-rate table is normalized to a runnable minimum when committed.

The editor is deliberately compact: calls occupy the primary column and the less-frequently changed load and phase controls sit alongside them. The table derives total RPS from call rates; plans whose chains cannot be edited in the table can set total RPS in Load. The open/closed model is not exposed. Warmup and settle share a row, concurrency is last, and `stages` and `breakpoint` are labelled as not implemented in the editor. The server verdict, sample-count consequence and explanatory notes sit below the working controls rather than above them.

Duration, warmup, concurrency cap and phase durations are editable here, with the §12.1 sample-count consequence shown live. A `stages` ramp and a `breakpoint` search carry their own shape and are shown rather than edited; they belong with the sweep that runs them (§17.6). Multi-step chains, session policies and polling remain in the plan document and export unchanged through this editor.

**Every figure on the page is computed server-side.** The percentages, the implied rates, the request counts and the verdict all come from one function, which is also the one the save path and the bundle gate call. The browser converts a typed rate into the share the document stores — the document has to be built before it can be submitted — and renders what it is told about everything else. A second rulebook in JavaScript would be a rulebook to drift from the first.

**Errors stop a run; warnings do not, and both are shown.** An error means the bundle will not assemble: a shortfall in the percentages, a chain with no steps, a pooled session with no pool size, a duplicate chain name or step id. A warning means the run will happen and will produce a number that should not be quoted: below the sample floor, a pool size on a chain that does not pool, a call no chain invokes, or a step reading a variable no earlier step extracts — the last of which is how reordering two steps silently breaks a chain.

**Saving and running are separate.** A mixture with errors still saves, because half-finished is a normal state to leave an afternoon's work in and an editor that refuses to save loses it. What errors stop is assembly: `GET /api/plans/{name}/bundle` refuses and names them, since the engine would reject the same document a moment later and a zip that cannot run still looks like an artifact. A bundle is assembled from the plan **on disk**, so the page says when the form has moved on from it.

**Running is the same act as assembling, one step further.** `POST /api/recordings` takes a profile, and naming a plan alongside it makes the recording a load run: the observer starts watching the boxes, the bundle is assembled from the plan on disk and the profile as it resolves *now*, and the engine is spawned against it. A load run is an observation with traffic attached — the same recorder, the same collectors, the same live view, the same Stop button — and the difference is `kind`, which keeps an environment watched at rest out of the same series as the same environment under load.

The order is the interesting part. The plan is refused before anything is opened if it cannot run, and the engine binary is looked for before a row exists, because a recording created for a run that never started is a recording somebody has to explain later. The bundle is assembled *after* the observer starts, from the profile the observer actually resolved, so the boxes traffic goes to are the boxes the recording is about.

**A run ends when the traffic does.** The observer has no opinion about when that is, so the engine's exit closes the recording, and pressing Stop asks the engine to finish rather than stopping the collectors out from under it. Both can happen at once — an engine finishing and a person pressing Stop race by nature — and whichever arrives first does the work.

**A run with nothing to watch is still a run.** Measuring a service nobody can log into is ordinary, so a load run against a profile with no collector is allowed, pinned to the boxes it sends to, and annotated to say the host statistics are absent rather than failed. An observation of the same profile is still refused: watching nothing is not a measurement.

**A save cannot move the plan hash.** The editor writes back the document it loaded with only the fields the form owns replaced — auth, datasets, generators, capture, SLOs and engine tuning survive a save they were never shown in, and are listed as carried so that "preserved" does not look like "gone". The bytes are written by the same rule the bundle is hashed under, so re-saving an unchanged plan is a no-op at the byte level rather than a rename of its whole series (§17.2).

### 20.2 Editing targets

Choose a profile and re-resolve it (§3.1), then include or exclude individual entries from the inventory. Each row shows what discovery found — instance type, AZ, task definition revision, image digest, health — because that is what a person needs to decide whether a box belongs in the run. Addressing mode (through the load balancer or direct to a container, §3.4) is a choice here, with its incomparability warning attached.

### 20.3 Inspecting calls

A read-only list, sufficient to understand what a chain does without opening a file:

- **Name** and **description** — a free-text `description` field on every call, which exists for this view. Generated plans populate it from the OpenAPI summary; LLM-authored plans should write one.
- **Method and path template**, with the variables it consumes and extracts made visible, so the dependencies between steps in a chain are legible.
- **Generation strategy** — inline template, dataset, Lua, Rust plugin, or exec sidecar (§7.2), with the generator's name and, for Lua, the file and entry point.
- **Assertions**, listed plainly — this is where a chain like `login-fail` explains itself as expecting a 401.

No editing at this time. The affordance offered instead is **export the bundle** (§4.4): change the call in the file, with the tooling and review a code change gets, and re-import. If hand-editing calls in the browser turns out to be needed, it should arrive as a deliberate decision with validation behind it, not as a text box.
