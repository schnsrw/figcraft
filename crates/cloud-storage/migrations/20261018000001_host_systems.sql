-- S6-001: registered embedding host systems and their public verification keys.
--
-- One row per host product per tenant. `system_id` is the same identifier external_mappings uses.
-- `issuer` is globally unique because an embed token names its issuer before any tenant is known;
-- the pre-tenant lookup goes through the SECURITY DEFINER function below (same pattern as share
-- links). Only PUBLIC verification keys are stored (Ed25519 raw 32 bytes, ES256 uncompressed
-- SEC1 point of 65 bytes); no host secret is held, so there is nothing to encrypt at rest.
CREATE TABLE host_systems (
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    system_id TEXT NOT NULL CHECK (char_length(btrim(system_id)) BETWEEN 1 AND 128),
    issuer TEXT NOT NULL CHECK (char_length(issuer) BETWEEN 1 AND 256),
    audiences TEXT[] NOT NULL CHECK (cardinality(audiences) BETWEEN 1 AND 8 AND octet_length(array_to_string(audiences, E'\n')) <= 2048),
    allowed_origins TEXT[] NOT NULL CHECK (cardinality(allowed_origins) <= 16 AND octet_length(array_to_string(allowed_origins, E'\n')) <= 4096),
    max_role TEXT NOT NULL CHECK (max_role IN ('viewer', 'commenter', 'editor')),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, system_id),
    UNIQUE (issuer)
);

CREATE TABLE host_system_keys (
    tenant_id UUID NOT NULL,
    system_id TEXT NOT NULL,
    kid TEXT NOT NULL CHECK (char_length(kid) BETWEEN 1 AND 128),
    alg TEXT NOT NULL CHECK (alg IN ('EdDSA', 'ES256')),
    public_key BYTEA NOT NULL,
    not_before BIGINT NOT NULL CHECK (not_before >= 0),
    expires_at BIGINT CHECK (expires_at IS NULL OR expires_at > not_before),
    retired_at BIGINT CHECK (retired_at IS NULL OR retired_at >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, system_id, kid),
    FOREIGN KEY (tenant_id, system_id) REFERENCES host_systems (tenant_id, system_id) ON DELETE CASCADE,
    CHECK ((alg = 'EdDSA' AND octet_length(public_key) = 32) OR (alg = 'ES256' AND octet_length(public_key) = 65))
);

ALTER TABLE host_systems ENABLE ROW LEVEL SECURITY;
ALTER TABLE host_systems FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON host_systems AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);
CREATE POLICY definer_lookup ON host_systems AS PERMISSIVE FOR SELECT
    USING (current_user = 'figcraft_migrator' AND current_setting('figcraft.definer_lookup', true) = 'on');

ALTER TABLE host_system_keys ENABLE ROW LEVEL SECURITY;
ALTER TABLE host_system_keys FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON host_system_keys AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);

-- The enabled host (in an active tenant) that registered `p_issuer`. Disabled hosts and closing
-- tenants are invisible here, so their tokens are refused before any key is loaded (S6-005).
CREATE FUNCTION public.figcraft_host_issuer(p_issuer TEXT)
RETURNS TABLE (tenant_id TEXT, system_id TEXT)
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
BEGIN
    PERFORM set_config('figcraft.definer_lookup', 'on', true);
    RETURN QUERY
        SELECT h.tenant_id::text, h.system_id
        FROM public.host_systems h JOIN public.tenants t ON t.id = h.tenant_id
        WHERE char_length(p_issuer) BETWEEN 1 AND 256 AND h.issuer = p_issuer
          AND h.enabled AND t.state = 'active'
        LIMIT 1;
    PERFORM set_config('figcraft.definer_lookup', '', true);
END
$fn$;
