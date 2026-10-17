-- S3-012 synthetic fixture for export result and thumbnail integration tests.
-- Principal 9 is an active editor of tenant 1; principal a is an active editor of tenant 2 only.
-- Each test run inserts its own documents (ids aa300000-0000-4000-8000-<random>) into the
-- workspaces and projects below, so object keys never repeat across reruns.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('aa000000-0000-4000-8000-000000000009', 'user', 'Export probe member'),
    ('aa000000-0000-4000-8000-00000000000a', 'user', 'Export probe other-tenant member');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('aa000000-0000-4000-8000-000000000001', 'exports-probe', 'Exports Probe', 'test', 'local'),
    ('aa000000-0000-4000-8000-000000000002', 'exports-other', 'Exports Other', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('aa000000-0000-4000-8000-000000000001', 'aa000000-0000-4000-8000-000000000009', 'editor', 'active'),
    ('aa000000-0000-4000-8000-000000000002', 'aa000000-0000-4000-8000-00000000000a', 'editor', 'active');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('aa100000-0000-4000-8000-000000000001', 'aa000000-0000-4000-8000-000000000001', 'Export workspace'),
    ('aa100000-0000-4000-8000-000000000002', 'aa000000-0000-4000-8000-000000000002', 'Export other workspace');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('aa200000-0000-4000-8000-000000000001', 'aa000000-0000-4000-8000-000000000001', 'aa100000-0000-4000-8000-000000000001', 'Export project'),
    ('aa200000-0000-4000-8000-000000000002', 'aa000000-0000-4000-8000-000000000002', 'aa100000-0000-4000-8000-000000000002', 'Export other project');
-- S4-013: completing an export re-checks that the requester may still export the document, so
-- each probe member is a viewer of its tenant's export workspace.
INSERT INTO public.workspace_members (tenant_id, workspace_id, principal_id, role) VALUES
    ('aa000000-0000-4000-8000-000000000001', 'aa100000-0000-4000-8000-000000000001', 'aa000000-0000-4000-8000-000000000009', 'viewer'),
    ('aa000000-0000-4000-8000-000000000002', 'aa100000-0000-4000-8000-000000000002', 'aa000000-0000-4000-8000-00000000000a', 'viewer');
