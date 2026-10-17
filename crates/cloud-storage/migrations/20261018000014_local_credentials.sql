-- S6-032: local accounts. A tenant-scoped credentials record per principal: the normalised login
-- (NFC, lowercase, bounded), the password hash as a PHC string that embeds its own algorithm
-- parameters (so they can be raised later and old hashes upgraded at the next successful login),
-- an `active`/`disabled` state and the failed-attempt counters the lockout policy reads. The
-- principal stays the identity: an OIDC user links through `identity_links`, and one person may
-- hold both. A login is unique inside its tenant (two tenants may both have `ada`); a principal
-- has at most one credential per tenant.
CREATE TABLE local_credentials (
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    principal_id UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    login TEXT NOT NULL CHECK (octet_length(login) BETWEEN 3 AND 256 AND login = lower(login) AND login = btrim(login)),
    password_hash TEXT NOT NULL CHECK (octet_length(password_hash) BETWEEN 32 AND 512),
    state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'disabled')),
    failed_attempts INTEGER NOT NULL DEFAULT 0 CHECK (failed_attempts >= 0),
    last_failed_at TIMESTAMPTZ,
    locked_until TIMESTAMPTZ,
    password_changed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, principal_id),
    UNIQUE (tenant_id, login)
);
CREATE INDEX local_credentials_principal_idx ON local_credentials (principal_id);

ALTER TABLE local_credentials ENABLE ROW LEVEL SECURITY;
ALTER TABLE local_credentials FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON local_credentials AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);
