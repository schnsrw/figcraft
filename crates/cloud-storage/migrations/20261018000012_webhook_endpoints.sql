-- S6-013: webhook endpoints registered by a host system. The signing secret is stored only as an
-- AES-256-GCM sealed blob (nonce, ciphertext and tag, bound to the tenant and endpoint as
-- associated data); the plaintext is shown once at creation or rotation and never again. A rotated
-- secret keeps working beside the new one until `previous_expires_at`, so a receiver can switch
-- without dropping deliveries.
CREATE TABLE webhook_endpoints (
    tenant_id UUID NOT NULL,
    id UUID NOT NULL,
    system_id TEXT NOT NULL,
    url TEXT NOT NULL CHECK (octet_length(url) BETWEEN 8 AND 2048),
    event_types TEXT[] NOT NULL CHECK (cardinality(event_types) <= 32),
    secret_sealed BYTEA NOT NULL CHECK (octet_length(secret_sealed) BETWEEN 29 AND 256),
    secret_version SMALLINT NOT NULL CHECK (secret_version > 0),
    previous_sealed BYTEA CHECK (previous_sealed IS NULL OR octet_length(previous_sealed) BETWEEN 29 AND 256),
    previous_expires_at TIMESTAMPTZ,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, id),
    FOREIGN KEY (tenant_id, system_id) REFERENCES host_systems (tenant_id, system_id) ON DELETE CASCADE,
    CHECK ((previous_sealed IS NULL) = (previous_expires_at IS NULL))
);
CREATE INDEX webhook_endpoints_system_idx ON webhook_endpoints (tenant_id, system_id, created_at, id);

ALTER TABLE webhook_endpoints ENABLE ROW LEVEL SECURITY;
ALTER TABLE webhook_endpoints FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON webhook_endpoints AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);
