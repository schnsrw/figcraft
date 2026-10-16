-- S3 follow-ups: one live plugin per publisher and name, a document recovery quarantine, and who
-- placed a retention hold and why.

-- Replaces the advisory lock S3-011 used to keep lookup-or-create from duplicating plugins.
CREATE UNIQUE INDEX plugins_live_publisher_name_idx ON plugins (tenant_id, publisher, name) WHERE state <> 'deleted';

-- A document whose durable history cannot be restored (no verified snapshot chain and no
-- replayable log) is quarantined for operator repair instead of being served (architecture.md,
-- "Room recovery"). A quarantined document is never opened by a relay.
ALTER TABLE documents ADD COLUMN quarantined_at TIMESTAMPTZ;
ALTER TABLE documents ADD COLUMN quarantine_reason TEXT CHECK (quarantine_reason IN (
    'snapshot_chain_unverifiable', 'log_gap', 'log_corrupt', 'replay_failed', 'operator'
));
ALTER TABLE documents ADD CONSTRAINT documents_quarantine_consistent
    CHECK ((quarantined_at IS NULL) = (quarantine_reason IS NULL));

-- Retention holds are `object_references` rows with owner_kind 'retention_hold'; this records the
-- principal that placed each hold and a bounded reason, so release can be audited.
CREATE TABLE retention_holds (
    tenant_id UUID NOT NULL,
    id UUID NOT NULL,
    object_id UUID NOT NULL,
    placed_by UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    reason TEXT NOT NULL CHECK (char_length(btrim(reason)) BETWEEN 1 AND 500),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, id),
    FOREIGN KEY (tenant_id, object_id) REFERENCES objects (tenant_id, id) ON DELETE RESTRICT
);
CREATE INDEX retention_holds_object_idx ON retention_holds (tenant_id, object_id);
