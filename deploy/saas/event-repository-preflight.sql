-- S2-014 synthetic fixture for document event repository integration tests.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('94000000-0000-4000-8000-000000000009', 'user', 'Event repository probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('94000000-0000-4000-8000-000000000001', 'repo-events', 'Repository Events', 'test', 'local'),
    ('94000000-0000-4000-8000-000000000002', 'repo-events-other', 'Repository Events Other', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('94000000-0000-4000-8000-000000000001', '94000000-0000-4000-8000-000000000009', 'tenant_owner', 'active');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('94100000-0000-4000-8000-000000000001', '94000000-0000-4000-8000-000000000001', 'Workspace Events 1');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('94200000-0000-4000-8000-000000000001', '94000000-0000-4000-8000-000000000001', '94100000-0000-4000-8000-000000000001', 'Project Events 1');
-- Doc one accepts edits; doc two is archived and must refuse them.
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name, state) VALUES
    ('94300000-0000-4000-8000-000000000001', '94000000-0000-4000-8000-000000000001', '94100000-0000-4000-8000-000000000001', '94200000-0000-4000-8000-000000000001', 'Events doc one', 'active'),
    ('94300000-0000-4000-8000-000000000002', '94000000-0000-4000-8000-000000000001', '94100000-0000-4000-8000-000000000001', '94200000-0000-4000-8000-000000000001', 'Events doc two', 'archived');
