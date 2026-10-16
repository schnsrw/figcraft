-- Remove only the synthetic event repository fixture rows from the disposable database. Events are
-- append-only for the runtime role, so this runs as the administrator.
DELETE FROM public.document_events WHERE tenant_id IN ('94000000-0000-4000-8000-000000000001', '94000000-0000-4000-8000-000000000002');
DELETE FROM public.documents WHERE tenant_id = '94000000-0000-4000-8000-000000000001';
DELETE FROM public.projects WHERE tenant_id = '94000000-0000-4000-8000-000000000001';
DELETE FROM public.workspaces WHERE tenant_id = '94000000-0000-4000-8000-000000000001';
DELETE FROM public.tenant_members WHERE tenant_id = '94000000-0000-4000-8000-000000000001';
DELETE FROM public.tenants WHERE id IN ('94000000-0000-4000-8000-000000000001', '94000000-0000-4000-8000-000000000002');
DELETE FROM public.principals WHERE id = '94000000-0000-4000-8000-000000000009';
