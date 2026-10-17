-- S5-003/004: one bounded, tenant-scoped owner record per document. PostgreSQL's clock is the
-- lease clock; the fencing token on documents remains the authority for durable writes.
CREATE TABLE room_leases (
    tenant_id UUID NOT NULL,
    document_id UUID NOT NULL,
    owner_id UUID NOT NULL,
    endpoint TEXT NOT NULL CHECK (octet_length(endpoint) BETWEEN 1 AND 512),
    fencing_token BIGINT NOT NULL CHECK (fencing_token > 0),
    expires_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, document_id),
    FOREIGN KEY (tenant_id, document_id) REFERENCES documents (tenant_id, id) ON DELETE CASCADE
);
CREATE INDEX room_leases_expiry_idx ON room_leases (expires_at);

ALTER TABLE room_leases ENABLE ROW LEVEL SECURITY;
ALTER TABLE room_leases FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON room_leases AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);
