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

## 2026-09-12 — A1.8 live stream. **A1 complete.**

- **A1.8 done, and A1 with it.** Starting a recording from the browser, watching it stream at 1s, and stopping it all work against the live host.
- `live.py`: a per-recording hub with a bounded replay buffer, plus start/stop/stream routes. SSE, so the browser handles reconnection and `Last-Event-ID` itself.
- **Cadence is decoupled from the source** — a ticker pushes one aggregated event per second no matter how fast samples arrive. **Slow clients coalesce**: an overrun subscriber is drained and told to resync rather than fed a backlog, and a client that fell past the replay buffer gets a fresh snapshot rather than a partial catch-up that would silently omit the middle.
- Persistence stays ahead of the stream: the recorder writes the row, *then* calls the tee, so a subscriber can never see a value that was not stored.
- **Four bugs found by using it, none by the tests:**
  - The live table was rebuilt every second, so the Stop button was destroyed under the cursor and text selection was impossible. Now the table is **patched cell by cell** (§14.3), verified by asserting the DOM node survives ticks.
  - `StaticFiles` sets no `Cache-Control`, so browsers heuristically cached the ES modules — an edited file kept serving stale code, and a half-updated module graph threw. Now `no-cache` (ETag still gives a 304).
  - A thrown render left the last markup on screen and killed every later update. `render` now catches and shows the failure.
  - A hard-killed process left a recording `running` forever. Startup now closes any such row as `aborted` with an annotation saying why — nothing can be running when the process has just begun.
- Verified in the browser: values updating (0.3% → 0.2% CPU, 102 → 105 samples), a full page reload **rejoining an in-progress recording**, Stop persisting a 116s recording.
- Verified: `scripts/check.sh` all green — 170 passed, 1 skipped.

## 2026-09-12 — A1.9 visual pass. Tabler vendored, icons, no CDN.

- **The page no longer touches the network to draw itself.** Tabler 1.0.0 CSS and JS are vendored under `api/web/vendor/tabler/`, byte-for-byte as published, with provenance and update instructions in a README beside them. Checked before copying: the stylesheet has no `@import`, every image in it is an inline `data:` URI, and the font stack ends at the system UI font — so nothing is fetched at render time either.
- **This is the point, not tidiness.** The tool watches private networks; a control plane that renders only when a CDN answers is a control plane that does not render on the box that needs it.
- `scripts/check.sh` gained a fourth contract rule that fails if a CDN URL reappears in `index.html`, `css/`, or `js/`. Verified by putting one back and watching it fail, then removing it.
- **Icons are inline SVG** (Tabler Icons, MIT) in `index.html` and a new `ui.js`, not a font or a sprite sheet — a menu whose icons arrive on a second request arrives late, and there is no flash before the modules load. `ui.js` throws on an unknown icon name rather than rendering a blank box nobody notices for weeks.
- **Shell rebuilt against Tabler's own components** rather than our overrides: sectioned menu with a brand mark, page pretitle/title/subtitle, `card-header` + `card-subtitle`, `datagrid` for recording detail, `empty` states instead of a bare sentence in a card, and `status` pills for connection and health. `app.css` shrank to brand, menu, and table density — everything in it now names a Tabler variable or sits in a namespace Tabler does not use, so the next version bump stays cheap.
- **Two bugs found while building it:**
  - The live connection badge was rebuilt every tick. Its pulse is a CSS animation with a 2s delay, so a node replaced once a second never reached it — the "live" dot never actually pulsed. Both the status and the gap note are now replaced only when their value changes, which is the same rule the cells already followed.
  - The Recordings table wrapped `observation` and the timestamps onto two lines at 1120px; column widths rebalanced against measured content widths rather than guessed.
- Content is unchanged — same four pages, same routes, same numbers. The one addition is menu section labels (Setup / Performance / Archive), so Recordings no longer reads as part of Performance.
- Verified in the browser against seeded data: every page, a live recording started from Config, the gap alert, Stop, and the status node surviving six seconds of ticks.
- Verified: `scripts/check.sh` all green — 170 passed, 1 skipped.

## 2026-09-12 — A1.9 follow-up: a truncated file, and contrast.

- **`config.js` was committed empty.** The line-ending cleanup in the previous commit used `open(f,'wb').write(open(f,'rb').read()…)`; `'wb'` truncates before the read runs, so the file was blanked *after* the last browser check and shipped at 0 bytes. It surfaced as `entry.view.render is not a function`, which points at the router rather than at the file.
- `scripts/check.sh` gained a fifth rule: **no tracked source file is empty.** An empty file is never intentional and always fails somewhere other than where it is. Verified by emptying a module and watching the check name it.
- **Contrast raised.** Muted text was `#6c7a91`, which is a marketing grey; menu labels now take the body colour (`#222`), muted text is `#4b5563`, and icons `#6b7280`. All variable overrides — the navbar's colour had to be set on `.navbar-vertical` because Tabler declares it there and a `:root` value never reaches it.
- **Corners softened** to Tabler's own large radius, 8px for cards and 6px for small controls; 2px beside an 8px card read as a mistake rather than a hierarchy.
- **The gap beside the menu halved, 31px to 16px.** Half of it was never margin: `.navbar-vertical` is `overflow-y: scroll`, which reserves a scrollbar gutter whether or not a four-item menu overflows, and an empty gutter reads as dead space. Now `auto`.
- Verified: `scripts/check.sh` all green — 170 passed, 1 skipped. Every route re-rendered in the browser with no new console errors.

## 2026-09-12 — Fullscreen fluid layout.

- **The page fills the window.** `container-xl` capped the content at 1320px and centred it, so on a wide screen the cards sat in a column with dead space either side — the actual complaint, which the previous pass misread as the gap beside the menu. Both containers are `container-fluid` now, and their gutter is zero: cards run flush to the menu on the left and to the window edge on the right.
- Recorded in design §2.4 so it does not get "fixed" back to a centred column: horizontal space in a measurement tool belongs to target columns, not to a reading margin.
- **Bootstrap's grid could not do this without a compensation to undo.** A `.row` carries a negative horizontal margin that its container's padding is meant to absorb; with the padding gone it hung 8px past the viewport and raised a horizontal scrollbar. Replaced with two rules in our own namespace — `.metrix-stack` for the single-column pages, `.metrix-split` (a two-column grid) for the recording detail. A single-column page did not need a row in the first place.
- Verified at 1920 and 1440: every route has cards at exactly the menu edge and the window edge, and zero horizontal overflow.

## 2026-09-12 — Edge inset back to 2em.

- Zeroing the container gutter overshot: the state between `container-xl` and that change was the one wanted. `--tblr-gutter-x: 2em` on the page containers, which on the 14px base is a 14px inset on each edge. It is one line, and half the gutter is the padding, so it is the only number to touch if the cards should sit tighter or looser.
- `.metrix-stack` and `.metrix-split` stay. They were introduced to dodge the negative margin a `.row` uses to cancel its container's padding, and that problem is gone with the padding back — but a single-column page still does not need a grid row, and the two-up split still reads better as a grid than as `col-6` twice.
- Verified at 1920 and 1440: 14px each side on every route, still no horizontal overflow.

## 2026-09-12 — The resolved root is on the page, and a checkout can keep its own.

- **The Config page opens with a Storage card**: the resolved `METRIX_HOME`, the database path under it, and *which rule chose it*. The root came from one of several places and two of them depend on state outside the process, so "which directory is this writing to" is a question the page should answer rather than leave to a tooltip.
- **`resolve_home` gained a project-local rule.** Order is now: explicit argument → `$METRIX_HOME` → **a `.metrix/` that already exists in the working directory** → `~/.metrix`.
- **It never creates that directory.** `mkdir .metrix` is how a checkout opts into keeping its own recordings; making the rule fire on absence would scatter a database into whatever directory the tool happened to be started from, and two runs from two directories would silently use two databases. `.metrix/` is gitignored.
- The default was always `~/.metrix`, an absolute path — starting in the repo never wrote into it. The new rule is opt-in, so that stays true until someone asks otherwise.
- `ResolvedHome` carries the reason alongside the path, and `describe_home_source` turns it into a sentence the page prints verbatim. `/api/health` reports `home`, `home_source`, and `database`.
- Tests pin the working directory in every resolution case — two of the rules read state outside the process, so a test that did not would pass or fail depending on where pytest was started. Added: the local directory is used when present, is never created, and is ignored when it is a file rather than a directory.
- `field()` moved to `ui.js`; two views had copies.
- Verified: `scripts/check.sh` all green — 175 passed, 1 skipped. Both resolution paths exercised by hand, and every route still renders with no horizontal overflow.

## 2026-09-12 — What an endpoint's two addresses mean, said out loud.

- The example profile reads `"address": "127.0.0.1:8080"` next to a collector on 9100, and nothing on screen said which was which. It was fair to read it as "visit 8080, find the process listening, watch it". **It is not.** `address` is the load target; `collect` is a separate connection statistics are read from. Neither is derived from the other, and the load target is never probed.
- **The Config page now shows the collection address**, not just the transport name: `scrape http://127.0.0.1:9100/metrics`, `ssh ec2-user@10.0.3.41:22`, or *not collected*. Columns renamed `Address` → **Load target** and `Collection` → **Observed via**, with a standing note above them saying what each is and that statistics are whole-machine rather than per-process.
- The string is built from the transport object itself (`SshTransport.describe`, `ScrapeTransport.describe`) rather than re-formatted for display, so the page cannot show an address the collector does not use — including the defaults it fills in, port 22 and 9100.
- **`docs/profiles.md`**: how to write a profile, aimed at users rather than at us. The two addresses, the three transports, what `/proc` actually yields, and the reverse-proxy case — one box means one set of numbers covering nginx and the app with no way to split them, so put them on separate machines if you need to tell them apart. Worked examples for each.
- `examples/profiles/staging-split.json`: nginx, two app boxes, and an ALB that cannot be logged into. A test now parses **every** example in that directory — they are what people copy, and nothing else in the suite read that folder.
- **One bug, visible only by looking:** the badge printed the transport and `describe()` prefixed it again, so the table read `ssh ssh ec2-user@…`. `describe()` returns the destination only; naming the transport is the badge's job.
- Verified: `scripts/check.sh` all green — 178 passed, 1 skipped. Three profiles rendered in the browser, including the unobservable ALB.

## 2026-09-12 — A1.10: Profiles is its own page, and profiles are editable.

- **Profiles split off Config.** Config is now what this process is and where it keeps things; Profiles is the environments a run can be pointed at. Both sit under Setup in the menu.
- **Create, edit and delete, endpoint by endpoint.** `POST /api/profiles`, `PUT /api/profiles/{name}`, `DELETE /api/profiles/{name}`, and `GET /api/profiles/{name}/document` for the on-disk form the editor loads — the display summary drops fields, so editing it would quietly lose them on save.
- **The editor submits a whole document, not a patch.** One write path, and it is the same `parse_profile` a hand-written file goes through: the form cannot save something the loader would reject, and there is no second set of rules in JavaScript to drift. A rejection comes back as a 422 whose message already names the field and the reason, and is shown in the form with everything typed still in place.
- **The name is fixed after creation.** It is part of a recording's series identity, so renaming through the editor would split one environment's history in two with nothing on screen to say so. The field is readonly rather than disabled — still selectable, just not editable here.
- **The draft lives in state, not in the DOM.** Every state change replaces the markup, so adding an endpoint row or switching a transport would otherwise wipe everything typed. The form is read back into the draft before any change that re-renders; verified in a browser by typing, switching a transport, adding a row, and checking nothing was lost.
- Transport-specific fields only: `ssh` shows user, `scrape` shows path, `none` shows neither. A port box beside *not collected* invites someone to fill it in and wonder why nothing happens.
- The last endpoint cannot be removed — a profile needs one, and a button that only produces a validation error is worse than no button. Delete asks first, and says recordings made against the profile are kept.
- **Two things found by driving it:** a `data-action` on a `<select>` was caught by the click listener, whose `preventDefault` would have stopped the dropdown opening — it is a change handler now; and a form with no submit button still submits on Enter, which reloaded the page and lost the draft, so Enter is taken as save.
- The 422 detail carried the `<submitted>:` source prefix, which is a useful file path when loading from disk and noise beside a form. Stripped, with a test that fails if it comes back.
- Verified in the browser: created a two-endpoint profile from an empty form, checked the file it wrote by hand, reopened it, broke it and watched the rejection keep the draft, then deleted it — declining the confirmation first.
- Verified: `scripts/check.sh` all green — 187 passed, 1 skipped.

## 2026-09-12 — Box identity, disk snapshots, and three honest task counts.

- **`proc.count` meant two different things.** Over SSH it was `/proc/loadavg`'s denominator — kernel scheduling entities, so processes *and* threads, in the hundreds. Over scrape it was `node_procs_running`, runnable processes, about 1. Same metric name, two orders of magnitude apart, in a design whose whole point is that a number does not depend on how it was collected.
- Split into three: **`proc.count`** (processes — numeric directories in `/proc`, counted by glob in the remote shell so it costs no extra forks), **`thread.count`** (the loadavg denominator), **`proc.running`** (`/proc/stat procs_running`). Over scrape the first two come from node_exporter's `processes` collector, which ships disabled — when it is off they are **absent rather than filled in from something else**.
- **The fixture was rigged.** The exporter fixture used `node_procs_running 431`, a total-looking number for a runnable count, which is exactly why the existing cross-transport test passed: it only checked that a metric was *present*. It now asserts the two transports produce the same *number* for the three counts, and would fail if this were reintroduced.
- **Host identity**, recorded once per target: hostname, os, kernel, arch, cpus. Not a series — it does not change during a run, and it is what answers "what was this measured on?" once the box is gone.
- **Filesystem usage**, read once before the run and once after it drains. Two `df` calls rather than a series on every mount, because the question is "did this consume disk, and how much". The delta is computed in the store so it has one definition; a mount with no finish reading has **no delta rather than a delta of zero**, since a failed probe is not the same as nothing being written.
- Both work over either transport — a POSIX script over SSH (`. /etc/os-release`, `uname`, `df -Pk`; no GNU-only flags, because the boxes worth measuring include the busybox ones), and `node_uname_info` / `node_os_info` / `node_filesystem_*` over scrape. New tests parse the same box both ways and assert the two descriptions agree.
- Probing is **best-effort and never fatal**. A box that will not answer a `df` is still worth observing, and a recording that refused to start because an identity probe timed out would be a worse tool.
- Migration `002_host_facts`, and the migration test now derives the expected version list from what is on disk rather than a literal that needs editing each time.
- New **Hosts** and **Disk** cards on the recording detail. The change column is signed and coloured: a run that freed space and one that consumed it are different findings, and "1.2 GB" alone does not say which.
- Pseudo-filesystems (`/proc`, `/sys`, `/dev`, `/run`) and zero-sized overlays are dropped by both readers — an overlay with nothing behind it reads as 100% full every run otherwise.
- Verified: `scripts/check.sh` all green — 209 passed, 1 skipped. Every route re-rendered with no console errors and no horizontal overflow.
## 2026-09-12 — Reload the profile list.

- **Reload** on the Profiles page re-reads `$METRIX_HOME/profiles/`. Profiles are files, and the file is the primary form — someone editing one in a text editor or pulling a change from version control had no way to see it short of reloading the whole page.
- The list is **not polled**. Replacing what a person is reading, unasked, would be worse than a button, and nothing here changes on its own.
- The button carries the time the files were last read. Without it, a reload that found no change looks exactly like a button that does nothing. It is offered on the empty state too, where a file appearing on disk is the likeliest reason to press it.
- Not offered over an open editor, where re-reading the directory would throw away whatever was being typed.
- **A test was asserting something it did not mean.** `test_every_shipped_example_parses` globbed `examples/profiles/` off disk, so a local profile kept there — ignored by git, which is exactly where one would put a real one while working — failed the suite. It now reads the tracked files, which is what "shipped" meant all along.
- Verified in the browser: added a profile and a deliberately broken one on disk with the page open, pressed Reload, and watched both appear — one in the list, one in the *will not parse* alert — with the timestamp advancing and no console errors. Then emptied the directory, reloaded from the empty state, and restored it.
- Verified: `scripts/check.sh` all green — 209 passed, 1 skipped.

## 2026-09-12 — A2.1: a hostname resolved to what is actually running behind it.

- **The chain, end to end**: Route 53 → ALB/NLB → listeners and host-header rules → target group → registered targets → the ECS service registered against that group → tasks, containers and image digests → EC2 instance, type, AZ, private DNS → autoscaling group with desired/min/max. `discovery/ecs.py`, the only module in the tool that imports boto3.
- **Partial resolution is the result, not an error.** An inventory carries *how far it got* — the furthest hop reached — and notes saying what it could not determine. A network balancer with nothing in ECS behind it, a task in bridge networking with no interface of its own, a target mid-deregistration: each stops somewhere sensible and says so. Only the spine raises, because a failure there returns an empty answer rather than a short one.
- **A refused hop costs its context, not the answer.** An account that withholds `autoscaling:Describe*` still yields tasks; the ASG fields are simply absent and the reason is a note. `DiscoveryError` is reserved for "could not start".
- **The shipped IAM policy is now checked against the code.** `test_discovery_policy.py` reads every AWS call out of `ecs.py` by AST, maps it to an IAM action, and fails if `policy/metrix-readonly.json` does not allow it — *and* if the policy grants something nothing calls. A permission document is only worth shipping if it stays true, and the alternative way to learn it is a sequence of AccessDenied errors.
- **Three recorded accounts, no credentials.** `api/tests/fixtures/aws/`: an ALB in front of a Fargate service (with the balancer on the second page of a paginated response, a host-header rule beating the listener default, and a draining target), an ALB in front of an EC2-launch-type service (bridge networking, container instances, two task-definition revisions on one service, an ASG), and an NLB with no ECS at all (one IP target resolved through its network interface, one belonging to nothing in the account).
- **Two spellings that would have been silent bugs.** A target group ARN ends `.../targetgroup/<name>/<id>`, so the obvious "last segment" gives a hex string where a name belongs. And `describe_rules` does not exist for a network load balancer — asking is an error, not an empty list — so rules are read only when the balancer is an application one.
- **A task and its instance are separate entries, and both are kept.** A Fargate task has no instance; a bridge-networked task has no address and is reached at the instance hosting it. The note about the second is emitted once per service rather than once per task.
- **A service registered against two target groups is not listed twice.** A traffic shift puts one service in both, and both walk down to the same tasks; an inventory that listed a box twice would double every host it is the collection list for. First sighting wins.
- One timestamp per snapshot rather than one per entry: a walk is a single instant, and per-entry times would differ only by the latency of the call that found each one. §3.3 of the design says so now.
- `POST /api/discovery/resolve` — a hostname, or a cluster and a service named directly. Sync rather than async on purpose: boto3 blocks, and an `async def` here would park every open SSE stream for the length of the walk. No page yet — the UI for discovery belongs with A2.3, where a profile is written from what was found.
- Verified: `scripts/check.sh` all green — 262 passed, 1 skipped. All three fixtures walked by hand and the resulting inventories read line by line.
