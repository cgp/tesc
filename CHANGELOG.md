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
