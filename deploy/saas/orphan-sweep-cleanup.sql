-- Remove only the synthetic register-first / orphan-sweep fixture rows from the disposable database.
-- Pointer rows go first, because they hold foreign keys to objects, documents and libraries.
CREATE TEMPORARY TABLE orphan_fixture_tenants (id UUID PRIMARY KEY);
INSERT INTO orphan_fixture_tenants (id) VALUES
    ('b0000000-0000-4000-8000-000000000001'), ('b0000000-0000-4000-8000-000000000002');
UPDATE public.documents SET current_snapshot_id = NULL WHERE tenant_id IN (SELECT id FROM orphan_fixture_tenants);
DELETE FROM public.jobs WHERE tenant_id IN (SELECT id FROM orphan_fixture_tenants);
DELETE FROM public.library_versions WHERE tenant_id IN (SELECT id FROM orphan_fixture_tenants);
DELETE FROM public.document_snapshots WHERE tenant_id IN (SELECT id FROM orphan_fixture_tenants);
DELETE FROM public.upload_reservations WHERE tenant_id IN (SELECT id FROM orphan_fixture_tenants);
DELETE FROM public.object_references WHERE tenant_id IN (SELECT id FROM orphan_fixture_tenants);
DELETE FROM public.objects WHERE tenant_id IN (SELECT id FROM orphan_fixture_tenants);
DELETE FROM public.libraries WHERE tenant_id IN (SELECT id FROM orphan_fixture_tenants);
DELETE FROM public.documents WHERE tenant_id IN (SELECT id FROM orphan_fixture_tenants);
DELETE FROM public.projects WHERE tenant_id IN (SELECT id FROM orphan_fixture_tenants);
DELETE FROM public.workspaces WHERE tenant_id IN (SELECT id FROM orphan_fixture_tenants);
DELETE FROM public.tenant_members WHERE tenant_id IN (SELECT id FROM orphan_fixture_tenants);
DELETE FROM public.tenants WHERE id IN (SELECT id FROM orphan_fixture_tenants);
DELETE FROM public.principals WHERE id = 'b0000000-0000-4000-8000-000000000009';
DROP TABLE orphan_fixture_tenants;
