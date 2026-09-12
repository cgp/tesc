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
