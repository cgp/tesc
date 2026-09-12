# Metrix — API Implementation Plan

The control plane, observation, and the front end. Nothing here requires the engine to exist — through A3 the product is a working host-observation tool. A4 is the single integration milestone.

Design: [design-api.md](design-api.md). Boundary and shared foundation (**F0, do this first**): [design-api-engine-contract.md](design-api-engine-contract.md). The other track: [implementation-engine.md](implementation-engine.md).

---

## 1. Project layout

```
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
```

The front end is a single static page — no templating, no build step. `boto3` appears under `discovery/` and nowhere else.

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

### 2.2 Running it

```bash
uv sync                    # once
uv run metrix-api          # serves the static page + API on :8080
```

One command. Everything else — starting load tests, starting observation-only recordings, browsing recordings — happens in the browser.

---

## 3. Milestones

Nothing here requires the engine. Through A3 the product is a host-observation tool; A4 is the only integration point.

### A1 — Observation (start here)

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

### A2 — Discovery

| Step | Deliverable |
|---|---|
| A2.1 | ECS discovery: hostname → ALB → target group → service → tasks → containers → instances (§3.1) |
| A2.2 | Resolved inventory: storage, run pinning, refresh at phase boundaries, `host_count_changed` |
| A2.3 | Reachability verification at profile setup; `targets.json` written from a profile |
| A2.4 | Environment baselines; baseline/settle deltas, recovery curves, leak detection (§9.7) |

**Done when:** a hostname resolves to a task list with image digests, and observation attaches to those identities.

### A3 — Analysis of recordings

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

### A4 — Integration with the engine

| Step | Deliverable |
|---|---|
| A4.1 | Bundle assembly and export (`GET /api/plans/{name}/bundle`) |
| A4.2 | Engine supervision, NDJSON ingest, load + host series on one timeline |
| A4.3 | Plan editor: calls, chains, percentages with implied RPS, validation display |
| A4.4 | `POST /api/plans/generate` — calls from OpenAPI/WSDL, starter mix (§8) |
| A4.5 | Sweep comparison view (§17.6) |

**Done when:** a run launched from the browser shows load and host metrics against one clock.

---

## 4. Rules to honor throughout

1. **boto3 appears only under `api/src/metrix_api/discovery/`.** The engine never gets a cloud identity.
2. **Every displayed percentile carries its sample count.** The rule lives once in `stats/` and the UI cannot bypass it.
3. **Secrets never reach SQLite, run directories, exports, or error samples.** Redaction is applied at capture, not at display.
4. **Desktop only.** No responsive breakpoints.
5. **Calls are read-only in the UI** (§20 of the design). Mix and targets are editable.

---

## 5. Testing

- **Unit:** NDJSON ingest, series identity, noise-floor and CI math, regression logic, discovery response parsing.
- **Integration:** full recording lifecycle, SSE reconnect with gap verification, purge behavior.
- **Recorded AWS fixtures:** discovery is tested against committed JSON fixtures of real `describe_*` responses — including partial-resolution cases (NLB with no ECS, task with no ENI, deregistered target). Live AWS is never required to run the suite.
- **Recorded engine fixtures:** a canned `summary.ndjson` from the F0.3 schema drives ingest, the stats table, and the charts long before an engine exists. This is what makes A4 wiring rather than discovery.

---

## 6. Risks

| Risk | Mitigation |
|---|---|
| Integrating with the engine late and badly | NDJSON shapes frozen in F0.3; fixture-driven ingest from A1 |
| Percentiles displayed without their support | Rule 2, enforced in one place |
| Discovery complexity leaking outward | Rule 1; inventory is the only thing that escapes `discovery/` |
| Front end drifting into a framework | One static page, no build step; a bundler would be a deliberate decision to revisit |

---

## 7. Where to start

F0.1–F0.3 first — skeleton, plan types, frozen schemas — since both tracks build on them.

Then **A1.1–A1.3**: `METRIX_HOME`, a profile with two explicit hosts, and the SSH collector sampling them at 1s. The milestone is a JSON series of real CPU and memory off a real box, persisted and reopenable.

Genuinely useful on its own, requires no Rust, and makes the domain concrete before any statistics work begins.

After that, A1.4–A1.7 complete the observation loop — scrape collector, recordings, the static page, and the live stream — which is the point at which the tool is worth showing to someone.
