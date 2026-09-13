-- 004: when a recording's bulk was dropped.
--
-- Everything is kept by default and nothing rolls up (design-api 17.1). The one
-- exception is the bulk stream -- per-request event records and retained error-sample
-- bodies -- which lives as files under runs/<id>/ rather than in this database,
-- exactly so that dropping it is a directory delete instead of a transaction that
-- has to vacuum SQLite afterwards.
--
-- What is recorded here is only *that it happened, and when*. Nothing else changes: a
-- purged recording keeps its summaries, its annotations, its host samples and its
-- place in every trend. A person opening it a year later needs to know that the
-- request-level evidence is gone rather than never having existed, and those are
-- different facts -- which is the whole reason this column is not simply inferred
-- from an empty directory.

ALTER TABLE recording ADD COLUMN purged_at TEXT;
