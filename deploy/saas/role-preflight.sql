-- S2-006 synthetic privilege checks, plus the S4-014 row-level security checks. DML and DDL
-- probes are rolled back.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM pg_roles
        WHERE rolname IN ('figcraft_migrator', 'figcraft_runtime')
            AND (rolinherit OR rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls)
    ) THEN
        RAISE EXCEPTION 'cloud database roles have elevated attributes';
    END IF;
    IF (SELECT count(*) FROM pg_roles WHERE rolname IN ('figcraft_migrator', 'figcraft_runtime')) <> 2 THEN
        RAISE EXCEPTION 'cloud database roles are missing';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_auth_members membership
        JOIN pg_roles member_role ON member_role.oid = membership.member
        WHERE member_role.rolname IN ('figcraft_migrator', 'figcraft_runtime')
    ) THEN
        RAISE EXCEPTION 'cloud database roles have memberships';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_tables
        WHERE schemaname = 'public' AND tableowner <> 'figcraft_migrator'
    ) THEN
        RAISE EXCEPTION 'application tables are not owned by the migrator';
    END IF;
    -- Every migrated table must be in the provisioning grant list; a missing one is unreadable at
    -- runtime even though its migration applied.
    IF EXISTS (
        SELECT 1 FROM pg_class
        WHERE relnamespace = 'public'::regnamespace AND relkind IN ('r', 'p')
            AND NOT has_table_privilege('figcraft_runtime', oid, 'SELECT')
    ) THEN
        RAISE EXCEPTION 'runtime role cannot read every application table';
    END IF;
    IF has_schema_privilege('figcraft_runtime', 'public', 'CREATE') THEN
        RAISE EXCEPTION 'runtime role can create schema objects';
    END IF;
    IF has_table_privilege('figcraft_runtime', 'public._sqlx_migrations', 'INSERT')
        OR has_table_privilege('figcraft_runtime', 'public._sqlx_migrations', 'UPDATE')
        OR has_table_privilege('figcraft_runtime', 'public._sqlx_migrations', 'DELETE') THEN
        RAISE EXCEPTION 'runtime role can mutate the migration ledger';
    END IF;
    IF has_table_privilege('figcraft_runtime', 'public.document_events', 'DELETE')
        OR has_table_privilege('figcraft_runtime', 'public.audit_events', 'DELETE') THEN
        RAISE EXCEPTION 'runtime role can delete append-only records';
    END IF;
    -- S4-014: every tenant-owned table (a tenant_id column, plus tenants) has row-level security
    -- enabled and forced, with the tenant_isolation policy.
    IF EXISTS (
        SELECT 1 FROM pg_class c
        WHERE c.relnamespace = 'public'::regnamespace AND c.relkind IN ('r', 'p')
            AND (c.relname = 'tenants' OR EXISTS (
                SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid AND a.attname = 'tenant_id' AND NOT a.attisdropped))
            AND c.relname <> 'tenant_context_probe'
            AND (NOT c.relrowsecurity OR NOT c.relforcerowsecurity
                OR NOT EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid AND p.polname = 'tenant_isolation'))
    ) THEN
        RAISE EXCEPTION 'a tenant-owned table lacks forced row-level security';
    END IF;
    IF (SELECT count(*) FROM pg_class c
        WHERE c.relnamespace = 'public'::regnamespace AND c.relkind IN ('r', 'p')
            AND c.relrowsecurity AND c.relforcerowsecurity) < 36 THEN
        RAISE EXCEPTION 'fewer tenant-owned tables than expected have row-level security';
    END IF;
    -- The pre-tenant lookups are SECURITY DEFINER, owned by the migrator, with a pinned search
    -- path, executable by the runtime role and not by PUBLIC.
    IF (SELECT count(*) FROM pg_proc p
        WHERE p.pronamespace = 'public'::regnamespace AND p.prosecdef
            AND pg_get_userbyid(p.proowner) = 'figcraft_migrator'
            AND p.proconfig @> ARRAY['search_path=pg_catalog, pg_temp']
            AND has_function_privilege('figcraft_runtime', p.oid, 'EXECUTE')
            AND NOT EXISTS (SELECT 1 FROM aclexplode(coalesce(p.proacl, acldefault('f', p.proowner))) acl
                            WHERE acl.grantee = 0 AND acl.privilege_type = 'EXECUTE')
            AND p.proname IN ('figcraft_live_share_link', 'figcraft_dead_share_links', 'figcraft_service_credential',
                              'figcraft_invitation_tenant', 'figcraft_principal_tenants')) <> 5 THEN
        RAISE EXCEPTION 'pre-tenant lookup functions are missing or mis-provisioned';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_proc p
        WHERE p.pronamespace = 'public'::regnamespace AND p.prosecdef
            AND p.proname NOT IN ('figcraft_live_share_link', 'figcraft_dead_share_links', 'figcraft_service_credential',
                                  'figcraft_invitation_tenant', 'figcraft_principal_tenants')
    ) THEN
        RAISE EXCEPTION 'unexpected SECURITY DEFINER function';
    END IF;
END;
$$;

BEGIN;
SET ROLE figcraft_runtime;
SELECT count(*) FROM public._sqlx_migrations;
DO $$
DECLARE
    blocked BOOLEAN := false;
BEGIN
    -- S4-014: without a tenant context the runtime role sees no tenant rows and cannot write one.
    IF EXISTS (SELECT 1 FROM public.tenants) OR EXISTS (SELECT 1 FROM public.tenant_members) THEN
        RAISE EXCEPTION 'runtime role sees tenant rows without a tenant context';
    END IF;
    BEGIN
        INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
            ('a0000000-0000-4000-8000-000000000009', 'role-probe-no-context', 'Role probe', 'test', 'local');
    EXCEPTION WHEN insufficient_privilege THEN
        blocked := true;
    END;
    IF NOT blocked THEN
        RAISE EXCEPTION 'runtime role wrote a tenant row without a tenant context';
    END IF;
END;
$$;
SELECT set_config('figcraft.tenant_id', 'a0000000-0000-4000-8000-000000000001', true);
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('a0000000-0000-4000-8000-000000000001', 'role-probe', 'Role probe', 'test', 'local');
INSERT INTO public.audit_events (id, tenant_id, action, resource_kind, outcome) VALUES
    ('a0000000-0000-4000-8000-000000000002', 'a0000000-0000-4000-8000-000000000001', 'role.probe', 'tenant', 'succeeded');
DO $$
DECLARE
    blocked BOOLEAN := false;
BEGIN
    BEGIN
        EXECUTE 'CREATE TABLE public.runtime_must_not_create (id INTEGER)';
    EXCEPTION WHEN insufficient_privilege THEN
        blocked := true;
    END;
    IF NOT blocked THEN
        RAISE EXCEPTION 'runtime role created a table';
    END IF;

    blocked := false;
    BEGIN
        EXECUTE 'ALTER TABLE public.tenants ADD COLUMN runtime_must_not_alter INTEGER';
    EXCEPTION WHEN insufficient_privilege THEN
        blocked := true;
    END;
    IF NOT blocked THEN
        RAISE EXCEPTION 'runtime role altered an application table';
    END IF;

    blocked := false;
    BEGIN
        EXECUTE 'DELETE FROM public.audit_events WHERE false';
    EXCEPTION WHEN insufficient_privilege THEN
        blocked := true;
    END;
    IF NOT blocked THEN
        RAISE EXCEPTION 'runtime role deleted audit records';
    END IF;
END;
$$;
ROLLBACK;

BEGIN;
SET ROLE figcraft_migrator;
CREATE TABLE public.migrator_role_probe (id INTEGER);
ROLLBACK;
