# Metrix — Engine Implementation Plan

The load generator. Buildable and testable with nothing but a shell, a bundle, and the mock target — no API, no database, no cloud credentials.

> **Status: B1.1–B1.2 complete.** The mock and standalone fixed-rate HTTP load path are implemented. B1.3, per-worker histograms and snapshot aggregation, is next.

Design: [design-engine.md](design-engine.md). Boundary and shared foundation (**F0, do this first**): [design-api-engine-contract.md](design-api-engine-contract.md). The other track: [implementation-api.md](implementation-api.md).

---

## 1. Project layout

```
├── engine/                          # Rust workspace — ships alone (§2.2)
│   ├── Cargo.toml                   # workspace root
│   ├── Cargo.lock
│   ├── crates/
│   │   ├── metrix-engine/           # the binary: runtime, scheduler, HTTP, target loop,
│   │   │                            #   and --emit-schemas (F0.3)
│   │   ├── metrix-plan/             # call / mix / targets types, validation, schema gen
│   │   ├── metrix-metrics/          # HDR histograms, counters, snapshots, NDJSON out
│   │   ├── metrix-gen/              # generators: template, dataset, lua, plugin, exec
│   │   └── metrix-mock/             # test target: latency, errors, ceilings (§5)
│   └── dist/                        # release artifacts: static binary + plan bundle
```

`metrix-plan` and `metrix-metrics` are separate crates because histogram merging and percentile math deserve a test suite without a runtime attached, and the schema generator needs the plan types without pulling in `hyper`. `metrix-mock` is a crate rather than a fixture so it runs as a standalone binary.

---

## 2. The bundle

The unit of execution and of transfer. Self-contained by design — this is what makes remote execution unremarkable.

```
checkout-mixed/
├── mix.json             # chains, percentages, load shape, phases
├── targets.json         # the boxes — swappable with --targets
├── calls/products.json  # individual request definitions
├── gen/order.lua        # ← bundle directory is the Lua sandbox root
└── data/users.csv
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

Buildable and testable with nothing but a shell, a bundle, and the mock target.

### B1 — Core load path

- [x] **B1.1** — `metrix-mock`: configurable latency distribution, error injection, slow start, capacity ceiling
- [x] **B1.2** — Open-model fixed-rate scheduler, HTTP/1.1 + HTTP/2 via `hyper`/`rustls`
- [ ] **B1.3** — `metrix-metrics`: per-worker HDR histograms and counters, merged on a 250ms tick
- [ ] **B1.4** — NDJSON `--summary` and `--events` output per the F0.3 contract
- [ ] **B1.5** — Self-metrics: send-schedule drift, in-flight, queue depth (§13.2)

**Done when:** `metrix-engine --plan dir/` holds 75 RPS for 30s against the mock from a bare shell, with drift reported and no API in existence.

*Why the mock first:* every statistical claim needs a target whose true behavior is known. Built later, the measurement tool gets validated against a service whose real latency nobody knows.

### B2 — Measurement you can trust

- [ ] **B2.1** — Phase timeline: baseline → warmup → measure → drain → settle (§10.1)
- [ ] **B2.2** — Percentile support rules: the 2250 floor, CIs on p99, p99.9 suppression (§12.1)
- [ ] **B2.3** — Coordinated-omission correction reported beside raw (§12.2)
- [ ] **B2.4** — Annotation detectors: `concurrency_cap_reached`, `rate_not_achieved`, `send_schedule_drift`, `sample_count_low` (§13.1)
- [ ] **B2.5** — `--calibrate` + machine profile; headroom check and refusal above 90% (§13.2)
- [ ] **B2.6** — Run metadata: plan hash, engine version, seed, machine profile (§9.8)

**Done when:** against a known injected distribution, reported percentiles match within their stated intervals; a capped run annotates correctly; a run past the calibrated ceiling is refused.

### B3 — Calls, chains, and the mixture

- [ ] **B3.1** — Calls as a separate document; `call` references resolved from mix steps
- [ ] **B3.2** — Chaining: sequential steps, variable scope, JSONPath + XPath extraction (§5)
- [ ] **B3.3** — Chains with percentages of a total rate; sum-to-100 validation (§4.5)
- [ ] **B3.4** — Assertions, `on_failure`, `repeat_until`, chain-abort accounting, expected-failure chains
- [ ] **B3.5** — Datasets and inline templating
- [ ] **B3.6** — Generation tiers: **Lua via `mlua` first** (§7.2), then Rust plugin, then exec sidecar
- [ ] **B3.7** — Lua corpus loading: read-only, bundle-rooted, in-memory, size-ceilinged
- [ ] **B3.8** — `auth` block (§6): all modes, single-flight refresh, auth traffic excluded
- [ ] **B3.9** — Session policy per chain: `fresh` / `reuse` / `pool` (§4.2)
- [ ] **B3.10** — Error-sample capture: first N per error class, redaction (§9.3)
- [ ] **B3.11** — Validation errors with JSON Pointer paths; single-chain execution (`--chain`)

**Done when:** the `examples/plans/checkout-mixed` bundle runs end to end — six chains at declared percentages, XML and JSON, extraction between steps, a Lua generator, OAuth with refresh, and a deliberately-failing chain whose 401s count as passes.

*Sequencing note:* calls before chains before the mixture, so each layer is testable alone. Lua before the exec sidecar — it is the default tier, and building the escape hatch first tends to make the escape hatch the default.

### B4 — Many targets, and limits

- [ ] **B4.1** — Sequential multi-target execution: target list, ordering, inter-target gap (§3.5)
- [ ] **B4.2** — Direct container addressing: Host override, SNI, `insecure_skip_verify` annotation (§3.4)
- [ ] **B4.3** — Breakpoint mode: stepped ramp, per-step statistics, `step_recovery` (§11)
- [ ] **B4.4** — Stop conditions incl. generator-vs-target discrimination and `generator_limited` (§11.3)
- [ ] **B4.5** — Refinement pass; report with knee / cliff / max-sustained / limiting resource
- [ ] **B4.6** — SLO evaluation and exit codes for CI (§16)
- [ ] **B4.7** — Static build, `engine/dist` bundle, ship-and-run over SSH (§2.2)

**Done when:** a hand-written bundle listing three mock targets runs all three in sequence; a breakpoint run finds a known ceiling within one step width; the same run against an under-provisioned generator aborts as `generator_limited` rather than reporting a number.

---

## 4. Rules to honor throughout

1. **The engine never depends on the API.** No Python on the load path, no config outside the bundle, no network call to the control plane. Every feature must be exercisable as `metrix-engine --plan dir/` on a machine with nothing else installed — enforced by a CI job that runs B1 acceptance with the API absent.
2. **No AWS SDK, ever.** The engine receives concrete addresses.
3. **The engine never blocks on a consumer.** Stream backpressure is dropped and annotated, never allowed to perturb the send loop.
4. **No allocation or locking on the hot path.** Per-worker aggregation, merged on the snapshot tick.
5. **Secrets are redacted at capture**, before anything reaches an error sample or an output stream.

---

## 5. Testing

The mock target (B1.1) is the measurement ground truth: seeded latency distributions (fixed, normal, lognormal, bimodal), injectable HTTP/disconnect/timeout errors, connection rejection and request-concurrency limits, slow start, and a capacity ceiling for breakpoint testing. Configuration and precise semantics are in [design-engine.md §2.2](design-engine.md#22-mock-target-b11), with a runnable [example](../examples/mock.json). Tests check the injected population separately from socket timing, so operating-system overhead is not mistaken for distribution truth.

- **Unit:** histogram merge, percentile and CI math, scheduler drift under synthetic load, bundle deserialization.
- **Integration:** engine against mock, asserting achieved rate, percentiles vs. injected truth, phase boundaries, annotation firing, multi-target sequencing.
- **Contract:** schema drift check; a corpus of valid and invalid bundles asserting that engine and API agree on both.

---

## 6. Risks

| Risk | Mitigation |
|---|---|
| Measuring the generator instead of the target | Self-metrics land in B1.5, before any feature that would tempt a conclusion |
| Percentile math wrong but plausible | Mock at B1.1, before B2, so statistics are asserted against known truth |
| Schema drift between engine and API | Generated schemas + CI check from F0.3 |
| Exec generators becoming the default | Lua built first (B3.6) |
| Creeping dependence on the control plane | Rule 1, with CI running B1 acceptance with the API absent |

---

## 7. Where to start

B1.3 then B1.4: build aggregation and NDJSON on the fixed-rate path. `examples/plans/mock-fixed` runs one static call against the mock; unsupported later-step features fail before sending traffic ([design-engine.md §2.3](design-engine.md#23-fixed-rate-execution-b12)). The Rust tests already run the copied binary at 75 RPS for 30s with only its bundle, requiring at least 98% achieved traffic and explicit skip/drift accounting.

Then compare the reported p50/p95/p99 against the injected distribution by hand. That comparison is the real milestone — everything downstream assumes those numbers are right.
