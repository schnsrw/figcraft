-- S2-017 synthetic fixture for snapshot metadata repository integration tests.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('93000000-0000-4000-8000-000000000009', 'user', 'Snapshot repository probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('93000000-0000-4000-8000-000000000001', 'repo-snap', 'Repository Snap', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('93000000-0000-4000-8000-000000000001', '93000000-0000-4000-8000-000000000009', 'tenant_owner', 'active');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('93100000-0000-4000-8000-000000000001', '93000000-0000-4000-8000-000000000001', 'Workspace Snap 1');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('93200000-0000-4000-8000-000000000001', '93000000-0000-4000-8000-000000000001', '93100000-0000-4000-8000-000000000001', 'Project Snap 1');
-- Doc one is the primary fixture document the test registers and transitions snapshots against.
-- Doc two exists only so the test can prove that pointing doc one's current_snapshot_id at a
-- snapshot that belongs to doc two is rejected by the composite foreign key.
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name) VALUES
    ('93300000-0000-4000-8000-000000000001', '93000000-0000-4000-8000-000000000001', '93100000-0000-4000-8000-000000000001', '93200000-0000-4000-8000-000000000001', 'Snap doc one'),
    ('93300000-0000-4000-8000-000000000002', '93000000-0000-4000-8000-000000000001', '93100000-0000-4000-8000-000000000001', '93200000-0000-4000-8000-000000000001', 'Snap doc two');
