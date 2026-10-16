-- S3-009 synthetic fixture for content-addressed asset integration tests.
-- Principal 9 is an active member of tenants 1 and 2; principal b is in no tenant. Tenant 1 has
-- active documents 21 and 22, archived document 23 and soft-deleted document 24; tenant 2 has
-- active document 25.
INSERT INTO public.principals (id, kind, display_name, state) VALUES
    ('a7000000-0000-4000-8000-000000000009', 'user', 'Asset probe member', 'active'),
    ('a7000000-0000-4000-8000-00000000000b', 'user', 'Asset probe outsider', 'active');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region, state) VALUES
    ('a7000000-0000-4000-8000-000000000001', 'assets-probe', 'Assets Probe', 'test', 'local', 'active'),
    ('a7000000-0000-4000-8000-000000000002', 'assets-other', 'Assets Other', 'test', 'local', 'active');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('a7000000-0000-4000-8000-000000000001', 'a7000000-0000-4000-8000-000000000009', 'editor', 'active'),
    ('a7000000-0000-4000-8000-000000000002', 'a7000000-0000-4000-8000-000000000009', 'editor', 'active');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('a7000000-0000-4000-8000-000000000011', 'a7000000-0000-4000-8000-000000000001', 'Asset workspace 1'),
    ('a7000000-0000-4000-8000-000000000012', 'a7000000-0000-4000-8000-000000000002', 'Asset workspace 2');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('a7000000-0000-4000-8000-000000000013', 'a7000000-0000-4000-8000-000000000001', 'a7000000-0000-4000-8000-000000000011', 'Asset project 1'),
    ('a7000000-0000-4000-8000-000000000014', 'a7000000-0000-4000-8000-000000000002', 'a7000000-0000-4000-8000-000000000012', 'Asset project 2');
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name, state) VALUES
    ('a7000000-0000-4000-8000-000000000021', 'a7000000-0000-4000-8000-000000000001', 'a7000000-0000-4000-8000-000000000011', 'a7000000-0000-4000-8000-000000000013', 'Asset doc one', 'active'),
    ('a7000000-0000-4000-8000-000000000022', 'a7000000-0000-4000-8000-000000000001', 'a7000000-0000-4000-8000-000000000011', 'a7000000-0000-4000-8000-000000000013', 'Asset doc two', 'active'),
    ('a7000000-0000-4000-8000-000000000023', 'a7000000-0000-4000-8000-000000000001', 'a7000000-0000-4000-8000-000000000011', 'a7000000-0000-4000-8000-000000000013', 'Asset doc archived', 'archived'),
    ('a7000000-0000-4000-8000-000000000025', 'a7000000-0000-4000-8000-000000000002', 'a7000000-0000-4000-8000-000000000012', 'a7000000-0000-4000-8000-000000000014', 'Asset doc other tenant', 'active');
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name, state, deleted_at) VALUES
    ('a7000000-0000-4000-8000-000000000024', 'a7000000-0000-4000-8000-000000000001', 'a7000000-0000-4000-8000-000000000011', 'a7000000-0000-4000-8000-000000000013', 'Asset doc deleting', 'deleting', now());
