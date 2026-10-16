-- Remove only the synthetic S2 gate fixture rows from the disposable database. Events and audit
-- events are append-only for the runtime role, so this runs as the administrator. The workspace,
-- project and document are created by the probe itself, so they are removed by tenant.
DELETE FROM public.document_events WHERE tenant_id = '99000000-0000-4000-8000-000000000001';
DELETE FROM public.audit_events WHERE tenant_id = '99000000-0000-4000-8000-000000000001';
DELETE FROM public.documents WHERE tenant_id = '99000000-0000-4000-8000-000000000001';
DELETE FROM public.projects WHERE tenant_id = '99000000-0000-4000-8000-000000000001';
DELETE FROM public.workspaces WHERE tenant_id = '99000000-0000-4000-8000-000000000001';
DELETE FROM public.tenant_members WHERE tenant_id = '99000000-0000-4000-8000-000000000001';
DELETE FROM public.tenants WHERE id = '99000000-0000-4000-8000-000000000001';
DELETE FROM public.principals WHERE id = '99000000-0000-4000-8000-000000000009';
