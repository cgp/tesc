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
metrix-engine --plan checkout-mixed/ --summary - --events events.ndjson
metrix-engine --plan checkout-mixed/ --targets prod-canary.json   # same mix, other boxes
metrix-engine --plan checkout-mixed/ --call create-order          # fire one request, print it
metrix-engine --calibrate --out machine-profile.json
```

The bundle carries its own targets document, so a sweep across eight containers is that same single invocation — no separate sweep command, no orchestrator above the engine. Bundles are normally exported from the API (`GET /api/plans/{name}/bundle`) rather than written by hand, but nothing about them requires the API to have existed.

**Remote execution** is the same command somewhere else:

```bash
scp engine/dist/metrix-engine loadbox:~/ && scp -r plans/checkout-mixed loadbox:~/
ssh loadbox 'metrix-engine --plan checkout-mixed/ --summary -' > summary.ndjson
```

Nothing about that path is special-cased. It is the plain shape of the tool, which is why it will still work when it is wired up properly later (M6.7).

---

## 3. Milestones

Ordered so that the riskiest assumptions get tested earliest and each milestone leaves the tool usable for something.

### M0 — Foundations

| Step | Deliverable |
|---|---|
| 0.1 | Repo skeleton, Rust workspace, `uv init` for the API, committed lockfiles |
| 0.2 | `metrix-plan` types: call, mix, targets — minimal but separate from the start |
| 0.3 | `schemars` schema generation + `scripts/check-schema.sh` |
| 0.4 | `metrix-mock`: HTTP target with configurable latency distribution, error injection, and slow-start behavior |
| 0.5 | CI: `cargo test`, `cargo clippy -D warnings`, `uv run pytest`, schema drift check |

**Done when:** `cargo run -p metrix-mock` serves a target whose p50/p99 you can dial, and the schema check passes in CI.

*Why the mock comes first:* every statistical claim in the design needs a target with known behavior to verify against. Building it later means validating the measurement tool against a service whose real latency you do not know.

### M1 — Walking skeleton

A thin vertical slice through every layer, before any layer is complete.

| Step | Deliverable |
|---|---|
| 1.1 | Engine: open-model fixed-rate scheduler, HTTP/1.1 + HTTP/2 via `hyper`/`rustls`, one call, **bundle in / NDJSON out with no API present** |
| 1.2 | `metrix-metrics`: per-worker HDR histograms + counters, merged on a 250ms tick |
| 1.3 | NDJSON summary output; `--events` per-request stream |
| 1.4 | Engine self-metrics: send-schedule drift, in-flight, queue depth (§13.2) |
| 1.5 | API: bundle assembly + export, engine supervision, NDJSON ingest, SQLite + run directory |
| 1.6 | Static front end: `index.html`, Tabler, left nav, ES module skeleton (`api` / `stream` / `state` / `table`) |
| 1.7 | SSE stream at 1s with `Last-Event-ID` replay; **Stats table page live** (§14) |

**Done when:** `metrix-engine --plan ...` runs 30s at 75 RPS against the mock **from a bare shell with the API stopped**, and separately, the same run launched from the browser updates the stats table once per second without lag or column jitter, with reconnect leaving no gap.

*Why this shape:* the pipeline is where integration risk lives — subprocess supervision, backpressure, stream reconnect. Proving it end-to-end at week two is worth far more than a feature-complete engine with nothing to display it.

### M2 — Measurement you can trust

Nothing after this point is meaningful if this milestone is wrong.

| Step | Deliverable |
|---|---|
| 2.1 | Phase timeline: baseline → warmup → measure → drain → settle (§10.1), phase events on the stream |
| 2.2 | Percentile support rules: the 2250 floor, CIs on p99, p99.9 suppression (§12.1) |
| 2.3 | Coordinated-omission correction reported beside raw (§12.2) |
| 2.4 | Run annotations + detector framework (§13.1), starting with `concurrency_cap_reached`, `rate_not_achieved`, `send_schedule_drift`, `sample_count_low` |
| 2.5 | `metrix-engine --calibrate` + machine profile; headroom check and refusal above 90% (§13.2) |
| 2.6 | Run metadata: plan hash, engine version, API version + lock hash, seed, machine profile (§9.8) |

**Done when:** against a mock with a known injected distribution, reported percentiles match the true values within their stated confidence intervals; a deliberately capped run raises `concurrency_cap_reached` with the right window percentage; and a run driven past the calibrated ceiling is refused rather than reported.

### M3 — Expressive plans

| Step | Deliverable |
|---|---|
| 3.1 | Calls as a separate document, `call` references from mix steps, cross-document validation; scenarios, weights, per-scenario rates, implied-RPS display (§4) |
| 3.2 | Chaining: sequential steps, variable scope, JSONPath + XPath extraction (§5) |
| 3.3 | Assertions, `on_failure` policy, `repeat_until`, chain-abort accounting |
| 3.4 | Datasets: CSV/JSONL, round_robin / random / unique_per_iteration |
| 3.5 | Inline templating (`{{ }}`, `rand`, `uuid`, `now`, `seq`, `pick`) |
| 3.6 | Generation tiers: **Lua via `mlua` first** (§7.2), then Rust plugin trait, then exec sidecar |
| 3.7 | Lua corpus loading: read-only, plan-directory-rooted, in-memory, size-ceilinged |
| 3.8 | `auth` block (§6): all modes, single-flight refresh, auth traffic excluded from load metrics |
| 3.9 | Error-sample capture: first N per error class, redaction (§9.3) |
| 3.10 | `POST /api/plans/validate` with JSON Pointer error paths, incl. unresolved `call` names; single-call execution (`--call`) |

**Done when:** the `examples/plans/checkout-mixed` plan runs end to end — XML and JSON, a chain with extraction, a Lua generator producing path and body, OAuth with refresh — and a deliberately broken plan returns errors an LLM can repair from.

*Sequencing note:* Lua before the exec sidecar, deliberately. It is the default tier, and building the escape hatch first tends to make the escape hatch the default.

### M4 — Targets and observation

| Step | Deliverable |
|---|---|
| 4.1 | Target profiles with explicit endpoint lists (no discovery); profile → run-spec targets block |
| 4.2 | Observer: SSH collection, 1s samples, normalized metric shape (§2.3) |
| 4.3 | HTTP scrape collector; graceful degradation and `collection_gap` annotation |
| 4.4 | Baseline/settle host statistics: delta-from-baseline, recovery curves, leak detection (§9.7) |
| 4.5 | Observation-only mode + environment baselines (§10.2) |
| 4.6 | ECS discovery: hostname → ALB → target group → service → tasks → containers → instances (§3.1) |
| 4.7 | Resolved inventory: storage, pinning to runs, refresh at phase boundaries |
| 4.8 | Direct container addressing: Host override, SNI, reachability verification at setup (§3.4) |
| 4.9 | **Engine: sequential multi-target execution** — target list, ordering, inter-target gap, per-target phased runs (§3.5) |

**Done when:** a hostname resolves to a task list with image digests; observation-only recording starts and stops from the front end with no plan attached; a run against one container carries correct Host and SNI; a hand-written spec listing three mock targets runs all three in sequence from a bare shell; and an unreachable VPC fails at profile setup rather than at run time.

### M5 — Analysis

| Step | Deliverable |
|---|---|
| 5.1 | Charts page (§15): RPS, percentile bands, histogram/CDF, errors, phase-breakdown, host overlay |
| 5.2 | Phase bands, annotation shading, collection gaps drawn as gaps |
| 5.3 | Recordings: archive, filters, baseline marking with `invalid` gating |
| 5.4 | Run series by setup identity; trend charts with measured noise bands (§17.3) |
| 5.5 | Regression flagging: outside band **and** sample-supported **and** not invalid (§17.4) |
| 5.6 | Comparison mode for the stats table (§14.4) and multi-run overlay |
| 5.7 | Run-group histogram merging (§17.5) |
| 5.8 | Purge button for events and error bodies; exports (JSON/CSV/static HTML) |

**Done when:** ten runs of one plan produce a trend with a believable noise band, an injected 30% regression is flagged while a 3% wobble is not, and merging five 30s runs yields a p99 with materially tighter bounds than any single run.

### M6 — Automation

| Step | Deliverable |
|---|---|
| 6.1 | Breakpoint mode (§11): stepped ramp, per-step statistics, `step_recovery` |
| 6.2 | Stop conditions incl. generator-vs-target discrimination and `generator_limited` abort (§11.3) |
| 6.3 | Refinement pass; breakpoint report with knee / cliff / max-sustained / limiting resource |
| 6.4 | Sweep comparison view (§17.6) over the multi-target runs from 4.9 |
| 6.5 | `POST /api/plans/generate` — calls deterministically from OpenAPI/WSDL, starter mix with flat weights (§8) |
| 6.6 | SLO evaluation, engine exit codes, machine-readable verdict for CI (§16) |
| 6.7 | **Remote execution**: static build, `engine/dist` bundle, ship-and-run over SSH, stream collection back to the API |

**Done when:** a breakpoint run against a mock with a known capacity ceiling finds it within one step width; the same run against a deliberately under-provisioned generator aborts as `generator_limited` rather than reporting a number; and a generated draft plan validates and runs without hand-editing.

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

`metrix-mock` serves configurable latency distributions (fixed, normal, lognormal, bimodal), injectable error rates and types, connection-refusal at a configurable concurrency, slow-start/warmup behavior, and a capacity ceiling for breakpoint testing. Because its true distribution is known, the engine's reported statistics can be asserted against it — which is the only way to test a measurement tool.

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
| Measuring the generator instead of the target | Self-metrics land in M1.4, before any feature that would tempt a conclusion |
| Percentile math wrong but plausible | M0.4 mock before M2, so statistics are asserted against known truth |
| Schema drift between engine and API | Generated schemas + CI check from M0.3 |
| Exec generators becoming the default | Lua built first (M3.6) |
| SSE backpressure perturbing a run | Engine never blocks (rule 2); verified in M1 acceptance |
| Discovery complexity leaking into the engine | Rules 1–2, with a CI job that runs the full M1 acceptance with the API stopped |
| Front end drifting into a framework | One static page, no build step; if a bundler becomes necessary, that is a decision to revisit deliberately |

---

## 7. First week

1. M0.1–0.3 — skeleton, plan types (call/mix/targets), schema generation.
2. M0.4 — the mock target, with a dial-able latency distribution.
3. M1.1–1.3 — fixed-rate scheduler and NDJSON output.
4. Run 30s at 75 RPS against the mock and compare the reported p50/p95/p99 against the injected distribution by hand.

Step 4 is the real milestone. Everything downstream assumes those numbers are right.
