-- S6-037: single-use, time-limited password links. A `reset` link (an administrator helps someone who
-- lost their password) lives at most one hour; a `setup` link (an administrator created the account,
-- the person chooses their first password) at most seven days. Only the SHA-256 of the 256-bit token
-- is stored; the token is shown once to the administrator and never again. At most one live link per
-- person: issuing a new one revokes the older ones.
CREATE TABLE password_resets (
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    id UUID NOT NULL,
    principal_id UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    purpose TEXT NOT NULL CHECK (purpose IN ('reset', 'setup')),
    token_hash BYTEA NOT NULL CHECK (octet_length(token_hash) = 32),
    created_by UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    expires_at TIMESTAMPTZ NOT NULL,
    used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    PRIMARY KEY (tenant_id, id),
    FOREIGN KEY (tenant_id, principal_id) REFERENCES local_credentials (tenant_id, principal_id) ON DELETE CASCADE,
    CHECK (expires_at > created_at),
    CHECK (expires_at <= created_at + CASE purpose WHEN 'reset' THEN interval '1 hour' ELSE interval '7 days' END)
);
CREATE UNIQUE INDEX password_resets_token_idx ON password_resets (token_hash);
CREATE INDEX password_resets_principal_idx ON password_resets (tenant_id, principal_id) WHERE used_at IS NULL AND revoked_at IS NULL;

ALTER TABLE password_resets ENABLE ROW LEVEL SECURITY;
ALTER TABLE password_resets FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON password_resets AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);
CREATE POLICY definer_lookup ON password_resets AS PERMISSIVE FOR SELECT
    USING (current_user = 'figcraft_migrator' AND current_setting('figcraft.definer_lookup', true) = 'on');

-- The tenant a link belongs to (the link holder has no session and names no tenant); consuming it
-- then runs inside that tenant, as for invitations (S4-014).
CREATE FUNCTION public.figcraft_reset_token_tenant(p_token_hash BYTEA)
RETURNS TEXT
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
    found_tenant TEXT;
BEGIN
    PERFORM set_config('figcraft.definer_lookup', 'on', true);
    SELECT r.tenant_id::text INTO found_tenant FROM public.password_resets r
    WHERE octet_length(p_token_hash) = 32 AND r.token_hash = p_token_hash
    LIMIT 1;
    PERFORM set_config('figcraft.definer_lookup', '', true);
    RETURN found_tenant;
END
$fn$;
REVOKE ALL ON FUNCTION public.figcraft_reset_token_tenant(BYTEA) FROM PUBLIC;
