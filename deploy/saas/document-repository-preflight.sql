-- S2-009 synthetic fixture for document lifecycle repository integration tests.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('c0000000-0000-4000-8000-000000000002', 'user', 'Document repository probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('e0000000-0000-4000-8000-000000000001', 'repo-e', 'Repository E', 'test', 'local'),
    ('f0000000-0000-4000-8000-000000000001', 'repo-f', 'Repository F', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('e0000000-0000-4000-8000-000000000001', 'c0000000-0000-4000-8000-000000000002', 'tenant_owner', 'active'),
    ('f0000000-0000-4000-8000-000000000001', 'c0000000-0000-4000-8000-000000000002', 'tenant_owner', 'active');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('e1000000-0000-4000-8000-000000000001', 'e0000000-0000-4000-8000-000000000001', 'Workspace E1'),
    ('f1000000-0000-4000-8000-000000000001', 'f0000000-0000-4000-8000-000000000001', 'Workspace F1');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('e2000000-0000-4000-8000-000000000001', 'e0000000-0000-4000-8000-000000000001', 'e1000000-0000-4000-8000-000000000001', 'Project E1'),
    ('f2000000-0000-4000-8000-000000000001', 'f0000000-0000-4000-8000-000000000001', 'f1000000-0000-4000-8000-000000000001', 'Project F1');
-- Doc one starts active and is driven through rename/archive/soft-delete/restore by the test.
-- Doc two starts pre-archived; doc three starts mid soft-delete grace period; doc four is already
-- in the terminal 'deleted' state, which no repository transition may leave.
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name, state) VALUES
    ('e3000000-0000-4000-8000-000000000001', 'e0000000-0000-4000-8000-000000000001', 'e1000000-0000-4000-8000-000000000001', 'e2000000-0000-4000-8000-000000000001', 'Doc one', 'active'),
    ('e3000000-0000-4000-8000-000000000002', 'e0000000-0000-4000-8000-000000000001', 'e1000000-0000-4000-8000-000000000001', 'e2000000-0000-4000-8000-000000000001', 'Doc two', 'archived'),
    ('e3000000-0000-4000-8000-000000000004', 'e0000000-0000-4000-8000-000000000001', 'e1000000-0000-4000-8000-000000000001', 'e2000000-0000-4000-8000-000000000001', 'Doc four', 'deleted');
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name, state, deleted_at) VALUES
    ('e3000000-0000-4000-8000-000000000003', 'e0000000-0000-4000-8000-000000000001', 'e1000000-0000-4000-8000-000000000001', 'e2000000-0000-4000-8000-000000000001', 'Doc three', 'deleting', now());
