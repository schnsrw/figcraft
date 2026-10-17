-- S6-003: one-use embed-token identifiers. A row is the proof that a host's token `jti` was
-- already exchanged. The primary key makes consumption atomic: of any number of concurrent
-- exchanges of one token, exactly one INSERT succeeds. Rows live until the token could no longer
-- validate anyway (its expiry plus the allowed clock skew) and are swept after that.
CREATE TABLE embed_token_uses (
    tenant_id UUID NOT NULL,
    system_id TEXT NOT NULL,
    jti TEXT NOT NULL CHECK (char_length(jti) BETWEEN 8 AND 128),
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, system_id, jti),
    FOREIGN KEY (tenant_id, system_id) REFERENCES host_systems (tenant_id, system_id) ON DELETE CASCADE
);
CREATE INDEX embed_token_uses_expiry_idx ON embed_token_uses (expires_at);

ALTER TABLE embed_token_uses ENABLE ROW LEVEL SECURITY;
ALTER TABLE embed_token_uses FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON embed_token_uses AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);
