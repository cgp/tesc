-- 005: what the engine measured, on the recording's clock.
--
-- The engine emits a snapshot every 250ms per target (F0.3). Two tables rather than
-- one because a snapshot has two shapes inside it: scalars that describe the whole
-- window, and a distribution per chain and per step.
--
-- **`t_ms` here is the recording's clock, not the engine's.** The engine counts from
-- its own start, which happens after the observer is already collecting a baseline;
-- the offset between the two is resolved once at ingest and applied to every record,
-- so a load figure and a host figure with the same `t_ms` describe the same moment.
-- That is the whole point of A4.2 and the reason the column is not simply copied.
--
-- Histograms are stored as the engine serialized them, HDR v2 base64. They are kept
-- because distributions merge and percentiles do not (design-api 17.5): merging two
-- windows means merging their histograms, and a stored p95 could not be merged with
-- anything. Nothing in SQLite reads inside the blob.

-- One row per snapshot: the scalars, including the generator's own self-metrics.
CREATE TABLE load_window (
    recording_id      TEXT NOT NULL REFERENCES recording (id) ON DELETE CASCADE,
    target_id         TEXT NOT NULL,
    t_ms              INTEGER NOT NULL,          -- recording clock, offset applied
    engine_t_ms       INTEGER NOT NULL,          -- as the engine reported it
    phase             TEXT NOT NULL,
    window_ms         INTEGER NOT NULL,

    -- Did we apply the load we asked for (design-api 15)? Both sides of that
    -- question, so the answer never depends on reading a chart.
    target_rate       REAL,
    achieved_rate     REAL,

    -- Are we measuring the target or ourselves (design-api 13.2)? Nullable because
    -- the frozen schema carries them as numbers whose collectors may not exist yet,
    -- and a zero that means "unavailable" is the most expensive lie a load tool
    -- can tell. Ingest maps an unavailable reading to NULL.
    in_flight         INTEGER,
    queue_depth       INTEGER,
    drift_ms          REAL,

    bytes_sent        INTEGER NOT NULL DEFAULT 0,
    bytes_received    INTEGER NOT NULL DEFAULT 0,
    connections_opened  INTEGER NOT NULL DEFAULT 0,
    connections_reused  INTEGER NOT NULL DEFAULT 0,

    -- Generator health as the engine reported it, kept whole. Opaque here; it is
    -- read back as a document and is not something to query across.
    generator         TEXT,                      -- JSON object, or null

    PRIMARY KEY (recording_id, target_id, t_ms)
) STRICT;

CREATE INDEX load_window_time ON load_window (recording_id, t_ms);

-- One row per chain, and one per step within it, per snapshot. `step` is null for
-- the chain's own end-to-end row, because a chain's duration is not the sum of its
-- step medians (design-api 14.1) and storing it as a step would invite that sum.
CREATE TABLE load_row (
    recording_id  TEXT NOT NULL REFERENCES recording (id) ON DELETE CASCADE,
    target_id     TEXT NOT NULL,
    t_ms          INTEGER NOT NULL,
    chain         TEXT NOT NULL,

    -- Empty for the chain's own end-to-end row. A STRICT table makes every primary
    -- key column NOT NULL, so the natural NULL is not available here -- and rather
    -- than let an empty string be a magic value nobody can see the meaning of, the
    -- CHECK below ties it to `kind`: `duration` rows are chains, and `total`/`ttfb`
    -- rows are steps. The read layer hands back None, so the sentinel never leaves
    -- this file.
    step          TEXT NOT NULL,
    kind          TEXT NOT NULL CHECK (kind IN ('duration', 'total', 'ttfb')),

    attempted     INTEGER NOT NULL DEFAULT 0,
    completed     INTEGER NOT NULL DEFAULT 0,
    failed        INTEGER NOT NULL DEFAULT 0,

    -- The distribution. `count` travels with every figure derived from it, which is
    -- the rule the whole tool is built on: a number the count cannot support is not
    -- reported, and the count has to be here for that to be checkable.
    count         INTEGER NOT NULL DEFAULT 0,
    min_us        INTEGER,
    max_us        INTEGER,
    mean_us       REAL,
    hdr           TEXT,                          -- HDR v2, base64, as emitted

    -- Status counts as the engine grouped them: {"200": 8, "401": 11}. A column per
    -- code is not a schema, and these are read back whole.
    statuses      TEXT,

    -- A table constraint, so it can mention two columns: `duration` rows are the
    -- chain's own, `total`/`ttfb` rows belong to a step. This is what stops the
    -- empty string being a value whose meaning lives only in a comment.
    CHECK ((kind = 'duration') = (step = '')),

    PRIMARY KEY (recording_id, target_id, t_ms, chain, step, kind)
) STRICT;

CREATE INDEX load_row_chain ON load_row (recording_id, chain, step, t_ms);

-- What the engine said about itself at the end: the exit code, and why it stopped.
-- Recorded on the recording rather than in a table of one row.
ALTER TABLE recording ADD COLUMN engine_exit_code INTEGER;
ALTER TABLE recording ADD COLUMN stopped_because TEXT;

-- The engine writes to the same annotation list the observer and people write to,
-- so a reader has one place to look before quoting a number. That needs a third
-- source, and SQLite cannot alter a CHECK -- so the table is rebuilt. Recording an
-- engine note as though a detector produced it would be a small lie in the data,
-- and which side noticed is exactly what a reader wants when a run looks wrong.
CREATE TABLE annotation_new (
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
    source        TEXT NOT NULL DEFAULT 'detector'
                  CHECK (source IN ('detector', 'operator', 'engine'))
) STRICT;

INSERT INTO annotation_new
    (id, recording_id, target_id, code, severity, phase, from_ms, to_ms, message,
     detail, source)
SELECT id, recording_id, target_id, code, severity, phase, from_ms, to_ms, message,
       detail, source
FROM annotation;

DROP TABLE annotation;
ALTER TABLE annotation_new RENAME TO annotation;
CREATE INDEX annotation_recording ON annotation (recording_id, from_ms);
