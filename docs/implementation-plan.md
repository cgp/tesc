# Metrix — Implementation Plan

Companion to [design-outline.md](design-outline.md). That document says what to build and why; this one says in what order, with what structure, and how to tell when each part is done.

Section references written as §N point into design-outline.md unless they name a section of this document.

---

## 1. Repository structure

A single repo with two build systems side by side. The Rust engine and the Python API are separate deployables that share exactly one contract — the plan format (§4 of the design: calls, mix, targets) — and nothing else. The engine must remain shippable on its own (§2.2), so the dependency only ever points one way: the API knows about the engine, never the reverse.

```
metrix/
├── README.md
├── docs/
│   ├── initial-design.md
│   ├── design-outline.md
│   └── implementation-plan.md
│
├── schema/                          # THE shared contract (§1.1)
│   ├── call.schema.json             # the three plan documents (§4 of the design),
│   ├── mix.schema.json              #   all generated from Rust types
│   ├── targets.schema.json
│   ├── profile.schema.json
│   └── events.schema.json           # NDJSON record shapes engine → API
│
├── engine/                          # Rust workspace — ships alone (§2.2)
│   ├── Cargo.toml                   # workspace root
│   ├── Cargo.lock
│   ├── crates/
│   │   ├── metrix-engine/           # the binary: runtime, scheduler, HTTP, target loop
│   │   ├── metrix-plan/             # call / mix / targets types, validation, schema gen
│   │   ├── metrix-metrics/          # HDR histograms, counters, snapshots, NDJSON out
│   │   ├── metrix-gen/              # generators: template, dataset, lua, plugin, exec
│   │   └── metrix-mock/             # test target with controllable latency/errors (§5.1)
│   └── dist/                        # release artifacts: static binary + plan bundle
│
├── api/                             # Python, managed with uv
│   ├── pyproject.toml
│   ├── uv.lock                      # committed
│   ├── src/metrix_api/
│   │   ├── main.py                  # FastAPI app factory (the only entry point)
│   │   ├── config.py                # METRIX_HOME resolution, settings
│   │   ├── routes/                  # plans, runs, profiles, recordings, stream
│   │   ├── runner/                  # run-spec assembly, engine supervision, NDJSON ingest
│   │   ├── discovery/               # ecs.py, inventory.py  (boto3 lives ONLY here)
│   │   ├── observer/                # collector.py, ssh.py, scrape.py
│   │   ├── stats/                   # histogram merge, percentiles, CI, noise floor
│   │   └── store/                   # SQLite schema, migrations, queries
│   ├── web/                         # the static front end — no templating
│   │   ├── index.html
│   │   ├── css/                     # Tabler + a little of our own
│   │   └── js/                      # api, stream, state, table, charts,
│   │                                #   config, recordings  (ES modules)
│   └── tests/
│
├── examples/
│   └── plans/
│       └── checkout-mixed/          # a complete worked bundle
│           ├── mix.json
│           ├── targets.json
│           ├── calls/products.json
│           ├── gen/order.lua
│           └── data/users.csv
│
├── policy/
│   └── metrix-readonly.json         # the IAM policy from §3.2 of the design
│
└── scripts/
    ├── dev.sh                       # start api + mock target for local work
    └── check-schema.sh              # CI: regenerate schema, fail on drift
```

### 1.1 The one shared contract

The **plan format** — the three documents of §4 of the design — is the only thing both languages must agree on, so it gets a single source of truth: **Rust types are authoritative**, and the schemas are generated from them via `schemars` and committed.

- Engine: deserializes with `serde`, so the types *are* the validation.
- API: validates each document against its schema **and resolves `call` references across them**, serves the schemas at `GET /api/schema/{call,mix,targets}` for LLM authors, and writes `targets.json` from a resolved profile.
- CI regenerates and fails on any diff, so the file cannot drift from the types.

This matters more than usual here: a plan that the API accepts and the engine then rejects would surface as a failed run rather than a validation error, which is exactly the loop the machine-authoring workflow depends on not having.

### 1.2 Why the engine is a workspace, not one crate

`metrix-plan` and `metrix-metrics` are separable and independently testable — histogram merging and percentile math deserve their own test suite without a runtime attached, and the schema generator needs the plan types without pulling in `hyper`. `metrix-mock` being a crate rather than a test fixture means it can run as a standalone binary during development.

---

## 2. Runtime layout

Where the app keeps things when it runs. Root is `$METRIX_HOME`, defaulting to `~/.metrix` (override in config or environment).

```
$METRIX_HOME/
├── config.toml                  # bind address, paths, AWS profile/region, defaults
│
├── profiles/                    # target profiles (§3 of the design)
│   ├── staging.yaml
│   └── prod-readonly.yaml
│
├── plans/                       # one bundle directory per plan (§4.4 of the design)
│   └── checkout-mixed/
│       ├── mix.json             # the mixture and load shape
│       ├── targets.json         # the boxes — swappable
│       ├── calls/products.json  # individual request definitions
│       ├── gen/order.lua        # ← bundle directory is the Lua sandbox root
│       └── data/users.csv
│
├── secrets/                     # optional local secret store for {{ secret.* }}
│
├── metrix.db                    # SQLite — everything queryable
│
└── runs/
    └── 2026-09-12T14-03-11Z_a3f9/
        ├── plan.snapshot/              # the whole bundle, exactly as it ran
        ├── inventory.snapshot.json     # exactly what it ran against
        ├── summary.ndjson              # engine summary stream, 250ms
        ├── observer.ndjson             # host samples, 1s
        ├── events.ndjson.zst           # per-request records      ⟵ purgeable
        └── errors/                     # retained error samples   ⟵ purgeable
            └── 500_orders-create_01.json
```

### 2.1 Why SQLite and files rather than one or the other

**SQLite holds what gets queried**: run metadata, series membership, rolled-up metric series, serialized HDR histograms, annotations, inventories, SLO verdicts. A full series loads into memory comfortably (§19 of the design), so no rollup tiering is needed and trends read real numbers at any age.

**Files hold the bulk streams**: per-request events and retained error-sample bodies. These are the only large things, and keeping them as files on disk makes the purge button (§17.1) a directory delete rather than a transaction that has to vacuum a database.

A plan directory is self-contained — plan, generators, datasets together — which is what makes the Lua sandbox root meaningful, what makes a plan copyable between environments, and what makes it shippable to a load box as a single tarball alongside the binary.

### 2.2 How it runs

**The control plane** — one command, and everything else happens in the browser: starting load tests, starting observation-only recordings, browsing recordings.

```bash
uv sync                    # once
uv run metrix-api          # serves the static page + API on :8080
```

**The engine** — one command, one self-contained bundle, no Python involved:

```bash
metrix-engine --plan checkout-mixed/ --summary - --events events.ndjson # standard run sequence
metrix-engine --plan checkout-mixed/ --targets prod-canary.json   # run again, same mix, against other boxes
metrix-engine --plan checkout-mixed/ --chain checkout              # run one chain, print it
metrix-engine --calibrate --out machine-profile.json
```

The bundle carries its own targets document, so a sweep across eight containers is that same single invocation — no separate sweep command, no orchestrator above the engine. Bundles are normally exported from the API (`GET /api/plans/{name}/bundle`) rather than written by hand, but nothing about them requires the API to have existed.

**Remote execution** is the same command somewhere else:

```bash
scp engine/dist/metrix-engine loadbox:~/ && scp -r plans/checkout-mixed loadbox:~/
ssh loadbox 'metrix-engine --plan checkout-mixed/ --summary -' > summary.ndjson
```

Nothing about that path is special-cased. It is the plain shape of the tool, which is why it will still work when it is wired up properly later (B4.7).

---

## 3. Milestones

**Two independent tracks**, either buildable to completion without the other. The engine runs hand-written bundles from a shell; the API does observation-only recordings with no engine installed. They meet only at F0's contracts and at A4. **Start with track A** — observing hosts and containers has no engine dependency and is useful on its own (§10.2 of the design).

### F0 — Shared foundation (both tracks depend on this)

| Step | Deliverable |
|---|---|
| F0.1 | Repo skeleton, Rust workspace, `uv init`, committed lockfiles |
| F0.2 | `metrix-plan` types: call, mix, targets — separate from the start |
| F0.3 | Generated schemas + `scripts/check-schema.sh`; NDJSON event shapes frozen in `schema/events.schema.json` |
| F0.4 | CI: `cargo test`, `cargo clippy -D warnings`, `uv run pytest`, schema drift check |

**Done when:** both tracks can build and test independently, and the NDJSON contract is written down before either side implements it.

---

### Track A — API, observation, and analysis

Nothing here requires the engine. Through A3 the product is a host-observation tool; A4 is the only integration point.

#### A1 — Observation (start here)

| Step | Deliverable |
|---|---|
| A1.1 | `METRIX_HOME` layout, config, SQLite schema and migrations |
| A1.2 | Target profiles with explicit endpoint lists |
| A1.3 | Observer: SSH collection, 1s samples, normalized metric shape (§2.3) |
| A1.4 | HTTP scrape collector; graceful degradation, `collection_gap` annotation |
| A1.5 | Observation-only recordings: start/stop, phases, persistence (§10.2) |
| A1.6 | Static front end: `index.html`, Tabler, left nav, ES module skeleton |
| A1.7 | SSE stream at 1s with `Last-Event-ID` replay; live host stats on the Stats page |

**Done when:** an observation-only recording of a live host streams to the browser at 1s, survives a reconnect with no gap, persists, and reopens later — with no engine built.

#### A2 — Discovery

| Step | Deliverable |
|---|---|
| A2.1 | ECS discovery: hostname → ALB → target group → service → tasks → containers → instances (§3.1) |
| A2.2 | Resolved inventory: storage, run pinning, refresh at phase boundaries, `host_count_changed` |
| A2.3 | Reachability verification at profile setup; `targets.json` written from a profile |
| A2.4 | Environment baselines; baseline/settle deltas, recovery curves, leak detection (§9.7) |

**Done when:** a hostname resolves to a task list with image digests, and observation attaches to those identities.

#### A3 — Analysis of recordings

| Step | Deliverable |
|---|---|
| A3.1 | Stats table (§14): rows, columns, live update, TSV/CSV export |
| A3.2 | Charts page (§15): phase bands, host overlays, gaps drawn as gaps |
| A3.3 | Recordings archive, filters, baseline marking with `invalid` gating |
| A3.4 | Run series by setup identity; trends with measured noise bands (§17.3) |
| A3.5 | Regression flagging: outside band **and** sample-supported **and** not invalid (§17.4) |
| A3.6 | Comparison mode (§14.4), multi-run overlay, histogram merging (§17.5) |
| A3.7 | Purge button; exports (JSON/CSV/static HTML) |

**Done when:** ten recordings of one environment produce a trend with a believable noise band.

#### A4 — Integration with the engine

| Step | Deliverable |
|---|---|
| A4.1 | Bundle assembly and export (`GET /api/plans/{name}/bundle`) |
| A4.2 | Engine supervision, NDJSON ingest, load + host series on one timeline |
| A4.3 | Plan editor: calls, chains, percentages with implied RPS, validation display |
| A4.4 | `POST /api/plans/generate` — calls from OpenAPI/WSDL, starter mix (§8) |
| A4.5 | Sweep comparison view (§17.6) |

**Done when:** a run launched from the browser shows load and host metrics against one clock.

---

### Track B — Engine

Buildable and testable with nothing but a shell, a bundle, and the mock target.

#### B1 — Core load path

| Step | Deliverable |
|---|---|
| B1.1 | `metrix-mock`: configurable latency distribution, error injection, slow start, capacity ceiling |
| B1.2 | Open-model fixed-rate scheduler, HTTP/1.1 + HTTP/2 via `hyper`/`rustls` |
| B1.3 | `metrix-metrics`: per-worker HDR histograms and counters, merged on a 250ms tick |
| B1.4 | NDJSON `--summary` and `--events` output per the F0.3 contract |
| B1.5 | Self-metrics: send-schedule drift, in-flight, queue depth (§13.2) |

**Done when:** `metrix-engine --plan dir/` holds 75 RPS for 30s against the mock from a bare shell, with drift reported and no API in existence.

*Why the mock first:* every statistical claim needs a target whose true behavior is known. Built later, the measurement tool gets validated against a service whose real latency nobody knows.

#### B2 — Measurement you can trust

| Step | Deliverable |
|---|---|
| B2.1 | Phase timeline: baseline → warmup → measure → drain → settle (§10.1) |
| B2.2 | Percentile support rules: the 2250 floor, CIs on p99, p99.9 suppression (§12.1) |
| B2.3 | Coordinated-omission correction reported beside raw (§12.2) |
| B2.4 | Annotation detectors: `concurrency_cap_reached`, `rate_not_achieved`, `send_schedule_drift`, `sample_count_low` (§13.1) |
| B2.5 | `--calibrate` + machine profile; headroom check and refusal above 90% (§13.2) |
| B2.6 | Run metadata: plan hash, engine version, seed, machine profile (§9.8) |

**Done when:** against a known injected distribution, reported percentiles match within their stated intervals; a capped run annotates correctly; a run past the calibrated ceiling is refused.

#### B3 — Calls, chains, and the mixture

| Step | Deliverable |
|---|---|
| B3.1 | Calls as a separate document; `call` references resolved from mix steps |
| B3.2 | Chaining: sequential steps, variable scope, JSONPath + XPath extraction (§5) |
| B3.3 | Chains with percentages of a total rate; sum-to-100 validation (§4.5) |
| B3.4 | Assertions, `on_failure`, `repeat_until`, chain-abort accounting, expected-failure chains |
| B3.5 | Datasets and inline templating |
| B3.6 | Generation tiers: **Lua via `mlua` first** (§7.2), then Rust plugin, then exec sidecar |
| B3.7 | Lua corpus loading: read-only, bundle-rooted, in-memory, size-ceilinged |
| B3.8 | `auth` block (§6): all modes, single-flight refresh, auth traffic excluded |
| B3.9 | Session policy per chain: `fresh` / `reuse` / `pool` (§4.2) |
| B3.10 | Error-sample capture: first N per error class, redaction (§9.3) |
| B3.11 | Validation errors with JSON Pointer paths; single-chain execution (`--chain`) |

**Done when:** the `examples/plans/checkout-mixed` bundle runs end to end — six chains at declared percentages, XML and JSON, extraction between steps, a Lua generator, OAuth with refresh, and a deliberately-failing chain whose 401s count as passes.

*Sequencing note:* calls before chains before the mixture, so each layer is testable alone. Lua before the exec sidecar — it is the default tier, and building the escape hatch first tends to make the escape hatch the default.

#### B4 — Many targets, and limits

| Step | Deliverable |
|---|---|
| B4.1 | Sequential multi-target execution: target list, ordering, inter-target gap (§3.5) |
| B4.2 | Direct container addressing: Host override, SNI, `insecure_skip_verify` annotation (§3.4) |
| B4.3 | Breakpoint mode: stepped ramp, per-step statistics, `step_recovery` (§11) |
| B4.4 | Stop conditions incl. generator-vs-target discrimination and `generator_limited` (§11.3) |
| B4.5 | Refinement pass; report with knee / cliff / max-sustained / limiting resource |
| B4.6 | SLO evaluation and exit codes for CI (§16) |
| B4.7 | Static build, `engine/dist` bundle, ship-and-run over SSH (§2.2) |

**Done when:** a hand-written bundle listing three mock targets runs all three in sequence; a breakpoint run finds a known ceiling within one step width; the same run against an under-provisioned generator aborts as `generator_limited` rather than reporting a number.

---

## 4. Cross-cutting decisions to honor throughout

These are easy to erode step by step, so they are worth restating as build-time rules:

1. **The engine never depends on the API.** No Python on the load path, no config outside the bundle, no network call to the control plane. Every engine feature must be exercisable as `metrix-engine --plan dir/` on a machine with nothing else installed.
2. **boto3 appears only under `api/src/metrix_api/discovery/`.** No AWS SDK in the engine, ever.
3. **The engine never blocks on a consumer.** Any stream backpressure is dropped and annotated, never allowed to perturb the send loop.
4. **No allocation or locking on the hot path.** Per-worker aggregation, merged on the snapshot tick.
5. **Every displayed percentile carries its sample count.** The rule lives in one place in `stats/` and the UI cannot bypass it.
6. **Secrets never reach SQLite, run directories, exports, or error samples.** Redaction is applied at capture, not at display.
7. **Desktop only.** No responsive breakpoints.

---

## 5. Testing strategy

### 5.1 The mock target is the measurement ground truth

`metrix-mock` (B1.1) serves configurable latency distributions (fixed, normal, lognormal, bimodal), injectable error rates and types, connection-refusal at a configurable concurrency, slow-start/warmup behavior, and a capacity ceiling for breakpoint testing. Because its true distribution is known, the engine's reported statistics can be asserted against it — which is the only way to test a measurement tool.

### 5.2 Layers

- **Rust unit:** histogram merge, percentile math, CI calculation, scheduler drift under synthetic load, plan deserialization.
- **Rust integration:** engine against mock, asserting achieved rate, reported percentiles vs. injected truth, phase boundaries, annotation firing.
- **Python unit:** NDJSON ingest, series identity, noise-floor math, regression logic, discovery response parsing against recorded boto3 fixtures.
- **Python integration:** full run lifecycle against the mock, SSE reconnect with gap verification, purge behavior.
- **Contract:** schema drift check; a corpus of valid and invalid plans asserting that engine and API agree on both.

### 5.3 Recorded AWS fixtures

Discovery is tested against committed JSON fixtures of real `describe_*` responses — including partial-resolution cases (NLB with no ECS, task with no ENI, deregistered target). Live AWS is never required to run the suite.

---

## 6. Sequencing risks

| Risk | Mitigation |
|---|---|
| **Tracks integrating late and badly** | The NDJSON shapes are frozen in F0.3 before either side implements them, and the API ingests a recorded fixture stream from day one — so A4 is wiring, not discovery |
| Measuring the generator instead of the target | Self-metrics land in B1.5, before any feature that would tempt a conclusion |
| Percentile math wrong but plausible | Mock at B1.1, before B2, so statistics are asserted against known truth |
| Schema drift between engine and API | Generated schemas + CI check from F0.3 |
| Exec generators becoming the default | Lua built first (B3.6) |
| SSE backpressure perturbing a run | Engine never blocks (rule 3); verified in B1 acceptance |
| Discovery complexity leaking into the engine | Rules 1–2, with a CI job that runs B1 acceptance with the API absent |
| Front end drifting into a framework | One static page, no build step; if a bundler becomes necessary, that is a decision to revisit deliberately |

---

## 7. First week

F0.1–F0.3 first — skeleton, plan types, and the frozen schemas — since both tracks build on them and they are an afternoon's work.

Then **A1.1–A1.3**: `METRIX_HOME`, a profile with two explicit hosts, and the SSH collector sampling them at 1s. The milestone is a JSON series of real CPU and memory off a real box, persisted and reopenable.

That is a genuinely useful thing on its own, it requires no Rust, and it makes the domain concrete before any of the statistics work begins. Track B's own first milestone — the mock target, then a fixed-rate scheduler holding 75 RPS for 30s — can start whenever, by whoever, without waiting.
