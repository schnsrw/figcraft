-- S6-034: per-tenant account settings. `password_enabled` / `oidc_enabled` are the tenant's allowed
-- sign-in methods (at least one must stay on; no row means both); `open_registration` (default OFF)
-- lets anyone who can reach the workspace create a password account without an invitation;
-- `require_totp` (S6-038) makes a second factor mandatory for password sign-in.
CREATE TABLE tenant_account_settings (
    tenant_id UUID PRIMARY KEY REFERENCES tenants (id) ON DELETE RESTRICT,
    password_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    oidc_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    open_registration BOOLEAN NOT NULL DEFAULT FALSE,
    require_totp BOOLEAN NOT NULL DEFAULT FALSE,
    updated_by UUID REFERENCES principals (id) ON DELETE RESTRICT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK (password_enabled OR oidc_enabled),
    CHECK (NOT open_registration OR password_enabled)
);

ALTER TABLE tenant_account_settings ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_account_settings FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenant_account_settings AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);

-- The id of an ACTIVE tenant by slug, for the pre-tenant sign-in form of a multi-tenant deployment
-- (slugs are public workspace names, not secrets). Same SECURITY DEFINER pattern as the other
-- pre-tenant lookups (S4-014): fixed SQL, pinned search_path, minimal row.
CREATE FUNCTION public.figcraft_tenant_by_slug(p_slug TEXT)
RETURNS TEXT
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
    found_tenant TEXT;
BEGIN
    PERFORM set_config('figcraft.definer_lookup', 'on', true);
    SELECT t.id::text INTO found_tenant FROM public.tenants t
    WHERE octet_length(p_slug) BETWEEN 1 AND 63 AND t.slug = p_slug AND t.state = 'active'
    LIMIT 1;
    PERFORM set_config('figcraft.definer_lookup', '', true);
    RETURN found_tenant;
END
$fn$;
REVOKE ALL ON FUNCTION public.figcraft_tenant_by_slug(TEXT) FROM PUBLIC;
