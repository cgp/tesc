# Metrix — agent context

Load generator with machine-authored test plans, plus host observation. Rust engine, Python/FastAPI control plane, static front end.

## Where things live

| | |
|---|---|
| `docs/design-api-engine-contract.md` | What each side owns, the handoff, decisions. **Read first.** |
| `docs/design-api.md` | Control plane, observation, front end |
| `docs/design-engine.md` | Load generation and measurement (not yet started) |
| `docs/implementation-api.md` | Track A checklist — **current work** |
| `docs/implementation-engine.md` | Track B checklist — not started |
| `CHANGELOG.md` | Running work log. Append, never consult. |

Section numbers (§N) are shared across the three design docs and are not contiguous within any one file.

## Process

1. Work the checklists in `implementation-*.md` in order. Tick `- [ ]` → `- [x]` as items land.
2. **`bash scripts/check.sh` must pass before you commit.** CI runs exactly that script — add checks there, never only to the workflow. Scope it while iterating: `check.sh engine` / `api` / `contract`.
3. Append to `CHANGELOG.md` at the end of a session: what shipped, what changed, which steps are done.
4. Design changes go in the design doc first, then the code.
5. Keep docs roughly the size they are. Replace rather than accumulate.

## Rules that must not erode

- **The engine never depends on the API.** No Python on the load path, no config outside the bundle, no call to the control plane. Every engine feature must run as `metrix-engine --plan dir/` on a bare machine.
- **`boto3` only under `api/src/metrix_api/discovery/`.** The engine gets concrete addresses, never a cloud identity.
- **The engine never blocks on a consumer.** Backpressure is dropped and annotated.
- **Every displayed percentile carries its sample count.** One implementation in `stats/`; the UI cannot bypass it.
- **Secrets are redacted at capture**, not at display.
- **Calls are read-only in the UI.** Mix and targets are editable.

## Conventions

- Python: **uv** (`uv sync`, `uv run`). No pip, no hand-managed venv.
- Front end: one static page, ES modules, Tabler CSS. No framework, no build step. **Desktop only.**
- Scope: HTTP/1.1 and HTTP/2. ECS discovery only. No gRPC, WebSocket, Kubernetes, or distributed load generation.
- Plans are three documents — calls, mix, targets — in a bundle directory.
- Commits: imperative subject, body explains *why*. Do not co-author git comments.

## Do not

- Write build-time or effort estimates. Ordering and dependencies, yes; durations, no.
- Add a percentile, chart, or stat without saying what question it answers.
- Report a number the sample count cannot support.
