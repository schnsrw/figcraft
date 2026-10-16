-- S2-009: record when a document's soft-delete grace period started.
-- The repository layer only drives transitions into and out of 'deleting'; finalizing a grace
-- period into the terminal 'deleted' state and scheduling object collection is later work, so this
-- constraint is intentionally scoped to the 'deleting' state only.

ALTER TABLE documents ADD COLUMN deleted_at TIMESTAMPTZ;
ALTER TABLE documents ADD CONSTRAINT documents_deleted_at_matches_state
    CHECK ((state = 'deleting') = (deleted_at IS NOT NULL));
