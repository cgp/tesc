-- 001: recordings, targets, phases, host samples, annotations.
--
-- Enough to record and compare host observation with no engine in existence. Load-run
-- tables (engine summaries, SLO verdicts) arrive with A4; adding them later is what
-- the migration mechanism is for.
--
-- SQLite holds what gets queried. The bulk streams -- per-request events and error
-- sample bodies -- stay as files under runs/<id>/, so the purge button is a directory
-- delete rather than a transaction that has to vacuum a database.

CREATE TABLE recording (
    id               TEXT PRIMARY KEY,          -- 2026-09-12T14-03-11Z_a3f9
    kind             TEXT NOT NULL CHECK (kind IN ('observation', 'load')),
    status           TEXT NOT NULL CHECK (status IN ('running', 'finished', 'aborted', 'failed')),

    -- What it ran against. profile is the name; addressing_mode is part of series
    -- identity because through-the-load-balancer and direct-to-container measure
    -- different network paths and must never be compared.
    profile          TEXT,
    addressing_mode  TEXT NOT NULL DEFAULT 'load_balancer'
                     CHECK (addressing_mode IN ('load_balancer', 'direct')),

    -- What produced it. Recorded so a comparison across a version change is not
    -- silently attributed to the target.
    api_version      TEXT NOT NULL,
    engine_version   TEXT,
    plan_name        TEXT,
    plan_hash        TEXT,
    machine_profile  TEXT,
    seed             INTEGER,

    -- Runs sharing this key form a series; trends and noise floors are computed
    -- within one. Derived in code from the identity tuple.
    series_key       TEXT NOT NULL,

    started_at       TEXT NOT NULL,             -- RFC 3339, display only
    finished_at      TEXT,
    -- Monotonic run length. Alignment uses t_ms offsets, never wall clock.
    duration_ms      INTEGER,

    -- Marked by a person. Blocked for recordings carrying an 'invalid' annotation.
    is_baseline      INTEGER NOT NULL DEFAULT 0 CHECK (is_baseline IN (0, 1)),
    note             TEXT
) STRICT;

CREATE INDEX recording_series ON recording (series_key, started_at);
CREATE INDEX recording_kind ON recording (kind, started_at);

-- One row per box the recording covers. A sweep is simply more than one.
CREATE TABLE recording_target (
    recording_id  TEXT NOT NULL REFERENCES recording (id) ON DELETE CASCADE,
    target_id     TEXT NOT NULL,
    position      INTEGER NOT NULL,             -- order within the sweep, from 1
    address       TEXT,
    host_header   TEXT,
    -- Inventory detail: instance type, AZ, image digest, task definition revision.
    -- Opaque here; it is what explains an outlier in a sweep.
    attributes    TEXT NOT NULL DEFAULT '{}',   -- JSON object

    PRIMARY KEY (recording_id, target_id)
) STRICT;

-- The phased timeline. Every statistic is read against the phase that produced it,
-- so phases are stored rather than inferred from timestamps.
CREATE TABLE phase (
    recording_id  TEXT NOT NULL REFERENCES recording (id) ON DELETE CASCADE,
    target_id     TEXT NOT NULL,
    phase         TEXT NOT NULL CHECK (
                      phase IN ('baseline', 'warmup', 'measure', 'drain', 'settle')),
    from_ms       INTEGER NOT NULL,
    to_ms         INTEGER,

    PRIMARY KEY (recording_id, target_id, phase, from_ms)
) STRICT;

-- Host samples in narrow form: one row per metric per sample. Keeps the collectors
-- free to report whatever a host exposes without a schema change, and makes
-- "this metric over this phase" a plain indexed query.
CREATE TABLE host_sample (
    recording_id  TEXT NOT NULL REFERENCES recording (id) ON DELETE CASCADE,
    target_id     TEXT NOT NULL,
    t_ms          INTEGER NOT NULL,             -- since the recording's monotonic start
    metric        TEXT NOT NULL,                -- cpu.user, mem.available, disk.await, ...
    value         REAL NOT NULL
) STRICT;

CREATE INDEX host_sample_lookup ON host_sample (recording_id, target_id, metric, t_ms);

-- A collection failure degrades rather than aborts: the interval is recorded as a
-- gap and drawn as one, never interpolated.
CREATE TABLE collection_gap (
    recording_id  TEXT NOT NULL REFERENCES recording (id) ON DELETE CASCADE,
    target_id     TEXT NOT NULL,
    from_ms       INTEGER NOT NULL,
    to_ms         INTEGER NOT NULL,
    reason        TEXT NOT NULL,

    PRIMARY KEY (recording_id, target_id, from_ms)
) STRICT;

-- Structured notes. 'invalid' is not cosmetic: such a recording cannot become a
-- baseline without an explicit override.
CREATE TABLE annotation (
    id            INTEGER PRIMARY KEY,
    recording_id  TEXT NOT NULL REFERENCES recording (id) ON DELETE CASCADE,
    target_id     TEXT,
    code          TEXT NOT NULL,
    severity      TEXT NOT NULL CHECK (severity IN ('info', 'warn', 'invalid')),
    phase         TEXT,
    from_ms       INTEGER NOT NULL,
    to_ms         INTEGER,
    message       TEXT NOT NULL,
    detail        TEXT,                         -- JSON object
    -- Machine detectors and people write to the same list, so they read together.
    source        TEXT NOT NULL DEFAULT 'detector'
                  CHECK (source IN ('detector', 'operator'))
) STRICT;

CREATE INDEX annotation_recording ON annotation (recording_id, from_ms);
CREATE INDEX annotation_severity ON annotation (recording_id, severity);
