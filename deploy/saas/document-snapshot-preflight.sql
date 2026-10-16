-- S3-008 synthetic fixture for document snapshot storage and room recovery integration tests.
-- Documents are created by the probes themselves (fresh ids per run) under these projects.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('a6000000-0000-4000-8000-000000000009', 'user', 'Document snapshot probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('a6000000-0000-4000-8000-000000000001', 'document-snapshots', 'Document Snapshots', 'test', 'local'),
    ('a6000000-0000-4000-8000-000000000002', 'document-snapshots-other', 'Document Snapshots Other', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('a6000000-0000-4000-8000-000000000001', 'a6000000-0000-4000-8000-000000000009', 'tenant_owner', 'active');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('a6100000-0000-4000-8000-000000000001', 'a6000000-0000-4000-8000-000000000001', 'Snapshot workspace'),
    ('a6100000-0000-4000-8000-000000000002', 'a6000000-0000-4000-8000-000000000002', 'Snapshot workspace other');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('a6200000-0000-4000-8000-000000000001', 'a6000000-0000-4000-8000-000000000001', 'a6100000-0000-4000-8000-000000000001', 'Snapshot project'),
    ('a6200000-0000-4000-8000-000000000002', 'a6000000-0000-4000-8000-000000000002', 'a6100000-0000-4000-8000-000000000002', 'Snapshot project other');
