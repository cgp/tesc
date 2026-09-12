-- 002: what each box *was*, and how much disk the run consumed.
--
-- Neither of these is a series. Identity does not change during a recording, and
-- filesystem usage is read twice -- once before anything starts and once after
-- everything has drained -- because what a run needs to answer is "did this consume
-- disk, and how much", which two readings answer for the cost of two `df` calls
-- rather than one per second on every mount.

-- One row per target. `facts` is a JSON object of whatever the transport could
-- report: hostname, os, kernel, arch, cpus. Opaque on purpose -- it is read back for
-- a person, never queried on, and the set will grow as transports learn to answer
-- more. A key nothing could supply is absent rather than empty.
CREATE TABLE host_identity (
    recording_id     TEXT NOT NULL REFERENCES recording(id) ON DELETE CASCADE,
    target_id        TEXT NOT NULL,
    facts            TEXT NOT NULL,
    PRIMARY KEY (recording_id, target_id)
) STRICT;

-- Two readings per mount: `at` is 'start' or 'finish'. A finish row missing for a
-- mount that has a start row means the second probe failed, which is visible as an
-- absent delta rather than as a zero.
CREATE TABLE filesystem_usage (
    recording_id     TEXT NOT NULL REFERENCES recording(id) ON DELETE CASCADE,
    target_id        TEXT NOT NULL,
    at               TEXT NOT NULL CHECK (at IN ('start', 'finish')),
    mount            TEXT NOT NULL,
    total_bytes      INTEGER NOT NULL,
    used_bytes       INTEGER NOT NULL,
    PRIMARY KEY (recording_id, target_id, at, mount)
) STRICT;
