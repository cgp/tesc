# Changelog

A running log of what got done and what changed. Append at the bottom; newest last.

This is a work log, not a reference — it records *what happened*, not *how things work*. Never look things up here; the design documents are the source of truth. Entries can be terse.

**Format:** one dated heading per working session. Under it, bullets for what shipped and one-liners for decisions that changed. Note checklist items completed as `F0.1 done`.

---

## 2026-09-11 — Design

- Reframed [initial-design.md](docs/initial-design.md) from a server-stats recorder into a load generator with machine-authored plans, with host observation as a co-equal capability rather than a side channel.
- Set the statistical floor at 30s × 75 RPS = 2250 requests. Supports p50/p95 solidly, p99 coarsely (95% CI spans p98.6–p99.4), does not support p99.9.
- Added the phased run timeline: baseline → warmup → measure → drain → settle.
- Added breakpoint search, run annotations, generator calibration.

## 2026-09-12 — Structure

- Engine became independently shippable: takes one plan bundle, owns sequential multi-target execution, no Python wrappers. Front end became a single static page (no Jinja, no framework, no build step).
- Split the plan into three documents — calls, mix, targets — composed into a bundle directory.
- Made chains the unit of the mixture. Percentages of one total rate, must sum to 100. Session policy per chain (`fresh` / `reuse` / `pool`). Expected-failure chains (a `login-fail` asserting 401 passes).
- Split the docs by side: contract, API design, engine design, and one implementation plan per track. Section numbers preserved across the split so cross-references still resolve.
- Added §20 to the API design: mix and targets are editable in the UI, calls are read-only.
- Removed build-time estimates from all documents.
- Converted implementation steps to checklists (56 items). Added this changelog, `AGENTS.md`, and a `CLAUDE.md` stub importing it.

**Next:** F0.1–F0.3, then A1.1–A1.3 ([implementation-api.md](docs/implementation-api.md) §7).

## 2026-09-12 — F0.1 skeleton

- **F0.1 done.** Repo skeleton, both build systems, both lockfiles committed.
- `engine/`: Cargo workspace, edition 2024, `unsafe_code = "forbid"` and `clippy::all = deny` at the workspace level. Five stub crates — plan, metrics, gen, mock, engine. The two binaries exit 1 with "not implemented" rather than pretending to work.
- `api/`: uv project, FastAPI, `uv run metrix-api` serves `/api/health` on 127.0.0.1:8080. Package stubs mirror the design's module split, with `discovery/` carrying a docstring noting it is the only place boto3 may be imported.
- `schema/`, `examples/plans/` are placeholders until F0.3. `policy/metrix-readonly.json` written out from §3.2.
- Verified: `cargo build`, `cargo clippy --all-targets` clean, `uv run pytest` 1 passed, and the server answers `{"status":"ok"}`.
- Noted: Starlette warns that `TestClient` with httpx is deprecated in favor of httpx2. Harmless now; revisit if it becomes noisy.

## 2026-09-12 — F0.2 plan types

- **F0.2 done.** `metrix-plan` has the three documents as three separate types: `Call`, `Mix`, `Targets`, plus `auth` and shared primitives. serde only; validation is F0.3.
- Wrote `examples/plans/checkout-mixed/` as the fixture — the six-chain mixture from the design, eight calls, two targets. 8 integration tests parse it and assert the properties the design claims: percentages total 100, every step names a call that exists, `login-bad-password` expresses its expected 401 as an assertion, image digests travel with targets, documents round-trip without loss.
- `Dur` parses `"30s"` / `"1h30m"` and re-serializes in the same spelling, so a round-trip through the API does not rewrite a file someone is reading. A bare `"30"` is rejected with "write 30s, not 30".
- Hit and fixed: `deny_unknown_fields` is incompatible with `#[serde(flatten)]` — the flattened keys read as unknown. Affected `Auth`, `Assertion`, `Condition`, `RepeatUntil`; comments added at each so it does not get re-added.
- Decisions: `description` defaults to empty rather than being required, so a scratch bundle still runs; `Target.attributes` is an opaque string map the engine echoes through, keeping inventory detail out of engine types.
- Verified: `cargo test` 12 passed, `cargo clippy --all-targets` clean.

## 2026-09-12 — F0.3 the contract

- **F0.3 done.** Four schemas generated from the Rust types and committed: `call`, `mix`, `targets`, `events` (2722 lines total).
- **NDJSON shapes frozen** in `metrix-metrics::events`. Records: `run_started`, `phase_changed`, `target_started/finished`, `summary`, `request`, `annotation`, `run_finished`. Summaries carry serialized HDR histograms rather than percentiles, because histograms merge and percentiles do not. `events_version` is 1; the API refuses a stream version it does not know.
- Schema generation lives in `metrix-engine --emit-schemas <dir>` rather than a sixth crate. It is the engine binary's only implemented function.
- `scripts/check-schema.sh` regenerates to a temp dir and diffs, catching both drift and orphaned schema files. Negative-tested: a one-word edit to a committed schema fails it.
- `examples/fixtures/summary.ndjson` is a canned 13-record stream. **This is how track A builds ingest, the table, and the charts before an engine exists** — and it validates against the same schema a real engine will emit.
- `api/tests/test_schema_contract.py` is the cross-language half: schemas are valid Draft 2020-12, the example bundle validates, the fixture stream validates line by line, and — because validating proves nothing unless the schema rejects things — a mix missing `chains` and a bare `"30"` duration are both refused.
- Fixture ordering bug caught by its own test: an annotation was stamped before a request that preceded it. `t_ms` is now asserted monotonic.
- Verified: `cargo test` 12 passed, clippy clean, `uv run pytest` 12 passed, drift check green.

## 2026-09-12 — F0.4 CI. Shared foundation complete.

- **F0.4 done. F0 is complete**, so both tracks are unblocked and independent from here.
- `scripts/check.sh` is the single definition of green: fmt, clippy `-D warnings`, cargo test, ruff, pytest, schema drift, plus two guards. CI calls that script and nothing else, so there is no second list to drift. Scoped runs: `check.sh engine | api | contract`.
- Two rule guards, both negative-tested: **boto3 outside `discovery/`** fails and names the file; **the engine depending on the control plane** fails. These are the rules most likely to erode one convenient import at a time.
- `.github/workflows/ci.yml` — three matrix jobs by scope, so a failure says which half broke. Rust toolchain only where needed, uv only for the api job. Nothing to push to yet; it is ready when there is.
- Ran `cargo fmt` across the tree (schemars derives had pushed some lines over) so `--check` can be a gate.
- Bug caught while wiring: `cargo --manifest-path X fmt` is invalid — `--manifest-path` belongs to the subcommand and `cargo fmt` does not accept it at all. The script cds into `engine/` instead.
- `scripts/check.sh` is now step 2 of the process in `AGENTS.md`, ahead of committing.

**Next:** A1.1 — `METRIX_HOME` layout, config, SQLite schema and migrations ([implementation-api.md](docs/implementation-api.md)).

## 2026-09-12 — A1.1 home, config, store

- **A1.1 done.** Track A has somewhere to put things.
- `config.py`: `METRIX_HOME` resolved once (explicit → `$METRIX_HOME` → `~/.metrix`), every path derived from that one root. `config.toml` is optional; unknown sections and keys are named rather than ignored, since a silently-ignored typo in an observation interval is a bad afternoon. `parse_duration` mirrors the engine's spelling, rejecting a bare `"30"` the same way.
- `store/`: forward-only numbered SQL migrations with gap and naming checks, WAL, foreign keys on per connection. Migration 001 covers observation — `recording`, `recording_target`, `phase`, `host_sample`, `collection_gap`, `annotation`. Load-run tables wait for A4; adding them later is what the mechanism is for.
- Schema decisions: `host_sample` is narrow (one row per metric per sample) so collectors can report whatever a host exposes without a schema change; `series_key` is stored on the recording and computed in code; `addressing_mode` is a column because through-the-LB and direct-to-container must never land in one series; `STRICT` tables and `CHECK` constraints throughout.
- Three bugs, all caught by tests asserting on messages and behavior rather than on success:
  - `slots=True` makes dataclass class attributes **slot descriptors, not defaults** — `ServerConfig.port` was being compared as a descriptor, so a config typo surfaced as a nonsense port error.
  - `sqlite3.executescript` **commits any open transaction before it runs**, so an outer `BEGIN` is discarded. The transaction now lives inside the script, which also makes the schema change and its version row atomic.
  - A failed migration left a half-applied schema until the rollback moved inside the same script.
- Verified: `scripts/check.sh` all green — 54 Python tests, 12 Rust.

## 2026-09-12 — A1.2 target profiles

- **A1.2 done.** `profiles.py`: parse, validate, save, list, and convert a profile into the engine's targets document.
- Profiles are **JSON, not YAML** as the runtime layout originally said — stdlib read *and* write, one fewer dependency, same format as plans. Doc updated.
- `to_targets()` output is tested against `schema/targets.schema.json` itself, so the handoff is checked against the engine's own contract rather than against an assumption about it. `only=[...]` selects a subset for testing one suspect container.
- Validation refuses what would otherwise waste an afternoon: duplicate endpoint ids, an address without a port, a name that disagrees with its filename, and — the useful one — **`addressing: direct` without a `host_header`**, since a container addressed by raw IP returns a 404 or a default backend and the run looks fine while measuring nothing.
- Endpoints carry `attributes` (instance type, AZ, image digest) straight through to targets, which is what will explain a sweep outlier later. IPv6 addresses parse with brackets stripped from the host.
- Found: `line-length = 100` in the ruff config was decorative, because ruff's default rule set does not include E501. Now selecting `E, F, I, UP, B, SIM`; two over-long lines fixed.
- `examples/profiles/local.json` is the day-one path: a dev server on this machine, no AWS, no discovery.
- Verified: `scripts/check.sh` all green — 78 Python tests, 12 Rust.

## 2026-09-12 — A1.3 the observer

- **A1.3 done.** SSH collection at 1s with a normalized metric shape: `observer/metrics.py` (names and `Sample`/`Gap`), `linux.py` (parse and derive), `collector.py` (scheduling, gaps, reconnect), `ssh.py` (transport).
- **The remote side is deliberately dumb** — a POSIX `sh` loop printing `/proc` files with markers, nothing installed. All parsing and arithmetic happen in Python, which is what makes the part that can be quietly wrong testable against captured text with no host involved.
- One long-lived SSH channel rather than an exec per sample: at 1s, per-sample connection and process spawn would dominate and the collector would be measuring itself.
- Deliberate behaviors, each with a test: the first sample emits gauges but **no rates** (inventing one would put a wrong number at t=0 on every chart); a counter going backwards is a reboot, not a negative rate; a stalled stream emits a gap and **restarts rate accumulation** rather than averaging across the hole; a dead transport is a gap plus a reconnect, never an aborted recording; one unreachable host does not stop the others; partitions do not double-count their disk and loopback is not network traffic.
- `iowait` and `steal` are separate metrics and excluded from `cpu.busy` — iowait is the disk, steal is a neighbour, and "CPU is busy" hides both.
- Nearly shipped a real bug: while shortening a line I changed the meminfo grep to `Swap:`, which matches neither `SwapTotal` nor `SwapFree`. The fixtures bypass grep, so no test would have caught it — the remote script is now structured so its lines fit without pattern surgery.
- **Untested against a live host.** The parser, the derivations, and the collector's failure handling are covered; `ssh.py` itself needs a real box.
- Verified: `scripts/check.sh` all green — 107 Python tests, 12 Rust.

## 2026-09-12 — A1.4 scrape collector

- **A1.4 done.** `prometheus.py` scrapes node_exporter-style metrics into the same shape the SSH transport produces.
- **Refactor the second transport forced:** the collector had hardcoded `/proc` parsing, so a transport could not differ in format. Transports now yield parsed `RawSample` counters and each owns its own parsing; `raw.py` holds the shared counter shape and the one `derive()`. Counter keys are unit-normalized at parse time (bytes, milliseconds), so `derive()` is about elapsed time rather than sector sizes.
- **The valuable tests are the agreement tests:** the same facts fed through `/proc` text and through exporter text must produce identical CPU percentages, disk byte rates, and disk-busy. A normalized shape that two transports disagree about is not normalized.
- `collection_gap` annotations: severity **warn**, not invalid — the recording is still worth having. The message names the target, the duration, the reason, and states the interval is never interpolated.
- Scrape tolerates transient failures (3 consecutive before giving up), since one failed request is a missing sample rather than a dead stream.
- Bug found by a test: **`float("NaN")` does not raise**, so exporter NaN values — written when a collector fails — were flowing through as real measurements. Now rejected explicitly along with `±Inf`.
- Second finding: my exporter fixture held CPU constant between samples, so the "every group metric is produced" test passed vacuously for the SSH transport and failed honestly for scrape. Both fixtures now vary.
- Verified: `scripts/check.sh` all green — 123 Python tests, 12 Rust.

## 2026-09-12 — A1.5 live host test

- **A1.5 done, and it works against a real machine.** Collection over SSH to a host on the LAN: 4 samples, no gaps, 28 metrics populated, clock skew 0.6s.
- Auth uses the **native SSH setup** — `~/.ssh/config` is read and default keys are offered, so if `ssh <host>` works from a shell the tests work. A profile naming a key still wins; this is the simpler path, not a replacement.
- Config lives in `api/tests/integration.toml`, **gitignored** (someone's LAN address is not a repo fact), with `integration.sample.toml` committed. Absent config means the tests skip, so CI stays green without a host.
- What the live tests cover that fixtures cannot: the remote shell accepts the script, this distribution's `/proc` parses, **every metric group actually yields a value**, CPU modes sum to ~100, no metric is negative, used memory does not exceed total, and skew is small enough for series to align.
- Real output from the host, for the record: 21.7 GB total memory, 726 processes, 1952 open descriptors, CPU 98.6% idle, network 1.2 KB/s in and 10.7 KB/s out. Nothing surprised the parser.
- Verified: `scripts/check.sh` all green — 126 passed, 1 skipped (the scrape test; that host has no exporter).

## 2026-09-12 — A1.6 observation recordings

- **A1.6 done.** `recording.py` (start/stop, lifecycle) and `store/recordings.py` (all reads and writes). A recording against the live host now persists and reopens with its samples, gaps, and annotations.
- **Writes land as they arrive**, not buffered to the end: a recording interrupted by a crash is still worth having up to where it stopped — the same reasoning as drawing gaps rather than hiding them.
- **Gaps and their annotations are written as one unit.** They answer different questions ("was this collected?" versus "what should the reader know?"), but a gap without its annotation is invisible in the UI, so one function writes both rather than two callers who might disagree.
- Observation-only collapses baseline and settle into a single window, recorded as `measure` — the phase the numbers are read from. The distinction only earns its keep once load exists.
- `series_key` is **readable rather than hashed** (`observation|staging|load_balancer|1s|api=0.0.0`): when a trend unexpectedly starts a new line, the reason should be visible without decoding anything. Interval is part of it — a metric sampled every 5s is not comparable with one sampled every second.
- A profile with no collectable endpoint is **refused before a row is opened**, since an empty recording is indistinguishable from a failed one.
- The live test now covers the whole A1 loop end to end: 4s against the real host, samples stored, groups honoured (disk absent when unrequested), no gaps, reopened from SQLite.
- Verified: `scripts/check.sh` all green — 140 passed, 1 skipped.

## 2026-09-12 — A1.7 the front end

- **A1.7 done, and confirmed in a browser against real data.** One static page, Tabler CSS, seven ES modules, no build step, no framework.
- Routes behind it: `/api/profiles`, `/api/recordings`, `/api/recordings/{id}`, `/api/recordings/{id}/series`. The series endpoint returns `[[t_ms, value], …]` per target — the shape a chart consumes.
- `api.js` is the only module that touches the network; views are pure functions of `state.js`. That separation is what keeps the A1.8 stream from having to know anything about the DOM.
- An unparseable profile is **listed as broken rather than dropped** — an environment silently missing is worse than a visible error.
- **Two bugs the test suite did not catch, both found by opening the page:**
  - `list_recordings` never populated `targets`, so the archive's target count always read zero. Now one query rather than N+1.
  - **`sqlite3` objects are thread-bound, and FastAPI runs a sync dependency's setup and teardown on different threadpool threads.** Every request 500'd under uvicorn. `TestClient` hid it by reusing one thread — so the fix is pinned by a test that deliberately moves a connection across threads.
- Removed `test_health.py`: the F0.1 stub asserted an exact body and is superseded by the route tests. It also built an app against the real `~/.metrix`.
- `B008` ignored in ruff — `Depends()` in an argument default is how FastAPI declares a dependency.
- Verified: `scripts/check.sh` all green — 153 passed, 1 skipped.
