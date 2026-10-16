-- Remove only the synthetic S2-020 failure-injection fixture rows from the disposable database.
-- Events are append-only for the runtime role, so this runs as the administrator.
DELETE FROM public.idempotency_keys WHERE tenant_id = '98000000-0000-4000-8000-000000000001';
DELETE FROM public.document_events WHERE tenant_id = '98000000-0000-4000-8000-000000000001';
DELETE FROM public.documents WHERE tenant_id = '98000000-0000-4000-8000-000000000001';
DELETE FROM public.projects WHERE tenant_id = '98000000-0000-4000-8000-000000000001';
DELETE FROM public.workspaces WHERE tenant_id = '98000000-0000-4000-8000-000000000001';
DELETE FROM public.tenant_members WHERE tenant_id = '98000000-0000-4000-8000-000000000001';
DELETE FROM public.tenants WHERE id = '98000000-0000-4000-8000-000000000001';
DELETE FROM public.principals WHERE id = '98000000-0000-4000-8000-000000000009';
