-- Remove only the synthetic legacy import fixture rows from the disposable database. Events are
-- append-only for the runtime role, so this runs as the administrator.
UPDATE public.documents SET current_snapshot_id = NULL WHERE tenant_id = 'ad000000-0000-4000-8000-000000000001';
DELETE FROM public.object_references WHERE tenant_id = 'ad000000-0000-4000-8000-000000000001';
DELETE FROM public.document_snapshots WHERE tenant_id = 'ad000000-0000-4000-8000-000000000001';
DELETE FROM public.document_events WHERE tenant_id = 'ad000000-0000-4000-8000-000000000001';
DELETE FROM public.external_mappings WHERE tenant_id = 'ad000000-0000-4000-8000-000000000001';
DELETE FROM public.objects WHERE tenant_id = 'ad000000-0000-4000-8000-000000000001';
DELETE FROM public.documents WHERE tenant_id = 'ad000000-0000-4000-8000-000000000001';
DELETE FROM public.projects WHERE tenant_id = 'ad000000-0000-4000-8000-000000000001';
DELETE FROM public.workspaces WHERE tenant_id = 'ad000000-0000-4000-8000-000000000001';
DELETE FROM public.tenant_members WHERE tenant_id = 'ad000000-0000-4000-8000-000000000001';
DELETE FROM public.tenants WHERE id = 'ad000000-0000-4000-8000-000000000001';
DELETE FROM public.principals WHERE id = 'ad000000-0000-4000-8000-000000000009';
