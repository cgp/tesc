-- 003: resolved inventories, and the one each recording ran against.
--
-- Discovery (design-api 3.1) is slow, rate-limited and answers differently from one
-- hour to the next, so what it found is stored rather than re-derived: a profile
-- resolves once and reuses the answer until its TTL expires, and a recording pins the
-- exact snapshot it ran against so a comparison months later knows what "the same
-- environment" meant at the time.
--
-- The whole snapshot is kept as its JSON document rather than exploded into rows.
-- Nothing queries inside it -- it is read back whole, for a person or for a diff --
-- and a table per resource role would have to change shape every time discovery
-- learns to record one more thing about a box.

CREATE TABLE inventory (
    id             INTEGER PRIMARY KEY,

    -- The profile this was resolved for, and what was asked: a hostname, or
    -- cluster/service. Null profile means a one-off resolution from the API.
    profile        TEXT,
    source         TEXT NOT NULL,
    -- The furthest hop the walk reached. Partial resolution is the normal case, so
    -- this is a property of a perfectly good snapshot, not an error code.
    reached        TEXT NOT NULL,

    -- The set of hosts, hashed: comparing two snapshots for "did the environment
    -- change under us" is one string comparison rather than a document diff.
    host_key       TEXT NOT NULL,
    document       TEXT NOT NULL,               -- the snapshot, as discovery produced it

    -- When this state was first seen, and when it was last confirmed unchanged. A
    -- re-resolution that finds the same thing extends the row rather than inserting
    -- a near-identical one, so the table stays a history of *changes*.
    discovered_at  TEXT NOT NULL,
    confirmed_at   TEXT NOT NULL
) STRICT;

CREATE INDEX inventory_profile ON inventory (profile, confirmed_at);

-- No cascade: an inventory outlives the recordings that point at it, and one snapshot
-- is commonly shared by every run made against an environment that did not change.
ALTER TABLE recording ADD COLUMN inventory_id INTEGER REFERENCES inventory (id);
