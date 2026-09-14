# Metrix — Engine Design

The load generator: what it executes, how it measures, and why the numbers can be trusted. Pairs with [design-api.md](design-api.md); the boundary is in [design-api-engine-contract.md](design-api-engine-contract.md).

> **Status: B1 complete.** Standalone fixed-rate HTTP execution produces interval histograms, bounded NDJSON and live scheduler self-metrics. See [implementation-engine.md](implementation-engine.md) for the remaining milestones.

The engine takes one self-contained plan bundle and emits NDJSON. It knows nothing about the API, the database, the UI, or AWS, and nothing in this design may assume otherwise.

*Section numbers are preserved from the original combined outline, so cross-references between these three documents remain valid. Numbers are therefore not contiguous within any one file.*

---

## 2. Architecture — the engine

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

---

### 2.2 Mock target (B1.1)

`cargo run --manifest-path engine/Cargo.toml -p metrix-mock -- --config examples/mock.json`
starts a standalone cleartext HTTP/1.1 / HTTP/2 (prior knowledge) target. `--listen`
overrides the default `127.0.0.1:8080`; port zero prints the assigned address.
With no config it returns JSON 200 responses after a fixed 10ms delay on any route.

The strict JSON config supplies `seed`, `latency`, optional `errors`, `slow_start`,
`capacity_rps`, `max_in_flight`, and `max_connections`. Latency is `fixed` (`ms`),
`normal` (`mean_ms`, `stddev_ms`), `lognormal` (`median_ms`, `sigma`, in log space),
or `bimodal` (`fast_ms`, `slow_ms`, `slow_probability`). Samples are clipped to
`[0, max_latency_ms]` (default ceiling 60000ms), including slow-start's extra delay,
which decreases linearly from `extra_latency_ms` to zero over `duration_ms` since
server start. These are injected delays after the request body is drained, not
promises about network latency; successful and HTTP-error responses expose the
planned delay in `x-metrix-mock-delay-ms` so measurements can be checked against it.

Each error has an unconditional `rate`; their sum must not exceed one. Types are
`http` (400–599 `status`), `disconnect` (no response), and `timeout` (no response
for `delay_ms`, then disconnect). HTTP/2 transport faults reset the affected stream.
A seeded RNG makes arrival-ordered decisions repeatable with the locked build;
concurrent arrival order and operating-system timing are not reproducible. All
configured milliseconds must be finite and in `[0, 3600000]`; slow-start duration
and timeout delay must be positive. Lognormal median must be positive and sigma
finite in `[0, 100]`. Unknown fields and invalid probabilities fail before binding.

`capacity_rps` is a token bucket with one second of burst, initially full; excess
requests get immediate 503s. `max_in_flight` independently rejects excess active
requests with 503, including multiplexed HTTP/2 streams. `max_connections` closes
newly accepted sockets at the limit (transport rejection, not a guaranteed TCP
ECONNREFUSED). Idle keep-alive sockets count until closed. Rejections do not consume
random samples. Limits are disabled when omitted; zero is invalid. Ctrl-C stops
the listener and cancels outstanding connections. The mock may allocate and lock;
the generator's hot-path constraints do not apply to this test target.

---

### 2.3 Fixed-rate execution (B1.2)

`metrix-engine --plan examples/plans/mock-fixed` executes the initial supported
subset: one target, one 100% chain, one static call, fixed open load, and the
full B2.1 phase timeline. Later-step features (assertions, extraction, generators,
auth, sessions beyond stateless `fresh`, mixtures, sweeps, SLOs, redirects and target
Host/SNI overrides) fail before network I/O. This keeps partial execution from
silently producing a different workload. Bundle files must stay within its root.

Arrival `n` is due at monotonic start + `n / rate`, with start inclusive and end
exclusive. Responses never move that clock. An occupied request or connection cap
drops that arrival; a late wake-up skips expired arrivals and admits at most the
latest due one. There is no catch-up burst or unbounded work queue. Admission uses
preallocated reusable future slots, with no per-request task spawn or scheduler
lock. HTTP framing, headers and connection establishment still allocate inside the
transport; the allocation-free rule applies to scheduling and aggregation state.
One deadline thread uses native `std::thread::sleep` timers (high-resolution on
current Windows); an atomic waker coalesces notifications when the scheduler is
busy. It sleeps until a bounded margin before the deadline, then spins for at most min(1ms, one-quarter of the arrival interval), avoiding final-sleep overshoot without consuming a full core at high rates. It never waits for admission or request completion, and cancellation stops
it within its bounded sleep slices. This avoids Tokio's coarse Windows timer
wake-ups dropping traffic at the 75 RPS acceptance rate.
The timeout (default 5000ms) covers connection/readiness, send and complete body
drain. Natural completion drains admitted requests under their original deadlines;
Ctrl-C cancels them and closes owned connections. Requests are never retried.

Each target's `http_version` is `auto` (default: cleartext HTTP/1.1, TLS ALPN preferring
HTTP/2), `http1`, or `http2` (cleartext prior knowledge; TLS requires ALPN `h2`).
TLS verifies the address hostname/IP using compiled-in WebPKI roots. One connection
is established before the arrival clock starts; HTTP/1.1 grows a reusable pool up
to `connections_per_host` (default 256), while HTTP/2 multiplexes over one socket.
Connection and stream failures are counted without logging request/response data.
`max_concurrency` defaults to 200; worker threads default to physical cores minus
one, minimum one. DNS resolution happens at setup; reconnects use those addresses.

`--summary` defaults to `-` (stdout); `--events` is opt-in. Each accepts a new file
or `-`, but cannot share a destination. Both carry lifecycle and annotations;
only summary carries 250ms interval histograms, and only events carries requests.
Writers run outside the scheduler with bounded queues, reserved lifecycle capacity
and nonblocking admission. Lost records raise `events_dropped` on either healthy
stream; stderr also reports losses and unfinished writers. Shutdown waits at most
500ms for output, so a blocked pipe may end without its final records. Files are
created exclusively, preventing accidental overwrites of a bundle or recording.
`--sample-rate` in [0, 1] selects iterations deterministically using `--seed`
(default 0); sampled requests are labelled and intentional omissions are counted
separately from backpressure. Request events retain identifiers and numeric timings,
never URLs, headers, bodies or raw transport errors. Unavailable DNS/connect/TLS
timings are omitted; cancellations use `other` with a fixed message and elapsed
attempt duration. Transport errors without an exact OS cause use `other`.

All timestamps use one monotonic run clock, including setup and final drain.
Every phase boundary flushes its partial window. Required metadata includes a
SHA-256 digest of length-framed relative
paths and the exact bytes of the mix, targets and referenced call files, sorted by
path. The frozen v1 schema requires numeric OS resource fields: until CPU, RSS and
file-descriptor probes land, zero is an unavailable sentinel explicitly identified
by a `self_metrics_unavailable` annotation, never evidence of generator health.
Each summary has a `generator_self_metrics` annotation carrying send-drift and
scheduler-lag sample counts, so an empty interval is distinguishable from zero lag.
Stderr retains the final traffic diagnostic. HTTP status codes are responses, not
assertion failures. Exit 0 means execution finished, 1 setup/internal or output
failure, and 130 interruption; SLO verdicts land in B4.6.

---

## 3. Targets as the engine sees them

*Profiles and ECS discovery are API-side (§3.1–3.3); the engine receives concrete addresses. See design-api.md.*

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
- Output is a **sweep**: one `run_id` groups the targets in a single monotonic time window. `run_started.targets` records seed-determined order; each target has its own start/finish and phase records. Histograms and sessions are independent, and gaps are cancellable. Every target is compiled before traffic begins. Each queued packet retains its target and step identity, so losing lifecycle records under backpressure cannot relabel data or stop the sweep.

The comparison this enables is the valuable part — same plan, same conditions, different container. You are looking for the odd one out: a task on a noisy neighbor, an instance of a different type, a container still running an older image digest. The sweep view ranks targets on each headline metric and flags any target outside the sweep's own spread, which is the §17.4 measured-noise-floor logic applied across targets instead of across time.

A sweep deliberately varies the target, so its runs do **not** form a single series under §17.2. They are grouped as a sweep; repeating the whole sweep produces a series *of sweeps*, which gives per-target trends over time and answers "is that one container always the slow one, or was it slow once?"

---

## 4. The plan format

**Three documents, kept deliberately separate**, because they are authored by different parties, change at different rates, and are reused differently:

| Document | Contains | Changes when |
|---|---|---|
| **Calls** — `calls/*.json` | Individual request definitions: method, path, headers, body, assertions, extraction | The service's API changes |
| **Mix** — `mix.json` | Named **chains** of calls, and what percentage of traffic each gets; total rate, load shape, phases | The question being asked changes |
| **Targets** — `targets.json` | The boxes to run against (§3) | The environment changes |

The value of the split is that each can move without disturbing the others. A new endpoint adds a call and touches no mixture. Changing "20% writes" to "40% writes" edits one number in one file and leaves every request definition untouched. Pointing the same test at a different set of containers replaces one file. A monolithic plan makes each of those a diff across everything.

It also matches how they are produced: calls are mechanical (derivable from OpenAPI or a codebase, §8), the mix is judgment, and targets come from discovery.

**These are normally not written by hand.** The UI is the authoring surface — edit the mixture, adjust percentages, see implied per-chain RPS before running. The API exports a complete, runnable bundle for anyone who wants to run or tweak one directly (§4.4), and hand-authoring is fully supported, but it is the exception rather than the assumed workflow.

JSON throughout (YAML accepted and converted), with published JSON Schemas so an LLM can be handed the schema alongside the codebase.

### 4.1 Calls — the individual test call

One file per call, or several grouped in one file. A call knows how to make one request and how to judge the response. It knows nothing about how often it runs or what runs before it.

```jsonc
// calls/products.json
{
  "list-products": {
    "description": "Product listing, first page — the most common read",
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
    "description": "Submit an order as XML; returns 201 or 202 with an order id",
    "method": "POST",
    "path": "/api/orders",
    "headers": { "Content-Type": "application/xml" },
    "body": { "generator": "order-xml", "args": { "user": "{{ users.email }}" } },
    "assert": [ { "status_in": [201, 202] }, { "xpath": "/order/id", "exists": true } ],
    "extract": { "order_id": { "xpath": "/order/id/text()" } }
  }
}
```

Because a call is standalone, it is also directly runnable for a quick check. The unit you name is the same unit the mixture names — a chain: `metrix-engine --chain checkout` runs one iteration of that chain, in order, and prints each response with its assertion results. A single-call chain is simply the degenerate case.

Naming chains rather than calls is what makes this work: a call like `get-product` depends on `{{ pid }}` extracted by an earlier call, so firing it in isolation would have nothing to bind. Running the chain supplies its own prerequisites by construction, and one command means one concept, whether it appears in `--chain` or in the mixture.

### 4.2 Mix — the load testing mixture

References calls by name, arranges them into chains, and sets the shape of the load.

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
    "rate": 150,                  // total chain iterations per second, split by percent
    "max_concurrency": 200        // hitting it annotates the run (§13.1)
  },

  "datasets": {
    "users": { "file": "data/users.csv", "mode": "round_robin" }
  },

  "generators": {
    "order-xml": { "type": "lua", "file": "gen/order.lua", "entry": "generate" }
  },

  // the mixture: named chains, percentages must total 100
  "chains": [
    { "name": "login-fail", "percent": 30, "session": "fresh",
      "steps": [ { "id": "post", "call": "login-bad-password" } ] },

    { "name": "login",     "percent": 10, "session": "fresh",
      "steps": [ { "id": "post", "call": "login" } ] },

    { "name": "logout",    "percent": 5,  "session": "reuse",
      "steps": [ { "id": "post", "call": "logout" } ] },

    { "name": "search",    "percent": 20, "session": "reuse",
      "steps": [ { "id": "query", "call": "search-products" } ] },

    { "name": "cart-add-remove", "percent": 20, "session": "pool", "pool_size": 50,
      "steps": [ { "id": "add",    "call": "cart-add" },
                 { "id": "remove", "call": "cart-remove" } ] },

    { "name": "checkout",  "percent": 15, "session": "fresh",
      "steps": [ { "id": "create", "call": "create-order" },
                 { "id": "poll",   "call": "get-order",
                   "repeat_until": { "json": "$.status", "equals": "complete",
                                     "max_attempts": 5, "interval_ms": 200 } } ] }
  ],

  "engine": { "worker_threads": 8, "connections_per_host": 256 },
  "capture": { "error_samples": 10, "body_max_kb": 64, "redact": ["Authorization"] },
  "observe": { "interval_ms": 1000, "collect": ["cpu", "memory", "disk", "net", "process"] },

  "slo": [
    { "metric": "p99_latency_ms", "chain": "browse", "max": 400 },
    { "metric": "error_rate", "max": 0.001 }
  ]
}
```

A step may override a call's fields inline for the one case where the same endpoint is used differently in two chains. Overrides are shallow and discouraged — two genuinely different requests should be two calls.

**Session policy is declared per chain**, because whether a chain needs a fresh session is a property of the behavior being modelled, not of the service:

| `session` | Behavior | Models |
|---|---|---|
| `fresh` | New session per iteration: new auth identity, empty cookie jar | A first-time or logged-out user. Exercises the login path and cold per-user caches on every iteration. |
| `reuse` | One session held for the life of the virtual user | A returning user working through a warm session |
| `pool` (+ `pool_size`) | A fixed set of sessions cycled across iterations | A realistic population — neither all-new nor all-one |

A session here means the cookie jar plus the auth identity binding (§6.2); the two move together, since a fresh session that reused a token would not be fresh in any way the service can tell. The distinction matters more than it looks: `fresh` on every chain overstates login load and destroys cache locality, while `reuse` everywhere hides both, and the difference between those two mistakes is easily a factor of two in apparent capacity.

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

- **The mixture is percentages of one total rate, and they must sum to 100.** A mix is a rate and a set of named chains with a share each — `150 rps`, `checkout 15%` — because that is how the mixture is actually reasoned about. Anything else is a validation error naming the shortfall or excess, rather than a silent renormalization that makes the numbers you read differ from the numbers you wrote. The editor accepts a target RPS per chain as an alternative input and converts it to a percentage, and always shows both.
- **Chains are the unit of the mixture**, so a percentage buys *iterations of that chain*, not requests. `cart-add-remove` at 20% of 150 rps is 30 iterations/s and 60 req/s. The Config screen shows both figures, since this is the easiest number in the file to misread.
- **Every call carries a `description`.** Free text, one line, and the only field in the format that exists purely for a human reading it — it is what the read-only call inspector shows (§20.3). Generated plans fill it from the OpenAPI summary.
- **`id` on every step**, so chart series, error reports, and SLOs have a stable key independent of which call the step invokes.
- **Expected failures are not errors.** A chain like `login-fail` asserts a 401; when it gets one, that is a pass. Assertions define the expected outcome, so a deliberately failing chain contributes to latency and throughput statistics without inflating the error rate — and starts raising errors only when it stops failing the way it should.
- **Assertions are declarative and enumerable.** No expression language. An LLM emits them straight from a schema, and a failure produces a specific message (`$.items expected min_length 1, got 0`) rather than a stack trace.
- **Templating is deliberately tiny** — `{{ var }}`, `{{ dataset.field }}`, and a fixed function set (`rand`, `uuid`, `now`, `seq`, `pick`). Anything more is the generator's job (§7), and a small inline language is what keeps plans statically checkable.
- **Validation is per document and cross-document.** `POST /api/plans/validate` checks each file against its schema *and* checks that every `call` reference resolves, returning JSON Pointer paths. An unresolved call name is the characteristic error of this design, so it gets a specific message naming the step and the missing call.
- **Every load-time error is `<file>#<json-pointer>: <message>`** — `calls/shop.json#/create-order/path`, `mix.json#/chains/1/percent`. A pointer rather than prose because the editor shows these against the field being edited, and prose is not something it can locate. A call names the file it was defined in, since a bundle can hold a dozen call files and the call's own name says nothing about which one to open. The pointer points at what is wrong rather than at what was edited: a percentage total is a property of `/chains`, and a chain that never runs is a property of `/chains/1/percent`.
- **`--chain <name>` runs one chain of the mixture on its own, at the whole rate.** For working on a plan rather than measuring with one: a chain at 3% sends a request every few seconds, and finding out whether its extraction works should not take four minutes. The run is annotated to say it was narrowed, because a run of one chain out of six is not a run of the mixture and a stored run that did not say so would be compared against ones that were.

---

## 5. Chaining

Each chain is a sequential chain executed by one virtual user with its own variable scope.

- **Extraction:** JSONPath for JSON, XPath for XML, plus header and regex extractors. Response `Content-Type` picks the default parser; an explicit extractor type overrides it.
- **Failure policy:** per-step `on_failure` of `abort` (default — record the chain as failed at this step), `continue`, or `retry` (once). Aborted chains are counted separately from failed requests, so one upstream 500 doesn't inflate the error rate three times over.
- **`repeat_until`** covers the async-job pattern (POST returns 202, poll for completion) without a loop construct. Every attempt is a real request and is counted as one; the waiting between them is kept out of request latency and lands in the chain's end-to-end duration. Exhausting `max_attempts` without the value ever matching fails the step — a step that gave up has not seen the job finish, and calling that a success reports a service that completes nothing as healthy.
- **A failed assertion is not a transport failure.** The request happened and the service answered; what is wrong is the answer. Counted under its own class and named by the assertion's index in the call, for the same reason aborted chains are counted apart: one upstream problem should appear once. A chain that expects a 401 therefore *passes* when it gets one.
- **Think time:** optional `delay_ms` between steps, fixed or distribution-based. Off by default — in short windows you usually want the chain tight.
- **Chain latency is reported end-to-end as well as per step.** Per-step numbers find the slow endpoint; end-to-end is what a user actually feels.

**Rate accounting with chains:** a chain `rate` means *iterations started per second*, not requests per second. A 3-step chain at 25/s is 75 req/s. The Config screen shows both numbers, because this is the single easiest thing to misread in a generated plan.

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
  rows          dataset rows by name
  rng           seeded RNG
  args          the call's own arguments
```

**A generator attaches to the call, not to its body.** The hook returns the whole request, so the field that names it cannot be `body` — a `body` block returning a path is a field lying about what it does. A call declares `generate: { generator, args }` alongside its method and path; whatever the hook returns replaces that part of the request, and whatever it omits keeps what the call wrote. The call therefore still reads as a request — method, a path, the headers — with the generator filling in what a template cannot express. `args` are templated like any other field, so a generator can be handed `{{ users.email }}` without knowing datasets exist.

`rows` is keyed by dataset name rather than being a single current row, because a plan reading a user and a product has two current rows and one of them would have to be the wrong one.

Seeded **per iteration** from the run seed, which is recorded in run metadata — so a run can be replayed with identical generated traffic. Without that, comparing two runs means comparing two different workloads. Per iteration rather than per virtual user: which VU picks up an arrival depends on how long the service took to answer the arrival before it, so a per-VU stream replays differently against a service that has since got slower. Keying it to the iteration number makes iteration 4,001 generate the same request in every run of the plan, which is the property the seed exists for. Every step of one iteration draws from the same stream in order, so a chain that posts `{{ uuid() }}` and then reads it back sends the same id twice.

### 7.2 Tiers

| Tier | Cost per call | Use |
|---|---|---|
| Inline template | ~ns | `{{ var }}` substitution in path, query, headers, body |
| Dataset | ~ns | CSV/JSONL, `round_robin` / `random` / `unique_per_iteration` |
| **Lua (embedded)** | ~µs | **Default for anything dynamic** |
| Rust plugin | ~ns–µs | Heavy generation: large XML, signing, compression |
| Exec sidecar | ~10–100µs + IPC | Escape hatch: a script that already exists |

**Datasets are read into memory at load and then only indexed.** No file handles on the hot path and no I/O inside the measured window — a generator reading from disk per request puts the test machine's page cache into the latency distribution. Which row an iteration gets is a pure function of the iteration number and the seed: no cursor and nothing shared between workers, which is what makes `round_robin` really round robin rather than round robin per worker, and what makes a replay send the same rows in the same order. `unique_per_iteration` is checked against the arithmetic of the run at load — a file with fewer rows than the run has iterations of the chains that read it is refused, naming both numbers, because wrapping would take away the one thing that mode promises.

**Lua is the default answer to "I need real logic here."** Embedded via `mlua`, one VM per worker thread, reused across requests — no process boundary, no serialization, no IPC. It is fast enough to sit on the hot path at the rates in scope, and it keeps generation logic inside the plan's directory rather than in a separate deployable.

```lua
-- gen/order.lua
function generate(ctx)
  local id = ctx.vars.pid or ctx.rng:int(1, 10000)
  return {
    path  = "/api/orders/" .. id,
    query = { region = ctx.rows.users.region, expand = "lines" },
    body  = string.format(
      "<order><user>%s</user><qty>%d</qty></order>",
      ctx.rows.users.email, ctx.rng:int(1, 5))
  }
end
```

```jsonc
"generators": {
  "order": { "type": "lua", "file": "gen/order.lua", "entry": "generate" }
}

// and the call that uses it
"create-order": {
  "method": "POST", "path": "/api/orders",
  "headers": { "Content-Type": "application/xml" },
  "generate": { "generator": "order", "args": { "user": "{{ users.email }}" } }
}
```

Sandboxed: no `os.execute`, no network, no `require` outside the plan directory. **Read-only file access within the plan directory is allowed** — a generator that needs a static corpus (sample payloads, a fixture set, a word list) should be able to read it without an exec sidecar.

Corpus files are **loaded once into memory at run start and shared across every Lua VM**, not read per call. They are expected to be small enough for that, and the engine enforces it: a configured size ceiling, checked at load, with a clear failure rather than a slow run. No streaming, no lazy reads, no file handles on the hot path — the whole point of allowing reads is convenience at setup, not I/O during the measured window.

A corpus is declared on the generator that reads it, as a directory inside the bundle and a ceiling over everything under it:

```jsonc
"order": { "type": "lua", "file": "gen/order.lua",
           "corpus": { "dir": "corpus", "max_kb": 4096 } }
```

There is no `read` call, because there is no reading: the files are already in memory when the script first runs, and the script sees them as two read-only globals.

```lua
local body = corpus["payloads/order.xml"]              -- by name
local pick = corpus[corpus_names[ctx.rng:int(1, #corpus_names)]]
```

Keys are paths relative to `dir`, with forward slashes on every platform so a plan reads the same on the box it was written on and the box it runs on. Values are byte strings, so a binary fixture works as well as a word list. Both globals are read-only: a VM outlives the iteration that used it, and a script that could write to the corpus would be leaking one iteration's state into the next. A name that is not there fails the generation naming what is, rather than substituting nil into a request.

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

## 9. Metrics to track

Organized by the question each answers. Time series are bucketed at 250ms internally and rolled up for display.

**B1.3 aggregation.** Preallocated logical worker partitions own counters and HDR
histograms exclusively; the B1.2 scheduler polls them in one task, so they are not
thread-local Tokio data (tasks can migrate). Admissions select partitions round
robin and retain that owner through completion. Each partition records without
locks, dynamic maps or resizing. Histograms use microseconds, three significant
digits, and an explicit one-hour ceiling; larger samples increment an overflow
count and are not clipped. Sub-microsecond durations record zero. Serialized
histograms use HDR V2 binary encoded as base64; empty distributions omit extrema,
mean and bytes. Exact extrema and mean are retained alongside the bucketed HDR.

Admissions increment attempted/started; terminal success increments completed;
transport failure increments failed/aborted. Cancellation is counted separately
and contributes no latency sample. HTTP statuses are counted even when body drain
fails; HTTP errors remain transport successes until assertions land. Chain duration
runs from admission to terminal result (including setup); request total runs from
send to terminal result, and TTFB is sampled only when response headers arrive.
Drift samples are recorded once when sends are observed, including requests still
in flight or subsequently cancelled. Payload bytes and opened/reused connections
are accounted at completion; the initial setup connection is counted once. Pending
cancelled attempts therefore contribute no terminal send/byte/connection samples.

On each 250ms tick, merge and reset partitions into an interval accumulator and
add it to cumulative totals. Windows use actual monotonic elapsed time; missed
ticks coalesce rather than inventing empty historical windows. Final drain or
cancellation flushes a partial window. The report retains totals and the last
window only. An optional bounded channel receives window copies via `try_send`;
full or closed channels increment a dropped-window count and never delay traffic.
Snapshot allocation/serialization occurs only off the per-request recording path.
NDJSON carries these populations in B1.4; percentile support rules remain B2.2.

### 9.1 Throughput & volume
- Requests attempted / completed / failed — overall, per chain, per step
- **Achieved RPS vs target RPS** over time (the gap is the headline number)
- Chain iterations started / completed / aborted
- Bytes sent and received; payload throughput MB/s — separate from request rate, since large XML bodies saturate links long before they saturate CPU
- Request and response body size distribution

### 9.2 Latency — per step, per chain, aggregate
- min / mean / p50 / p75 / p90 / p95 / p99 / max, plus stddev. p99.9 is computed and stored but displayed only when the sample count supports it (§12.1) — at the 30s/75 RPS floor it does not.
- Full HDR histogram retained per run, so percentiles can be recomputed over any time slice after the fact and runs can be merged correctly — you cannot average percentiles
- Rolling percentiles over time (p50 / p95 / p99 bands)
- **Phase breakdown:** DNS resolve, TCP connect, TLS handshake, request write, **time-to-first-byte**, body transfer, total. TTFB vs total separates "the server is thinking" from "the response is big or the link is slow."
- **End-to-end duration** per chain
- **Corrected latency** (coordinated-omission adjusted, §12.2) reported alongside raw, never instead of it
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

### 9.8 Run metadata (reproducibility & comparison)
Every `run_started` record carries the plan hash, plan name, engine version, seed, start timestamp, target ids and machine-profile id when calibrated. These fields are the stable standalone identity of a B2 run: the same bundle and seed can be replayed, while the machine profile keeps a generator change from being mistaken for a target change. Later target build data, environment tags and notes are API-owned recording metadata. Without this identity the Recordings archive becomes a pile of numbers nobody trusts.

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

B2.1 runs this timeline after target connection setup, using one monotonic clock.
Zero-length baseline, warmup and settle phases are skipped; drain is always marked.
Baseline and settle emit idle summaries and send no requests. Warmup is optional,
at the configured fixed rate; `load.duration` covers measure only. The two traffic
phases have absolute schedules anchored to their boundaries, with expired arrivals
skipped on late wakes. Warmup connections and request slots carry into measure.
Each request keeps its admission phase, including its event, send drift and terminal
result. Separate accumulators exclude all warmup samples from the measured report,
even when they finish in measure or drain. Such completions produce additional
`warmup` summaries over the same window; their target rate is zero and companion
annotations state the current timeline phase. The primary summary carries global
in-flight/queue gauges; additional warmup summaries carry only warmup gauges.
Measure results completed during drain remain in measured totals. Drain waits for
all admitted attempts under their original deadlines, then closes connections and
starts settle. Ctrl-C cancels requests, flushes the current phase and emits no
unreached phase transitions. Full phase support remains independent of an observer.

**Delta-from-baseline** is a first-class derived series: for every host metric, the observed value minus its baseline-phase median. This is usually the series you actually want to read, and is what the target-side charts plot by default, with absolute values a toggle away.

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

A step is a miniature run: its own warmup exclusion, histogram and annotations. Only the first step has the baseline idle window; intermediate settle windows use `step_recovery`, and the final step uses the full settle window. The API can join host observations against these step timestamps. Step results are never pooled into one run-wide percentile — that would average a healthy 75 RPS step with a collapsing 900 RPS one and describe neither.

`load.breakpoint` holds the search parameters shown above; `load.duration` remains required for document compatibility. `step_duration` is required (normally 30s) for the reason in §12.1: below that, a step's p99 is not worth acting on, and a breakpoint run that locates a knee using unsupported percentiles is worse than no run at all.

`step_recovery` matters more than it looks. Without an idle gap, step N+1 inherits step N's queue backlog, and the search finds a false early cliff that is really just accumulated debt. The gap is observed, not slept through — it is a miniature settle phase.

### 11.2 Stop conditions

After a one-second measurement grace, the search checks cumulative conditions on each 250ms tick (error-rate decisions require at least 100 terminal requests). The search halts at the first triggered condition, records which one fired, and proceeds directly to drain and settle (§10.1) — the recovery curve after a deliberate overload is one of the more useful things this mode produces.

| Condition | Meaning |
|---|---|
| `error_rate` exceeded | The cliff — it is returning failures |
| `p99_latency_ms` / `p99_multiple_of_baseline` | The knee — still correct, no longer acceptable |
| `rate_shortfall_pct` | Saturation — it cannot absorb what is offered |
| `max_rate` reached | No breakpoint in range. Report exactly that; do not imply one was found. |
| generator-limited | *We* broke, not the target — §11.3 |

### 11.3 Distinguishing target saturation from generator saturation

The failure mode that makes a breakpoint run worthless: offered rate stops climbing, latency rises, and the run reports a target limit that is actually the generator's limit.

Before attributing any shortfall to the target, the engine checks its own state (§9.6, §13.2): send-schedule drift, queue depth, generator CPU, body-generation latency, socket and ephemeral-port exhaustion. The implemented probes are drift, missed arrivals, concurrency-cap occupancy, connection admission and body-generation failures/cost. Recognized local socket memory/FD/address exhaustion is distinguished from target refusal. OS CPU/FD gauges remain explicitly unavailable, so the engine makes no claim from their sentinel zeros. If any measured probe is degraded at the same step, the run **aborts with a `generator_limited` annotation and reports no breakpoint**. It does not guess, and it does not publish a number with a caveat attached — a caveated number gets quoted without the caveat.

The headroom check (§13.2) converts chain iterations to request demand using weighted chain depth, declared polling limits and one failure retry, then caps `max_rate` at 90% of the calibrated request ceiling by default, so most such runs are prevented rather than detected.

A full step with unsupported configured p99 thresholds stops as `insufficient_samples` and publishes no capacity scalars. Error rate counts transport and assertion failures, preserving expected-failure calls.

### 11.4 Refinement

With `refine: true`, once the cliff is bracketed between the last good rate and the first bad one, the engine runs one bisection pass at the midpoint for a full `step_duration`. Before refinement, the preceding drain/settle honors at least `step_recovery`, even when final settle was set to zero. The complete sweep timeline is checked for representability including gaps and refinement. The refinement probe runs its full measurement duration even after a target threshold is crossed; generator failure still aborts it. One pass, not a full binary search: with 30s steps the added precision stops paying for its wall clock quickly, and run-to-run variance (§12.2) is soon wider than the remaining bracket. The report states the bracket, not a false-precision single number.

### 11.5 The breakpoint report

- **Max sustained rate** — highest step where every SLO held for the full step
- **Knee** — lowest rate where p99 exceeded the configured multiple of the first measured step’s supported p99 (idle baseline contains no requests)
- **Cliff** — lowest rate where errors or shortfall crossed the threshold
- **Limiting resource** — which host metric was nearest saturation at the last good step, from §9.7. Standalone output uses null with an explicit attribution-unavailable reason; the API owns host observations and can enrich it.
- **Failure mode** — what the errors actually were at the cliff: timeouts, refused connections, 5xx, listen-queue drops. "It broke" and "it began refusing connections at the listen queue" are different findings.
- **Recovery** — time to return to baseline after the overload, from the settle phase; null in standalone output because an idle HTTP window cannot measure host recovery.

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

The engine's `stats/` implementation requires 10 samples beyond a percentile for
crude support and 100 for stable support: p50 needs 20/200 samples, p95 200/2,000,
p99 1,000/10,000, and p99.9 10,000/100,000. The 2,250 floor is a planned-volume
warning, not a blanket gate. Actual unsampled measured histograms determine support;
warmup and cancelled attempts do not count. Any histogram overflow suppresses its
percentiles rather than claiming support from a truncated population.

The frozen histogram records remain mergeable. A final `load_percentiles` annotation
reports measured chain duration, request total and TTFB, each percentile carrying
its count, overflow count, support level, nullable value and two-sided 95% interval.
Intervals use binomial order-statistic ranks (equal tails, with discrete coverage at
least 95%), expanded outward to HDR bucket bounds. They assume independent samples
from one stationary population; they do not measure run-to-run variation. Partial
runs are labelled. `planned_sample_count_low` warns before traffic when measured
duration × rate is below 2,250; it does not include warmup or idle time.

**Consequence for regression detection:** at the floor, compare **p95** when the question is "did this get worse?" and reserve p99 for "is the tail catastrophic?". The comparison view leads with the metric the sample count can defend rather than always leading with p99.

### 12.2 Everything else short windows break

1. **Warmup contaminates everything.** JIT, connection pools, caches, autoscalers. The `warmup` window is measured and charted but excluded from the summary, so you can *see* the warmup effect instead of having it silently averaged into your p99.
2. **Coordinated omission.** In a closed model a slow response delays the next request, hiding latency. The open scheduler reports raw and schedule-corrected latency side by side. Correction measures from the planned arrival: chain duration adds admission delay; request total and TTFB add send-schedule drift. This is the [scheduled-time approach](https://github.com/giltene/wrk2), with one corrected sample per real terminal sample. Skipped arrivals remain explicit shortfalls, with no fabricated samples or interval-based HDR expansion. Pre-send failures have chain samples only; cancellation adds none. Corrected samples retain admission phase, overflow handling and percentile support rules. Each interval's `schedule_corrected_latency` annotation carries mergeable corrected histograms beside the raw summary; final `load_percentiles` adds `schedule_corrected` results. This answers how latency changes when generator delay is included; it cannot recover outcomes of requests never sent.
3. **Run-to-run variance.** A single 30s run is a sample, not a measurement — and at 2250 samples the p99 noise floor is wide enough that this matters more, not less. Recordings supports **run groups**: the same plan N times, with the spread shown and the observed noise floor stated, so a "regression" smaller than the spread is labelled as one. This is the cheapest available correction to a short-window methodology.
4. **Time alignment.** Every series is stamped against the run's monotonic start so load and target-side charts overlay exactly. Generator-to-target clock skew is measured at run start and recorded.

---

## 13. Run annotations, validity, and our own limits

Two related problems: a run can be invalidated by the generator rather than the target, and whoever reads the chart later will not remember which. Both are solved by attaching structured, automatic annotations to the run itself.

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

B2.4 evaluates traffic phases on cumulative 250ms snapshots and phase-end flushes.
Cap occupancy integrates observed in-flight transitions, clipped to each phase;
warmup requests still occupying slots count toward the measured cap. Any occupancy
warns, escalating above 25% of the full measured duration (elapsed duration on
interruption). Rate compares observed phase-admitted sends with discrete offered
arrivals after one second, allowing one pending arrival on live snapshots; drain
and idle time never dilute the denominator. Drops carry separate late, concurrency
and connection counts. Drift uses actual observed sends, including unfinished
requests, and records maximum drift and sample count. Drain completions retain
admission phase. `engine.rate_tolerance_pct` defaults to 2 (0–99 allowed);
`engine.send_drift_threshold_ms` defaults to the larger of 5ms and one arrival
period, capped at one hour (explicit values 1–3,600,000 allowed). Both live thresholds
are strict. Detectors emit on first detection, severity changes and phase-end
updates, with phase ranges and counts. Final `sample_count_low` lists the raw
percentiles suppressed by `stats/`, including histogram overflow, and labels partial
runs. These warnings do not change exit codes; SLO verdicts remain B4.6.

**On `concurrency_cap_reached` specifically:** hitting the cap is not inherently a failure — it is often exactly the closed-model test you intended. What matters is that the headline numbers stop describing the target and start describing the cap, and nothing in a chart shows that. Hence the explicit note, the shaded chart region, and the escalation to `invalid` when the cap dominates the window.

### 13.2 Tracking our own limits

The generator is measuring instrument and load source at once, so its capability has to be a known quantity rather than an assumption.

**Calibration.** `metrix-engine --plan bundle/ --calibrate` is a mode of the same binary, so a load box calibrates itself without a target or control plane. It measures an in-process null loop and a persistent-connection loopback echo; TLS-shaped plans run the echo through a locally generated TLS server and client. The profile records both rates, using 90% of observed loopback throughput as the ceiling. It samples one worker and the configured worker count, stores `machine-profile.json` beside the three plan documents, and binds it to architecture, logical and physical core counts, request-body bytes, TLS, chain depth, and generation mode. A changed machine or shape must be calibrated again rather than silently reusing a stale ceiling.

**Worker threads.** `engine.worker_threads` (default: physical cores − 1) sets the Tokio runtime's thread count, with `connections_per_host` and optional core pinning alongside. Raising it is the first lever for generator headroom, and calibration is per thread count — so the machine profile records a ceiling curve across thread counts rather than a single number, and the headroom check below knows what raising it would buy.

Worth stating plainly: **the services in scope are expected to cap out well below the generator's ceiling**, which is the comfortable case — it means the measurement is of the target throughout. The threading knob exists for the exception, and the calibration curve is what tells you which case you are in *before* the run rather than after. If a target genuinely outruns a tuned single box, the honest output is the `generator_limited` annotation, not a bigger number.

**Headroom check, before the run starts.** Demanded RPS (accounting for chain multiplication — §5) is compared against the calibrated loopback ceiling. No profile means no check; a matching bundle-local profile makes the ratio available in `generator.headroom_ratio` and the run-start record:

| Headroom | Behavior |
|---|---|
| < 50% of ceiling | Proceed |
| 50–70% | Proceed, `info` annotation recording the ratio |
| 70–90% | Warn before start; run carries a `warn` annotation |
| > 90% | Refuse by default; `engine.allow_generator_limited: true` permits it and emits an `invalid` annotation |

In breakpoint mode this is also what caps `max_rate` by default (§11.3) — the search stops at the point where the tool would begin measuring itself.

**During the run,** the §9.6 self-metrics feed the detectors above. The Performance screen's generator-health strip shows current headroom as a live gauge beside the target's numbers, so the two are read together rather than the client's limits being discovered afterwards in a log.

B1.5 measures send drift from the scheduled arrival to the transport send call,
using preallocated atomic slot state; completion latency never delays that sample.
The send boundary is the Hyper API call, not a packet timestamp: HTTP/2 stream
capacity waits internal to Hyper are not exposed by this metric.
`in_flight` counts admitted attempts awaiting a terminal result. `queue_depth` is
the subset still waiting to send, including connection establishment and transport
readiness; skipped arrivals are counted separately and never enter this queue.
Both gauges are sampled at each snapshot and return to zero on drain/cancellation.
`scheduler_lag_ms` is the maximum lateness of observed 250ms summary timer wakes
in the interval, including timer resolution and executor delay, rather than an
estimate from target latency. Missed ticks coalesce into one observed sample.
Final partial windows carry the interval's samples; zero samples means unavailable,
as stated by the companion annotation. OS resource probes do not run on the request path.

**In run metadata,** the calibrated ceiling, the machine profile id, and the observed peak headroom are recorded (§9.8). Without this, a comparison across a generator hardware change silently attributes a generator improvement to the target.

**Known limitation, stated rather than hidden:** a single-box generator has a ceiling, and for a sufficiently fast target the honest answer is "this tool cannot generate enough load to find your limit." The design's job is to say that clearly instead of reporting the ceiling it hit as though it were the target's. Distributed generation (§19) is the eventual answer; the annotation is the answer today.

---

## 16. SLOs and CI

Plans carry SLO assertions: request `p50_latency_ms`, `p95_latency_ms`, `p99_latency_ms`, `p99_9_latency_ms`, `error_rate` (terminal transport/assertion failures), and `achieved_rate` (admitted chain iterations per measured second). A chain scope uses that chain’s requests. Min/max bounds are inclusive. Each target and breakpoint step is evaluated independently; final verdicts retain the worst supported observation and any breach. Each verdict names the bound it is about, because a floor and a ceiling can carry the same number. Unsupported percentiles are advisory (`supported: false`, numeric observed sentinel zero), with their sample support in companion annotations. Exit codes are 0 success/advisory, 1 setup/internal/output failure, 2 SLO breach, 3 generator-limited or insufficient breakpoint samples, and 130 interruption. Fixed runs finish their requested timeline and then say whether their numbers can be believed: sustained admission or concurrency-cap failures, exhausted local connections, generation errors and a calibrated-ceiling override invalidate them, carried by an `Invalid` annotation naming the evidence rather than by a `stopped_because`, since nothing stopped. Admission and cap failures are shares, and a share needs a denominator: below a hundred offered arrivals it is not evidence, the same floor the error rate a search stops on already has. Send drift and a chain that broke do not invalidate a fixed run either: drift is a warning the run already carries, and a broken chain is a finding about the service rather than about the generator that reported it. A capacity search holds both against itself, because the number it reports is an extrapolation from the window it measured. An output failure outranks any verdict, because the verdict is what could not be written down. Breakpoint capacity probes stop promptly and enforce drift thresholds as well. A breakpoint probe may stop at target thresholds without execution failure. The API exposes the machine-readable verdict. `observe` is an API-owned request and standalone execution merely annotates that observation is unavailable. This closes the loop for the machine-authored use case: an agent can generate a plan, run it, and read a pass/fail with the specific violated thresholds — without parsing charts.
