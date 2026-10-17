-- S4-014: row-level security on every tenant-owned table.
--
-- Every table with a tenant_id column, plus tenants itself, gets ENABLE and FORCE ROW LEVEL
-- SECURITY and one policy that admits a row only when its tenant equals the transaction-local
-- `figcraft.tenant_id` set by begin_tenant_transaction. Outside a tenant transaction the setting is
-- unset (NULL) or reset to '' (after an earlier SET LOCAL on the same pooled connection); NULLIF
-- maps both to NULL, so the predicate is never true and the query sees zero rows and cannot write.
-- The policy applies to every role (FORCE includes the owner); only superusers, which the runtime
-- and migrator never are, bypass it.
--
-- Global tables have no tenant and no tenant policy, deliberately: principals, identity_links,
-- browser_sessions and oidc_login_attempts belong to a person or a login flow, not to a tenant (a
-- principal can be a member of several tenants), and _sqlx_migrations is the schema ledger.
--
-- A few lookups must happen before the tenant is known, because a presented secret names it:
-- share link by token hash, service-account credential by secret hash, invitation by token hash,
-- and the list of tenants a principal belongs to. Those go through the SECURITY DEFINER functions
-- below. They run as their owner (figcraft_migrator), take only the hash or principal, return only
-- the minimal row, use fixed SQL with a pinned search_path, and turn `figcraft.definer_lookup` on
-- only around their one query (a function-level SET of a custom setting needs a superuser, so
-- they call set_config and reset it before returning). The `definer_lookup` SELECT policy admits a row only when the current user
-- is figcraft_migrator *and* that flag is on, so it opens nothing to the runtime role (which can set
-- the flag but is never figcraft_migrator) and nothing to a migration that did not ask for it.
-- Provisioning grants EXECUTE on these functions to figcraft_runtime only.

DO $rls$
DECLARE
    target RECORD;
BEGIN
    FOR target IN
        SELECT c.relname, a.attname
        FROM pg_class c
        JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
        WHERE c.relnamespace = 'public'::regnamespace AND c.relkind IN ('r', 'p')
            AND ((a.attname = 'tenant_id') OR (c.relname = 'tenants' AND a.attname = 'id'))
        ORDER BY c.relname
    LOOP
        EXECUTE format('ALTER TABLE public.%I ENABLE ROW LEVEL SECURITY', target.relname);
        EXECUTE format('ALTER TABLE public.%I FORCE ROW LEVEL SECURITY', target.relname);
        EXECUTE format(
            'CREATE POLICY tenant_isolation ON public.%I AS PERMISSIVE FOR ALL '
            'USING (%I = NULLIF(current_setting(''figcraft.tenant_id'', true), '''')::uuid) '
            'WITH CHECK (%I = NULLIF(current_setting(''figcraft.tenant_id'', true), '''')::uuid)',
            target.relname, target.attname, target.attname
        );
    END LOOP;
END
$rls$;

-- Read access for the definer lookups, only on the tables they read.
CREATE POLICY definer_lookup ON public.tenants AS PERMISSIVE FOR SELECT
    USING (current_user = 'figcraft_migrator' AND current_setting('figcraft.definer_lookup', true) = 'on');
CREATE POLICY definer_lookup ON public.tenant_members AS PERMISSIVE FOR SELECT
    USING (current_user = 'figcraft_migrator' AND current_setting('figcraft.definer_lookup', true) = 'on');
CREATE POLICY definer_lookup ON public.documents AS PERMISSIVE FOR SELECT
    USING (current_user = 'figcraft_migrator' AND current_setting('figcraft.definer_lookup', true) = 'on');
CREATE POLICY definer_lookup ON public.share_links AS PERMISSIVE FOR SELECT
    USING (current_user = 'figcraft_migrator' AND current_setting('figcraft.definer_lookup', true) = 'on');
CREATE POLICY definer_lookup ON public.service_accounts AS PERMISSIVE FOR SELECT
    USING (current_user = 'figcraft_migrator' AND current_setting('figcraft.definer_lookup', true) = 'on');
CREATE POLICY definer_lookup ON public.service_account_credentials AS PERMISSIVE FOR SELECT
    USING (current_user = 'figcraft_migrator' AND current_setting('figcraft.definer_lookup', true) = 'on');
CREATE POLICY definer_lookup ON public.team_invitations AS PERMISSIVE FOR SELECT
    USING (current_user = 'figcraft_migrator' AND current_setting('figcraft.definer_lookup', true) = 'on');

-- The live share link a token hash names (S4-005), with its expiry and the share-session end.
CREATE FUNCTION public.figcraft_live_share_link(p_token_hash BYTEA, p_max_session_secs BIGINT)
RETURNS TABLE (id TEXT, tenant_id TEXT, document_id TEXT, role TEXT, created_by TEXT, link_expires_at BIGINT, session_expires_at BIGINT)
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
BEGIN
    PERFORM set_config('figcraft.definer_lookup', 'on', true);
    RETURN QUERY
        SELECT s.id::text, s.tenant_id::text, s.document_id::text, s.role, s.created_by::text,
               floor(extract(epoch FROM s.expires_at))::bigint,
               floor(extract(epoch FROM LEAST(COALESCE(s.expires_at, 'infinity'::timestamptz),
                     now() + make_interval(secs => LEAST(GREATEST(p_max_session_secs, 0), 31536000)::double precision))))::bigint
        FROM public.share_links s
        JOIN public.documents d ON d.tenant_id = s.tenant_id AND d.id = s.document_id
        JOIN public.tenants t ON t.id = s.tenant_id
        WHERE octet_length(p_token_hash) = 32 AND s.token_hash = p_token_hash
          AND s.revoked_at IS NULL AND (s.expires_at IS NULL OR s.expires_at > now())
          AND d.state = 'active' AND t.state = 'active'
        LIMIT 1;
    PERFORM set_config('figcraft.definer_lookup', '', true);
END
$fn$;

-- Of the given (tenant, link) pairs (at most 1024), those that no longer open anything.
CREATE FUNCTION public.figcraft_dead_share_links(p_tenant_ids UUID[], p_link_ids UUID[])
RETURNS TABLE (tenant_id TEXT, id TEXT)
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
BEGIN
    IF coalesce(cardinality(p_tenant_ids), 0) > 1024 OR coalesce(cardinality(p_tenant_ids), 0) <> coalesce(cardinality(p_link_ids), 0) THEN
        RAISE EXCEPTION 'invalid share link batch' USING ERRCODE = '22023';
    END IF;
    PERFORM set_config('figcraft.definer_lookup', 'on', true);
    RETURN QUERY
        SELECT x.tenant_id::text, x.id::text
        FROM unnest(p_tenant_ids, p_link_ids) AS x (tenant_id, id)
        WHERE NOT EXISTS (
            SELECT 1 FROM public.share_links s
            JOIN public.documents d ON d.tenant_id = s.tenant_id AND d.id = s.document_id
            JOIN public.tenants t ON t.id = s.tenant_id
            WHERE s.tenant_id = x.tenant_id AND s.id = x.id AND s.revoked_at IS NULL
              AND (s.expires_at IS NULL OR s.expires_at > now()) AND d.state = 'active' AND t.state = 'active');
    PERFORM set_config('figcraft.definer_lookup', '', true);
END
$fn$;

-- The live service-account credential a secret hash names (S4-004). `stale` says whether
-- last_used_at is older than the given interval; the caller updates it inside the tenant.
CREATE FUNCTION public.figcraft_service_credential(p_secret_hash BYTEA, p_last_used_interval_secs BIGINT)
RETURNS TABLE (credential_id TEXT, tenant_id TEXT, service_account_id TEXT, principal_id TEXT, scopes TEXT[], stale BOOLEAN)
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
BEGIN
    PERFORM set_config('figcraft.definer_lookup', 'on', true);
    RETURN QUERY
        SELECT c.id::text, c.tenant_id::text, a.id::text, a.principal_id::text, a.scopes,
               (c.last_used_at IS NULL OR c.last_used_at <= now() - make_interval(secs => GREATEST(p_last_used_interval_secs, 0)::double precision))
        FROM public.service_account_credentials c
        JOIN public.service_accounts a ON a.tenant_id = c.tenant_id AND a.id = c.service_account_id
        JOIN public.principals p ON p.id = a.principal_id
        JOIN public.tenants t ON t.id = c.tenant_id
        WHERE octet_length(p_secret_hash) = 32 AND c.secret_hash = p_secret_hash
          AND c.revoked_at IS NULL AND c.expires_at > now()
          AND a.state = 'active' AND p.state = 'active' AND t.state = 'active'
        LIMIT 1;
    PERFORM set_config('figcraft.definer_lookup', '', true);
END
$fn$;

-- The tenant an invitation token hash belongs to (S4-009); acceptance then runs in that tenant.
CREATE FUNCTION public.figcraft_invitation_tenant(p_token_hash BYTEA)
RETURNS TEXT
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
    found_tenant TEXT;
BEGIN
    PERFORM set_config('figcraft.definer_lookup', 'on', true);
    SELECT i.tenant_id::text INTO found_tenant FROM public.team_invitations i
    WHERE octet_length(p_token_hash) = 32 AND i.token_hash = p_token_hash
    LIMIT 1;
    PERFORM set_config('figcraft.definer_lookup', '', true);
    RETURN found_tenant;
END
$fn$;

-- The tenants where a principal has an active membership (the pre-selection listing), keyset
-- paged by tenant id, at most 101 rows. `p_include_closing` keeps deleting/deleted tenants.
CREATE FUNCTION public.figcraft_principal_tenants(p_principal_id UUID, p_after UUID, p_limit INTEGER, p_include_closing BOOLEAN)
RETURNS TABLE (id TEXT, slug TEXT, name TEXT, state TEXT, plan_id TEXT, home_region TEXT, role TEXT)
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
BEGIN
    PERFORM set_config('figcraft.definer_lookup', 'on', true);
    RETURN QUERY
        SELECT t.id::text, t.slug, t.name, t.state, t.plan_id, t.home_region, m.role
        FROM public.tenants t JOIN public.tenant_members m ON m.tenant_id = t.id
        WHERE m.principal_id = p_principal_id AND m.state = 'active'
          AND (p_include_closing OR t.state NOT IN ('deleting', 'deleted'))
          AND (p_after IS NULL OR t.id > p_after)
        ORDER BY t.id
        LIMIT LEAST(GREATEST(p_limit, 0), 101);
    PERFORM set_config('figcraft.definer_lookup', '', true);
END
$fn$;

REVOKE ALL ON FUNCTION public.figcraft_live_share_link(BYTEA, BIGINT) FROM PUBLIC;
REVOKE ALL ON FUNCTION public.figcraft_dead_share_links(UUID[], UUID[]) FROM PUBLIC;
REVOKE ALL ON FUNCTION public.figcraft_service_credential(BYTEA, BIGINT) FROM PUBLIC;
REVOKE ALL ON FUNCTION public.figcraft_invitation_tenant(BYTEA) FROM PUBLIC;
REVOKE ALL ON FUNCTION public.figcraft_principal_tenants(UUID, UUID, INTEGER, BOOLEAN) FROM PUBLIC;
