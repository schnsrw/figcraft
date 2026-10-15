-- S2-008 synthetic fixture for bounded, tenant-scoped repository integration tests.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('c0000000-0000-4000-8000-000000000001', 'user', 'Repository probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('a0000000-0000-4000-8000-000000000001', 'repo-a', 'Repository A', 'test', 'local'),
    ('b0000000-0000-4000-8000-000000000002', 'repo-b', 'Repository B', 'test', 'local'),
    ('c0000000-0000-4000-8000-000000000003', 'repo-c', 'Repository C', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('a0000000-0000-4000-8000-000000000001', 'c0000000-0000-4000-8000-000000000001', 'tenant_owner', 'active'),
    ('b0000000-0000-4000-8000-000000000002', 'c0000000-0000-4000-8000-000000000001', 'tenant_owner', 'active'),
    ('c0000000-0000-4000-8000-000000000003', 'c0000000-0000-4000-8000-000000000001', 'viewer', 'suspended');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('a1000000-0000-4000-8000-000000000001', 'a0000000-0000-4000-8000-000000000001', 'Workspace A1'),
    ('a1000000-0000-4000-8000-000000000002', 'a0000000-0000-4000-8000-000000000001', 'Workspace A2'),
    ('b1000000-0000-4000-8000-000000000001', 'b0000000-0000-4000-8000-000000000002', 'Workspace B1');
INSERT INTO public.workspaces (id, tenant_id, name)
    SELECT gen_random_uuid(), 'a0000000-0000-4000-8000-000000000001', 'Bulk workspace ' || ordinal
    FROM generate_series(1, 105) AS ordinal;
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('a2000000-0000-4000-8000-000000000001', 'a0000000-0000-4000-8000-000000000001', 'a1000000-0000-4000-8000-000000000001', 'Project A1'),
    ('b2000000-0000-4000-8000-000000000001', 'b0000000-0000-4000-8000-000000000002', 'b1000000-0000-4000-8000-000000000001', 'Project B1');
INSERT INTO public.projects (id, tenant_id, workspace_id, name)
    SELECT gen_random_uuid(), 'a0000000-0000-4000-8000-000000000001', 'a1000000-0000-4000-8000-000000000001', 'Bulk project ' || ordinal
    FROM generate_series(1, 105) AS ordinal;
