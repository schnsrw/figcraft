-- Remove only the synthetic S3-015 garbage-collection fixture rows from the disposable database.
-- Pointer rows go first, because they hold foreign keys to objects, documents and jobs.
CREATE TEMPORARY TABLE gc_fixture_tenants (id UUID PRIMARY KEY);
INSERT INTO gc_fixture_tenants (id) VALUES
    ('ab000000-0000-4000-8000-000000000001'), ('ab000000-0000-4000-8000-000000000002'),
    ('ab000000-0000-4000-8000-000000000003'), ('ab000000-0000-4000-8000-000000000004'),
    ('ab000000-0000-4000-8000-000000000005'), ('ab000000-0000-4000-8000-000000000006');
UPDATE public.documents SET current_snapshot_id = NULL WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.exports WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.jobs WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.document_thumbnails WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.document_snapshots WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.library_versions WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.plugin_versions WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.upload_reservations WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.retention_holds WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.object_references WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.objects WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.libraries WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.plugins WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.documents WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.projects WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.workspaces WHERE tenant_id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.tenants WHERE id IN (SELECT id FROM gc_fixture_tenants);
DELETE FROM public.principals WHERE id = 'ab000000-0000-4000-8000-000000000009';
DROP TABLE gc_fixture_tenants;
