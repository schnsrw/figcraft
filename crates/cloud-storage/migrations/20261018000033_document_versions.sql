-- S6-041: named versions. A version is a name a person gave to the document as it was at event
-- sequence `seq` (0 = the empty starting state). It is a pointer into the append-only log, not a
-- copy: the state is rebuilt from the verified snapshot at or below `seq` plus the log up to it, and a
-- version is only created after that rebuild has succeeded (so every listed version is restorable).
-- Restoring one never rewrites anything: it appends one new batch at the head. Deleting a version
-- only hides the name (`deleted_at`); the history it pointed into is untouched.
CREATE TABLE document_versions (
    tenant_id UUID NOT NULL,
    id UUID NOT NULL,
    document_id UUID NOT NULL,
    seq BIGINT NOT NULL CHECK (seq >= 0),
    name TEXT NOT NULL CHECK (char_length(btrim(name)) BETWEEN 1 AND 120),
    created_by UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    deleted_at TIMESTAMPTZ,
    deleted_by UUID REFERENCES principals (id) ON DELETE RESTRICT,
    PRIMARY KEY (tenant_id, id),
    FOREIGN KEY (tenant_id, document_id) REFERENCES documents (tenant_id, id) ON DELETE RESTRICT,
    CHECK ((deleted_at IS NULL) = (deleted_by IS NULL))
);
CREATE INDEX document_versions_document_idx ON document_versions (tenant_id, document_id, created_at DESC, id) WHERE deleted_at IS NULL;

ALTER TABLE document_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE document_versions FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON document_versions AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);
