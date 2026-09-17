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

## 2026-09-12 — A2.2: the answer is stored, pinned, and checked again at the end.

- **A profile can now say where to find its endpoints instead of listing them.** A `discover` block names a hostname (or a cluster and a service), how to collect from whatever is found, and how long a resolution stands. `examples/profiles/staging-discovered.json`.
- **The file keeps the question and the store keeps the answer.** Resolved endpoints are never written back into the profile: that would freeze one walk into a document whose whole purpose is to ask for a fresh one. `to_document` drops them, and a round trip proves it.
- **An unchanged re-resolution extends the row rather than repeating it.** `discovered_at` is when this state was first seen, `confirmed_at` when it was last verified. Two walks of an untouched account differ only in when they happened, and a row per walk would make *when did this change* unanswerable without diffing every document in the table. Migration `003_inventory`.
- **Every recording pins the snapshot it ran against.** Read from the pin, never resolved again — the tasks it names have very likely been replaced since, and the answer to "what did this measure" must not change when the environment does. A new **What it ran against** card on the recording detail shows the resources, their placement, task definition and image digests.
- **`host_count_changed`.** Run start and run end are phase boundaries, and discovery is walked again at both; a host set that moved between them becomes an annotation naming what appeared and what went away. **Warn, not invalid** — an environment that scaled under load may be exactly what was being measured, and that is the reader's call to make.
- **Collection stays with the set pinned at the start.** A host series that begins halfway through a recording is worse than an absent one, and every average over "the environment" would change meaning mid-chart. The annotation is what says the comparison was against a moving target.
- **The comparison is over host identities, not over the document.** A task redeployed onto the same boxes is not the environment changing size; comparing snapshots whole would flag every deployment, which is how a flag gets ignored.
- **The endpoint id is the inventory resource id, verbatim** — that identity is the join between host metrics and load metrics (§3.3), so shortening it would break the thing it exists for. The *display* is shortened instead: `task/3f1c5a7e…` in a column heading, the full id in its title.
- **Whole-machine statistics mean one host per box.** An instance when the walk resolved one, the task only when it did not — collecting from both a box and the tasks on it would count the same CPU twice under two names. Fargate has nothing to log into, so there the task is the host.
- A host resolved without a port is **noted rather than given an invented one**: an address that is not addressable is worth recording and is not worth guessing at.
- **Profiles page**: a discovered profile shows what it was resolved from, how far the walk got, when it was last confirmed, and a **Resolve** button that always walks — a button that handed back the cache someone was trying to get past would appear to do nothing. Editing is not offered for one: the form edits endpoint rows, and a discovered profile has none of its own.
- A refresh that cannot reach AWS costs the check, not the recording. Failing to close a recording because a control-plane call timed out would be a worse tool.
- Verified in a browser against a recorded account: resolved a profile from cold, watched the endpoints and the draining-target note appear, recorded against the discovered tasks, and read the pinned inventory back on the recording. No console errors, no horizontal overflow at 1264px.
- Verified: `scripts/check.sh` all green — 297 passed, 1 skipped.

## 2026-09-12 — Preserve profile editors during background updates

- Reworked shared front-end state subscriptions to select each view's dependencies; the shell, health badge, error banner and active view update independently.
- Fixed health polling and unrelated live/list/error updates clearing new or edited profile forms. Active drafts retain their fields, focus and selection until an intentional editor action or navigation.
- Updated the API design and A1.10 implementation details. Added a Node regression test to `scripts/check.sh api` covering form preservation, health failure/recovery, route changes and live Stats updates; no additional milestones completed.

## 2026-09-12 — A2.3: two questions per box, and a targets document that means it.

- **Verify** on a profile asks both of a profile's claims at once: can a connection be opened to the load target (with the TLS handshake, when TLS is on), and does the collector answer a real probe. Reported separately, because they fail for different reasons and are fixed by different people — a firewall is not a missing exporter.
- **The collector check is a probe, not a port check.** An SSH login that succeeds and then cannot run the stats script, and an exporter answering 404 on the configured path, both pass a port check and then produce a recording full of gaps. A probe that answers names the box it reached; one that answers with nothing it can read is a failure, because that is the wrong-path case exactly.
- **Checked in parallel.** These are timeouts, not work: eight boxes behind one firewall would otherwise take eight timeouts, in series, to say the same thing once. A test fails if that regresses.
- **The result is not stored.** Reachability is true of a moment, and a green tick from yesterday shown as current would be worse than no tick — so it lives in memory for that sitting and the band says so.
- **`load` on an endpoint** — *watch this box, do not send to it*. The two roles an endpoint can play were conflated: `to_targets` emitted every endpoint, so a discovered profile would have pointed load at the tasks *and* the balancer. Discovery now sets the flag from `addressing`, and the default target selection is the endpoints that take traffic.
- Every endpoint still carries an address, because that is where the machine is. The Profiles page counts both roles — *1 sent to · 2 of 3 observed* — and draws a watched-only address muted, so a column headed **Load target** never claims something it should not.
- **`GET /api/profiles/{name}/targets`** builds the engine's `targets.json` from a profile, with `?only=` for a subset. The shape is held to `schema/targets.schema.json` by the suite rather than at runtime: the schema is a contract between two programs, so it is checked where a mismatch can be fixed rather than where it can only be reported.
- **`target_unreachable`**, the run-time counterpart: a target that produced not one sample for a whole recording. **Invalid**, not warn — the box was named in the profile, so a reader counts it among what was measured, and an average over "the environment" that silently omits one of its machines is worse than no average.
- **§3.4 and §3.5 of the design existed only as cross-references.** Two other sections pointed at a §3.4 that was never written. It says what addressing decides and why the two endpoint roles are separate flags; §3.5 says what verification is and why it is never automatic.
- Front-end regression tests for the new rendering, alongside the harness that arrived with the editor-preservation work: the watched-only address, the shortened discovered id, the in-progress state, both answers per endpoint, and that a failure detail from the API is escaped rather than interpreted.
- Verified in a browser against a live exporter on loopback and a port with nothing behind it: one box named itself, one failed with the transport's own words, the skips stayed skips, and a recording against the dead box came back carrying `target_unreachable`. No console errors, no horizontal overflow at 1264px.
- Verified: `scripts/check.sh` all green — 321 passed, 1 skipped, 6 front-end tests.

## 2026-09-13 — A2.4: what normal looks like, and whether it came back.

- **`stats/` has content for the first time**, and one rule holds it together: every number carries the count behind it, and a number the count cannot support is not reported at all. A median needs three samples, a p95 needs twenty — below that the 95th percentile is the maximum wearing a hat, and once it reaches a table someone quotes it. An unsupported figure comes back absent rather than caveated.
- **A recording can be the environment baseline for its series**, and later ones are measured against it. This is what an observation-only recording is *for* (§10.2): a box that idles at 60% CPU and one that idles at 5% are different stories, and only a recording of the environment doing nothing tells you which you have.
- **Refused for a recording carrying an `invalid` annotation**, unless someone overrides it deliberately. A baseline is what everything later is judged against, so adopting one taken while a target was unreachable would quietly poison every comparison instead of failing one. The refusal names the annotation; the browser asks before overriding. This is the first thing that makes the severity mechanism do work.
- **"Different" is measured against the baseline's own spread**, not a percentage: 2× its IQR, the band the trend view uses for regressions, floored at 2% of the median so a metric that barely moves does not flag on rounding.
- **Deltas are coloured by good or bad, not by sign.** More free memory and more CPU are both "up", and only one is a problem — `WORSE_WHEN` is where that knowledge lives.
- **Pooled across boxes, and per box.** A discovered environment replaces its tasks on every deployment, so matching this recording's targets against the baseline's matches nothing; pooling every box's readings describes *a typical box here*, which survives the tasks being replaced. A named box appears only where it disagrees with the pooled verdict — otherwise one finding printed once per machine buries the one that differs.
- **Recovery, from the settle phase**: time back inside the band, the worst value seen after the traffic stopped, and whether it came back at all. Queues, collections and flushes often peak *after* the last request, which a measure-phase summary misses entirely. A dip into the band does not count as recovery — it has to stay there, since the first touch is the most flattering possible reading.
- **`not_returned_to_baseline`**, raised when a recording closes. Warn, not invalid: it is the cheapest evidence of a leak there is, and evidence rather than proof. A metric that ends outside its band on the *good* side is not flagged, because calling that a leak teaches people to ignore the flag.
- Silent for an observation-only recording, which has no settle phase: baseline and settle collapse into one window there, so there is no "after" to measure and nothing to claim.
- **A bug the deltas exposed**: `bytes()` scaled with `while (n >= 1024)`, which never fires for a negative number — so a downward delta rendered as `-800000000 B`. It scales the magnitude now, and a test pins it.
- **Another the browser exposed**: `is_baseline` arrives as a JSON boolean and the button checked `=== 1`, so it never showed as set. The Python test could not catch it — `True == 1` — so both tests now assert the type the API actually sends.
- Verified in a browser against a seeded quiet baseline, a busy day and a phased run with a deliberate memory drift: the comparison named four metrics with their counts, the leak showed as *never came back* while everything else recovered, the 409 refusal fired on the invalid recording and the override took, and the old baseline was demoted. No console errors, no horizontal overflow at 1264px.
- Verified: `scripts/check.sh` all green — 355 passed, 1 skipped, 14 front-end tests.

## 2026-09-13 — A3.1: the stats table shows a distribution, not a last value.

- **The Stats page was showing the most recent reading per box.** A single sample answers almost nothing about a minute of collection; the table now shows what each metric actually did — Count, Min, Median, p95, Max and Std dev, the default columns of §14.2 with the load-side ones absent until an engine produces them.
- **One view, live and finished** (§14.3). They differ only in where the numbers come from — the SSE stream, or `/summary` — and both arrive already summarised. Nothing in the browser computes a percentile: the sample-count rule lives in `stats/` and a median computed in JavaScript would be a second copy of exactly the rule that has to hold in one place.
- **The live recording keeps its values in memory and summarises them server-side each tick.** The cost is a sort per metric per second, which at 1s sampling is milliseconds even for an hour-long recording. The alternative — recomputing from SQLite every tick — grows with the recording, and the other alternative puts the arithmetic in the UI.
- **Rows are grouped under their target, with the pooled view first** as the run total, mirroring how §14.1 groups steps under their chain. Sorting is *within* a group; sorting across them would dissolve the grouping and lose which box a row belongs to.
- **A withheld figure is an em dash carrying its count**, never a blank and never a zero. Watching this live is the clearest demonstration of the rule yet: p95 is absent on each box until it has twenty samples, while the pooled row — which reaches twenty first — already has one.
- **Sortable on any column**, numbers descending on the first click: the reason to sort by p95 is to find the worst row, and making that a second click is friction on the common case. A withheld figure sorts last whichever way the column points, because it is absent rather than small.
- **Copy TSV and download CSV**, both in the order shown. Raw numbers, not formatted ones — these get pasted into a spreadsheet, where "7.5 GB" is a string. A withheld figure exports as an empty cell rather than a zero somebody would then average.
- **Three bugs found by watching it run.** The group header's span froze while the numbers beside it advanced, because `patch()` rewrote the cells and not the header. Sorting did nothing on a live table, because the live path did not subscribe to the sort and patching cannot move a row — it forces a full render now, which is fine on a click. And a span counted rows rather than moments, so a box with four metrics claimed four times as many samples as it had.
- A success notice now sits in the same slot as the error banner, and clears itself after a few seconds. A copy button with no visible effect is indistinguishable from a broken one — and when the browser refuses clipboard access, that refusal is what it says.
- `rendering.test.mjs` was updated rather than worked around: the live payload legitimately changed shape from `latest` to `summaries`, and the fixture encoded the old contract.
- Verified in a browser against a live scrape and a seeded recording: the table filled as samples arrived, p95 appeared as each group crossed twenty, sorting held across live updates, and the export matched what was on screen. No console errors, no horizontal overflow at 1264px.
- Verified: `scripts/check.sh` all green — 356 passed, 1 skipped, 25 front-end tests.

## 2026-09-13 — A3.2: charts, with the gaps left in.

- **The Charts page draws.** One chart per host metric, every target overlaid, all of them on one time axis with one crosshair — move the pointer over any chart and the rest follow, each reading its own value in its own units.
- **uPlot vendored** under `web/vendor/uplot`, byte-for-byte from jsdelivr with a README naming the version, the licence and where it came from — the same arrangement Tabler has, for the same reason: a tool that watches a private network must not need the public one to draw itself. Neither file fetches anything; the only URL in either is the project link in a banner comment.
- **A gap is a break in the line, and getting that right needed thought.** A plotting library joins across a missing x and draws a straight segment through the window nothing was collected in — a picture of data that does not exist, indistinguishable from a flat healthy stretch at exactly the moment someone is looking for the opposite. So the x-axis is the union of every target's sample times, a target missing one gets a `null` there, and **each recorded gap contributes an x of its own**, for the case where every target stopped at once and there is no timestamp in the window to hang a null on.
- Under the crosshair, a box inside its gap reads `—` rather than a number. Verified against a recording where one box lost collection for twelve seconds and the other did not: one line breaks, the other continues, and the legend says which is which.
- **Phase bands behind every chart**, one per phase rather than one per target, with a dashed boundary even where the band is invisible — the measure phase has no tint, and where it starts is exactly what a reader is looking for. Annotations shade the window they cover, coloured by severity, so a collection gap is both a break and a shaded region.
- **The baseline median is drawn as a dashed horizontal line**: "how far from normal did it get" is the question §15 gives this chart, and it cannot be answered by a line on its own.
- **Every chart carries a one-line caption saying what it answers**, because §15 says none ships without one. A metric with no caption written for it gets a generic line rather than an empty subtitle.
- **Charts are patched, not rebuilt** — the same bargain the live table struck. A canvas replaced once a second loses the crosshair and any zoom the reader set, which is most of what makes a chart worth watching while it fills. Verified by marking a canvas node and finding it still there several ticks later.
- **`/series` now returns every metric in one request**, with the phases, gaps, annotations and baseline medians a chart draws behind the lines. It used to be one request per metric for the last value alone — the recording detail was making fifteen calls to render a column.
- Verified in a browser: a seeded recording with three phases, a twelve-second gap on one box and an annotation region drew correctly; a live recording against a real exporter grew from 8 to 13 points across five seconds without rebuilding a canvas. No console errors, no horizontal overflow at 1264px.
- Verified: `scripts/check.sh` all green — 358 passed, 1 skipped, 34 front-end tests.

## 2026-09-13 — A3.3: an archive you can scan, and a quoting bug in the escaper.

- **Every row says whether it can be trusted before it is opened** — the worst annotation severity it carries, or *clean*. Scanning for the run that went wrong is what this list is for, and a bad row that looks identical to a good one until you open it makes that impossible. An `invalid` badge says in its tooltip why it matters: those numbers cannot be trusted and that recording cannot become a baseline.
- **Filters**: search across id, profile and note, plus kind, profile, status, severity and baseline. **Applied by the server**, because the list is capped — filtering the rows a page happens to hold would answer "nothing matches" for a recording two pages down, and a filter that lies is worse than no filter.
- **The header says what it is hiding**: the count against the size of the whole archive, and stated as a *match* whenever a filter is set. "2 captured" under an active filter reads as though nothing were filtered, and the next thing to get doubted is the filter.
- **The choices come from the whole archive, not the filtered page.** A filter list that empties itself as you narrow is one you cannot get back out of.
- **Two empty states, not one.** "No recordings yet" and "nothing matches what you asked for" need different words and different ways out; showing the first to someone who has just typed a filter reads as data loss. The filter bar stays visible over the second, so it can be undone.
- Baseline marking and its `invalid` gate were already in place from A2.4; this is the page that surfaces them.
- **A real bug in `escape()`, found by a test written for the search box.** It handed the string to `textContent` and read `innerHTML` back, which escapes `&`, `<` and `>` and **not** quotes — so anything containing a `"` broke out of the attribute it was written into. Nearly a hundred call sites across the front end pass ids, profile names, target names and annotation messages into attributes. It now escapes quotes too, and no longer needs a DOM to do it.
- The filter bar overflowed the page at the width it actually gets: seven controls with sensible minimums do not fit on one line. It wraps now — a filter bar on two rows beats one that makes the page scroll sideways.
- Verified in a browser: filtered by severity and by baseline, searched for a term that matches everything and one that matches nothing, and cleared back to the full list — the counts, the empty state and the disabled Clear button all followed. No horizontal overflow at 1280px, no console errors.
- Verified: `scripts/check.sh` all green — 364 passed, 1 skipped, 45 front-end tests.

## 2026-09-13 — A3.4: run series, and a band measured rather than chosen.

- **Runs group themselves into series by setup identity** (§17.2) — kind, profile, addressing mode, interval and API version. No manual tagging, which would be skipped exactly when it mattered. Change any element and the runs belong to a different series; the old history is still there, under its own identity. `GET /api/series` lists them, `GET /api/series/trend?key=…` returns one with every metric across it.
- **A new Archive › Series page**, rather than a section of a recording. A run's own page answers *what happened*; this is the only view that answers *is that better or worse than the last ten*, and a two-run diff cannot, because it has no idea what normal variation looks like. Each recording links to its series and back.
- **The band is measured from the runs before each point, never from a window containing it.** This is the decision the whole view rests on: a window that includes the point it is judging widens to swallow exactly the movement it exists to detect — a jump twice the size of normal noise drags the median and the IQR up with it and lands comfortably inside its own band. It is drawn as a step, holding from each run until the next one re-measures it, because a smooth ribbon would show a band that was never in force at any moment on the chart.
- **A band the run count cannot support is not drawn**, the sample-count rule applied to runs instead of samples. Below five usable runs there is no band, so no point can be outside one — it fails safe. The list of series says how many more runs each one needs, rather than making that discoverable one click at a time.
- **The floor on band width is wider between runs than within one — 5% against 2%.** Caught by running the page against a seeded history: the within-window floor describes how far a *sample* strays from its own median, and it flagged two perfectly ordinary runs. Two runs of the same setup differ by more than two percent as a matter of course, and a floor that denies it turns an ordinary Tuesday into a finding.
- **An invalid run is drawn hollow, kept out of the band, and breaks both lines through it.** That it failed validity is part of the history; its numbers are not, and a line drawn through it — value or baseline-phase — is a claim the data does not support. A run whose sample count could not support a median breaks the line too, rather than being drawn as a zero.
- **Environment drift, from the baseline phase**, as a second line: what the environment was doing before the run asked it for anything. A metric that crept up alongside its own idle is not an application regression. Absent for an observation-only series, where baseline and settle collapse into one window and there is no separate "before".
- **The x axis is real time, not run index.** Runs are not evenly spaced, and a chart that pretended they were would hide the fortnight nobody recorded anything in — often the explanation for the step everyone is staring at.
- **The word *regression* is not used.** §17.4 wants the move, the sample count and validity together before that verdict is earned; this step computes only the geometry, and the page says "outside, the bad way" and no more. A view that called the first condition by the name of the verdict would be wrong on precisely the runs the other two exist to catch.
- The runs table shows the metric that moved rather than the one that sorts first — the reason to open a run is almost always the metric that left its band, and a column of `conn.established` is not what anyone came for.
- Verified in a browser against a ten-run history with one invalid run and a CPU jump: the band appears at exactly the sixth run and nowhere earlier, the invalid run is hollow with both lines broken around it, and a two-run series says why it has no band instead of drawing bare points. No console errors, no horizontal overflow at 1280px.
- Verified: `scripts/check.sh` all green — 388 passed, 1 skipped, 67 front-end tests.

## 2026-09-13 — B1.1: standalone engine mock target

- Implement metrix-mock as a standalone HTTP/1.1 and HTTP/2 prior-knowledge server with strict JSON configuration and an ephemeral-port option.
- Add seeded fixed, normal, lognormal, and bimodal latency; HTTP errors, disconnects, and timeouts; linear slow start; a token-bucket capacity ceiling; and independent request/connection limits.
- Expose planned response delay and outcome headers, cancel active connections on shutdown, and document timing, clipping, burst, and transport-rejection semantics with examples/mock.json.
- Add distribution, validation, CLI, and real-socket tests covering keep-alive, multiplexing, fault recovery, limit release, and cancellation. Mark B1.1 complete; B1.2 remains next.
- Restore the existing synthetic NDJSON contract fixture and exempt that exact file from the runtime-data ignore rule, fixing two schema tests in fresh checkouts.
- Validation: bash scripts/check.sh passes (19 mock tests, 384 API tests, 67 front-end tests; 5 live-host tests skipped). Standalone binary smoke test returns HTTP 200 with the configured default 10ms delay.

## 2026-09-13 — A page explanation, behind a button.

- **The Series subtitle was confusing** — "the same setup over time, against the band its own runs measure" packed the mechanism into the one line that should carry the question. It now asks it: *is a setup getting better or worse, run by run?*
- **A help button beside the page title**, opening the long version: how runs group themselves, what the band is and why it is measured from the runs before each point, what is drawn hollow and left out of it, and what the page deliberately will not call a regression. The short answer belongs in the subtitle; the long one is read once, by someone who has just arrived, and is in the way every time after that.
- A native `<dialog>`, which brings the focus trap and the inert background for nothing. Dismissed by Escape, by the close button, or by clicking away. Clicking away needs script because the backdrop is not an element — a click on it arrives with the dialog as its target, exactly like a click on the dialog's own padding, and only the coordinates tell them apart.
- **Escape is handled rather than assumed.** A modal `<dialog>` is supposed to close itself on Escape and mostly does; it was found not to in one embedded browser, where the keydown arrived trusted, no `cancel` event fired, and the dialog stayed open with no keyboard way out. Three lines to not depend on it.
- The button is drawn only on pages that have written an explanation. One that opens an empty dialog teaches people the help button is not worth pressing.
- Leaving the page closes it, rather than leaving an explanation of the previous page floating over the next one.
- Verified in a browser: opened from the button, dismissed all three ways, hidden on Config, no console errors, no horizontal overflow at 1280px.
- Verified: `scripts/check.sh` all green — 388 passed, 1 skipped, 76 front-end tests.

## 2026-09-13 — A3.5: the word regression, and the three things it costs.

- **A metric is flagged only when all three hold** (§17.4): it moved beyond the band its own history supports, **and** its sample count supports the claim, **and** the run carries no `invalid` note. Written out as three separate conditions in `stats/trend.py` rather than folded into one boolean, because any one of them alone produces false positives at a rate that teaches people to ignore the flag — which costs more than never having flagged anything.
- **A flag in the good direction is a *change*, not a regression.** Worth reading — an unexplained improvement usually means the test stopped doing part of the work — but not something to fail a build on, so the two are named apart.
- **Not being able to check is a first-class answer and never a pass.** Too short a history, too few samples, or a run already marked invalid are three different reasons the check has nothing to say, and the verdict names the metric and the reason for each. A pipeline that treats *could not check* as *passed* gets one useful signal out of this endpoint, the wrong one, and it gets it on the runs that went most wrong.
- **`GET /api/series/verdict?key=…`** is the machine-readable half: the latest run, or one named with `&recording=…`. `status` is `regressed`, `changed`, `ok` or `unknown`; fail on `regressed`. The trend response carries the same document, so the page and the pipeline read one judgement rather than two implementations of it — there is a test that asserts the two are byte-identical.
- **The series list says how each series' latest run stands**, so the archive can be scanned for the one that moved instead of opened one series at a time. `unknown` is drawn as its own state rather than as a quiet pass.
- **The series page leads with the verdict**: what moved, what normal was, the size of the move, and — underneath — every metric that could not be checked, with the reason. "Nothing moved" is only reassuring if something was actually looked at, so the card says how many metrics it checked.
- The runs table now uses the same word the badge and the CI status use. Three vocabularies for one judgement is how a page and a pipeline come to look as though they disagree.
- **A formatting bug the verdict table made obvious: a change in a percentage is not a percentage.** CPU moving from 21% to 57% moved 36 percentage *points*, and the cell read `+35.7% (+167.8%)` — two meanings of one symbol side by side. There is now a `metricChange` formatter that says `+35.7 pts (+167.8% of normal)`, and the baseline comparison card, which had the same cell, uses it too.
- Verified in a browser against a ten-run history: the regression named two metrics with the count behind each, the third was listed as checked and within band, a run held out for its invalid note said so rather than passing, and a two-run series reported `unknown` with all three of its metrics named as *not enough history yet*. No console errors, nothing clipped, no horizontal overflow at 1280px.
- Verified: `scripts/check.sh` all green — 414 passed, 1 skipped, 87 front-end tests.

## 2026-09-13 — B1.2: fixed-rate scheduler and HTTP transport

- Make metrix-engine --plan executable for one static call, one target, fixed open load and zero phases; reject unsupported later-step semantics before network I/O and keep bundle file references inside its root.
- Schedule absolute arrivals with reusable request slots, bounded concurrency and connection admission, explicit late/cap skips, deadline-bounded drain and cancellation. Use a dedicated native-sleep clock with coalesced atomic wake-ups to avoid coarse Windows timers reducing the offered load.
- Add direct hyper HTTP/1.1 pooling and HTTP/2 multiplexing, verified rustls TLS/ALPN, whole-request timeouts, body draining and connection recovery without retries. Add targets.http_version and regenerate its shared schema.
- Add examples/plans/mock-fixed, end-of-run count/drift diagnostics, and tests for validation, request construction, scheduling, faults, TLS and cancellation. Run a copied binary at 75 RPS for 30 seconds from a temporary directory containing only the binary and bundle; require at least 98% of offered arrivals and report every skip.
- Mark B1.2 complete; B1.3 aggregation is next. No NDJSON, percentile, phase or SLO implementation is claimed by this step.
- Validation: bash scripts/check.sh passes, including 20 new engine tests, the standalone 75 RPS/30s acceptance, 384 API tests, 67 front-end tests and schema drift checks (5 live-host tests skipped).

## 2026-09-13 — A3.6: comparison mode, and the one arithmetic that makes an aggregate mean anything.

- **Pick runs from a series and read them side by side** (§14.4): every run's figures in its own column, a delta against the earliest of them, and one line per run on a shared axis (§17.5). Two runs up to six — past six an overlay has more lines than there are colours anyone can tell apart, and the button says so before the request rather than after it is refused.
- **Distributions merge; percentiles do not.** This is the whole of it. The **merged** column is every reading from all the runs in one distribution, summarised once — not an average of the columns beside it, because the mean of several medians is a number no run ever produced and the mean of several p95s has no interpretation at all. The demo makes it visible: three runs with p95s of 23.6%, 22.7% and 59.8% merge to **59.2%**, where averaging would have said 35.4% — a value nothing measured.
- **It is also the cheapest route past a short window** (design-engine 12.2). Runs that are each too short to support a p95 merge into a set that is, and the count travelling with that figure is a real count of real readings rather than a borrowed one. Tested directly: three eight-sample windows, none supporting a p95, merging into 24 that do.
- **Merging is refused across runs of different setups**, and the page names which part of the identity differs rather than dropping the column silently. Their combined distribution describes nothing that exists while carrying a sample count that would make it look authoritative. The columns and the overlay stay, because reading two setups against each other deliberately is the sanctioned way to compare across a setup change — it is the pooling that is wrong, not the looking.
- **One phase at a time, across runs**: baseline against baseline is environment drift, measure against measure is the actual question, settle against settle says whether recovery is degrading. Only phases every selected run recorded are offered; a phase half the group lacks makes columns empty for no stated reason. The window is in the hash, so a link to "settle against settle" reopens as that.
- The overlay draws **seconds since each run started**, not wall clock — that is what makes two runs of the same plan lie on top of each other — and pools each run's boxes into one line, because six runs across three boxes is eighteen lines and not a chart.
- Deltas are suppressed entirely wherever either side's count cannot support the claim, and a delta inside the reference run's own spread says so. A table is exactly where a spurious percentage gets quoted, and the caveat never travels with the quote.
- **Ticking a run patches the page rather than rebuilding it.** A full render replaces the checkbox under the pointer and takes focus off it, which is precisely wrong for the one interaction here that anyone does several times in a row. Same bargain the live table and the charts already make.
- Verified in a browser: three runs of one setup merged and drew three overlays; switching to the baseline phase showed the third run was already hot before its load started, which is environment drift rather than a regression; a mixed-profile pair lost its merged column and said why. No console errors, no horizontal overflow at 1280px.
- Verified: `scripts/check.sh` all green — 436 passed, 1 skipped, 108 front-end tests.

## 2026-09-13 — A3.7: a purge you can read, three ways out, and the end of Track A3.

- **The purge is a button and never a policy.** A retention rule runs on a schedule and is therefore certain to delete the evidence for the one run somebody needed, on the day they needed it, with nobody present to notice. This only happens because a person asked.
- **It drops the bulk and costs no figure.** The schema already made that possible: per-request events and error bodies live as files under `runs/<id>/` rather than in SQLite, so a purge is a directory delete rather than a transaction that then has to vacuum a database. Host samples are not bulk — they are the rolled-up series this tool exists to read — so a purged recording keeps every number, every note, and its place in every trend. There is a test asserting the summaries are byte-identical across a purge, and another that a purged series still draws its trend.
- **The confirmation names a measured size and says what survives.** Not an estimate: a dialog that says "frees about 2 GB" and frees four kilobytes teaches people to stop reading dialogs, which is expensive on the one that mattered. And it lists what stays as well as what goes, because a dialog enumerating only losses reads as though everything is being lost.
- **Purged-and-empty is recorded as distinct from never-written.** A stamp goes on either way. One is a decision somebody made and the other is a run with no engine attached, and a page that conflates them is lying about evidence.
- **Three exports, because they have three readers.** JSON is everything including the samples. CSV comes two ways — the *series* long rather than wide, since the collectors report whatever a host exposes; the *summary* with `n` as a column rather than a footnote, because a spreadsheet is exactly where a figure gets separated from its caveat. An unsupported figure is an empty cell, never a zero.
- **The static HTML report is genuinely self-contained**: no stylesheet link, no script tag, no external URL, charts as inline SVG drawn here. Its reader is the only one who cannot open the recording and ask a follow-up, so it carries its own explanations and **leads with whether the numbers can be trusted** — an `invalid` note found after the figures are quoted arrived too late. 12 KB for a two-box, three-metric, forty-second recording.
- **Gaps break the polyline in the report too.** That drawing rule now holds in all three places this tool draws a line, and the SVG version is tested by counting polylines: a gapped recording draws more of them than a whole one.
- Fixed a column clipped since A3.5: the baseline comparison's Change cell grew when `metricChange` added "of normal", and "+35.5 pts (+165.9% of normal)" no longer fits 20%.
- Verified in a browser: the confirmation read back exactly as written, the purge freed 2.0 MB and left the figures untouched, the report rendered with three SVG charts and zero external elements, and the footer recorded the purge. No console errors, no clipped cells, no horizontal overflow at 1280px.
- Verified: `scripts/check.sh` all green — 466 passed, 1 skipped, 113 front-end tests.
- **Track A3 is complete.** With no engine in existence, Metrix is a working host-observation tool: discover an environment, watch it, compare against a baseline, trend a series, flag a regression, compare runs, and take the answer somewhere else. A4 is the single engine-integration milestone.

## 2026-09-13 — B1.3: worker metrics and interval aggregation

- Add exclusive logical worker partitions with preallocated, non-resizing HDR histograms and fixed-index counters. Round-robin admissions retain their partition through terminal completion, using the existing scheduler owner without per-request locks or maps.
- Record chain duration, sent-to-terminal request total, TTFB and finished-send drift with individual sample counts; track statuses, transport causes, payload bytes, connection creation/reuse and cancellation separately. Preserve exact extrema/means alongside HDR V2 base64 serialization, and explicitly count samples above the one-hour range without clipping.
- Merge/reset partitions every 250ms into interval and cumulative accumulators; flush the final partial window after drain or cancellation. Retain bounded report state and provide optional bounded snapshot delivery with try_send and dropped-window accounting.
- Add tests for uneven-population merging, serialization, empty/zero/overflow samples, failure/cancellation populations, interval conservation, final drain and stalled/closed consumers. Extend existing load tests to reconcile metrics with execution counts. Mark B1.3 complete; B1.4 NDJSON output is next.
- Validation: bash scripts/check.sh passes, including six new aggregation/snapshot tests and the existing standalone 75 RPS/30s acceptance; 384 API tests and 67 front-end tests pass (5 live-host tests skipped). Shared schemas are unchanged.

## 2026-09-13 — B1.4: bounded NDJSON output

- Shipped bounded NDJSON --summary (stdout by default) and opt-in --events streams with shared lifecycle, measure/drain boundaries, 250ms interval histograms, request identifiers, fixed transport errors and cancellation records.
- Added deterministic --sample-rate/--seed selection, separate intentional omission/backpressure counts, events_dropped annotations on healthy streams and a 500ms output shutdown bound. New output files are created exclusively; streams cannot share a destination.
- Added required run identity, timestamp, engine version and SHA-256 digest of exact loaded bundle documents. Request capture excludes URLs, headers, query values, bodies and raw transport errors. Frozen v1 numeric self-metric fields are explicitly annotated unavailable pending their collectors.
- Added standalone stream, conservation, sampling, redaction, failure/cancellation, overwrite protection and unread-pipe regressions. scripts/check.sh validates genuinely emitted NDJSON against the frozen event schema in the full suite.
- B1.4 complete; B1.5 self-metrics is next. Work remains isolated on engine in its worktree.
- Validation: full bash scripts/check.sh passed, including emitted-stream schema validation, 384 Python tests and 67 front-end tests; scoped engine checks passed again after the final queue/completion adjustments.

## 2026-09-13 — B1.5: live scheduler self-metrics

- Shipped live send-schedule drift using preallocated atomic request-slot state, including unfinished and cancelled sends. Completion latency no longer delays or duplicates drift samples; final diagnostics distinguish observed sends from finished sends.
- Added per-snapshot in-flight and pre-send queue gauges, with zero gauges after drain/cancellation. Queue depth covers admitted connection/readiness waits before the Hyper send call; Hyper's internal HTTP/2 peer-capacity queue is explicitly outside that observation boundary.
- Added observed 250ms timer-wake lateness, interval maxima/sample counts and final scheduler-lag diagnostics. Companion generator_self_metrics annotations distinguish missing observations from measured zero values without changing the frozen schema. CPU/RSS/FD probes remain explicitly unavailable.
- Added regressions for live drift before slow responses, sample conservation, cancellation, stalled TLS/pre-send timeouts, HTTP/2 peer-capacity limits, executor stalls and emitted metric/sample-count pairing. Reused exact distribution maxima without histogram encoding for scalar drift output.
- B1.5 and B1 complete; B2.1 phase timeline is next. Changes remain isolated on engine in its worktree.
- Validation: full bash scripts/check.sh passed, including Rust fmt/clippy/tests, the copied binary 75 RPS/30s acceptance run, emitted NDJSON schema validation, 384 Python tests and 67 front-end regressions.

## 2026-09-13 — Delete, alongside purge, because they lose different things.

- **A recording can now be deleted outright**, selected from the archive and removed in one action. Purging answers *we are done with the request-level evidence*; deleting answers *this should not be in the history at all* — a bad run, a misconfigured target, an experiment nobody is interested in any more.
- **The two are kept apart rather than folded into one control with a checkbox.** A purge cannot cost you a number and a delete takes all of them, and a person reaching for the first should never be one misread away from the second. The button and the dialog both say which is which: *a purge keeps the figures and drops only the request-level data; this keeps nothing.*
- **A baseline in the selection is called out by id.** Deleting one leaves every later recording in its series with nothing to be compared against — a consequence that lands on a page the person deleting is not looking at.
- **A selection cannot outlive the rows it was made over.** It is cleared whenever the list is reloaded, so narrowing a filter can never turn *delete the three I picked* into *delete three I can no longer see*.
- **Cascade deletion is asserted, not assumed.** SQLite honours `ON DELETE CASCADE` only when `PRAGMA foreign_keys` is on — off by default — so a test counts rows in all six child tables before and after. Orphaned samples would be invisible, never read again, and would grow the database forever. The pinned inventory is deliberately not cascaded: one snapshot is commonly shared by every run against an environment that did not change, and it outlives them.
- Fixed the checkbox column overflowing every row it was in: the `.form-check` wrapper carries padding for a label that is not there. It is now a bare input with an `aria-label`, the same shape the series page already uses to pick runs.
- Verified in a browser: the dialog named the baseline by id, declining deleted nothing, confirming removed both recordings and their files, the archive count fell 12 → 10, the deleted ids 404, and the button disabled itself again. No clipped cells, no horizontal overflow at 1280px, no console errors.
- Verified: `scripts/check.sh` all green — 470 passed, 1 skipped, 122 front-end tests.

## 2026-09-13 — A4.1: the bundle, and a hash both sides agree on.

- **`GET /api/plans/{name}/bundle?profile=X` assembles the directory the engine runs**, as a zip that unpacks to exactly it. One of the two things that cross the boundary (contract C1), and the point of exporting is that what the UI runs and what a person runs by hand are the same artifact.
- **A stored plan is the mix and the calls; `targets.json` is never stored with it.** It is written from the named profile at assembly time, because the profile is what knows how to resolve a hostname into the boxes actually behind it and the engine knows nothing about profiles. One plan against two environments came out differing in exactly one file, which is the whole design in one observation.
- **The bytes are deterministic, and that is correctness rather than tidiness.** The engine identifies a plan by a SHA-256 over the bytes it read, and that hash is part of a run's series identity — so a reordered object or a zip carrying the clock would move it, and every run would start a fresh series with no history. Documents are written one way (sorted keys, two-space indent, trailing newline, UTF-8) and zip entries take a fixed timestamp. A test reorders the stored mix and asserts the hash does not move; another asserts a real change does.
- **The hash rule is implemented twice, in two languages, so it is checked against the thing itself.** `scripts/check-bundle-contract.py` assembles a bundle, hands it to the real binary, and compares the API's hash with the `plan_hash` the engine reports. It points at a dead address on purpose — the engine emits `run_started` before it tries to connect — so it needs no mock and no network. Verified it actually catches drift by narrowing the length framing from eight bytes to four and watching it fail.
- **A `targets.json` inside a stored plan is reported and then ignored**, rather than obeyed or refused. Obeying it would give two sources of truth; refusing it would break the export-edit-reimport loop the export exists for. A file that is silently ignored is a file somebody will edit expecting it to matter.
- Plans are validated on the way in against the schemas generated from the engine's own Rust types, so a bundle this produces cannot be one the engine fails to parse. A call reference that leaves the plan directory is refused — a plan is a unit of transfer, and one that reads a file outside itself is not. Parse errors carry line and column and never the offending text, because a call document holds headers and query values.
- A plan that exists but does not validate is a 422 rather than a 404: *typo in the name* and *fix your mix* are different problems. Broken plans stay in the list with their reason, for the same reason broken profiles do.
- Verified end to end with the real binary: downloaded the zip, unpacked it, ran `metrix-engine --plan` on it, and got back the same hash the `X-Metrix-Plan-Hash` header carried. Worth knowing that a bundle for a multi-box profile exports correctly but will not *run* until multi-target sequencing lands — the engine refuses it by name, before traffic, which is the division of responsibility working rather than a gap in the export. `&target=app-1` exports one box and runs today.
- Verified: `scripts/check.sh` all green — 498 passed, 1 skipped, 122 front-end tests.

## 2026-09-13 — A4.2: the engine runs, and both clocks become one.

- **The API spawns the engine, reads its NDJSON, and stores what it says.** Verified end to end against the real binary and the real mock: bundle assembled, 60 RPS for eight seconds, 475 responses, exit 0, 34 windows ingested — and the host observer writing on the same clock throughout.
- **Resolving the two clocks is the whole step.** The engine counts from its own start; the observer has been collecting since before it was spawned, because that is what a baseline is. The offset comes from the wall-clock instant each side says its own clock started, not from when a record arrived — arrival is delayed by pipe buffering and by whatever the reader was busy with, and folding that in would put the API's latency inside a measurement. In the demo the engine reported `279..8049` and it was stored as `2338..10108`: a +2059ms shift matching the two-second baseline exactly. The engine's original `t_ms` is stored beside the shifted one so the alignment can be audited rather than believed.
- **Load metrics join the host ones in the existing series endpoint**, so the charts draw both with no new drawing code and no second axis. Seven charts on one page: `cpu.busy` and `mem.used_bytes` from the observer, `load.achieved_rate`, `target_rate`, `in_flight`, `queue_depth` and `drift_ms` from the engine.
- **A shared crosshair turned out not to be a shared axis.** uPlot fits each chart to its own data, so with host data spanning 0–11s and load data 2.3–10.1s the same pixel meant a different moment on each chart — which is precisely the misreading this page exists to prevent. Every chart is now drawn over the recording's whole span. Found by looking at it, not by a test.
- **Reading the stream never stalls the engine.** Backpressure makes it drop records and annotate, so a slow reader silently costs measurement data: the summary is drained continuously into the store, stderr is drained on its own task so a large diagnostic cannot deadlock the run, and cancellation asks the engine to finish rather than killing it — a killed engine loses the record that says how the run went.
- **A bug the real run caught that the tests had blessed.** I read `self_metrics_unavailable` as covering everything the generator reports about itself, and so discarded `in_flight`, `queue_depth` and `drift_ms` — three numbers B1.5 genuinely measures. The engine's own message says which fields it means: *zero in cpu_pct, rss_bytes and open_fds means unavailable*. Those three are now dropped rather than stored as zero, because a chart drawing `cpu_pct: 0` reports a generator loafing along at nothing; the scheduler's gauges are kept, including their zeros. My tests had passed because they encoded the same wrong belief as the code.
- Histograms are stored as the engine serialized them, HDR v2 base64, because distributions merge and percentiles do not — a stored p95 could not be merged with anything. Nothing derives a percentile in SQL; the `/load` table reports exact counters and extrema and leaves merging to `stats/`.
- Engine annotations join the recording's own list with `source: engine`, so there is one place to look before quoting a number, and which side noticed is part of the answer. That needed the annotation table rebuilt, since SQLite cannot alter a CHECK.
- A chain's end-to-end row is stored apart from its steps, because a chain's duration is not the sum of its step medians and a flat list invites that sum. STRICT tables make primary key columns NOT NULL, so the chain row's empty `step` is tied to `kind` by a table CHECK rather than left as a magic value — and the read layer hands back `None`, so the sentinel never leaves the store.
- Verified: `scripts/check.sh` all green — 525 passed, 1 skipped, 125 front-end tests.

## 2026-09-13 — A4.3: the mixture becomes editable, and says what it buys.

- **The Plans page lands**: the plan library, the mixture editor, and the calls read-only beside it. Plans were reachable only through the API after A4.1; this is where they get a page.
- **A percentage is not a quantity anybody can judge, so the page converts it.** Each chain shows the iterations a second its share comes to and the requests that is over its steps — a two-step chain at 20% of 150/s is 30 iterations and 60 requests a second, and conflating the two under-counts the load by the length of the chain. A share can be typed either way and each box moves the other.
- **Every figure is computed server-side, in the same function the bundle is gated on.** The browser converts a typed rate into the share the document stores — the document has to exist before it can be submitted — and renders what it is told about everything else. `POST /api/plans/{name}/validate` answers for a document that has not been saved, so the sample-count consequence of a duration and a rate is visible while they are being chosen. A second rulebook in JavaScript would be a rulebook to drift from the first.
- **The §12.1 floor is stated before the run instead of after it**, twice, because the two fail independently: the run as a whole against 2250 requests, and each chain against its own share. checkout-mixed clears the floor at 10,125 requests while three of its six chains do not, and those three are the ones whose percentiles would have been quoted.
- **Percentages that do not total 100 are named, never renormalized** — adjusting five chains to accommodate a typo in the sixth measures a mixture nobody chose. The message says the shortfall or the excess.
- **Errors stop a run; warnings do not, and a mixture with errors still saves.** Half-finished is a normal state to leave an afternoon's work in, and an editor that refuses to save loses it. What errors stop is assembly: the bundle endpoint refuses and lists them, since the engine would reject the same document a moment later and a zip that cannot run still looks like an artifact.
- **A warning that earned its place: a step reading a variable no earlier step extracts.** Reordering two steps in the checkout chain immediately reported that `poll` now reads `{{ order_id }}` before `create` writes it — the most common way a hand-edited chain breaks and the least visible, because the run reports it as an assertion failure against the service, which starts a bug hunt in the wrong codebase. It also found one in the shipped example: `cart-add-remove` reads `{{ pid }}`, which only the `search` chain extracts.
- **The editor writes back the document it loaded** with only the fields the form owns replaced. A plan carries auth, datasets, generators, capture, SLOs and engine tuning that no control on the page shows; rebuilding the document from the inputs would delete every one of them on the first save. They are listed as carried, so that "preserved" does not read as "gone".
- **A save cannot move the plan hash.** The bytes are written by the rule the bundle is hashed under, so re-saving an unchanged plan is a no-op at the byte level rather than a rename of the series every run of it belongs to. The plan's own name is fixed for the same reason.
- **A bundle is assembled from the plan on disk**, which is the plan a run would use — so the page says when the form has moved on from it. Found by exporting an edited plan and getting the stored one back.
- The form patches its own figures rather than re-rendering: every committed field re-checks the mixture, and rebuilding the markup each time would take the focus out from under somebody tabbing through the chains. It falls back to a full render when the shape changes — a session policy that grows a pool size, a step added, a mode switched.
- Verified: `scripts/check.sh` all green — 559 passed, 1 skipped, 142 front-end tests.
- **Open question for the engine track**: `PERCENT_EPSILON` is 0.01, but its comment says it exists so that `16.67 x 6` is not a validation failure — that totals 100.02 and is rejected. The API matches the constant rather than the comment, and a test pins that, so the two sides cannot disagree; which of the two is right is a decision for `metrix-plan`.
## 2026-09-13 — Engine B2.1

- Implemented baseline, optional warmup, measure, drain and settle with absolute traffic schedules and boundary snapshots. Idle phases emit summaries without sending requests; drain retains each admitted request's original timeout and closes the pool before settle.
- Kept request events and samples tied to admission phase, including late warmup completions and cancellations. Separate warmup accumulators and summaries prevent measured histograms and counters from including warmup work. Moved the execution future onto the heap for the Windows CLI stack.
- Updated the engine design and checklist; B2.1 is complete and B2.2 is next. Added phase, cancellation, timeout, default and overflow coverage plus schema validation of real phase streams.
- Validation: bash scripts/check.sh passed, including Rust tests and the 75 RPS standalone acceptance run, 384 API tests (5 skipped), 67 front-end tests, schema drift and emitted NDJSON contract checks.

## 2026-09-13 — Engine B2.2

- Added load-percentile support in Rust stats/: 10 tail samples for crude support, 100 for stable support, with p99.9 suppressed below 10,000 samples. Every percentile carries its histogram count, overflow count, support, nullable value and binomial order-statistic 95% interval expanded to HDR bucket bounds. Overflow and unrepresentable counts suppress claims.
- Warn before traffic when planned measured volume is below 2,250. Emit final measured chain, request-total and TTFB percentiles through the frozen annotation contract; calculations run on the writer thread. Warmup and cancellation samples are excluded, event sampling leaves counts intact, and interrupted reports are labelled partial.
- Updated design and checklist: B2.2 complete, B2.3 next. Validation: bash scripts/check.sh passed, including exact interval coverage and support-boundary tests, the standalone 75 RPS/30s percentile report, 384 API tests (5 skipped), 67 front-end tests and frozen-schema checks of genuine output.

## 2026-09-13 — Engine B2.3

- Added schedule-corrected chain duration, request total and TTFB beside raw latency. Each terminal sample has one corrected counterpart measured from planned arrival, including admission or send delay; skipped arrivals stay explicit shortfalls and cancellation creates no latency samples.
- Retained corrected HDR histograms through worker merges, phase boundaries and drain. Emit interval histograms with overflow counts and final percentiles with the existing sample-support and confidence-interval rules through the frozen annotation contract. Warmup remains separate and serialization stays on writer threads.
- Updated design and checklist: B2.3 complete, B2.4 next. Validation: bash scripts/check.sh passed, including HTTP/1.1 and HTTP/2 delay tests, correction arithmetic, HDR round-trip, overflow and count conservation, the standalone 75 RPS/30s run, 384 API tests (5 skipped), 67 front-end tests and emitted-stream schema checks.

## 2026-09-13 — Engine B2.4

- Added concurrency-cap, offered-rate, send-drift, and low-sample-count annotations. Detectors account for traffic phases, partial runs, cap occupancy, and actual admitted sends; percentile suppression uses the shared stats support rules.
- Added validated bundle settings for rate tolerance and send-drift thresholds, and regenerated the compatible mix schema. Annotation generation stays on the bounded output writer and preserves the frozen NDJSON contract.
- Completed B2.4; B2.5 calibration and headroom is next.
- Validation: full `bash scripts/check.sh` passed, including Rust checks and tests, standalone execution, 384 API tests (5 skipped), 67 frontend tests, generated schemas, and emitted NDJSON validation.

## 2026-09-13 — Engine B2.5

- Added `metrix-engine --plan bundle/ --calibrate`, which stores a hardware- and shape-bound `machine-profile.json` in the bundle. It measures null-executor and loopback echo ceilings at one and configured worker counts, including a local TLS path for TLS-shaped plans.
- Added bundle-local headroom preflight. Matching profiles populate run metadata and summary health; demand above 90% of the conservative loopback ceiling is refused before output or target setup unless `engine.allow_generator_limited` explicitly permits an invalid annotated run.
- Completed B2.5; B2.6 run metadata is next.
- Validation: full `bash scripts/check.sh` passed, including Rust checks and tests, standalone execution, API and frontend tests, generated schemas, and emitted NDJSON validation.

## 2026-09-13 — Engine B2.6

- Completed standalone run identity: `run_started` now has integration coverage for its plan hash, engine version, explicit CLI seed, and optional calibrated machine-profile id without changing the frozen NDJSON schema.
- Clarified which reproducibility metadata the engine owns and which remains API recording metadata.
- Completed B2.6; B3.1 separate call resolution is next.
- Validation: full `bash scripts/check.sh` passed, including Rust checks and tests, standalone execution, API and frontend tests, generated schemas, and emitted NDJSON validation.

## 2026-09-13 — The notes badge counts warnings, not notes.

- **A row in the archive said `warn ×139` about a run carrying eleven warnings.** The badge paired the worst severity with the total count, which was harmless while a recording carried three notes and became a lie the moment the B2 engine started emitting a routine info annotation per interval: a hundred and twenty-eight ordinary observations, drawn in the colour that means act on this.
- It now counts the severity it names. The rest of the breakdown moved to the title, which is where a figure that is context rather than a finding belongs.
- Found by merging the engine track and looking at the first real run through it, not by a test — the test that existed asserted the wrong number, because it was written when three notes and three warnings were the same thing.

## 2026-09-13 — A recording id that is actually unique.

- **`new_id` had sixteen bits of randomness behind a timestamp accurate to the second.** That is the whole of what separates two recordings started inside one — and a sweep starts several at once. At 16 bits, two hundred ids in the same second collide about a third of the time.
- Found because it happened: `scripts/check.sh` failed on `UNIQUE constraint failed: recording.id` after the B2 merge, in a test that had passed a minute earlier. `recording.id` is a primary key, so a collision is an IntegrityError where a run should have started.
- Widened to forty bits and pinned with a bound rather than by generating ids and hoping — a probabilistic test of a probabilistic property fails on the unlucky run and passes on the next, which is the same mistake one level up. The docstring no longer claims uniqueness it cannot provide: the suffix is random, not reserved against the store, and small enough to never be seen is not the same as impossible.

## 2026-09-13 — A4.4: a plan from a description of the service.

- **`POST /api/plans/generate` turns a service description into a draft plan**, from any of five sources: an OpenAPI 3 document, a WSDL, a HAR capture, an access log, or a bare list of routes. One call per operation, parameters filled from the schema's own examples and types, assertions from the codes it declares, and a starter mixture that clears the sample-count floor so the thing runs as it stands.
- **There is no model in it.** Generation is deterministic — the same document gives the same plan down to the bytes, which is what makes a diff between two generated plans mean the service changed rather than that a sampler rolled differently. The one place order could have leaked in, an enumeration's declared order, is sorted.
- **The mechanical half only.** Weights, chains, and what a meaningful assertion looks like beyond a status code are left to whoever reads it. **No chain is ever inferred**: every generated chain is one step long, because guessing that one call feeds another is unreliable in exactly the cases that matter, and a wrong chain runs cleanly while testing a flow the service does not have.
- **Nothing is copied out of a capture except the shape of the request.** A HAR holds session cookies, bearer tokens and whatever was typed into a login form, and a plan is a file that ends up in a repository. Method, path and status are read; bodies and headers are not, and the todos say how many were left behind. A test asserts that a capture containing a password produces a plan that does not contain it.
- **The document is pasted, never fetched.** No URL form, and a remote `$ref` inside a document is left where it is. A control plane that retrieves whatever address it is handed is a request forwarder inside the network it exists to observe, which is not what somebody asked for when they asked for a skeleton. A WSDL declaring its own entities is declined rather than parsed carefully.
- **The one guess a traffic source makes is which path segments are identifiers**, because otherwise a week of logs becomes four thousand calls that are one operation. It is named in the todos rather than made quietly: a service whose routes really are numeric should see the guess rather than find it later.
- **A draft is marked by a `draft.json` sidecar, not a field in `mix.json`.** That document's shape is the engine's and the plan hash covers its bytes; a control-plane review state has no business in a schema the load generator parses, or in the identity of a run. It warns rather than refuses — a skeleton that runs is the point — and clearing it is its own action, because editing one percentage is not a review.
- **Regenerating replaces the calls and leaves the mixture alone**, which is the whole reason the two are separate documents: the mechanical half goes stale when the service changes and the tuned half does not. Verified end to end in the browser — a plan generated, reviewed, tuned to 80/10/10 at 300/s, then regenerated against a grown description: one new call, and the weights exactly where they were left. A regeneration that would orphan a step is refused rather than written.
- **A bug the browser caught that the tests could not**: the editor held a hand-assembled copy of the plan response, so the draft marker never reached the view that draws it. The copy is gone — the editor keeps the response — and the class of bug goes with it.
- Verified: `scripts/check.sh` all green — 602 passed, 1 skipped, 151 front-end tests.

## 2026-09-13 — A4.5: which box is the odd one out.

- **The sweep view lands**: one recording's boxes ranked against each other on every metric, over any phase the recording carries. A recording covering more than one box already *is* a sweep — the targets, their phases and their attributes are stored per box — so nothing had to be tagged or grouped for this to exist, and it reads a watched environment and a load run the same way.
- **The reference is the sweep's own spread, not its history.** §17.4's measured band turned sideways: the middle of the other boxes is normal, and twice their interquartile range is how far from it still counts as ordinary.
- **Every box is judged against the others and never against a set containing itself.** The trend view refuses to measure a band over a window holding the point it judges, because such a window widens to swallow the movement it exists to detect; one slow container inflating the spread it is then compared against is the same mistake with the axes swapped. A test proves the exclusion is doing the work: with two boxes at twice the rest, a band measured over all of them hides both, and the leave-one-out finds both. Each row carries its own band, and the bar behind it draws that one.
- **Outside the band is not the verdict**, kept apart in the data and on the page. A box carrying an `invalid` note is outside and not flagged, is kept out of everybody else's band — a reading nobody trusts cannot define normal — and a note naming no target belongs to the whole recording, so it covers every box in it rather than leaving each one judging the rest.
- **A sweep too small to describe its own spread is ranked and not judged.** An interquartile range over three numbers is not a description of spread. The ordering, the attributes and the baselines are still shown, and every unjudged row says which of the three conditions it missed, because a blank in a ranked table reads as a pass.
- **The third band floor gets its own number and its own reason.** 2% is how far a sample strays from its window's median, 5% how far one run's median strays from the last one's, and 5% again here — how far one box strays from its siblings. Same number as the trend by coincidence of judgment, not by inheritance.
- **The attributes sit beside the ranking** because that is where the answer usually is. In the demo the flagged task is the one on `sha256:4b8a02` and `shop-api:46` while its six siblings are on `:47` — visible in its row without opening anything.
- **The baseline column catches the phantom.** A box that was already at 44% before the traffic started looks exactly like one that buckled under it. In the demo that box ranks sixth of seven on the measured window and is not flagged; its baseline is what says why it was near the top.
- **The history is asked only about boxes something was flagged on**, and each earlier sweep is re-ranked by the same rule, so "flagged in 4 of 4" means what it says. A box no earlier sweep carried — the normal case for ephemeral tasks — says so rather than drawing an empty trend.
- **Bars are drawn over what the sweep covers rather than from zero.** A zero-based bar answers how big a number is, which the number beside it already answers exactly; the question here is how far a box is from the others, so the band is the reference and the origin is not.
- Verified: `scripts/check.sh` all green — 625 passed, 1 skipped, 164 front-end tests.
- **Every item on the A checklist is now ticked, and A4's own "done when" is still not met**: nothing launches a run from the browser. `run_engine` exists and was proved end to end in A4.2, but no route calls it. That is the gap between a finished checklist and a finished milestone, and it is recorded here rather than quietly ticked past.

## 2026-09-13 — A run, launched from the browser.

- **`POST /api/recordings` takes a plan.** Naming one alongside the profile makes the recording a load run: the observer starts watching, the bundle is assembled from the plan on disk and the profile as it resolves *now*, and the engine is spawned against it. One route, because a load run is an observation with traffic attached — the same recorder, the same collectors, the same live view, the same Stop button. The difference is `kind`, which keeps an environment watched at rest out of the same series as the same environment under load.
- **The order is the design.** The plan is refused before anything is opened if it cannot run, and the engine binary is looked for before a row exists: a recording created for a run that never started is a recording somebody has to explain later. The bundle is assembled *after* the observer starts, from the profile the observer actually resolved, so the boxes traffic goes to are the boxes the recording is about.
- **A run ends when the traffic does.** The observer would happily keep collecting, so the engine's exit closes the recording, and Stop asks the engine to finish rather than pulling the collectors out from under it. Both can happen at once — an engine finishing and a person pressing Stop race by nature — and whichever arrives first does the work. Finding that took a test: the first version stopped the recording twice and the second attempt ran against a closed database.
- **The engine's phase timeline becomes the recording's.** `phase_changed` now lands in the `phase` table, shifted onto the recording's clock and written against the boxes being watched rather than the engine's load target — traffic goes to one socket and statistics come from another. Without this a launched run had one undivided window, and the baseline column, the phase bands and the drift comparison all had nothing to read.
- **What produced a run is pinned from `run_started`**: plan name, plan hash, engine version, seed. The hash most of all — it is what a run is filed under, and without it a stored run cannot be told apart from a run of a plan that has since been edited. The recording page shows it, linked back to the plan.
- **A run with nothing to watch is still a run.** Measuring somebody else's service is ordinary, so a load run against a profile with no collector is allowed, pinned to the boxes it sends to, and annotated to say the host statistics are absent rather than failed. An observation of the same profile is still refused: watching nothing is not a measurement. The live table says which case it is instead of sitting empty.
- **Verified end to end against the real binary**: one POST, and a finished `load` recording carrying its plan hash, engine version, exit 0, all five phases on the recording's clock (baseline 44–2071ms through settle 10126–12156ms), and its chain and step rows — then the same thing again through the browser's Run button.
- Verified: `scripts/check.sh` all green — 644 passed, 1 skipped, 168 front-end tests.
- **A4's "done when" is met**: a run launched from the browser shows load and host metrics against one clock. The A track is complete.

## 2026-09-14 — Calls, chains, and the mixture. The B3 track.

- **A step is judged by what came back, not only by whether something did.** `assert`
  says what the answer had to look like and the first assertion that does not hold is
  reported by its index in the call — declarative and enumerable, because an
  expression language turns a failed assertion into a debugging session about the
  expression. A failed assertion is not a transport failure: the request happened and
  the service answered, and the two are counted apart for the same reason aborted
  chains are counted apart from failed requests. That is also what makes a
  deliberately-failing chain work, where a step asserting 401 passes when it gets one.
- **`repeat_until` that gives up is a step that failed.** A poll that exhausted its
  attempts has not seen the job finish, and counting that as a success reports a
  service completing nothing as healthy. The waiting between attempts stays out of
  request latency and lands in the chain's end-to-end duration.
- **Writing that test found a bug in capture**: a polling step never kept the body it
  polled, because capture was decided by the call and the selector belongs to the
  step. Every poll read nothing, found nothing, and ran to its ceiling against a
  service that had answered correctly first time.
- **Traffic varies from a file and from a fixed function set.** `{{ users.email }}` is
  a dataset field, `{{ uuid() }}` a generator call, `{{ order_id }}` a chain variable
  — told apart by shape, all resolved when the plan compiles, so a misspelled column
  or an unknown function is a load error naming what the file does have. Which row an
  iteration reads is a pure function of the iteration number and the seed: no cursor,
  nothing shared between workers, which is what makes `round_robin` round robin rather
  than round robin per worker. `unique_per_iteration` is checked against the
  arithmetic of the run and refused with both numbers, because wrapping quietly would
  take away the one thing that mode is for.
- **Generated values are keyed per iteration, not per virtual user.** Which VU picks
  up an arrival depends on how long the service took to answer the one before it, so a
  per-VU stream replays differently against a service that has since got slower.
  Iteration 4,001 now generates the same request in every run of the plan, which is
  the property a recorded seed exists for.
- **Three generation tiers behind one interface.** Lua is the default: one VM per
  worker thread in a thread local, never behind a lock, standard libraries chosen
  rather than pruned so `io`, `package` and `debug` are never opened at all. The
  plugin tier is its registry and ships empty. The exec sidecar keeps a pool of
  processes speaking one JSON object per line, started before the arrival clock and
  before the target — a plan whose own script will not start is the plan's problem,
  and an unreachable target must not be reported instead.
- **A generator attaches to the call, not to its body.** The hook returns the whole
  request, and a `body` block that can set the path is a field lying about what it
  does. Arguments are templated, so a script is handed `{{ users.email }}` without
  ever learning that datasets exist.
- **Generation is measured as generation**, in its own per-generator histogram rather
  than inside the step's latency — a slow script folded into response time reads as a
  slow endpoint. Failure is its own class for the same reason: nothing was sent.
- **`prefetch` is refused with its reason rather than approximated.** A buffered
  request carries the dataset row, the sequence number and the draws of the iteration
  it was built for; handing it to a later one makes the run unreplayable.
- **A script can read a corpus without reading the machine.** The declared directory is
  held in memory before the clock starts and handed to each VM as Lua strings built
  once — a `read` per request would allocate a payload's worth on the hot path, which
  is the cost loading once exists to avoid. Read-only, because a VM outlives the
  iteration that used it.
- **Auth is a first-class block and none of it is load.** Token calls go out over their
  own pool, a separate one even for `login_request` against the target itself, because
  a login that borrowed a connection would spend capacity the load was given. Tokens
  are pre-warmed to the size `identity` implies, before the clock.
- **Refresh is single-flight, and the detail that makes it work is which token a caller
  says was rejected.** Passing the one it actually sent — rather than whatever is
  current by the time it asks — means the stragglers still carrying the old credential
  find the new one and take it. Seventeen rejections now cost one refresh; reading the
  current value cost two, and the naive version costs seventeen and produces a spike
  that reads as the target degrading.
- **A session is the cookie jar plus the auth identity, and they move together.** A
  fresh session that reused a token would not be fresh in any way the service can
  tell, so the identity is chosen by the session rather than by the slot running it.
  `fresh` overstates login load and destroys cache locality, `reuse` hides both, and
  the gap between those two mistakes is easily a factor of two in apparent capacity.
- **The first few failures are kept in full, per class.** One flood of
  connection-refused would otherwise evict the single 500 that explains the problem.
  The budget is checked before a body is buffered, from the status line, so a broken
  run producing errors by the thousand stops keeping them. What counts as worth
  sampling lives in one place now — the chain and the scheduler had disagreed, and
  every sample of the commonest failure there is came out with an empty request.
- **Secrets are redacted by value, not only by name.** The engine remembers the literal
  value of every `{{ secret.X }}` it resolved and removes it wherever it appears;
  redaction covering only the names somebody remembered to list is a promise rather
  than a mechanism.
- **Every load error is `<file>#<json-pointer>: <message>`.** Calls used to say
  `call "create-order"/path`, which names the call and not the file, and a bundle can
  hold a dozen call files. The pointer points at what is wrong rather than what was
  edited: a percentage total is a property of `/chains`, a chain that never runs is a
  property of `/chains/1/percent`.
- **`--chain` runs one chain alone at the whole rate**, and annotates the run as
  narrowed: a run of one chain out of six is not a run of the mixture, and a stored run
  that did not say so would sit beside runs that were.
- **The engine was one HTTP/2 send path away from overflowing the main thread's
  megabyte.** The scheduler is a single state machine owning preallocated accumulators
  and polling a chain iteration inline, so it now runs on a thread whose stack this run
  asked for rather than the one the linker chose.
- **The API's readiness check treated a generator name as a template prefix**, which
  would have called a plan ready that the engine refuses to load.
- **The example bundle had never been finished**: `gen/order.lua` and `data/users.csv`
  were named and absent, and `cart-add` read a variable only another chain extracts.
  Both fixed; the second was caught by the new pointer errors.
- Verified: `scripts/check.sh` all green — 244 engine tests, 644 API passed with 1
  skipped, 168 front-end tests.
- **B3's eleven items are all ticked and its "done when" is met in substance**: one
  test runs the example's six chains, its XML and JSON, its extraction, its Lua
  generator, OAuth with a mid-run refresh, and its 401-as-a-pass chain together. The
  committed bundle still does not load, for three reasons that all belong to B4 — two
  targets with `shuffle` and a `gap`, a `host_header`, and an `slo` block. It is aimed
  at fictional ECS tasks and could never have run offline. Recorded here rather than
  ticked past.

## B4.1 — Sequential targets

Run every target in resolved or seed-shuffled order with full independent phase timelines and cancellable gaps. Add --targets, upfront target validation, and ordered target identity on bounded output queues. Three-target integration coverage verifies windows and gaps. B4.1 complete.

## B4.2 — Direct container addressing

Separate socket destination from Host and TLS identity. Verify certificates against SNI (falling back to Host), support explicit insecure TLS with an annotation, and retain handshake signature checks. Echo target attributes at setup. B4.2 complete.

## B4.3 — Breakpoint steps

Validate bounded additive/geometric ramps and measure each step with its own warmup, histograms, and observed recovery phase. Emit per-step statistics and carry iteration identity across steps. B4.3 complete.

## B4.4 — Stop conditions and attribution

Evaluate supported latency, errors and arrival shortfall on measurement ticks, drain under original deadlines and observe final settle after a stop. Generator drift, missed arrivals, cap occupancy, connection limits and generation cost take precedence; calibrated profiles cap search demand. Under-sampled latency searches report no capacity. B4.4 complete.

## B4.5 — Refinement and capacity report

Run one full-duration midpoint probe after a bracketed failure; report a bracket, max sustained rate, knee, cliff and failure statuses/classes. Suppress every capacity scalar on generator failure or interruption. Standalone host limiting-resource and recovery attribution are explicitly unavailable, for observer enrichment. B4.5 complete.

## B4.6 — CI verdicts

Validate and evaluate inclusive SLO bounds independently for each target and rate step, aggregate the worst supported observation, retain every breach, and emit machine-readable verdicts naming the bound each one is about. Error rates read terminal request outcomes rather than chain counts, so a threshold and the stop condition beside it judge the same population. An unsupported threshold is advisory, and the annotation beside the verdicts carries the sample support that made it so. Accept API-owned `observe` declarations, annotated as unavailable, without an API dependency. B4.6 complete.

What invalidates a result now depends on what was asked. A capacity search extrapolates from the window it measured, so send drift and a chain that broke both turn its number into a guess. A fixed run was asked for a timeline and either delivered it or did not: drift within it is a warning it already carries, and a chain that broke is a finding about the service, not a confession that the generator was too small. Counting extraction failures as `generator_limited` made a fixed run against a service returning the wrong body exit 3, which blamed the load generator and buried the breach behind an abort. Admission and cap failures are shares, and a share needs a denominator: at fifty arrivals in a one-second window, two missed to a busy box read as four percent, so below a hundred offered arrivals a share is no longer evidence -- the floor the error rate beside it already had. The reason a fixed run is invalid now rides on the `Invalid` annotation that names the evidence, rather than on `stopped_because`, because nothing stopped it. An output failure outranks every verdict, in the record as well as in the process exit code, because the verdict is what could not be written down.

## Housekeeping — target directory size

`engine/target` had reached 36.6 GiB across 51,645 files, which is not a build
artifact so much as a working copy that no longer fits anywhere convenient. Two
causes, both ours. The dev profile emitted full debug info, and on Windows/MSVC
that means a separate PDB per crate at 130-140 MiB each; `[profile.dev] debug =
"line-tables-only"` keeps the `file:line` that a panic in a load run is actually
read for and drops the largest PDB to 53 MiB. And every full check seeded a fresh
set of incremental compile sessions that nothing would ever read again, 25.6 GiB
of them in 47,083 files; `check.sh` now runs the engine's fmt, clippy and test
with `CARGO_INCREMENTAL=0`, because a verification pass is one-shot by
definition. Hand-run `cargo build` and `cargo check` are untouched and still
incremental -- the edit loop is exactly where that cache pays for itself.

After a clean and a full rebuild the same tree is 2.32 GiB in 2,951 files. No
behaviour changed; `scripts/check.sh engine` passes cold.

## Arrival-clock diagnostics

Measure pending-deadline-to-timer-wake and wake-to-dispatch delays independently through a preallocated bounded atomic ring. Emit count-supported timing histograms/percentiles, coalescing and diagnostic loss, skip-batch frequencies and phase-end accounting as summary annotations and final stderr diagnostics. Keep warmup/measure totals separate and scheduling, admission and event schemas unchanged. Tests cover concurrent publication, bounded loss, deferred samples, synthetic executor stalls, phase separation and output conservation.

Validation: `bash scripts/check.sh engine`, `bash scripts/check.sh contract`, and real emitted NDJSON schema validation all pass on Windows.

## Snapshot execution-loop diagnostics

Measure aggregation, window construction, complete flush and output-packet construction separately. Emit per-snapshot durations, count-supported target/step totals and stderr maxima to diagnose recurring quarter-second dispatch stalls. Keep queue admission and writer serialization outside packet construction, and omit construction samples when capacity refuses a packet before work starts. Test execution explicitly waived by the user for this instrumentation change.

## Reduce snapshot dispatch work

Add same-snapshot paired flush-plus-packet durations and supported totals. Skip empty histogram count-array merge/reset work while preserving overflow metadata. Share immutable snapshot windows with the output queue and perform writer-specific copying on the writer thread, removing a duplicate histogram clone from arrival dispatch. Owned snapshot consumers retain their copy. Tests explicitly waived while the user measures this path.

### 2026-09-14 - Move snapshots off arrival dispatch

- Moved histogram merge/reset, cumulative diagnostics and snapshot window construction to a dedicated aggregation thread using three preallocated reusable buffer sets. Periodic pool saturation coalesces windows without dropping measurements or waiting on snapshot consumers.
- Preserved phase boundaries, cancellation accounting and SLO evaluation; breakpoint stop assessments use diagnostics captured with the same cumulative metric cutoff. Added `snapshot_coalesced_ticks` and a `snapshot_backpressure` annotation to identify deferred publication. Snapshot flush timings now measure aggregation-thread work.
- Added bounded-pool, measurement conservation, stalled-consumer and cutoff-aligned stop tests. Full `bash scripts/check.sh` passed. The remaining B4 packaging step stays deferred.

### 2026-09-14 - Record the manual engine performance baseline

- Added docs/engine-performance-baseline.md with repeated 4k/8k/10k/12k/16k Linux results, counted 10k/12k timing distributions, and the accepted 10k working baseline. Further performance optimization remains deferred.
- Documented a measurement-led improvement strategy and supervised regression comparisons that capture processor, network, target, build and workload conditions, preserve artifacts and outliers, and distinguish provisional guardrails from portable CI guarantees. Linked the reference from the engine testing notes. Documentation only; no engine behavior or B4 checklist changes.

## 2026-09-16 — Plans editor density prototype

- Made the Plan editor denser: chains now lead the workspace, with load and phase controls alongside; kept all existing validation and controls intact.
- Updated the plan-editing design note and added front-end coverage for the new layout structure.

## 2026-09-16 — Basic plan editor

- Added Basic and Advanced tabs to the plan editor. Basic presents valid fixed-rate, single-call chains as a compact table and derives the stored total rate and percentages from row RPS values.
- Simplified Load in both modes: removed editable total RPS and model, paired warmup with settle, moved concurrency last, and labelled stages and breakpoint as not implemented in the editor.
- Moved the plan verdict and explanations below the working controls, added icon-only row actions, and covered compatibility, low-RPS normalization, and the shared Load layout with front-end tests.
- Automated checks pass; browser screenshot comparison remains blocked and is recorded in `design-qa.md`.

## 2026-09-16 — Table-based profile editor

- Moved the editable profile name into the editor header and made profile replacement move the backing file when that name changes; existing recordings retain their stored profile name, and rename collisions are refused.
- Replaced endpoint editor cards with a compact table, per-row edit/remove actions, an add action in the table header, static SSH observation labels, and a narrow observation-defaults card for description, sample interval, and collected metric groups.
- Added per-endpoint addressing kinds for IP, ALB, ELB, ECS, and Fargate while retaining legacy profile-file compatibility. Discovery now labels resolved load balancers, ECS hosts, and Fargate tasks; the editor explains discovery support for each selected kind.
- Collapsed SSH user, host, and port controls into one `user@host:port` field. Resolved defaults are shown without making them explicit in the saved document, including when the endpoint address changes, and bracketed IPv6 destinations round-trip correctly.
- Kept the compact SSH target in its own editable endpoint-table cell rather than a second detail row, and rebalanced the table so Addressing and Address do not consume the target's width.
- Updated profile documentation and regression coverage. Ruff, 651 Python tests (1 skipped), and 178 front-end tests pass; the profile editor was also checked in the browser at the desktop viewport.

## 2026-09-16 — Stored schema library

- Added a Schemas setup view above Profiles with an upload target and a table of stable filename/type ids, original filenames, source types, parsed call counts, and view/delete actions.
- Store uploaded UTF-8 sources under `$METRIX_HOME/schemas/<id>/`. Uploads run through the same OpenAPI 3, WSDL 1.1, HAR, access-log, or route-list parser used by plan generation before an entry is persisted; invalid files leave no entry and collisions do not overwrite.
- Added a schema detail view with the parsed call names, methods and paths alongside the escaped original source, plus list, detail, upload, and delete API routes.
- Documented the storage and parsing boundary and completed A4.6. The required API check passes: Ruff, 659 Python tests (1 skipped), and 181 front-end tests.
- Added a large dotted drag-and-drop target beside the schema picker. Dropped files upload and parse immediately using the selected source type; multiple files report failures individually without losing successful uploads. The empty state no longer uses an upload-looking icon, and the picker links to definitions for every accepted format.
- Added a direct Swagger 2.0 source adapter. It locally reads `definitions`, `basePath`, path/query/header/body/form parameters, response codes, and security into the same generated-call model as OpenAPI 3; no document conversion or outbound request occurs. Swagger 2.0 is available in both Plans and Schemas, and A4.7 is complete. The required API check passes: Ruff, 662 Python tests (1 skipped), and 181 front-end tests.

## 2026-09-17 — WADL source adapter

- Added a local WADL 2009/02 adapter for plan generation and stored schemas. Resource trees, required template/query/header parameters, request media types, documentation, and declared successful response codes become generated calls; external grammars and references are never fetched.
- Added Plans/Schemas picker support, API descriptions, design documentation, and regression coverage. Ruff, 55 focused Python tests, and 33 focused front-end tests pass.
