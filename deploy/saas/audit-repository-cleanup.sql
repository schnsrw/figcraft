-- Remove only the synthetic S2-012 fixture rows from the disposable database.
DELETE FROM public.audit_events WHERE tenant_id = '92000000-0000-4000-8000-000000000001';
DELETE FROM public.tenant_members WHERE tenant_id = '92000000-0000-4000-8000-000000000001';
DELETE FROM public.tenants WHERE id = '92000000-0000-4000-8000-000000000001';
DELETE FROM public.principals WHERE id = '92000000-0000-4000-8000-000000000002';
