-- S2-006 synthetic privilege checks. DML and DDL probes are rolled back.
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
        SELECT 1 FROM pg_class
        WHERE relname IN ('_sqlx_migrations', 'tenants', 'documents', 'document_events', 'audit_events')
            AND relnamespace = 'public'::regnamespace
            AND pg_get_userbyid(relowner) <> 'figcraft_migrator'
    ) THEN
        RAISE EXCEPTION 'application tables are not owned by the migrator';
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
END;
$$;

BEGIN;
SET ROLE figcraft_runtime;
SELECT count(*) FROM public._sqlx_migrations;
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
