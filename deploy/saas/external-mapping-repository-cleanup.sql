-- Remove only the synthetic S2-010 fixture rows from the disposable database.
-- external_mappings is cleared first because its tenant_id foreign key is ON DELETE RESTRICT; the
-- repository integration test commits one mapping row under this tenant to exercise cross-tenant
-- visibility, so this also removes that row rather than only the preflight-inserted rows.
DELETE FROM public.external_mappings WHERE tenant_id = '90000000-0000-4000-8000-000000000001';
DELETE FROM public.tenant_members WHERE tenant_id = '90000000-0000-4000-8000-000000000001';
DELETE FROM public.tenants WHERE id = '90000000-0000-4000-8000-000000000001';
DELETE FROM public.principals WHERE id = 'c0000000-0000-4000-8000-000000000003';
