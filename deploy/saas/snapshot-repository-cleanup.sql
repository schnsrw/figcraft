-- Remove only the synthetic S2-017 fixture rows from the disposable database.
-- `documents.current_snapshot_id` and `document_snapshots.document_id` are a circular foreign-key
-- pair; clearing the pointer first keeps this cleanup correct even if a future test commits one
-- (today's test rolls its snapshot work back, so this is currently a no-op).
UPDATE public.documents SET current_snapshot_id = NULL WHERE tenant_id = '93000000-0000-4000-8000-000000000001';
DELETE FROM public.document_snapshots WHERE tenant_id = '93000000-0000-4000-8000-000000000001';
DELETE FROM public.documents WHERE tenant_id = '93000000-0000-4000-8000-000000000001';
DELETE FROM public.projects WHERE tenant_id = '93000000-0000-4000-8000-000000000001';
DELETE FROM public.workspaces WHERE tenant_id = '93000000-0000-4000-8000-000000000001';
DELETE FROM public.tenant_members WHERE tenant_id = '93000000-0000-4000-8000-000000000001';
DELETE FROM public.tenants WHERE id = '93000000-0000-4000-8000-000000000001';
DELETE FROM public.principals WHERE id = '93000000-0000-4000-8000-000000000009';
