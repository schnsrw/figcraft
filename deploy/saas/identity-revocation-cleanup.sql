-- Remove the synthetic S4-013/S4-017/S4-018 fixture rows (UUID prefix d5) that
-- crates/cloud-storage/tests/identity_revocation.rs and crates/cloud-relay/tests/member_revocation.rs
-- create with random tails on every run. Run as the database administrator on a disposable database
-- only: foreign-key and append-only triggers are bypassed for this session so every tenant-owned
-- table can be cleared in one pass. The tests need no preflight (they create their own rows).
-- Object-store bytes under tenants/d5.../ are left for the orphan sweep.
SET session_replication_role = replica;
DO $$
DECLARE
    owned RECORD;
BEGIN
    FOR owned IN
        SELECT c.table_name FROM information_schema.columns c
        JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name AND t.table_type = 'BASE TABLE'
        WHERE c.table_schema = 'public' AND c.column_name = 'tenant_id'
    LOOP
        EXECUTE format('DELETE FROM public.%I WHERE tenant_id::text LIKE %L', owned.table_name, 'd5%');
    END LOOP;
END
$$;
DELETE FROM public.browser_sessions WHERE principal_id::text LIKE 'd5%';
DELETE FROM public.support_access_grants WHERE granted_to::text LIKE 'd5%';
DELETE FROM public.tenants WHERE id::text LIKE 'd5%';
DELETE FROM public.principals WHERE id::text LIKE 'd5%';
SET session_replication_role = origin;
