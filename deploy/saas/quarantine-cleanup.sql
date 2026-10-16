-- Remove only the synthetic quarantine fixture rows from the disposable database. Events and audit
-- events are append-only for the runtime role, so this runs as the administrator.
DELETE FROM public.audit_events WHERE tenant_id IN ('af000000-0000-4000-8000-000000000001', 'af000000-0000-4000-8000-000000000002');
DELETE FROM public.document_events WHERE tenant_id IN ('af000000-0000-4000-8000-000000000001', 'af000000-0000-4000-8000-000000000002');
DELETE FROM public.documents WHERE tenant_id IN ('af000000-0000-4000-8000-000000000001', 'af000000-0000-4000-8000-000000000002');
DELETE FROM public.projects WHERE tenant_id IN ('af000000-0000-4000-8000-000000000001', 'af000000-0000-4000-8000-000000000002');
DELETE FROM public.workspaces WHERE tenant_id IN ('af000000-0000-4000-8000-000000000001', 'af000000-0000-4000-8000-000000000002');
DELETE FROM public.tenant_members WHERE tenant_id IN ('af000000-0000-4000-8000-000000000001', 'af000000-0000-4000-8000-000000000002');
DELETE FROM public.tenants WHERE id IN ('af000000-0000-4000-8000-000000000001', 'af000000-0000-4000-8000-000000000002');
DELETE FROM public.principals WHERE id = 'af000000-0000-4000-8000-000000000009';
