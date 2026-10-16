-- S3-003 synthetic fixture for object metadata repository integration tests.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('a1000000-0000-4000-8000-000000000009', 'user', 'Object repository probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('a1000000-0000-4000-8000-000000000001', 'objects-probe', 'Objects Probe', 'test', 'local'),
    ('a1000000-0000-4000-8000-000000000002', 'objects-other', 'Objects Other', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('a1000000-0000-4000-8000-000000000001', 'a1000000-0000-4000-8000-000000000009', 'tenant_owner', 'active');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('a1100000-0000-4000-8000-000000000001', 'a1000000-0000-4000-8000-000000000001', 'Workspace Objects 1');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('a1200000-0000-4000-8000-000000000001', 'a1000000-0000-4000-8000-000000000001', 'a1100000-0000-4000-8000-000000000001', 'Project Objects 1');
-- Doc one accepts edits; doc two is archived and must refuse them.
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name, state) VALUES
    ('a1300000-0000-4000-8000-000000000001', 'a1000000-0000-4000-8000-000000000001', 'a1100000-0000-4000-8000-000000000001', 'a1200000-0000-4000-8000-000000000001', 'Objects doc one', 'active'),
    ('a1300000-0000-4000-8000-000000000002', 'a1000000-0000-4000-8000-000000000001', 'a1100000-0000-4000-8000-000000000001', 'a1200000-0000-4000-8000-000000000001', 'Objects doc two', 'archived');
